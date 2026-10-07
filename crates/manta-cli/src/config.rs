//! MAN-261: the daemon's single config file. One TOML parse, a strict
//! top-level check, the `MANTA_*` environment overlay, then one typed parse
//! per table. CLI flags are merged on top in main.rs (`resolve`), giving the
//! precedence CLI flag > `MANTA_<TABLE>_<KEY>` > file > built-in default.
//! See docs/SPEC-decode-core.md §9 and
//! docs/DECISIONS/2026-10-06-man261-config-surface.md.
//!
//! `manta_server::config::DaemonConfigFile` and
//! `manta_decode::config_file::DecodeConfigFile` stay deliberately permissive
//! at the top level: this module is the only place that knows the full table
//! set, so strictness about unknown tables lives here, once.

use anyhow::{anyhow, bail, Context, Result};
use manta_decode::config_file::DecodeConfigToml;
use manta_decode::decoder::DecodeConfig;
use manta_engine::config_file::DetectorConfigToml;
use manta_engine::DetectorConfig;
use manta_server::config::{RbnUplinkConfig, ServerConfig};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Every top-level table manta reads. Adding a table means adding it here.
pub(crate) const KNOWN_TABLES: &[&str] = &[
    "server",
    "rbn_uplink",
    "input",
    "spot",
    "detector",
    "decode",
];

/// SPEC §9 keys that are still compile-time constants (D3): rejected with a
/// specific message rather than serde's generic `unknown field`.
const CONSTANT_ONLY_KEYS: &[(&str, &str, &str)] = &[
    ("detector", "floor_quantile", "manta-dsp::floor"),
    ("detector", "floor_window_ms", "manta-dsp::floor"),
    ("detector", "block_channels", "manta-dsp::floor"),
    ("detector", "block_allowance_db", "manta-dsp::floor"),
    ("decode", "mu_ratio_bounds", "manta-decode::timing"),
    ("decode", "char_gap_dits", "manta-decode::timing"),
    ("decode", "word_gap_dits", "manta-decode::timing"),
    ("decode", "cluster_alpha", "manta-decode::timing"),
];

const ENV_PREFIX: &str = "MANTA_";
/// The `--config` fallback (run/soak/doctor only).
pub(crate) const ENV_CONFIG: &str = "MANTA_CONFIG";
const ENV_TABLES: &[(&str, &str)] = &[
    ("SERVER_", "server"),
    ("INPUT_", "input"),
    ("SPOT_", "spot"),
    ("DETECTOR_", "detector"),
    ("DECODE_", "decode"),
];
/// Build-time only (crates/manta-cli/build.rs), never runtime config.
const ENV_IGNORED: &[&str] = &["MANTA_GIT_SHA"];
/// String-typed keys: an env value is taken verbatim, never TOML-probed, so
/// `MANTA_INPUT_PASSWORD=12345678` stays a string.
const STRING_TYPED_ENV_KEYS: &[(&str, &str)] = &[
    ("server", "station_callsign"),
    ("server", "bind_addr"),
    ("input", "type"),
    ("input", "device"),
    ("input", "path"),
    ("input", "host"),
    ("input", "password"),
    ("input", "driver"),
    ("spot", "blocklist_path"),
    ("spot", "notch_path"),
    ("decode", "engine"),
];

/// Whether `load` applies the `MANTA_*` overlay. `decode`/`oracle` pass
/// `Ignore`: they are the deterministic golden-vector and measurement tools,
/// and their output must not depend on the ambient environment (D8).
pub(crate) enum Env<'a> {
    Ignore,
    Read(&'a [(OsString, OsString)]),
}

/// Every table of the resolved file + environment, typed and validated.
#[derive(Debug)]
pub(crate) struct Loaded {
    /// `path.display()` for messages, or "environment" when no file was given.
    pub origin: String,
    pub server: Option<ServerConfig>,
    pub rbn_uplink: Vec<RbnUplinkConfig>,
    pub decode: DecodeConfig,
    pub detector: DetectorConfig,
    pub spot: SpotFile,
    pub input: InputFile,
    /// Top-level tables present after the overlay (for "ignored" notes).
    pub present: BTreeSet<String>,
}

/// `[spot]`, with relative file paths already resolved (file values against
/// the config file's directory, env values against the CWD).
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct SpotFile {
    pub allowlist: Vec<String>,
    pub blocklist_path: Option<PathBuf>,
    pub notch_path: Option<PathBuf>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SpotToml {
    allowlist: Option<Vec<String>>,
    blocklist_path: Option<PathBuf>,
    notch_path: Option<PathBuf>,
}

/// `input.type` (D5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum InputKind {
    Audio,
    File,
    Kiwi,
    Soapy,
    Hpsdr,
}

impl InputKind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            InputKind::Audio => "audio",
            InputKind::File => "file",
            InputKind::Kiwi => "kiwi",
            InputKind::Soapy => "soapy",
            InputKind::Hpsdr => "hpsdr",
        }
    }

    /// Source keys this type accepts; the shared keys are valid with any type.
    fn keys(self) -> &'static [&'static str] {
        match self {
            InputKind::Audio => &["device"],
            InputKind::File => &["path", "iq"],
            InputKind::Kiwi => &["host", "port", "freq_hz", "password"],
            InputKind::Soapy => &["driver", "freq_hz", "rate_hz", "gain_db"],
            InputKind::Hpsdr => &["host", "port", "freq_hz", "rate_hz"],
        }
    }
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct InputToml {
    #[serde(rename = "type")]
    kind: Option<InputKind>,
    device: Option<String>,
    path: Option<PathBuf>,
    iq: Option<bool>,
    host: Option<String>,
    port: Option<u16>,
    freq_hz: Option<f64>,
    password: Option<String>,
    driver: Option<String>,
    rate_hz: Option<f64>,
    gain_db: Option<f64>,
    // Shared keys, valid with any (or no) type.
    freq_correction_ppm: Option<f64>,
    center_freq_hz: Option<f64>,
    capture_rate_hz: Option<f64>,
    replay_epoch: Option<i64>,
}

/// `[input]`: the source it describes (`None` when untyped) plus the shared
/// keys.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct InputFile {
    pub source: Option<SourceFromFile>,
    pub shared: SharedInput,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct SharedInput {
    pub freq_correction_ppm: Option<f64>,
    pub center_freq_hz: Option<f64>,
    pub capture_rate_hz: Option<f64>,
    pub replay_epoch: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SourceFromFile {
    Audio {
        device: Option<String>,
    },
    File {
        path: PathBuf,
        iq: bool,
    },
    Kiwi {
        host: String,
        port: u16,
        freq_hz: f64,
        password: String,
    },
    Soapy {
        driver: String,
        freq_hz: f64,
        rate_hz: f64,
        gain_db: Option<f64>,
    },
    Hpsdr {
        host: String,
        port: u16,
        freq_hz: f64,
        rate_hz: f64,
    },
}

impl SourceFromFile {
    pub(crate) fn kind(&self) -> InputKind {
        match self {
            SourceFromFile::Audio { .. } => InputKind::Audio,
            SourceFromFile::File { .. } => InputKind::File,
            SourceFromFile::Kiwi { .. } => InputKind::Kiwi,
            SourceFromFile::Soapy { .. } => InputKind::Soapy,
            SourceFromFile::Hpsdr { .. } => InputKind::Hpsdr,
        }
    }
}

/// `--config`'s fallback: a non-empty `MANTA_CONFIG`, read as an `OsString`
/// so a non-UTF-8 path works.
pub(crate) fn config_path_from_env(vars: &[(OsString, OsString)]) -> Option<PathBuf> {
    vars.iter()
        .find(|(k, v)| k == ENV_CONFIG && !v.is_empty())
        .map(|(_, v)| PathBuf::from(v))
}

/// Reads, checks and types the config file (if any) plus, with `Env::Read`,
/// the `MANTA_*` overlay. Every error names the file, table and key.
pub(crate) fn load(path: Option<&Path>, env: Env<'_>) -> Result<Loaded> {
    let (origin, mut doc) = match path {
        Some(p) => (p.display().to_string(), read_doc(p)?),
        None => ("environment".to_string(), toml::Table::new()),
    };
    let base_dir = path.map(|p| match p.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => PathBuf::from("."),
    });
    check_top_level(&doc, &origin)?;
    let overlay = match env {
        Env::Ignore => EnvOverlay::default(),
        Env::Read(vars) => apply_env_overlay(&mut doc, vars)?,
    };
    reject_constant_only_keys(&doc, &origin)?;

    let server = take_typed::<ServerConfig>(&doc, "server", &origin, &overlay)?;
    let rbn_uplink = take_uplinks(&doc, &origin)?;
    if !rbn_uplink.is_empty() && server.is_none() {
        bail!(
            "{origin}: [[rbn_uplink]] needs a [server] table (the uplink forwards the spots \
             the servers publish)"
        );
    }
    let decode_toml =
        take_typed::<DecodeConfigToml>(&doc, "decode", &origin, &overlay)?.unwrap_or_default();
    let decode = crate::validate_decode_config(decode_toml.into_decode_config(), &origin)?;
    let detector = take_typed::<DetectorConfigToml>(&doc, "detector", &origin, &overlay)?
        .unwrap_or_default()
        .into_detector_config()
        .map_err(|e| anyhow!("{origin}: [detector]: {e}"))?;
    let resolve_path = |table: &str, key: &str, p: PathBuf| -> PathBuf {
        match &base_dir {
            Some(dir) if p.is_relative() && !overlay.keys.contains(&(table.into(), key.into())) => {
                dir.join(p)
            }
            _ => p,
        }
    };
    let spot_toml = take_typed::<SpotToml>(&doc, "spot", &origin, &overlay)?.unwrap_or_default();
    let spot = SpotFile {
        allowlist: spot_toml.allowlist.unwrap_or_default(),
        blocklist_path: spot_toml
            .blocklist_path
            .map(|p| resolve_path("spot", "blocklist_path", p)),
        notch_path: spot_toml
            .notch_path
            .map(|p| resolve_path("spot", "notch_path", p)),
    };
    let input_toml = take_typed::<InputToml>(&doc, "input", &origin, &overlay)?.unwrap_or_default();
    let input = validate_input(input_toml, &|p| resolve_path("input", "path", p))
        .map_err(|e| anyhow!("{origin}: [input]: {e}{}", overlay.suffix("input")))?;

    Ok(Loaded {
        origin,
        server,
        rbn_uplink,
        decode,
        detector,
        spot,
        input,
        present: doc.keys().cloned().collect(),
    })
}

fn read_doc(p: &Path) -> Result<toml::Table> {
    let bytes = std::fs::read(p).with_context(|| format!("reading config file {}", p.display()))?;
    let text = String::from_utf8(bytes)
        .map_err(|e| anyhow!("config file {} is not valid UTF-8: {e}", p.display()))?;
    toml::from_str::<toml::Table>(crate::strip_bom(&text))
        .map_err(|e| anyhow!("parsing config file {}: {e}", p.display()))
}

/// D2: unknown tables, top-level scalars and wrongly shaped known tables are
/// hard errors that name the offender.
fn check_top_level(doc: &toml::Table, origin: &str) -> Result<()> {
    for (key, value) in doc {
        let is_table = value.is_table()
            || value
                .as_array()
                .is_some_and(|a| !a.is_empty() && a.iter().all(toml::Value::is_table));
        if !KNOWN_TABLES.contains(&key.as_str()) {
            if !is_table {
                bail!(
                    "unrecognized top-level key `{key}` in {origin} -- keys belong inside a \
                     table such as [server]"
                );
            }
            let suggestion = KNOWN_TABLES
                .iter()
                .filter(|known| levenshtein(key, known) <= 2)
                .min_by_key(|known| levenshtein(key, known))
                .map(|known| format!(" (did you mean [{known}]?)"))
                .unwrap_or_default();
            bail!(
                "unrecognized table [{key}] in {origin}{suggestion} -- manta reads [server], \
                 [[rbn_uplink]], [input], [spot], [detector], [decode]"
            );
        }
        match (key.as_str(), value) {
            ("rbn_uplink", toml::Value::Array(_)) => {}
            ("rbn_uplink", _) => {
                bail!("{origin}: rbn_uplink must be written [[rbn_uplink]] (an array of tables)")
            }
            ("input", toml::Value::Array(_)) => bail!(
                "{origin}: [[input]] (multiple sources) is not supported yet -- use a single \
                 [input] table"
            ),
            (_, toml::Value::Table(_)) => {}
            (_, toml::Value::Array(_)) => {
                bail!("{origin}: [[{key}]] is not supported -- use a single [{key}] table")
            }
            _ => bail!("{origin}: `{key}` must be a table ([{key}]), not a value"),
        }
    }
    Ok(())
}

/// D3: SPEC §9 lists these keys for reference, but they are still constants.
fn reject_constant_only_keys(doc: &toml::Table, origin: &str) -> Result<()> {
    for (table, key, home) in CONSTANT_ONLY_KEYS {
        if doc
            .get(*table)
            .and_then(toml::Value::as_table)
            .is_some_and(|t| t.contains_key(*key))
        {
            bail!(
                "{origin}: {table}.{key} is not configurable yet: it is a compile-time constant \
                 in {home} (SPEC §9 lists it for reference)"
            );
        }
    }
    Ok(())
}

fn take_typed<T: serde::de::DeserializeOwned>(
    doc: &toml::Table,
    table: &str,
    origin: &str,
    overlay: &EnvOverlay,
) -> Result<Option<T>> {
    let Some(value) = doc.get(table) else {
        return Ok(None);
    };
    value
        .clone()
        .try_into::<T>()
        .map(Some)
        .map_err(|e| anyhow!("{origin}: [{table}]: {e}{}", overlay.suffix(table)))
}

fn take_uplinks(doc: &toml::Table, origin: &str) -> Result<Vec<RbnUplinkConfig>> {
    let Some(value) = doc.get("rbn_uplink") else {
        return Ok(Vec::new());
    };
    value
        .clone()
        .try_into::<Vec<RbnUplinkConfig>>()
        .map_err(|e| anyhow!("{origin}: [[rbn_uplink]]: {e}"))
}

fn finite_positive(key: &str, v: f64) -> std::result::Result<f64, String> {
    if !v.is_finite() || v <= 0.0 {
        return Err(format!(
            "input.{key} must be a finite, positive number, got {v}"
        ));
    }
    Ok(v)
}

/// D5: type/key consistency, required keys, and the same value checks the
/// equivalent CLI flags apply.
fn validate_input(
    t: InputToml,
    resolve_path: &dyn Fn(PathBuf) -> PathBuf,
) -> std::result::Result<InputFile, String> {
    let shared = SharedInput {
        freq_correction_ppm: t
            .freq_correction_ppm
            .map(crate::check_freq_correction_ppm)
            .transpose()?,
        center_freq_hz: t
            .center_freq_hz
            .map(|v| crate::check_dial_freq_hz("input.center_freq_hz", v))
            .transpose()?,
        capture_rate_hz: t
            .capture_rate_hz
            .map(|v| crate::check_capture_rate_hz("input.capture_rate_hz", v))
            .transpose()?,
        replay_epoch: t
            .replay_epoch
            .map(|v| crate::check_replay_epoch("input.replay_epoch", v))
            .transpose()?,
    };
    let set: [(&str, bool); 10] = [
        ("device", t.device.is_some()),
        ("path", t.path.is_some()),
        ("iq", t.iq.is_some()),
        ("host", t.host.is_some()),
        ("port", t.port.is_some()),
        ("freq_hz", t.freq_hz.is_some()),
        ("password", t.password.is_some()),
        ("driver", t.driver.is_some()),
        ("rate_hz", t.rate_hz.is_some()),
        ("gain_db", t.gain_db.is_some()),
    ];
    let Some(kind) = t.kind else {
        if let Some((key, _)) = set.iter().find(|(_, is_set)| *is_set) {
            return Err(format!(
                "input.{key} needs input.type (one of audio, file, kiwi, soapy, hpsdr)"
            ));
        }
        return Ok(InputFile {
            source: None,
            shared,
        });
    };
    let name = kind.name();
    for (key, is_set) in set {
        if is_set && !kind.keys().contains(&key) {
            return Err(format!(
                "input.{key} does not apply to input.type = \"{name}\" ({name} takes {})",
                kind.keys().join(", ")
            ));
        }
    }
    fn req<T>(v: Option<T>, key: &str, name: &str) -> std::result::Result<T, String> {
        v.ok_or_else(|| format!("input.{key} is required when input.type = \"{name}\""))
    }
    let port = |default: u16| -> std::result::Result<u16, String> {
        match t.port {
            Some(0) => Err("input.port must be between 1 and 65535, got 0".to_string()),
            Some(p) => Ok(p),
            None => Ok(default),
        }
    };
    let source = match kind {
        InputKind::Audio => SourceFromFile::Audio {
            device: t.device.clone(),
        },
        InputKind::File => SourceFromFile::File {
            path: resolve_path(req(t.path.clone(), "path", name)?),
            iq: t.iq.unwrap_or(false),
        },
        InputKind::Kiwi => SourceFromFile::Kiwi {
            port: port(8073)?,
            host: req(t.host.clone(), "host", name)?,
            freq_hz: finite_positive("freq_hz", req(t.freq_hz, "freq_hz", name)?)?,
            password: t.password.clone().unwrap_or_default(),
        },
        InputKind::Soapy => SourceFromFile::Soapy {
            driver: req(t.driver.clone(), "driver", name)?,
            freq_hz: finite_positive("freq_hz", req(t.freq_hz, "freq_hz", name)?)?,
            rate_hz: finite_positive("rate_hz", req(t.rate_hz, "rate_hz", name)?)?,
            gain_db: match t.gain_db {
                Some(g) if !g.is_finite() => {
                    return Err(format!("input.gain_db must be finite, got {g}"))
                }
                g => g,
            },
        },
        InputKind::Hpsdr => SourceFromFile::Hpsdr {
            port: port(crate::HPSDR_CONTROL_PORT)?,
            host: req(t.host.clone(), "host", name)?,
            freq_hz: crate::check_hpsdr_freq_hz("input.freq_hz", req(t.freq_hz, "freq_hz", name)?)?,
            rate_hz: crate::check_hpsdr_rate_hz("input.rate_hz", req(t.rate_hz, "rate_hz", name)?)?,
        },
    };
    Ok(InputFile {
        source: Some(source),
        shared,
    })
}

/// What the environment contributed: per table, the variable names (for
/// error attribution), and every `(table, key)` it set (so env-sourced
/// relative paths resolve against the CWD, not the config file's directory).
#[derive(Default)]
struct EnvOverlay {
    vars_by_table: BTreeMap<String, Vec<String>>,
    keys: BTreeSet<(String, String)>,
}

impl EnvOverlay {
    fn suffix(&self, table: &str) -> String {
        match self.vars_by_table.get(table) {
            Some(vars) if !vars.is_empty() => format!(" (environment: {})", vars.join(", ")),
            _ => String::new(),
        }
    }
}

/// D8: `MANTA_<TABLE>_<KEY>` overlays the parsed document before typed
/// parsing. Read from `vars` (collected with `std::env::vars_os`), never
/// from the process environment directly, so tests pass data in.
fn apply_env_overlay(doc: &mut toml::Table, vars: &[(OsString, OsString)]) -> Result<EnvOverlay> {
    let mut entries: Vec<(String, &'static str, String, String)> = Vec::new();
    for (name, value) in vars {
        let Some(name) = name.to_str() else {
            if name.to_string_lossy().starts_with(ENV_PREFIX) {
                bail!(
                    "environment variable name {} is not valid UTF-8",
                    name.to_string_lossy()
                );
            }
            continue;
        };
        if !name.starts_with(ENV_PREFIX) || name == ENV_CONFIG || ENV_IGNORED.contains(&name) {
            continue;
        }
        let rest = &name[ENV_PREFIX.len()..];
        if rest.starts_with("RBN_UPLINK_") {
            bail!("{name}: [[rbn_uplink]] cannot be set from the environment; use the config file");
        }
        let Some((table, key)) = ENV_TABLES.iter().find_map(|(prefix, table)| {
            rest.strip_prefix(prefix)
                .filter(|k| !k.is_empty())
                .map(|k| (*table, k.to_ascii_lowercase()))
        }) else {
            bail!(
                "unrecognized environment variable {name} (manta reads {ENV_CONFIG} and \
                 MANTA_<SERVER|INPUT|SPOT|DETECTOR|DECODE>_<KEY>)"
            );
        };
        let Some(value) = value.to_str() else {
            bail!("{name} is not valid UTF-8");
        };
        if value.is_empty() {
            continue;
        }
        entries.push((name.to_string(), table, key, value.to_string()));
    }
    entries.sort();

    // A file [input] of a different type than MANTA_INPUT_TYPE is dropped
    // as a unit, so the overlay never builds a mixed-type table.
    if let Some((_, _, _, env_type)) = entries
        .iter()
        .find(|(_, table, key, _)| *table == "input" && key == "type")
    {
        let file_type = doc
            .get("input")
            .and_then(|t| t.get("type"))
            .and_then(toml::Value::as_str);
        if file_type.is_some_and(|f| f != env_type) {
            doc.remove("input");
        }
    }

    let mut overlay = EnvOverlay::default();
    for (name, table, key, raw) in entries {
        let string_typed = STRING_TYPED_ENV_KEYS.contains(&(table, key.as_str()));
        let value = env_value_to_toml(&raw, string_typed);
        let entry = doc
            .entry(table.to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let toml::Value::Table(t) = entry else {
            bail!("{name}: [{table}] in the config file is not a table");
        };
        t.insert(key.clone(), value);
        overlay
            .vars_by_table
            .entry(table.to_string())
            .or_default()
            .push(name);
        overlay.keys.insert((table.to_string(), key));
    }
    Ok(overlay)
}

/// Types an env value by parsing it as the TOML `x = <value>` (so `9300`,
/// `true`, `1.5` and `["W1AW"]` work), falling back to a bare string.
fn env_value_to_toml(raw: &str, string_typed: bool) -> toml::Value {
    if !string_typed {
        if let Ok(mut t) = toml::from_str::<toml::Table>(&format!("x = {raw}")) {
            if t.len() == 1 {
                if let Some(v) = t.remove("x") {
                    return v;
                }
            }
        }
    }
    toml::Value::String(raw.to_string())
}

/// Plain Levenshtein distance, for the `did you mean` suggestion.
fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_cfg(body: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(body.as_bytes()).unwrap();
        f
    }

    fn load_file(body: &str) -> Result<Loaded> {
        let f = write_cfg(body);
        load(Some(f.path()), Env::Ignore)
    }

    fn err_of(body: &str) -> String {
        match load_file(body) {
            Ok(_) => panic!("expected an error for {body:?}"),
            Err(e) => format!("{e:#}"),
        }
    }

    fn vars(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect()
    }

    fn load_env(body: Option<&str>, pairs: &[(&str, &str)]) -> Result<Loaded> {
        let v = vars(pairs);
        match body {
            Some(body) => {
                let f = write_cfg(body);
                load(Some(f.path()), Env::Read(&v))
            }
            None => load(None, Env::Read(&v)),
        }
    }

    const SERVER: &str = "[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"127.0.0.1\"\n\
                          telnet_port = 0\njson_port = 0\nmetrics_port = 0\n";

    // ---- Phase 1: strict top level, [server]/[[rbn_uplink]]/[decode]

    #[test]
    fn unknown_table_is_rejected_with_its_name_and_a_suggestion() {
        let err = err_of("[detectr]\non_snr_db = 99.0\n");
        assert!(err.contains("unrecognized table [detectr]"), "{err}");
        assert!(err.contains("did you mean [detector]?"), "{err}");
    }

    #[test]
    fn unknown_table_far_from_every_known_name_has_no_suggestion() {
        let err = err_of("[telemetry]\nx = 1\n");
        assert!(err.contains("unrecognized table [telemetry]"), "{err}");
        assert!(!err.contains("did you mean"), "{err}");
    }

    #[test]
    fn top_level_scalar_is_rejected() {
        let err = err_of("station_callsign = \"W1AW\"\n");
        assert!(
            err.contains("unrecognized top-level key `station_callsign`"),
            "{err}"
        );
    }

    #[test]
    fn array_of_input_tables_is_rejected() {
        let err = err_of("[[input]]\ntype = \"audio\"\n");
        assert!(
            err.contains("[[input]] (multiple sources) is not supported yet"),
            "{err}"
        );
    }

    #[test]
    fn rbn_uplink_as_a_plain_table_is_rejected() {
        let err = err_of(&format!("{SERVER}[rbn_uplink]\nenabled = false\n"));
        assert!(
            err.contains("rbn_uplink must be written [[rbn_uplink]]"),
            "{err}"
        );
    }

    #[test]
    fn rbn_uplink_without_server_is_rejected() {
        let err = err_of("[[rbn_uplink]]\nenabled = false\ntarget_host = \"x\"\ntarget_port = 1\n");
        assert!(
            err.contains("[[rbn_uplink]] needs a [server] table"),
            "{err}"
        );
    }

    #[test]
    fn server_table_is_optional() {
        let loaded = load_file("[decode]\nengine = \"legacy\"\n").unwrap();
        assert!(loaded.server.is_none());
        assert!(loaded.rbn_uplink.is_empty());
    }

    #[test]
    fn server_errors_name_the_key_and_file() {
        let f = write_cfg("[server]\nstation_callsign = \"W1AW\"\ntelnet_port = \"x\"\n");
        let err = format!("{:#}", load(Some(f.path()), Env::Ignore).unwrap_err());
        assert!(err.contains("telnet_port"), "{err}");
        assert!(err.contains(&f.path().display().to_string()), "{err}");
    }

    #[test]
    fn decode_validation_messages_are_unchanged() {
        let f = write_cfg("[decode]\nbeam_width = 0\n");
        let err = load(Some(f.path()), Env::Ignore).unwrap_err().to_string();
        assert_eq!(
            err,
            format!(
                "[decode] beam_width must be nonzero in {} (0 disables the beam decoder \
                 entirely)",
                f.path().display()
            )
        );
    }

    #[test]
    fn constant_only_spec_keys_say_not_configurable_yet() {
        for (table, key, home) in CONSTANT_ONLY_KEYS {
            let err = err_of(&format!("[{table}]\n{key} = 1\n"));
            assert!(
                err.contains(&format!("{table}.{key} is not configurable yet")),
                "{err}"
            );
            assert!(err.contains(home), "{err}");
        }
    }

    #[test]
    fn bom_is_stripped() {
        let loaded = load_file("\u{feff}[decode]\nengine = \"legacy\"\n").unwrap();
        assert!(loaded.present.contains("decode"));
    }

    #[test]
    fn non_utf8_file_is_an_error_naming_the_file() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"[decode]\nengine = \"\xff\"\n").unwrap();
        let err = load(Some(f.path()), Env::Ignore).unwrap_err().to_string();
        assert!(err.contains("not valid UTF-8"), "{err}");
        assert!(err.contains(&f.path().display().to_string()), "{err}");
    }

    #[test]
    fn missing_file_error_names_the_path_without_server_config_wording() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.toml");
        let err = format!("{:#}", load(Some(&path), Env::Ignore).unwrap_err());
        assert!(
            err.contains(&format!("reading config file {}", path.display())),
            "{err}"
        );
        assert!(!err.contains("--server-config"), "{err}");
    }

    // ---- Phase 2: [detector] and [spot]

    #[test]
    fn detector_table_reaches_loaded() {
        let loaded = load_file("[detector]\non_snr_db = 20.0\n").unwrap();
        assert_eq!(loaded.detector.on_snr_db, 20.0);
    }

    #[test]
    fn detector_constant_only_keys_say_not_configurable_yet() {
        let err = err_of("[detector]\nfloor_quantile = 0.25\n");
        assert!(
            err.contains("detector.floor_quantile is not configurable yet"),
            "{err}"
        );
    }

    #[test]
    fn detector_errors_name_the_table() {
        let err = err_of("[detector]\ntrack_cap = 0\n");
        assert!(err.contains("[detector]: detector.track_cap"), "{err}");
    }

    #[test]
    fn spot_table_parses_allowlist_and_resolves_paths_against_the_config_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manta.toml");
        std::fs::write(
            &path,
            "[spot]\nallowlist = [\"W1AW\"]\nblocklist_path = \"bad.txt\"\n",
        )
        .unwrap();
        let loaded = load(Some(&path), Env::Ignore).unwrap();
        assert_eq!(loaded.spot.allowlist, vec!["W1AW".to_string()]);
        assert_eq!(loaded.spot.blocklist_path, Some(dir.path().join("bad.txt")));
        assert_eq!(loaded.spot.notch_path, None);
    }

    #[test]
    fn spot_unknown_key_is_rejected() {
        let err = err_of("[spot]\nblocklist = \"x\"\n");
        assert!(err.contains("unknown field"), "{err}");
        assert!(err.contains("blocklist_path"), "{err}");
    }

    // ---- Phase 3: [input]

    #[test]
    fn untyped_input_with_shared_keys_is_valid() {
        let loaded =
            load_file("[input]\nfreq_correction_ppm = 2.5\ncenter_freq_hz = 7030000.0\n").unwrap();
        assert_eq!(loaded.input.source, None);
        assert_eq!(loaded.input.shared.freq_correction_ppm, Some(2.5));
        assert_eq!(loaded.input.shared.center_freq_hz, Some(7_030_000.0));
    }

    #[test]
    fn out_of_range_freq_correction_ppm_is_rejected_with_the_flag_message() {
        let err = err_of("[input]\nfreq_correction_ppm = 999999\n");
        assert!(
            err.contains("freq_correction_ppm 999999 is outside the supported range [-1000, 1000]"),
            "{err}"
        );
    }

    #[test]
    fn center_freq_hz_capture_rate_and_replay_epoch_use_the_flag_validators() {
        let err = err_of("[input]\ncenter_freq_hz = 0.0\n");
        assert!(
            err.contains("input.center_freq_hz must be a finite, positive number of Hz, got 0"),
            "{err}"
        );
        let err = err_of("[input]\ncapture_rate_hz = 500.0\n");
        assert!(
            err.contains("input.capture_rate_hz must be a finite number of Hz >= 1000, got 500"),
            "{err}"
        );
        let err = err_of("[input]\nreplay_epoch = -1\n");
        assert!(
            err.contains("input.replay_epoch must be Unix seconds between 0 and"),
            "{err}"
        );
    }

    #[test]
    fn source_key_without_type_is_rejected() {
        let err = err_of("[input]\nhost = \"x\"\n");
        assert!(err.contains("input.host needs input.type"), "{err}");
    }

    #[test]
    fn key_for_another_type_is_rejected() {
        let err =
            err_of("[input]\ntype = \"kiwi\"\nhost = \"h\"\nfreq_hz = 7e6\ndriver = \"rtlsdr\"\n");
        assert!(
            err.contains("input.driver does not apply to input.type = \"kiwi\""),
            "{err}"
        );
        assert!(
            err.contains("kiwi takes host, port, freq_hz, password"),
            "{err}"
        );
    }

    #[test]
    fn required_keys_per_type() {
        for (body, key, kind) in [
            ("type = \"kiwi\"\nhost = \"h\"\n", "freq_hz", "kiwi"),
            ("type = \"file\"\niq = true\n", "path", "file"),
            (
                "type = \"soapy\"\ndriver = \"d\"\nfreq_hz = 7e6\n",
                "rate_hz",
                "soapy",
            ),
            (
                "type = \"hpsdr\"\nfreq_hz = 7e6\nrate_hz = 48000.0\n",
                "host",
                "hpsdr",
            ),
        ] {
            let err = err_of(&format!("[input]\n{body}"));
            assert!(
                err.contains(&format!(
                    "input.{key} is required when input.type = \"{kind}\""
                )),
                "{err}"
            );
        }
    }

    #[test]
    fn hpsdr_rate_bounds_apply_on_every_build() {
        let err =
            err_of("[input]\ntype = \"hpsdr\"\nhost = \"h\"\nfreq_hz = 7e6\nrate_hz = 20e6\n");
        assert!(
            err.contains("input.rate_hz must be a finite number of Hz between"),
            "{err}"
        );
    }

    #[test]
    fn nan_freq_hz_is_rejected() {
        let err = err_of("[input]\ntype = \"kiwi\"\nhost = \"h\"\nfreq_hz = nan\n");
        assert!(
            err.contains("input.freq_hz must be a finite, positive number"),
            "{err}"
        );
    }

    #[test]
    fn port_zero_is_rejected() {
        let err = err_of("[input]\ntype = \"kiwi\"\nhost = \"h\"\nport = 0\nfreq_hz = 7e6\n");
        assert!(
            err.contains("input.port must be between 1 and 65535"),
            "{err}"
        );
    }

    #[test]
    fn unknown_input_type_is_rejected() {
        let err = err_of("[input]\ntype = \"rtl\"\n");
        assert!(err.contains("unknown variant"), "{err}");
        assert!(err.contains("kiwi"), "{err}");
    }

    #[test]
    fn kiwi_defaults_port_and_password() {
        let loaded =
            load_file("[input]\ntype = \"kiwi\"\nhost = \"h\"\nfreq_hz = 7030000.0\n").unwrap();
        assert_eq!(
            loaded.input.source,
            Some(SourceFromFile::Kiwi {
                host: "h".into(),
                port: 8073,
                freq_hz: 7_030_000.0,
                password: String::new(),
            })
        );
    }

    #[test]
    fn file_path_resolves_against_the_config_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manta.toml");
        std::fs::write(&path, "[input]\ntype = \"file\"\npath = \"v1.wav\"\n").unwrap();
        let loaded = load(Some(&path), Env::Ignore).unwrap();
        assert_eq!(
            loaded.input.source,
            Some(SourceFromFile::File {
                path: dir.path().join("v1.wav"),
                iq: false,
            })
        );
    }

    // ---- Phase 4: the MANTA_* environment tier

    #[test]
    fn env_overrides_a_file_value() {
        let loaded = load_env(
            Some("[input]\nfreq_correction_ppm = 2.5\n"),
            &[("MANTA_INPUT_FREQ_CORRECTION_PPM", "3.0")],
        )
        .unwrap();
        assert_eq!(loaded.input.shared.freq_correction_ppm, Some(3.0));
    }

    #[test]
    fn env_values_are_typed_by_toml_probe() {
        let loaded = load_env(
            None,
            &[
                ("MANTA_SERVER_STATION_CALLSIGN", "W1AW"),
                ("MANTA_SERVER_TELNET_PORT", "9300"),
                ("MANTA_DETECTOR_ON_SNR_DB", "15"),
                ("MANTA_SPOT_ALLOWLIST", "[\"W1AW\",\"K1ABC\"]"),
                ("MANTA_DECODE_TAU_HI_BOUNDS_MS", "[100,400]"),
            ],
        )
        .unwrap();
        assert_eq!(loaded.server.unwrap().telnet_port, 9300);
        assert_eq!(loaded.detector.on_snr_db, 15.0);
        assert_eq!(
            loaded.spot.allowlist,
            vec!["W1AW".to_string(), "K1ABC".into()]
        );
        assert_eq!(loaded.decode.demod.tau_hi_bounds_ms, (100.0, 400.0));
        let loaded = load_env(
            None,
            &[
                ("MANTA_INPUT_TYPE", "file"),
                ("MANTA_INPUT_PATH", "x.wav"),
                ("MANTA_INPUT_IQ", "true"),
            ],
        )
        .unwrap();
        assert_eq!(
            loaded.input.source,
            Some(SourceFromFile::File {
                path: PathBuf::from("x.wav"),
                iq: true,
            })
        );
    }

    #[test]
    fn string_typed_keys_keep_numeric_looking_values_as_strings() {
        let loaded = load_env(
            None,
            &[
                ("MANTA_INPUT_TYPE", "kiwi"),
                ("MANTA_INPUT_HOST", "1"),
                ("MANTA_INPUT_FREQ_HZ", "7030000"),
                ("MANTA_INPUT_PASSWORD", "12345678"),
            ],
        )
        .unwrap();
        let Some(SourceFromFile::Kiwi { host, password, .. }) = loaded.input.source else {
            panic!("expected a kiwi source");
        };
        assert_eq!((host.as_str(), password.as_str()), ("1", "12345678"));
        for (k, v) in [
            ("MANTA_INPUT_DEVICE", "1"),
            ("MANTA_INPUT_PATH", "1979-05-27"),
        ] {
            let ty = if k.ends_with("DEVICE") {
                "audio"
            } else {
                "file"
            };
            assert!(
                load_env(None, &[("MANTA_INPUT_TYPE", ty), (k, v)]).is_ok(),
                "{k}"
            );
        }
    }

    #[test]
    fn empty_env_value_is_unset() {
        let loaded = load_env(
            Some("[input]\nfreq_correction_ppm = 2.5\n"),
            &[("MANTA_INPUT_FREQ_CORRECTION_PPM", "")],
        )
        .unwrap();
        assert_eq!(loaded.input.shared.freq_correction_ppm, Some(2.5));
    }

    #[test]
    fn unknown_manta_variable_is_rejected_naming_it() {
        let err = format!(
            "{:#}",
            load_env(None, &[("MANTA_INPUT_HSOT", "h")]).unwrap_err()
        );
        assert!(err.contains("unknown field `hsot`"), "{err}");
        assert!(err.contains("MANTA_INPUT_HSOT"), "{err}");
        let err = format!("{:#}", load_env(None, &[("MANTA_FOO", "1")]).unwrap_err());
        assert!(
            err.contains("unrecognized environment variable MANTA_FOO"),
            "{err}"
        );
    }

    #[test]
    fn manta_git_sha_is_ignored() {
        assert!(load_env(None, &[("MANTA_GIT_SHA", "abc123")]).is_ok());
    }

    #[test]
    fn rbn_uplink_cannot_be_set_from_env() {
        let err = format!(
            "{:#}",
            load_env(None, &[("MANTA_RBN_UPLINK_ENABLED", "true")]).unwrap_err()
        );
        assert!(
            err.contains("[[rbn_uplink]] cannot be set from the environment"),
            "{err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_manta_value_is_rejected_naming_the_variable() {
        use std::os::unix::ffi::OsStringExt;
        let v = vec![(
            OsString::from("MANTA_INPUT_HOST"),
            OsString::from_vec(b"h\xff".to_vec()),
        )];
        let err = load(None, Env::Read(&v)).unwrap_err().to_string();
        assert!(err.contains("MANTA_INPUT_HOST is not valid UTF-8"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_unrelated_variable_is_skipped() {
        use std::os::unix::ffi::OsStringExt;
        let v = vec![
            (
                OsString::from("ZZ_UNRELATED"),
                OsString::from_vec(b"\xff".to_vec()),
            ),
            (OsString::from_vec(b"ZZ_\xff".to_vec()), OsString::from("x")),
        ];
        assert!(load(None, Env::Read(&v)).is_ok());
    }

    #[test]
    fn env_type_that_differs_from_the_file_drops_the_file_input_table() {
        let loaded = load_env(
            Some("[input]\ntype = \"kiwi\"\nhost = \"h\"\nfreq_hz = 7e6\nfreq_correction_ppm = 2.5\n"),
            &[("MANTA_INPUT_TYPE", "file"), ("MANTA_INPUT_PATH", "x.wav")],
        )
        .unwrap();
        assert_eq!(
            loaded.input.source,
            Some(SourceFromFile::File {
                path: PathBuf::from("x.wav"),
                iq: false,
            })
        );
        assert_eq!(loaded.input.shared.freq_correction_ppm, None);
    }

    #[test]
    fn typed_errors_name_the_contributing_env_vars() {
        let err = format!(
            "{:#}",
            load_env(Some(SERVER), &[("MANTA_SERVER_TELNET_PORT", "abc")]).unwrap_err()
        );
        assert!(err.contains("MANTA_SERVER_TELNET_PORT"), "{err}");
    }

    #[test]
    fn env_server_table_without_a_file_starts_servers() {
        let loaded = load_env(None, &[("MANTA_SERVER_STATION_CALLSIGN", "W1AW")]).unwrap();
        assert!(loaded.server.is_some());
        assert_eq!(loaded.origin, "environment");
    }

    #[test]
    fn env_relative_paths_resolve_against_the_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manta.toml");
        std::fs::write(&path, "[spot]\nnotch_path = \"n.txt\"\n").unwrap();
        let v = vars(&[("MANTA_SPOT_BLOCKLIST_PATH", "bad.txt")]);
        let loaded = load(Some(&path), Env::Read(&v)).unwrap();
        assert_eq!(loaded.spot.blocklist_path, Some(PathBuf::from("bad.txt")));
        assert_eq!(loaded.spot.notch_path, Some(dir.path().join("n.txt")));
    }

    #[test]
    fn manta_config_is_the_config_path_fallback() {
        assert_eq!(config_path_from_env(&vars(&[("MANTA_CONFIG", "")])), None);
        assert_eq!(
            config_path_from_env(&vars(&[("MANTA_CONFIG", "/etc/manta.toml")])),
            Some(PathBuf::from("/etc/manta.toml"))
        );
        // MANTA_CONFIG itself is not an overlay key.
        assert!(load_env(None, &[("MANTA_CONFIG", "/etc/manta.toml")]).is_ok());
    }

    #[test]
    fn every_build_rs_env_name_is_ignored() {
        let build_rs = include_str!("../build.rs");
        let names: Vec<&str> = build_rs
            .split("rerun-if-env-changed=")
            .skip(1)
            .filter_map(|rest| {
                rest.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .next()
            })
            .filter(|name| name.starts_with(ENV_PREFIX))
            .collect();
        assert!(!names.is_empty());
        for name in names {
            assert!(ENV_IGNORED.contains(&name), "{name} must be in ENV_IGNORED");
        }
    }
}
