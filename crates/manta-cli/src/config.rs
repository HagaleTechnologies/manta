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
use manta_server::config::{RbnCompareConfig, RbnUplinkConfig, ServerConfig};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

/// Every top-level table manta reads. Adding a table means adding it here.
pub(crate) const KNOWN_TABLES: &[&str] = &[
    "server",
    "rbn_uplink",
    "rbn_compare",
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

/// MAN-273: `DetectorConfig` fields the source supplies, never the operator
/// (not in SPEC §9): rejected with their own message.
const SOURCE_DERIVED_KEYS: &[(&str, &str, &str)] = &[(
    "detector",
    "guard_hz",
    "derived from the source's audio front end (manta-dsp::hilbert, MAN-273)",
)];

const ENV_PREFIX: &str = "MANTA_";
/// The `--config` fallback (run/soak/doctor only).
pub(crate) const ENV_CONFIG: &str = "MANTA_CONFIG";
const ENV_TABLES: &[(&str, &str)] = &[
    ("SERVER_", "server"),
    ("RBN_COMPARE_", "rbn_compare"),
    ("INPUT_", "input"),
    ("SPOT_", "spot"),
    ("DETECTOR_", "detector"),
    ("DECODE_", "decode"),
];
/// Build-time only (crates/manta-cli/build.rs), never runtime config.
/// `cargo run`/`cargo test` export build.rs's `rustc-env` values into the
/// processes they start, so these must not trip the unknown-name check.
const ENV_IGNORED: &[&str] = &["MANTA_GIT_SHA", "MANTA_FEATURES"];
/// String-typed keys: an env value is taken verbatim, never TOML-probed, so
/// `MANTA_INPUT_PASSWORD=12345678` stays a string.
const STRING_TYPED_ENV_KEYS: &[(&str, &str)] = &[
    ("server", "station_callsign"),
    ("server", "bind_addr"),
    ("server", "metrics_bind_addr"),
    ("server", "live_bind_addr"),
    ("server", "operator_name"),
    ("server", "operator_qth"),
    ("server", "operator_grid"),
    ("rbn_compare", "feed_host"),
    ("rbn_compare", "login_callsign"),
    ("input", "type"),
    ("input", "device"),
    ("input", "path"),
    ("input", "record_iq"),
    ("input", "iq_format"),
    ("input", "host"),
    ("input", "mac"),
    ("input", "password"),
    // MAN-305: `MANTA_INPUT_PROTOCOL=2` is the string "2", like the file key.
    ("input", "protocol"),
    ("input", "driver"),
    ("input", "group"),
    ("input", "interface"),
    ("spot", "blocklist_path"),
    ("spot", "notch_path"),
    ("spot", "cty_path"),
    ("spot", "scp_path"),
    // MAN-316: paths and URLs verbatim, and an all-digit SHA-256 must stay
    // a string rather than TOML-probe as a number.
    ("spot", "list_cache_dir"),
    ("spot", "cty_url"),
    ("spot", "scp_url"),
    ("spot", "cty_sha256"),
    ("spot", "scp_sha256"),
    ("spot", "pattern_path"),
    ("spot", "validation_level"),
    ("decode", "engine"),
    ("detector", "crash_filter"),
];

/// MAN-316: where `refresh_lists` downloads each list when `cty_url` /
/// `scp_url` is unset. Plain HTTP: manta has no TLS support.
pub(crate) const DEFAULT_CTY_URL: &str = "http://www.country-files.com/cty/cty.dat";
pub(crate) const DEFAULT_SCP_URL: &str = "http://www.supercheckpartial.com/MASTER.SCP";

/// MAN-316: a list pin (`--cty-sha256`/`--scp-sha256`, `[spot].cty_sha256`/
/// `scp_sha256`): exactly 64 hexadecimal digits, returned lowercased so it
/// compares equal to `list_cache::sha256_hex`. Also the flags' clap
/// `value_parser`.
pub(crate) fn parse_sha256(s: &str) -> std::result::Result<String, String> {
    if s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(s.to_ascii_lowercase())
    } else {
        Err("expected 64 hexadecimal digits (a SHA-256)".to_string())
    }
}

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
    /// MAN-301: `[rbn_compare]` as written, validated (`None` when the
    /// table is absent). Present even when `enabled = false`: `manta grade
    /// --config` reads its tolerances. `run` merges `--rbn-compare`/
    /// `--rbn-spotter` over it in `resolve`.
    pub rbn_compare: Option<RbnCompareConfig>,
    pub decode: DecodeConfig,
    pub detector: DetectorConfig,
    pub spot: SpotFile,
    /// The first (or only) input: what every one-receiver command opens.
    pub input: InputFile,
    /// MAN-13: every input, named, in config order; `inputs[0].input ==
    /// input`. One entry for a single `[input]` table (or none at all).
    pub inputs: Vec<NamedInput>,
    /// Top-level tables present after the overlay (for "ignored" notes).
    pub present: BTreeSet<String>,
    /// The whole document after the `MANTA_*` overlay, untyped: which keys
    /// were actually set, as written (`manta config check`'s summary and
    /// placeholder scan, MAN-76).
    pub raw: toml::Table,
    /// The `MANTA_<TABLE>_<KEY>` variables the overlay applied, sorted.
    pub env_vars: Vec<String>,
    /// Stderr notes about the file itself (an inert table), printed by the
    /// caller with `resolve`'s merge notes.
    pub notes: Vec<String>,
}

/// `[spot]`, with relative file paths already resolved (file values against
/// the config file's directory, env values against the CWD).
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct SpotFile {
    pub allowlist: Vec<String>,
    pub blocklist_path: Option<PathBuf>,
    pub notch_path: Option<PathBuf>,
    pub cty_path: Option<PathBuf>,
    pub scp_path: Option<PathBuf>,
    /// MAN-316: download current lists while `run` reads a live receiver.
    pub refresh_lists: bool,
    /// MAN-316: where downloaded lists persist; resolved like `cty_path`.
    pub list_cache_dir: Option<PathBuf>,
    /// MAN-316: `None` is [`DEFAULT_CTY_URL`]; a value parsed as an
    /// `http://` URL at load. See [`SpotFile::cty_url`].
    pub cty_url: Option<String>,
    pub scp_url: Option<String>,
    /// MAN-316: a pinned list version, validated and lowercased by
    /// [`parse_sha256`] at load.
    pub cty_sha256: Option<String>,
    pub scp_sha256: Option<String>,
    pub pattern_path: Option<PathBuf>,
    pub validation_level: manta_spot::ValidationLevel,
}

impl SpotFile {
    /// MAN-316: the URL `refresh_lists` downloads cty.dat from.
    pub(crate) fn cty_url(&self) -> &str {
        self.cty_url.as_deref().unwrap_or(DEFAULT_CTY_URL)
    }

    /// MAN-316: the URL `refresh_lists` downloads MASTER.SCP from.
    pub(crate) fn scp_url(&self) -> &str {
        self.scp_url.as_deref().unwrap_or(DEFAULT_SCP_URL)
    }
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SpotToml {
    allowlist: Option<Vec<String>>,
    blocklist_path: Option<PathBuf>,
    notch_path: Option<PathBuf>,
    cty_path: Option<PathBuf>,
    scp_path: Option<PathBuf>,
    refresh_lists: Option<bool>,
    list_cache_dir: Option<PathBuf>,
    cty_url: Option<String>,
    scp_url: Option<String>,
    cty_sha256: Option<String>,
    scp_sha256: Option<String>,
    pattern_path: Option<PathBuf>,
    #[serde(default, deserialize_with = "deserialize_validation_level")]
    validation_level: manta_spot::ValidationLevel,
}

fn deserialize_validation_level<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<manta_spot::ValidationLevel, D::Error> {
    let value = <String as serde::Deserialize>::deserialize(deserializer)?;
    match value.as_str() {
        "minimal" => Ok(manta_spot::ValidationLevel::Minimal),
        "normal" => Ok(manta_spot::ValidationLevel::Normal),
        _ => Err(serde::de::Error::unknown_variant(
            &value,
            &["minimal", "normal"],
        )),
    }
}

/// MAN-316: a `[spot]` list URL must be one `list_http` can fetch. Checked at
/// load, so a bad URL fails `config check`/`run` before any source opens
/// rather than at the first refresh days later.
fn check_list_url(key: &str, url: Option<String>) -> std::result::Result<Option<String>, String> {
    let Some(url) = url else {
        return Ok(None);
    };
    match crate::list_http::HttpUrl::parse(&url) {
        Ok(_) => Ok(Some(url)),
        Err(reason) => {
            let mut e =
                format!("{key} must be an http:// URL (manta has no TLS support), got \"{url}\"");
            // An `http://` value that still fails says why.
            if url
                .get(..7)
                .is_some_and(|p| p.eq_ignore_ascii_case("http://"))
            {
                e.push_str(&format!(": {reason}"));
            }
            Err(e)
        }
    }
}

/// MAN-316: a `[spot]` pin key, through the flags' parser.
fn check_pin(key: &str, pin: Option<String>) -> std::result::Result<Option<String>, String> {
    pin.map(|p| {
        parse_sha256(&p)
            .map_err(|_| format!("{key} must be 64 hexadecimal digits (a SHA-256), got \"{p}\""))
    })
    .transpose()
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
    Ka9q,
}

impl InputKind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            InputKind::Audio => "audio",
            InputKind::File => "file",
            InputKind::Kiwi => "kiwi",
            InputKind::Soapy => "soapy",
            InputKind::Hpsdr => "hpsdr",
            InputKind::Ka9q => "ka9q",
        }
    }

    /// Source keys this type accepts; the shared keys are valid with any type.
    pub(crate) fn keys(self) -> &'static [&'static str] {
        match self {
            InputKind::Audio => &["device"],
            InputKind::File => &[
                "path",
                "iq",
                "realtime",
                "loop",
                "iq_format",
                "sample_rate_hz",
            ],
            InputKind::Kiwi => &[
                "host",
                "port",
                "freq_hz",
                "password",
                "bands",
                "tuning_range_hz",
            ],
            InputKind::Soapy => &[
                "driver",
                "freq_hz",
                "rate_hz",
                "gain_db",
                "bands",
                "tuning_range_hz",
            ],
            InputKind::Hpsdr => &[
                "host",
                "mac",
                "port",
                "protocol",
                "freq_hz",
                "rate_hz",
                "receivers",
                "bands",
                "tuning_range_hz",
            ],
            InputKind::Ka9q => &["group", "port", "ssrc", "interface"],
        }
    }
}

#[derive(Debug, Default, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct InputToml {
    /// MAN-13: the input's name (required with two or more `[[input]]`).
    name: Option<String>,
    #[serde(rename = "type")]
    kind: Option<InputKind>,
    device: Option<String>,
    path: Option<PathBuf>,
    iq: Option<bool>,
    realtime: Option<bool>,
    #[serde(rename = "loop")]
    looped: Option<bool>,
    /// MAN-309: headerless IQ (`cf32`/`ci16`) and its stored sample rate.
    iq_format: Option<String>,
    sample_rate_hz: Option<f64>,
    host: Option<String>,
    mac: Option<String>,
    port: Option<u16>,
    protocol: Option<ProtocolValue>,
    freq_hz: Option<f64>,
    /// MAN-310: `"all"`, band names or centres in kHz, instead of
    /// `freq_hz`. Typed by hand, so a wrong type gets manta's message.
    bands: Option<toml::Value>,
    /// MAN-310: with `bands`, the receiver's tuning range, Hz.
    tuning_range_hz: Option<Vec<f64>>,
    password: Option<String>,
    driver: Option<String>,
    rate_hz: Option<f64>,
    gain_db: Option<f64>,
    group: Option<String>,
    // i64, not u32, so a negative or too-large value gets the range message
    // rather than serde's.
    ssrc: Option<i64>,
    receivers: Option<toml::Value>,
    interface: Option<String>,
    // Shared keys, valid with any (or no) type.
    freq_correction_ppm: Option<f64>,
    center_freq_hz: Option<f64>,
    capture_rate_hz: Option<f64>,
    replay_epoch: Option<i64>,
    /// MAN-309: `run` records this input's IQ here.
    record_iq: Option<PathBuf>,
}

/// MAN-305: `input.protocol`, written `"auto"`, `"1"`, `"2"` or the
/// integers 1 and 2; anything else is refused when the input is built.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(untagged)]
enum ProtocolValue {
    Text(String),
    Number(i64),
}

impl ProtocolValue {
    fn protocol(&self) -> std::result::Result<manta_input::hpsdr::HpsdrProtocol, String> {
        use manta_input::hpsdr::HpsdrProtocol;
        match self {
            ProtocolValue::Text(t) if t == "auto" => Ok(HpsdrProtocol::Auto),
            ProtocolValue::Text(t) if t == "1" => Ok(HpsdrProtocol::One),
            ProtocolValue::Text(t) if t == "2" => Ok(HpsdrProtocol::Two),
            ProtocolValue::Number(1) => Ok(HpsdrProtocol::One),
            ProtocolValue::Number(2) => Ok(HpsdrProtocol::Two),
            ProtocolValue::Text(t) => Err(format!(
                "input.protocol must be \"auto\", \"1\" or \"2\", got {}",
                crate::fmt::quoted(t)
            )),
            ProtocolValue::Number(n) => Err(format!(
                "input.protocol must be \"auto\", \"1\" or \"2\", got {n}"
            )),
        }
    }
}

/// MAN-13: one `[[input]]` entry (or the single `[input]` table) and its
/// name. The single form's name defaults to its type (`audio` untyped).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NamedInput {
    pub name: String,
    pub input: InputFile,
    /// MAN-310: the config entry this input came from: `None` for the
    /// single `[input]` table, else the `[[input]]` entry's name. Several
    /// inputs share it when the entry's `bands` plans several streams.
    pub entry: Option<String>,
    /// MAN-310: this stream's share of a split band plan, Hz from its
    /// centre (`band_plan::PlannedStream::spawn_band_hz`).
    pub spawn_band_hz: Option<(f64, f64)>,
}

/// MAN-13: the longest input name.
const MAX_INPUT_NAME_CHARS: usize = 32;

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
    /// MAN-309: `run` records this input's IQ to this path.
    pub record_iq: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SourceFromFile {
    Audio {
        device: Option<String>,
    },
    /// `realtime`/`looped` are MAN-269's paced and looping replay; `run`
    /// applies them, `soak`/`doctor` ignore them with a note.
    File {
        path: PathBuf,
        iq: bool,
        realtime: bool,
        looped: bool,
        /// MAN-309: `iq_format`/`sample_rate_hz` -- a headerless IQ file
        /// in this layout at this stored rate, centred on
        /// `input.center_freq_hz`. `None`: a WAV or a manta recording.
        raw: Option<(manta_input::RawIqFormat, f64)>,
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
        receivers: Option<usize>,
        host: Option<String>,
        mac: Option<String>,
        port: u16,
        /// MAN-305 D2: `input.protocol`, `Auto` when absent.
        protocol: manta_input::hpsdr::HpsdrProtocol,
        freq_hz: f64,
        rate_hz: f64,
    },
    /// MAN-140: one ka9q-radio (radiod) IQ channel. `group` passed
    /// `check_group` at load; a host name is resolved at open.
    Ka9q {
        group: String,
        port: u16,
        ssrc: Option<u32>,
        interface: Option<Ipv4Addr>,
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
            SourceFromFile::Ka9q { .. } => InputKind::Ka9q,
        }
    }

    /// The tuned centre, for the types that take `freq_hz`.
    pub(crate) fn freq_hz(&self) -> Option<f64> {
        match self {
            SourceFromFile::Kiwi { freq_hz, .. }
            | SourceFromFile::Soapy { freq_hz, .. }
            | SourceFromFile::Hpsdr { freq_hz, .. } => Some(*freq_hz),
            _ => None,
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
    // MAN-13: a one-entry `[[input]]` is the single form.
    if let Some(toml::Value::Array(entries)) = doc.get("input") {
        if entries.len() == 1 && entries[0].is_table() {
            let only = entries[0].clone();
            doc.insert("input".into(), only);
        }
    }
    let overlay = match env {
        Env::Ignore => EnvOverlay::default(),
        Env::Read(vars) => apply_env_overlay(&mut doc, vars)?,
    };
    reject_constant_only_keys(&doc, &origin)?;

    let server = take_typed::<ServerConfig>(&doc, "server", &origin, &overlay)?;
    // MAN-138: the location keys' cross-key rules, before any source or
    // listener opens (the same point `[input]`'s `validate_input` runs).
    if let Some(s) = &server {
        s.station_location()
            .map_err(|e| anyhow!("{origin}: [server]: {e}{}", overlay.suffix("server")))?;
    }
    let rbn_uplink = take_uplinks(&doc, &origin)?;
    if !rbn_uplink.is_empty() && server.is_none() {
        bail!(
            "{origin}: [[rbn_uplink]] needs a [server] table (the uplink forwards the spots \
             the servers publish)"
        );
    }
    // MAN-319: a live uplink (`dry_run = false`) will one day register with
    // the collector, and registration carries the station's 6-character
    // locator. Refuse at config load -- every entry point (check, run,
    // doctor, SIGHUP reload) shares this -- rather than letting a live
    // uplink start without it. Dry-run blocks are exempt: rehearsal must
    // not need what only the live path transmits. `station_location` above
    // has already rejected any malformed grid, so only absent / 4 / 6
    // remain to tell apart.
    for uplink in &rbn_uplink {
        if uplink.enabled && !uplink.dry_run {
            let grid = server.as_ref().and_then(|s| s.operator_grid.as_deref());
            if !grid.is_some_and(|g| g.chars().count() == 6) {
                let short = short_grid_suffix(grid);
                bail!(
                    "{origin}: [[rbn_uplink]] target {}:{} has dry_run = false -- sending \
                     spots needs [server].operator_grid set to a 6-character Maidenhead \
                     locator (e.g. \"FN03ab\"){short}",
                    uplink.target_host,
                    uplink.target_port,
                );
            }
        }
    }
    let mut notes = Vec::new();
    let rbn_compare = take_typed::<RbnCompareConfig>(&doc, "rbn_compare", &origin, &overlay)?;
    if let Some(rc) = &rbn_compare {
        rc.validate()
            .map_err(|e| anyhow!("{origin}: {e}{}", overlay.suffix("rbn_compare")))?;
        match &server {
            None if rc.enabled => bail!("{}", rbn_compare_needs_server(&origin)),
            None => notes.push(RBN_COMPARE_INERT_NOTE.to_string()),
            Some(s) => rc
                .check_spotters_against_station(&s.station_callsign)
                .map_err(|e| anyhow!("{origin}: {e}{}", overlay.suffix("rbn_compare")))?,
        }
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
    let spot_err = |e: String| anyhow!("{origin}: [spot]: {e}{}", overlay.suffix("spot"));
    let spot = SpotFile {
        allowlist: spot_toml.allowlist.unwrap_or_default(),
        blocklist_path: spot_toml
            .blocklist_path
            .map(|p| resolve_path("spot", "blocklist_path", p)),
        notch_path: spot_toml
            .notch_path
            .map(|p| resolve_path("spot", "notch_path", p)),
        cty_path: spot_toml
            .cty_path
            .map(|p| resolve_path("spot", "cty_path", p)),
        scp_path: spot_toml
            .scp_path
            .map(|p| resolve_path("spot", "scp_path", p)),
        refresh_lists: spot_toml.refresh_lists.unwrap_or(false),
        list_cache_dir: spot_toml
            .list_cache_dir
            .map(|p| resolve_path("spot", "list_cache_dir", p)),
        cty_url: check_list_url("cty_url", spot_toml.cty_url).map_err(&spot_err)?,
        scp_url: check_list_url("scp_url", spot_toml.scp_url).map_err(&spot_err)?,
        cty_sha256: check_pin("cty_sha256", spot_toml.cty_sha256).map_err(&spot_err)?,
        scp_sha256: check_pin("scp_sha256", spot_toml.scp_sha256).map_err(&spot_err)?,
        pattern_path: spot_toml
            .pattern_path
            .map(|p| resolve_path("spot", "pattern_path", p)),
        validation_level: spot_toml.validation_level,
    };
    let inputs = match doc.get("input") {
        Some(toml::Value::Array(entries)) if !entries.is_empty() => take_input_array(
            entries,
            &origin,
            &|key, p| resolve_path("input", key, p),
            &mut notes,
        )?,
        _ => {
            let input_toml =
                take_typed::<InputToml>(&doc, "input", &origin, &overlay)?.unwrap_or_default();
            let name = match input_toml.name.clone() {
                Some(name) => {
                    check_input_name(&name).map_err(|e| anyhow!("{origin}: {e}"))?;
                    name
                }
                None => input_toml.kind.map_or("audio", InputKind::name).to_string(),
            };
            let (inputs, entry_notes) = take_entry(input_toml, name, None, "[input]", &|key, p| {
                resolve_path("input", key, p)
            })
            .map_err(|e| anyhow!("{origin}: [input]: {e}{}", overlay.suffix("input")))?;
            notes.extend(entry_notes);
            if inputs.len() > manta_engine::MAX_INPUTS {
                bail!(
                    "{origin}: input.bands plans {} for [input] ({}), and manta runs at most {} \
                     inputs; list fewer bands",
                    manta_server::band_plan::count_streams(inputs.len()),
                    plan_list(&inputs),
                    manta_engine::MAX_INPUTS
                );
            }
            check_stream_names(&inputs, &origin)?;
            if inputs.len() > 1 {
                check_input_entries(&inputs, &origin)?;
            }
            inputs
        }
    };
    let input = inputs[0].input.clone();

    let mut env_vars: Vec<String> = overlay.vars_by_table.into_values().flatten().collect();
    env_vars.sort();
    Ok(Loaded {
        origin,
        server,
        rbn_uplink,
        rbn_compare,
        decode,
        detector,
        spot,
        input,
        inputs,
        present: doc.keys().cloned().collect(),
        raw: doc,
        env_vars,
        notes,
    })
}

/// MAN-301: an enabled `[rbn_compare]` (from the file, the environment or
/// `--rbn-compare`) without `[server]`.
pub(crate) fn rbn_compare_needs_server(origin: &str) -> String {
    format!(
        "{origin}: [rbn_compare] needs a [server] table (grades are published on the JSON \
         stream and /metrics, which only the daemon runs)"
    )
}

/// MAN-301: a disabled `[rbn_compare]` without `[server]` is inert.
pub(crate) const RBN_COMPARE_INERT_NOTE: &str = "note: [rbn_compare] is off and ignored";

/// `check_top_level`'s list of the tables manta reads, built from
/// `KNOWN_TABLES` so a new table cannot drift it. `rbn_uplink` is an array
/// of tables, so it keeps its double brackets.
fn known_tables_list() -> String {
    KNOWN_TABLES
        .iter()
        .map(|t| match *t {
            "rbn_uplink" => format!("[[{t}]]"),
            _ => format!("[{t}]"),
        })
        .collect::<Vec<_>>()
        .join(", ")
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
                "unrecognized table [{key}] in {origin}{suggestion} -- manta reads {}",
                known_tables_list()
            );
        }
        match (key.as_str(), value) {
            ("rbn_uplink", toml::Value::Array(_)) => {}
            ("rbn_uplink", _) => {
                bail!("{origin}: rbn_uplink must be written [[rbn_uplink]] (an array of tables)")
            }
            // MAN-13: several receivers, one entry each.
            ("input", toml::Value::Array(_)) => {}
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
    for (table, key, why) in SOURCE_DERIVED_KEYS {
        if doc
            .get(*table)
            .and_then(toml::Value::as_table)
            .is_some_and(|t| t.contains_key(*key))
        {
            bail!("{origin}: {table}.{key} is not configurable: it is {why}");
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

/// MAN-319's locator gate names a set-but-too-short grid so the operator
/// sees which value to fix; empty when no grid is configured at all.
fn short_grid_suffix(grid: Option<&str>) -> String {
    match grid {
        Some(g) => format!(" -- \"{g}\" is only {} characters", g.chars().count()),
        None => String::new(),
    }
}

fn finite_positive(key: &str, v: f64) -> std::result::Result<f64, String> {
    if !v.is_finite() || v <= 0.0 {
        return Err(format!(
            "input.{key} must be a finite, positive number, got {v}"
        ));
    }
    Ok(v)
}

/// A float as an operator writes it in Hz: integral values keep one
/// decimal (`30000000.0`, never the bare `30000000`).
fn float_hz(v: f64) -> String {
    if v.is_finite() && v == v.trunc() {
        format!("{v:.1}")
    } else {
        format!("{v}")
    }
}

/// MAN-140 D16: `input.group` is an IPv4 multicast literal or a host name,
/// checked without resolving it. `check_group`'s IPv6 message is a full
/// clause of its own, so it follows a colon; the other reads on from the key.
fn check_ka9q_group(group: String) -> std::result::Result<String, String> {
    match manta_input::ka9q::check_group(&group) {
        Ok(()) => Ok(group),
        Err(msg) if msg.starts_with("IPv6") => Err(format!("input.group: {msg}")),
        Err(msg) => Err(format!("input.group {msg}")),
    }
}

/// `input.ssrc`: an RTP SSRC, 32 bits unsigned.
fn check_ka9q_ssrc(ssrc: i64) -> std::result::Result<u32, String> {
    u32::try_from(ssrc)
        .map_err(|_| format!("input.ssrc must be between 0 and {}, got {ssrc}", u32::MAX))
}

/// `input.interface`: the local IPv4 address to join the group on; an
/// interface name is not accepted.
fn check_ka9q_interface(interface: &str) -> std::result::Result<Ipv4Addr, String> {
    interface
        .parse()
        .map_err(|_| format!("input.interface must be an IPv4 address, got {interface}"))
}

/// D5: type/key consistency, required keys, and the same value checks the
/// equivalent CLI flags apply.
/// MAN-13: an input name is a metric label and a log word: letters,
/// digits, '.', '_' and '-', starting with a letter or digit, at most 32
/// characters.
fn check_input_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.chars().count() <= MAX_INPUT_NAME_CHARS
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !ok {
        let name = crate::fmt::quoted(name);
        bail!(
            "[[input]] name {name} may use only letters, digits, '.', '_' and '-', start with \
             a letter or digit, and be at most {MAX_INPUT_NAME_CHARS} characters"
        );
    }
    Ok(())
}

/// MAN-13: `[[input]]` with two or more entries: each typed, named and
/// validated like `[input]`, then the cross-entry rules.
fn take_input_array(
    entries: &[toml::Value],
    origin: &str,
    resolve_path: &dyn Fn(&str, PathBuf) -> PathBuf,
    notes: &mut Vec<String>,
) -> Result<Vec<NamedInput>> {
    if entries.len() > manta_engine::MAX_INPUTS {
        bail!(
            "{origin}: at most {} [[input]] entries are supported, found {}",
            manta_engine::MAX_INPUTS,
            entries.len()
        );
    }
    let mut inputs: Vec<NamedInput> = Vec::with_capacity(entries.len());
    for (i, entry) in entries.iter().enumerate() {
        let t = entry
            .clone()
            .try_into::<InputToml>()
            .map_err(|e| anyhow!("{origin}: [[input]] entry {}: {e}", i + 1))?;
        let Some(name) = t.name.clone() else {
            bail!(
                "{origin}: [[input]] entry {} has no name -- with more than one [[input]], give \
                 each one a name, for example name = \"20m\"",
                i + 1
            );
        };
        check_input_name(&name).map_err(|e| anyhow!("{origin}: {e}"))?;
        let quoted = crate::fmt::quoted(&name);
        if inputs.iter().any(|n| n.name == name) {
            bail!("{origin}: [[input]] name {quoted} is used twice");
        }
        if t.kind.is_none() {
            bail!(
                "{origin}: [[input]] {quoted} has no type -- with more than one [[input]], each \
                 one names its receiver type"
            );
        }
        let section = format!("[[input]] {quoted}");
        let (streams, entry_notes) =
            take_entry(t, name.clone(), Some(name), &section, resolve_path)
                .map_err(|e| anyhow!("{origin}: {section}: {e}"))?;
        notes.extend(entry_notes);
        inputs.extend(streams);
    }
    if inputs.len() > manta_engine::MAX_INPUTS {
        let mut per_entry: Vec<(String, usize)> = Vec::new();
        for n in &inputs {
            let entry = n.entry.clone().unwrap_or_default();
            match per_entry.last_mut() {
                Some((e, count)) if *e == entry => *count += 1,
                _ => per_entry.push((entry, 1)),
            }
        }
        bail!(
            "{origin}: the [[input]] entries plan {} ({}), and manta runs at most {} inputs; \
             list fewer bands",
            manta_server::band_plan::count_streams(inputs.len()),
            per_entry
                .iter()
                .map(|(e, n)| format!("{} {n}", crate::fmt::quoted(e)))
                .collect::<Vec<_>>()
                .join(", "),
            manta_engine::MAX_INPUTS
        );
    }
    check_stream_names(&inputs, origin)?;
    check_input_entries(&inputs, origin)?;
    validate_hpsdr_allocations(&inputs, origin)?;
    Ok(inputs)
}

/// Ordered, offline allocation shared by config validation and the daemon.
#[derive(Debug, Clone)]
pub(crate) struct HpsdrAllocation {
    /// MAN-306: `None` discovers the radio at open, selected by `mac`
    /// when set (normalized `AA:BB:CC:DD:EE:FF`).
    pub host: Option<String>,
    pub mac: Option<String>,
    pub port: u16,
    pub rate: f64,
    pub receivers: Option<usize>,
    /// MAN-305: the radio's protocol, taken from the first member. A
    /// lone input may resolve `auto` by discovery at open; several
    /// receivers on one radio speak protocol 1 in manta.
    pub protocol: manta_input::hpsdr::HpsdrProtocol,
    pub members: Vec<usize>,
    pub frequencies: Vec<f64>,
}

impl HpsdrAllocation {
    /// The radio as the operator configured it, for messages: `host:port`,
    /// or how discovery selects it (MAN-306).
    pub fn endpoint(&self) -> String {
        match (&self.host, &self.mac) {
            (Some(host), _) => format!("{host}:{}", self.port),
            (None, Some(mac)) => format!("MAC {mac} (discovery, port {})", self.port),
            (None, None) => format!("discovery (port {})", self.port),
        }
    }
}

pub(crate) fn hpsdr_allocations(inputs: &[NamedInput]) -> Vec<HpsdrAllocation> {
    let mut groups: Vec<HpsdrAllocation> = Vec::new();
    for (index, input) in inputs.iter().enumerate() {
        if let Some(SourceFromFile::Hpsdr {
            host,
            mac,
            port,
            protocol,
            rate_hz,
            freq_hz,
            receivers,
        }) = &input.input.source
        {
            // MAN-306 x MAN-307: entries naming one radio the same way share
            // it -- one host (case-insensitive), or hostless entries with
            // the same MAC selector (or none). Different spellings of one
            // radio are caught when the run resolves them to one address.
            let same_radio = |g: &HpsdrAllocation| {
                g.port == *port
                    && match (&g.host, host) {
                        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                        (None, None) => g.mac == *mac,
                        _ => false,
                    }
            };
            if let Some(group) = groups.iter_mut().find(|g| same_radio(g)) {
                group.members.push(index);
                group.frequencies.push(*freq_hz);
            } else {
                groups.push(HpsdrAllocation {
                    host: host.clone(),
                    mac: mac.clone(),
                    port: *port,
                    rate: *rate_hz,
                    receivers: *receivers,
                    protocol: *protocol,
                    members: vec![index],
                    frequencies: vec![*freq_hz],
                });
            }
        }
    }
    groups
}

fn validate_hpsdr_allocations(inputs: &[NamedInput], origin: &str) -> Result<()> {
    for group in hpsdr_allocations(inputs) {
        let first = &inputs[group.members[0]].name;
        if group.members.len() > 1
            && matches!(group.protocol, manta_input::hpsdr::HpsdrProtocol::Two)
        {
            bail!("{origin}: [[input]] \"{first}\": HPSDR protocol 2 opens one receiver per radio in manta; give each band its own receiver with protocol \"1\"");
        }
        for &index in group.members.iter().skip(1) {
            let name = &inputs[index].name;
            if let Some(SourceFromFile::Hpsdr {
                rate_hz,
                receivers,
                protocol,
                ..
            }) = &inputs[index].input.source
            {
                if receivers.is_some() {
                    bail!("{origin}: [[input]] \"{name}\": receivers belongs on the first input for this HPSDR radio (\"{first}\")");
                }
                if *protocol != group.protocol {
                    bail!("{origin}: [[input]] \"{name}\": protocol must match the HPSDR radio's other receivers (set by input \"{first}\")");
                }
                if *rate_hz != group.rate {
                    bail!("{origin}: [[input]] \"{name}\": rate_hz must match the HPSDR radio's other receivers ({} Hz, set by input \"{first}\")", group.rate);
                }
            }
        }
        if let Some(capacity) = group.receivers {
            if group.members.len() > capacity {
                bail!("{origin}: [[input]] \"{first}\": HPSDR radio at {} declares {capacity} receivers; {} bands are configured", group.endpoint(), group.members.len());
            }
        }
        manta_input::HpsdrConfig {
            // Offline: validation never reads the host.
            host: group.host.clone().unwrap_or_default(),
            port: group.port,
            ddc_count: group.members.len(),
            sample_rate_hz: group.rate,
            center_freq_hz: group.frequencies.clone(),
        }
        .validate()
        .map_err(|e| anyhow!("{origin}: [[input]] \"{first}\": {e}"))?;
    }
    Ok(())
}

/// MAN-310: one config entry's inputs: the entry itself, or with `bands`
/// one copy per planned stream (`freq_hz` = the stream's centre), each
/// checked by `validate_input` like a hand-written entry. Returns the
/// inputs and the plan's notes, prefixed with `section`.
fn take_entry(
    t: InputToml,
    name: String,
    entry: Option<String>,
    section: &str,
    resolve_path: &dyn Fn(&str, PathBuf) -> PathBuf,
) -> std::result::Result<(Vec<NamedInput>, Vec<String>), String> {
    use manta_server::band_plan;
    check_input_keys(&t)?;
    let Some(bands) = t.bands.clone() else {
        if t.tuning_range_hz.is_some() {
            return Err("input.tuning_range_hz applies only with input.bands".to_string());
        }
        let input = validate_input(t, resolve_path)?;
        let one = NamedInput {
            name,
            input,
            entry,
            spawn_band_hz: None,
        };
        return Ok((vec![one], Vec::new()));
    };
    if t.freq_hz.is_some() {
        return Err(
            "input.bands and input.freq_hz both set the receiver's centre; keep one".to_string(),
        );
    }
    let range = match t.tuning_range_hz.as_deref() {
        None => band_plan::DEFAULT_TUNING_RANGE_HZ,
        Some(&[lo, hi]) if lo.is_finite() && hi.is_finite() && 0.0 <= lo && lo < hi => (lo, hi),
        Some(v) => {
            return Err(format!(
                "input.tuning_range_hz must be [low, high] in Hz with 0 <= low < high, got [{}]",
                v.iter()
                    .map(|x| float_hz(*x))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        }
    };
    let texts = band_item_texts(&bands)?;
    let items = texts
        .iter()
        .map(|t| band_plan::parse_item(t))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let kind = t
        .kind
        .ok_or_else(|| "input.bands needs input.type (one of kiwi, soapy, hpsdr)".to_string())?;
    let rate_hz = match kind {
        InputKind::Kiwi => None,
        InputKind::Hpsdr => Some(crate::check_hpsdr_rate_hz(
            "input.rate_hz",
            t.rate_hz
                .ok_or("input.rate_hz is required when input.type = \"hpsdr\"")?,
        )?),
        _ => Some(finite_positive(
            "rate_hz",
            t.rate_hz
                .ok_or("input.rate_hz is required when input.type = \"soapy\"")?,
        )?),
    };
    let capture_rate_hz = t
        .capture_rate_hz
        .map(|v| crate::check_capture_rate_hz("input.capture_rate_hz", v))
        .transpose()?;
    let width_hz = crate::band_streams::stream_width_hz(kind, rate_hz, capture_rate_hz);
    let usable_hz = crate::band_streams::usable_width_hz(width_hz);
    let plan = band_plan::plan(&items, range, usable_hz)?;
    let n = plan.streams.len();
    if n > 1 && matches!(kind, InputKind::Hpsdr | InputKind::Soapy) {
        let (why, own) = match kind {
            InputKind::Hpsdr => (
                "bands opens one stream on an HPSDR radio",
                "[[input]] on that radio (several receivers on one radio are one \
                 entry each)",
            ),
            _ => ("manta opens one stream from a SoapySDR device", "device"),
        };
        return Err(format!(
            "input.bands plans {} ({}), but {why}; list bands that fit one stream (a {width_hz} \
             Hz stream skims {} kHz), or give each its own {own}",
            band_plan::count_streams(n),
            plan.band_list(),
            band_plan::exact_khz(usable_hz)
        ));
    }
    // MAN-310 (Codex review, PR #345): one absolute centre override cannot
    // describe several differently tuned streams.
    if n > 1 && t.center_freq_hz.is_some() {
        return Err(format!(
            "input.bands plans {} ({}), but input.center_freq_hz fixes one centre for all of \
             them; drop center_freq_hz, or list bands that fit one stream",
            band_plan::count_streams(n),
            plan.band_list()
        ));
    }
    // MAN-309: one recording file holds one stream's IQ.
    if n > 1 && t.record_iq.is_some() {
        return Err(format!(
            "input.bands plans {} ({}), but input.record_iq names one file for all of them; \
             drop record_iq, or list bands that fit one stream",
            band_plan::count_streams(n),
            plan.band_list()
        ));
    }
    let shown = texts
        .iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let mut notes = vec![format!(
        "note: {section}: bands = [{shown}] plans {}",
        plan.summary()
    )];
    notes.extend(
        plan.notes()
            .into_iter()
            .map(|note| format!("note: {section}: {note}")),
    );
    let mut inputs = Vec::with_capacity(n);
    for (i, stream) in plan.streams.iter().enumerate() {
        let mut copy = t.clone();
        copy.bands = None;
        copy.tuning_range_hz = None;
        copy.freq_hz = Some(stream.centre_hz);
        // One instant starts every replayed input (MAN-13): the first
        // stream keeps the entry's replay_epoch.
        if i > 0 {
            copy.replay_epoch = None;
        }
        let input = validate_input(copy, resolve_path)?;
        inputs.push(NamedInput {
            name: match n {
                1 => name.clone(),
                _ => format!("{name}-{}", stream.label),
            },
            input,
            entry: entry.clone(),
            spawn_band_hz: stream.spawn_band_hz,
        });
    }
    Ok((inputs, notes))
}

/// MAN-310: `input.bands` as item texts: a string, a number (its TOML
/// text) or a list of them.
fn band_item_texts(v: &toml::Value) -> std::result::Result<Vec<String>, String> {
    fn one(v: &toml::Value) -> Option<String> {
        match v {
            toml::Value::String(s) => Some(s.clone()),
            toml::Value::Integer(i) => Some(i.to_string()),
            toml::Value::Float(f) => Some(f.to_string()),
            _ => None,
        }
    }
    let wrong = || {
        format!(
            "input.bands must be \"all\", a band such as \"40m\", a centre frequency in kHz \
             such as \"7030\", or a list of them; got {v}"
        )
    };
    match v {
        toml::Value::Array(items) => items.iter().map(|i| one(i).ok_or_else(wrong)).collect(),
        v => one(v).map(|t| vec![t]).ok_or_else(wrong),
    }
}

/// MAN-310: a split plan's streams, per band, for the input-cap message.
fn plan_list(inputs: &[NamedInput]) -> String {
    let mut parts: Vec<(String, usize)> = Vec::new();
    for n in inputs {
        let band = n
            .input
            .source
            .as_ref()
            .and_then(SourceFromFile::freq_hz)
            .map(manta_server::band::band_for_freq_hz)
            .unwrap_or("unknown")
            .to_string();
        match parts.last_mut() {
            Some((b, count)) if *b == band => *count += 1,
            _ => parts.push((band, 1)),
        }
    }
    parts
        .iter()
        .map(|(b, n)| match n {
            1 => b.clone(),
            n => format!("{b} ({n})"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// MAN-310: a generated stream name must fit the name rules and not repeat
/// another input's name.
fn check_stream_names(inputs: &[NamedInput], origin: &str) -> Result<()> {
    for (i, n) in inputs.iter().enumerate() {
        let quoted = crate::fmt::quoted(&n.name);
        if n.name.chars().count() > MAX_INPUT_NAME_CHARS {
            bail!(
                "{origin}: the stream name {quoted} is longer than {MAX_INPUT_NAME_CHARS} \
                 characters; shorten the entry's name"
            );
        }
        if let Some(earlier) = inputs[..i].iter().find(|o| o.name == n.name) {
            bail!(
                "{origin}: the stream name {quoted} is already input {}",
                crate::fmt::quoted(&earlier.name)
            );
        }
    }
    Ok(())
}

/// MAN-13: the rules that relate several `[[input]]` entries.
fn check_input_entries(inputs: &[NamedInput], origin: &str) -> Result<()> {
    let is_file = |n: &NamedInput| matches!(n.input.source, Some(SourceFromFile::File { .. }));
    if let (Some(file), Some(live)) = (
        inputs.iter().find(|n| is_file(n)),
        inputs.iter().find(|n| !is_file(n)),
    ) {
        bail!(
            "{origin}: [[input]] mixes file replay ({}) with a live receiver ({}); one \
             daemon runs either recordings or receivers",
            crate::fmt::quoted(&file.name),
            crate::fmt::quoted(&live.name)
        );
    }
    let mut first_file: Option<(bool, bool)> = None;
    for (i, n) in inputs.iter().enumerate() {
        let name = crate::fmt::quoted(&n.name);
        if i > 0 && n.input.shared.replay_epoch.is_some() {
            bail!(
                "{origin}: [[input]] {name}: replay_epoch belongs on the first [[input]] \
                 entry; every replayed input starts at that instant"
            );
        }
        match &n.input.source {
            Some(SourceFromFile::File {
                realtime, looped, ..
            }) => match first_file {
                None => first_file = Some((*realtime, *looped)),
                Some(first) if first != (*realtime, *looped) => bail!(
                    "{origin}: [[input]] {name}: realtime and loop must match the first file \
                     input's"
                ),
                Some(_) => {}
            },
            Some(SourceFromFile::Audio { .. }) if n.input.shared.center_freq_hz.is_none() => {
                bail!(
                    "{origin}: [[input]] {name}: type = \"audio\" needs center_freq_hz when \
                     there is more than one input, so its frequencies can be compared with the \
                     other inputs'"
                );
            }
            _ => {}
        }
    }
    Ok(())
}

/// MAN-309: `input.iq_format` and `input.sample_rate_hz` describe a
/// headerless IQ file together, and only an IQ one (`iq = true`). Its centre
/// is `input.center_freq_hz` or `--dial-freq-hz`, checked once merged.
fn raw_iq(t: &InputToml) -> std::result::Result<Option<(manta_input::RawIqFormat, f64)>, String> {
    let (format, rate) = match (&t.iq_format, t.sample_rate_hz) {
        (None, None) => return Ok(None),
        (Some(format), Some(rate)) => (format, rate),
        (Some(_), None) => return Err("input.iq_format needs input.sample_rate_hz".to_string()),
        (None, Some(_)) => return Err("input.sample_rate_hz needs input.iq_format".to_string()),
    };
    if t.iq != Some(true) {
        return Err("input.iq_format needs input.iq = true".to_string());
    }
    let format = format
        .parse::<manta_input::RawIqFormat>()
        .map_err(|e| format!("input.iq_format: {e}"))?;
    if !rate.is_finite() || rate <= 0.0 || rate > manta_input::MAX_IQ_WAV_RATE_HZ {
        return Err(format!(
            "input.sample_rate_hz must be above 0 and at most {} Hz, got {rate}",
            manta_input::MAX_IQ_WAV_RATE_HZ as u64
        ));
    }
    Ok(Some((format, rate)))
}

fn check_hpsdr_mac(text: &str) -> std::result::Result<String, String> {
    let mac = crate::parse_hpsdr_mac(text).map_err(|_| {
        "input.mac must contain twelve hexadecimal digits, for example AA:BB:CC:DD:EE:FF"
            .to_string()
    })?;
    Ok(manta_input::hpsdr::discovery::format_mac(&mac))
}

fn validate_input(
    t: InputToml,
    resolve_path: &dyn Fn(&str, PathBuf) -> PathBuf,
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
        record_iq: t.record_iq.clone().map(|p| resolve_path("record_iq", p)),
    };
    check_input_keys(&t)?;
    let Some(kind) = t.kind else {
        return Ok(InputFile {
            source: None,
            shared,
        });
    };
    let name = kind.name();
    fn req<T>(v: Option<T>, key: &str, name: &str) -> std::result::Result<T, String> {
        v.ok_or_else(|| format!("input.{key} is required when input.type = \"{name}\""))
    }
    // MAN-310: kiwi, soapy and hpsdr take a centre or a band plan.
    let req_freq = |v: Option<f64>| -> std::result::Result<f64, String> {
        v.ok_or_else(|| {
            format!("input.freq_hz or input.bands is required when input.type = \"{name}\"")
        })
    };
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
            path: resolve_path("path", req(t.path.clone(), "path", name)?),
            iq: t.iq.unwrap_or(false),
            realtime: t.realtime.unwrap_or(false),
            looped: t.looped.unwrap_or(false),
            raw: raw_iq(&t)?,
        },
        InputKind::Kiwi => SourceFromFile::Kiwi {
            port: port(8073)?,
            host: req(t.host.clone(), "host", name)?,
            freq_hz: finite_positive("freq_hz", req_freq(t.freq_hz)?)?,
            password: t.password.clone().unwrap_or_default(),
        },
        InputKind::Soapy => SourceFromFile::Soapy {
            driver: req(t.driver.clone(), "driver", name)?,
            freq_hz: finite_positive("freq_hz", req_freq(t.freq_hz)?)?,
            rate_hz: finite_positive("rate_hz", req(t.rate_hz, "rate_hz", name)?)?,
            gain_db: match t.gain_db {
                Some(g) if !g.is_finite() => {
                    return Err(format!("input.gain_db must be finite, got {g}"))
                }
                g => g,
            },
        },
        InputKind::Hpsdr => SourceFromFile::Hpsdr {
            receivers: t
                .receivers
                .as_ref()
                .map(|v| {
                    v.as_integer()
                        .filter(|n| (1..=255).contains(n))
                        .map(|n| n as usize)
                        .ok_or_else(|| "receivers must be an integer from 1 to 255".to_string())
                })
                .transpose()?,
            port: port(crate::HPSDR_CONTROL_PORT)?,
            host: {
                if t.host.is_some() && t.mac.is_some() {
                    return Err("input.host and input.mac cannot both be set".into());
                }
                t.host.clone()
            },
            mac: t.mac.as_deref().map(check_hpsdr_mac).transpose()?,
            protocol: t
                .protocol
                .as_ref()
                .map(ProtocolValue::protocol)
                .transpose()?
                .unwrap_or_default(),
            freq_hz: crate::check_hpsdr_freq_hz("input.freq_hz", req_freq(t.freq_hz)?)?,
            rate_hz: crate::check_hpsdr_rate_hz("input.rate_hz", req(t.rate_hz, "rate_hz", name)?)?,
        },
        InputKind::Ka9q => SourceFromFile::Ka9q {
            port: port(manta_input::ka9q::wire::DATA_PORT)?,
            group: check_ka9q_group(req(t.group.clone(), "group", name)?)?,
            ssrc: t.ssrc.map(check_ka9q_ssrc).transpose()?,
            interface: t
                .interface
                .as_deref()
                .map(check_ka9q_interface)
                .transpose()?,
        },
    };
    Ok(InputFile {
        source: Some(source),
        shared,
    })
}

/// D5: every source key set belongs to `input.type`, and a source key
/// needs a type.
fn check_input_keys(t: &InputToml) -> std::result::Result<(), String> {
    let set: [(&str, bool); 22] = [
        ("receivers", t.receivers.is_some()),
        ("device", t.device.is_some()),
        ("path", t.path.is_some()),
        ("iq", t.iq.is_some()),
        ("realtime", t.realtime.is_some()),
        ("loop", t.looped.is_some()),
        ("iq_format", t.iq_format.is_some()),
        ("sample_rate_hz", t.sample_rate_hz.is_some()),
        ("host", t.host.is_some()),
        ("mac", t.mac.is_some()),
        ("port", t.port.is_some()),
        ("protocol", t.protocol.is_some()),
        ("freq_hz", t.freq_hz.is_some()),
        ("password", t.password.is_some()),
        ("driver", t.driver.is_some()),
        ("rate_hz", t.rate_hz.is_some()),
        ("gain_db", t.gain_db.is_some()),
        ("group", t.group.is_some()),
        ("ssrc", t.ssrc.is_some()),
        ("interface", t.interface.is_some()),
        ("bands", t.bands.is_some()),
        ("tuning_range_hz", t.tuning_range_hz.is_some()),
    ];
    let Some(kind) = t.kind else {
        if let Some((key, _)) = set.iter().find(|(_, is_set)| *is_set) {
            return Err(format!(
                "input.{key} needs input.type (one of audio, file, kiwi, soapy, hpsdr, ka9q)"
            ));
        }
        return Ok(());
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
    Ok(())
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
                 MANTA_<SERVER|RBN_COMPARE|INPUT|SPOT|DETECTOR|DECODE>_<KEY>)"
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

    // MAN-13: one variable cannot say which of several receivers it sets.
    if let Some(toml::Value::Array(inputs)) = doc.get("input") {
        if let Some((name, _, key, _)) = entries.iter().find(|(_, table, _, _)| *table == "input") {
            bail!(
                "{name} cannot say which of the {} [[input]] entries it sets; set {key} in \
                 that entry instead",
                inputs.len()
            );
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

    #[test]
    fn pattern_validation_level_accepts_minimal_and_rejects_unknown() {
        let loaded = load_file("[spot]\nvalidation_level = 'minimal'\n").unwrap();
        assert_eq!(
            loaded.spot.validation_level,
            manta_spot::ValidationLevel::Minimal
        );
        let err = err_of("[spot]\nvalidation_level = 'strict'\n");
        assert!(err.contains("minimal") && err.contains("normal"), "{err}");
    }

    #[test]
    fn pattern_settings_env_override_file_and_file_paths_are_relative() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manta.toml");
        std::fs::write(
            &path,
            "[spot]\nvalidation_level = 'normal'\npattern_path = 'patterns.lst'\n",
        )
        .unwrap();
        let env = vars(&[
            ("MANTA_SPOT_VALIDATION_LEVEL", "minimal"),
            ("MANTA_SPOT_PATTERN_PATH", "2026"),
        ]);
        let loaded = load(Some(&path), Env::Read(&env)).unwrap();
        assert_eq!(
            loaded.spot.validation_level,
            manta_spot::ValidationLevel::Minimal
        );
        assert_eq!(loaded.spot.pattern_path, Some(PathBuf::from("2026")));
        let loaded = load(Some(&path), Env::Ignore).unwrap();
        assert_eq!(
            loaded.spot.pattern_path,
            Some(dir.path().join("patterns.lst"))
        );
    }

    #[test]
    fn env_cty_and_scp_paths_are_string_typed_and_cwd_relative() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manta.toml");
        std::fs::write(
            &path,
            "[spot]\ncty_path = 'file.dat'\nscp_path = 'file.scp'\n",
        )
        .unwrap();
        let v = vars(&[
            ("MANTA_SPOT_CTY_PATH", "2026"),
            ("MANTA_SPOT_SCP_PATH", "2027"),
        ]);
        let loaded = load(Some(&path), Env::Read(&v)).unwrap();
        assert_eq!(loaded.spot.cty_path, Some(PathBuf::from("2026")));
        assert_eq!(loaded.spot.scp_path, Some(PathBuf::from("2027")));
    }

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

    #[test]
    fn a_multi_stream_plan_refuses_a_fixed_center() {
        // MAN-310 (Codex review, PR #345): every generated stream would
        // otherwise report the same overridden centre.
        let err = err_of(
            "[input]\ntype = \"kiwi\"\nhost = \"h\"\nbands = [\"60m\"]\n\
             center_freq_hz = 5357000.0\n",
        );
        assert!(
            err.contains("input.center_freq_hz fixes one centre for all of them"),
            "{err}"
        );
        // One stream keeps the override.
        let l = load_file(
            "[input]\ntype = \"kiwi\"\nhost = \"h\"\nbands = [\"7030\"]\n\
             center_freq_hz = 7030000.0\n",
        )
        .unwrap();
        assert_eq!(l.inputs.len(), 1);
    }

    #[test]
    fn a_multi_stream_plan_refuses_one_recording_file() {
        // MAN-309: every generated stream would otherwise record to the
        // same destination.
        let err = err_of(
            "[input]\ntype = \"kiwi\"\nhost = \"h\"\nbands = [\"60m\"]\n\
             record_iq = \"s.cf32\"\n",
        );
        assert!(
            err.contains("input.record_iq names one file for all of them"),
            "{err}"
        );
        // One stream keeps its recording.
        let l = load_file(
            "[input]\ntype = \"kiwi\"\nhost = \"h\"\nbands = [\"7030\"]\n\
             record_iq = \"s.cf32\"\n",
        )
        .unwrap();
        assert_eq!(l.inputs.len(), 1);
        assert!(l.inputs[0].input.shared.record_iq.is_some());
    }

    #[test]
    fn a_tiny_soapy_rate_is_refused_without_materializing_the_plan() {
        let err = err_of("[input]\ntype = \"soapy\"\nrate_hz = 0.001\nbands = \"all\"\n");
        assert!(err.contains("more than 256 streams"), "{err}");
    }

    #[test]
    fn an_expanded_entry_carries_its_entry_and_spawn_band() {
        let l = load_file("[input]\ntype = \"kiwi\"\nhost = \"h\"\nbands = [\"60m\"]\n").unwrap();
        let names: Vec<&str> = l.inputs.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["kiwi-60m-1", "kiwi-60m-2"]);
        assert!(l.inputs.iter().all(|n| n.entry.is_none()));
        assert_eq!(
            l.inputs[0].spawn_band_hz,
            Some((f64::NEG_INFINITY, 3_843.75))
        );
        assert_eq!(l.inputs[1].spawn_band_hz, Some((-3_843.75, f64::INFINITY)));
        assert_eq!(l.input, l.inputs[0].input);
        assert!(l.notes[0].starts_with("note: [input]: bands = [\"60m\"] plans 2 streams"));

        let l = load_file(
            "[[input]]\nname = \"hl2\"\ntype = \"hpsdr\"\nhost = \"h\"\nrate_hz = 96000.0\n\
             bands = \"40m\"\n\n[[input]]\nname = \"k\"\ntype = \"kiwi\"\nhost = \"h\"\n\
             freq_hz = 7030000.0\n",
        )
        .unwrap();
        assert_eq!(l.inputs[0].name, "hl2");
        assert_eq!(l.inputs[0].entry.as_deref(), Some("hl2"));
        assert_eq!(l.inputs[0].spawn_band_hz, None);
        assert_eq!(
            l.inputs[0]
                .input
                .source
                .as_ref()
                .and_then(SourceFromFile::freq_hz),
            Some(7_020_000.0)
        );
        assert_eq!(l.inputs[1].entry.as_deref(), Some("k"));
    }

    #[test]
    fn each_stream_passes_validate_input() {
        let with_freq = err_of("[input]\ntype = \"kiwi\"\nhost = \"h\"\nport = 0\nfreq_hz = 7e6\n");
        let with_bands =
            err_of("[input]\ntype = \"kiwi\"\nhost = \"h\"\nport = 0\nbands = \"60m\"\n");
        assert!(
            with_freq.ends_with("input.port must be between 1 and 65535, got 0"),
            "{with_freq}"
        );
        assert!(
            with_bands.ends_with("input.port must be between 1 and 65535, got 0"),
            "{with_bands}"
        );
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

    const HPSDR_DISCOVERY: &str =
        "[input]\ntype = \"hpsdr\"\nfreq_hz = 7030000.0\nrate_hz = 192000.0\n";

    #[test]
    fn hpsdr_discovery_accepts_neither_address_or_one_selector() {
        for selector in [
            "",
            "host = \"radio.local\"\n",
            "mac = \"aa-bbcc-ddee-ff\"\n",
            "mac = \"aabbccddeeff\"\n",
        ] {
            assert!(
                load_file(&format!("{HPSDR_DISCOVERY}{selector}")).is_ok(),
                "{selector}"
            );
        }
        let err = err_of(&format!(
            "{HPSDR_DISCOVERY}host = \"h\"\nmac = \"aabbccddeeff\"\n"
        ));
        assert!(
            err.contains("input.host and input.mac cannot both be set"),
            "{err}"
        );
    }

    #[test]
    fn hpsdr_discovery_mac_env_is_string_typed_and_normalized() {
        let loaded = load_env(
            Some(HPSDR_DISCOVERY),
            &[("MANTA_INPUT_MAC", "001122334455")],
        )
        .unwrap();
        let debug = format!("{:?}", loaded.input.source);
        assert!(debug.contains("00:11:22:33:44:55"), "{debug}");
        assert!(err_of(&format!("{HPSDR_DISCOVERY}mac = \"AA\"\n")).contains("input.mac"));
        assert!(
            err_of("[input]\ntype = \"audio\"\nmac = \"001122334455\"\n")
                .contains("input.mac does not apply")
        );
        assert!(err_of(&format!("{HPSDR_DISCOVERY}device = \"wrong\"\n"))
            .contains("hpsdr takes host, mac, port"));
    }

    #[test]
    fn hpsdr_hostless_inputs_with_one_selector_share_one_radio() {
        // MAN-306 x MAN-307: hostless entries with the same MAC selector (or
        // none) name one discovered radio, so they share its receivers.
        for selector in ["", "mac = \"aabbccddeeff\"\n"] {
            let body = ["a", "b"].map(|name| format!("[[input]]\nname = \"{name}\"\ntype = \"hpsdr\"\nfreq_hz = 7030000.0\nrate_hz = 192000.0\n{selector}")).join("\n");
            let loaded = load_file(&body).unwrap_or_else(|e| panic!("{body} -> {e:#}"));
            let groups = hpsdr_allocations(&loaded.inputs);
            assert_eq!(groups.len(), 1, "{body}");
            assert_eq!(groups[0].members, vec![0, 1]);
            assert_eq!(groups[0].host, None);
        }
    }

    #[test]
    fn hpsdr_hostless_inputs_with_different_selectors_are_separate_radios() {
        let entry = |name: &str, selector: &str| {
            format!("[[input]]\nname = \"{name}\"\ntype = \"hpsdr\"\nfreq_hz = 7030000.0\nrate_hz = 192000.0\n{selector}")
        };
        // Distinct MACs, or a pinned entry beside an unqualified one: two
        // radios offline. The run refuses the pair if both resolve to one
        // address.
        for (first, second) in [
            ("mac = \"aabbccddeeff\"\n", "mac = \"001122334455\"\n"),
            ("mac = \"aabbccddeeff\"\n", ""),
            ("", "mac = \"aabbccddeeff\"\n"),
            ("host = \"192.0.2.10\"\n", ""),
        ] {
            let body = format!("{}\n{}", entry("a", first), entry("b", second));
            let loaded = load_file(&body).unwrap_or_else(|e| panic!("{body} -> {e:#}"));
            let groups = hpsdr_allocations(&loaded.inputs);
            assert_eq!(groups.len(), 2, "{body}");
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

    // ---- MAN-13: [[input]] ----

    const KA9Q_40: &str =
        "[[input]]\nname = \"40m\"\ntype = \"ka9q\"\ngroup = \"239.1.2.3\"\nssrc = 7030000\n";
    const KA9Q_20: &str = "[[input]]\nname = \"20m\"\ntype = \"ka9q\"\ngroup = \"239.1.2.3\"\nssrc = 14030000\nfreq_correction_ppm = 0.8\n";

    fn file_entry(name: &str, extra: &str) -> String {
        format!("[[input]]\nname = \"{name}\"\ntype = \"file\"\npath = \"/x/{name}.wav\"\n{extra}")
    }

    #[test]
    fn array_of_input_tables_loads_each_entry_in_order() {
        let l = load_file(&format!("{KA9Q_40}{KA9Q_20}")).unwrap();
        let names: Vec<&str> = l.inputs.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["40m", "20m"]);
        assert_eq!(l.input, l.inputs[0].input);
        let Some(SourceFromFile::Ka9q { ssrc, .. }) = &l.inputs[1].input.source else {
            panic!("{:?}", l.inputs[1]);
        };
        assert_eq!(*ssrc, Some(14_030_000));
        assert_eq!(l.inputs[1].input.shared.freq_correction_ppm, Some(0.8));
        assert_eq!(l.inputs[0].input.shared.freq_correction_ppm, None);
    }

    #[test]
    fn one_entry_input_array_is_the_single_form() {
        let array = load_file("[[input]]\ntype = \"audio\"\n").unwrap();
        let table = load_file("[input]\ntype = \"audio\"\n").unwrap();
        assert_eq!(array.input, table.input);
        assert_eq!(array.inputs, table.inputs);
        assert_eq!(array.inputs[0].name, "audio");
    }

    #[test]
    fn input_names_are_required_with_two_or_more_entries() {
        let err = err_of(&format!(
            "{KA9Q_40}[[input]]\ntype = \"kiwi\"\nhost = \"k\"\nfreq_hz = 14030000.0\n"
        ));
        assert!(
            err.ends_with(
                ": [[input]] entry 2 has no name -- with more than one [[input]], give each one a \
                 name, for example name = \"20m\""
            ),
            "{err}"
        );
    }

    #[test]
    fn input_names_must_be_unique() {
        let err = err_of(&format!("{KA9Q_20}{KA9Q_20}"));
        assert!(
            err.ends_with(": [[input]] name \"20m\" is used twice"),
            "{err}"
        );
    }

    #[test]
    fn input_name_charset_and_length() {
        let want = "may use only letters, digits, '.', '_' and '-', start with a letter or digit, \
                    and be at most 32 characters";
        for bad in ["a b", "-x", "", &"x".repeat(33)] {
            let err = err_of(&format!("{KA9Q_40}{}", file_entry(bad, "")));
            assert!(
                err.contains(&format!("[[input]] name {bad:?} {want}")),
                "{err}"
            );
        }
        let err = err_of("[input]\nname = \"a b\"\ntype = \"audio\"\n");
        assert!(err.contains(want), "{err}");
        for good in ["20m", "rsp.1", "kiwi_a-2", &"x".repeat(32)] {
            assert!(load_file(&format!("{}{}", file_entry("a", ""), file_entry(good, ""))).is_ok());
        }
    }

    #[test]
    fn more_than_sixteen_inputs_are_refused() {
        let body: String = (0..17).map(|i| file_entry(&format!("in{i}"), "")).collect();
        let err = err_of(&body);
        assert!(
            err.ends_with(": at most 16 [[input]] entries are supported, found 17"),
            "{err}"
        );
    }

    #[test]
    fn every_entry_needs_a_type_with_two_or_more() {
        let err = err_of(&format!("{KA9Q_40}[[input]]\nname = \"20m\"\n"));
        assert!(
            err.ends_with(
                ": [[input]] \"20m\" has no type -- with more than one [[input]], each one names \
                 its receiver type"
            ),
            "{err}"
        );
    }

    #[test]
    fn file_and_live_inputs_cannot_mix() {
        let err = err_of(&format!("{}{KA9Q_20}", file_entry("a", "")));
        assert!(
            err.ends_with(
                ": [[input]] mixes file replay (\"a\") with a live receiver (\"20m\"); one daemon \
                 runs either recordings or receivers"
            ),
            "{err}"
        );
    }

    #[test]
    fn replay_epoch_belongs_on_the_first_entry() {
        let first = file_entry("a", "replay_epoch = 1700000000\n");
        assert!(load_file(&format!("{first}{}", file_entry("b", ""))).is_ok());
        let err = err_of(&format!(
            "{}{}",
            file_entry("a", ""),
            file_entry("b", "replay_epoch = 1700000000\n")
        ));
        assert!(
            err.ends_with(
                ": [[input]] \"b\": replay_epoch belongs on the first [[input]] entry; every \
                 replayed input starts at that instant"
            ),
            "{err}"
        );
    }

    #[test]
    fn file_inputs_must_agree_on_realtime_and_loop() {
        let err = err_of(&format!(
            "{}{}",
            file_entry("a", "realtime = true\n"),
            file_entry("b", "")
        ));
        assert!(
            err.ends_with(": [[input]] \"b\": realtime and loop must match the first file input's"),
            "{err}"
        );
        assert!(load_file(&format!(
            "{}{}",
            file_entry("a", "loop = true\n"),
            file_entry("b", "loop = true\n")
        ))
        .is_ok());
    }

    fn hpsdr_entry(name: &str, host: &str, extra: &str) -> String {
        format!("[[input]]\nname = \"{name}\"\ntype = \"hpsdr\"\nhost = \"{host}\"\nrate_hz = 192000.0\nfreq_hz = 14030000.0\n{extra}")
    }

    #[test]
    fn hpsdr_allocation_is_ordered_and_offline() {
        let text = format!(
            "{}{}{}{}{}",
            hpsdr_entry("a", "RADIO.invalid", "receivers = 10\n"),
            hpsdr_entry("other", "another.invalid", ""),
            hpsdr_entry("b", "radio.invalid", "port = 1024\n"),
            hpsdr_entry("c", "radio.invalid", ""),
            hpsdr_entry("d", "radio.invalid", "")
        );
        let loaded = load_file(&text).unwrap();
        let groups = hpsdr_allocations(&loaded.inputs);
        assert_eq!(groups[0].members, vec![0, 2, 3, 4]);
        assert_eq!(groups[0].receivers, Some(10));
        assert_eq!(groups[1].members, vec![1]);
        assert!(load_file(&format!(
            "{}{}",
            hpsdr_entry("a", "radio", ""),
            hpsdr_entry("b", "radio", "port = 1025\n")
        ))
        .is_ok());
    }

    #[test]
    fn hpsdr_groups_share_one_protocol_and_refuse_protocol_2() {
        // A protocol-2 radio opens one receiver per radio in manta, so a
        // shared radio must speak protocol 1.
        let err = err_of(
            &(hpsdr_entry("a", "radio", "protocol = \"2\"\n") + &hpsdr_entry("b", "radio", "")),
        );
        assert!(
            err.contains(
                "HPSDR protocol 2 opens one receiver per radio in manta; give each band its own \
                 receiver with protocol \"1\""
            ),
            "{err}"
        );
        // Members of one radio must agree on the protocol.
        let err = err_of(
            &(hpsdr_entry("a", "radio", "") + &hpsdr_entry("b", "radio", "protocol = \"2\"\n")),
        );
        assert!(
            err.contains(
                "protocol must match the HPSDR radio's other receivers (set by input \"a\")"
            ),
            "{err}"
        );
        // A lone input keeps the protocol it configured, including 2.
        assert!(load_file(&hpsdr_entry("a", "radio", "protocol = \"2\"\n")).is_ok());
        // Protocol 1 on every member of a shared radio is accepted.
        assert!(load_file(
            &(hpsdr_entry("a", "radio", "protocol = \"1\"\n")
                + &hpsdr_entry("b", "radio", "protocol = \"1\"\n"))
        )
        .is_ok());
    }

    #[test]
    fn hpsdr_declarations_and_active_limits_are_checked() {
        for value in ["0", "-1", "1.5", "256", "\"four\""] {
            let err = err_of(&hpsdr_entry(
                "a",
                "radio",
                &format!("receivers = {value}\n"),
            ));
            assert!(
                err.contains("receivers must be an integer from 1 to 255"),
                "{err}"
            );
        }
        let first = hpsdr_entry("a", "radio", "receivers = 4\n");
        let rest: String = (1..6)
            .map(|i| hpsdr_entry(&format!("b{i}"), "radio", ""))
            .collect();
        assert!(err_of(&(first + &rest)).contains("declares 4 receivers; 6 bands are configured"));
        let err = err_of(
            &(hpsdr_entry("a", "radio", "") + &hpsdr_entry("b", "radio", "receivers = 4\n")),
        );
        assert!(
            err.contains("receivers belongs on the first input for this HPSDR radio (\"a\")"),
            "{err}"
        );
        let err = err_of(
            &(hpsdr_entry("a", "radio", "")
                + &hpsdr_entry("b", "radio", "").replace("192000.0", "96000.0")),
        );
        assert!(err.contains("rate_hz must match the HPSDR radio's other receivers (192000 Hz, set by input \"a\")"), "{err}");
        let nine: String = (0..9)
            .map(|i| hpsdr_entry(&format!("b{i}"), "radio", ""))
            .collect();
        assert!(load_file(&nine).is_err());
        for header in ["[input]", "[[input]]"] {
            let text = hpsdr_entry("a", "radio", "receivers = 4\n").replace("[[input]]", header);
            assert!(load_file(&text).is_ok());
        }
        let err = err_of(
            "[input]\ntype = \"kiwi\"\nhost = \"kiwi\"\nfreq_hz = 7030000.0\nreceivers = 4\n",
        );
        assert!(
            err.contains("input.receivers does not apply to input.type = \"kiwi\""),
            "{err}"
        );
    }

    #[test]
    fn audio_input_needs_center_freq_hz_with_two_or_more() {
        let err = err_of(&format!(
            "{KA9Q_40}[[input]]\nname = \"b\"\ntype = \"audio\"\n"
        ));
        assert!(
            err.ends_with(
                ": [[input]] \"b\": type = \"audio\" needs center_freq_hz when there is more than \
                 one input, so its frequencies can be compared with the other inputs'"
            ),
            "{err}"
        );
        assert!(load_file(&format!(
            "{KA9Q_40}[[input]]\nname = \"b\"\ntype = \"audio\"\ncenter_freq_hz = 7030000.0\n"
        ))
        .is_ok());
    }

    #[test]
    fn manta_input_env_is_refused_with_two_or_more_inputs() {
        let f = write_cfg(&format!(
            "{KA9Q_40}{KA9Q_20}{}",
            KA9Q_20.replace("20m", "15m")
        ));
        let v = vars(&[("MANTA_INPUT_FREQ_CORRECTION_PPM", "1.5")]);
        let err = format!("{:#}", load(Some(f.path()), Env::Read(&v)).unwrap_err());
        assert_eq!(
            err,
            "MANTA_INPUT_FREQ_CORRECTION_PPM cannot say which of the 3 [[input]] entries it \
             sets; set freq_correction_ppm in that entry instead"
        );
    }

    #[test]
    fn name_is_accepted_on_a_single_input_table() {
        let l = load_file("[input]\nname = \"rsp\"\ntype = \"audio\"\n").unwrap();
        assert_eq!(l.inputs.len(), 1);
        assert_eq!(l.inputs[0].name, "rsp");
        let untyped = load_file("[input]\nfreq_correction_ppm = 1.0\n").unwrap();
        assert_eq!(untyped.inputs[0].name, "audio");
        let typed =
            load_file(&file_entry("x", "").replace("[[input]]\nname = \"x\"\n", "[input]\n"))
                .unwrap();
        assert_eq!(typed.inputs[0].name, "file");
    }

    #[test]
    fn per_entry_keys_are_validated_with_the_entrys_name_in_the_error() {
        let err = err_of(&format!(
            "{KA9Q_40}[[input]]\nname = \"b\"\ntype = \"kiwi\"\nhost = \"k\"\nfreq_hz = \
             14030000.0\nssrc = 5\n"
        ));
        assert!(err.contains(": [[input]] \"b\": input.ssrc"), "{err}");
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

    /// MAN-319: the live-uplink locator gate's message, pinned once here so
    /// the implementation below cannot drift from the docs' phrasing.
    const MAN319_LOCATOR_MSG: &str =
        "dry_run = false -- sending spots needs [server].operator_grid \
                                     set to a 6-character Maidenhead locator";

    fn live_uplink(host: &str) -> String {
        format!(
            "[[rbn_uplink]]\nenabled = true\ntarget_host = \"{host}\"\ntarget_port = 7300\ndry_run \
             = false\n"
        )
    }

    #[test]
    fn live_uplink_without_operator_grid_fails_load() {
        let err = err_of(&format!("{SERVER}{}", live_uplink("127.0.0.1")));
        assert!(err.contains(MAN319_LOCATOR_MSG), "{err}");
        assert!(err.contains("target 127.0.0.1:7300"), "{err}");
    }

    #[test]
    fn live_uplink_with_a_four_character_grid_fails_load() {
        let err = err_of(&format!(
            "{SERVER}operator_grid = \"FN03\"\n{}",
            live_uplink("127.0.0.1")
        ));
        assert!(err.contains(MAN319_LOCATOR_MSG), "{err}");
        assert!(err.contains("\"FN03\" is only 4 characters"), "{err}");
    }

    #[test]
    fn live_uplink_with_a_six_character_grid_loads() {
        let loaded = load_file(&format!(
            "{SERVER}operator_grid = \"FN03ab\"\n{}",
            live_uplink("127.0.0.1")
        ))
        .unwrap();
        assert_eq!(loaded.rbn_uplink.len(), 1);
    }

    #[test]
    fn dry_run_uplink_without_a_grid_still_loads() {
        // `dry_run` omitted: the default (true, MAN-159) needs no locator.
        load_file(&format!(
            "{SERVER}[[rbn_uplink]]\nenabled = true\ntarget_host = \"x\"\ntarget_port = 7300\n"
        ))
        .unwrap();
    }

    #[test]
    fn explicitly_dry_run_uplink_without_a_grid_still_loads() {
        load_file(&format!(
            "{SERVER}[[rbn_uplink]]\nenabled = true\ntarget_host = \"x\"\ntarget_port = \
             7300\ndry_run = true\n"
        ))
        .unwrap();
    }

    #[test]
    fn disabled_uplink_with_dry_run_false_and_no_grid_still_loads() {
        load_file(&format!(
            "{SERVER}[[rbn_uplink]]\nenabled = false\ntarget_host = \"x\"\ntarget_port = \
             7300\ndry_run = false\n"
        ))
        .unwrap();
    }

    #[test]
    fn second_live_uplink_without_a_grid_is_named() {
        // A dry-run entry first, then the live one: the error names the
        // live target, not the first entry.
        let err = err_of(&format!(
            "{SERVER}[[rbn_uplink]]\nenabled = true\ntarget_host = \"dry.example\"\
             \ntarget_port = 7000\ndry_run = true\n{}",
            live_uplink("live.example")
        ));
        assert!(err.contains(MAN319_LOCATOR_MSG), "{err}");
        assert!(err.contains("target live.example:7300"), "{err}");
        assert!(!err.contains("dry.example"), "{err}");
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

    /// MAN-324: the live-state keys get `MANTA_SERVER_*` variables from
    /// the generic overlay, and the environment beats the file.
    #[test]
    fn manta_server_live_port_overrides_the_file() {
        let body = format!("{SERVER}live_port = 7303\nlive_bind_addr = \"127.0.0.1\"\n");
        for raw in ["0.0.0.0", "0", "true"] {
            let loaded = load_env(
                Some(&body),
                &[
                    ("MANTA_SERVER_LIVE_PORT", "17303"),
                    ("MANTA_SERVER_LIVE_BIND_ADDR", raw),
                ],
            )
            .unwrap_or_else(|err| panic!("live_bind_addr={raw}: {err:#}"));
            let server = loaded.server.unwrap();
            assert_eq!(server.live_port, Some(17303));
            assert_eq!(server.live_bind_addr, raw);
        }
    }

    /// MAN-138: the numeric location keys load from the file and from
    /// `MANTA_SERVER_*`, TOML-probed like every other number.
    #[test]
    fn server_location_keys_load_from_the_file_and_the_environment() {
        let body = format!("{SERVER}operator_lat = 41.714775\n");
        let loaded = load_env(Some(&body), &[("MANTA_SERVER_OPERATOR_LON", "-72.72726")]).unwrap();
        let server = loaded.server.unwrap();
        assert_eq!(server.operator_lat, Some(41.714775));
        assert_eq!(server.operator_lon, Some(-72.72726));

        let loaded = load_env(
            Some(SERVER),
            &[
                ("MANTA_SERVER_OPERATOR_LAT", "41"),
                ("MANTA_SERVER_OPERATOR_LON", "-72.72726"),
            ],
        )
        .unwrap();
        assert_eq!(loaded.server.unwrap().operator_lat, Some(41.0));
    }

    #[test]
    fn an_inconsistent_station_location_is_rejected_before_io() {
        for (keys, message) in [
            (
                "operator_lat = 41.7\n",
                "operator_lat is set but operator_lon is not",
            ),
            (
                "operator_lon = -72.7\n",
                "operator_lon is set but operator_lat is not",
            ),
            (
                "operator_grid = \"FN31pr\"\noperator_lat = 40.0\noperator_lon = -74.0\n",
                "is outside operator_grid \"FN31pr\"",
            ),
        ] {
            let f = write_cfg(&format!("{SERVER}{keys}"));
            let err = format!("{:#}", load(Some(f.path()), Env::Ignore).unwrap_err());
            assert!(
                err.starts_with(&format!("{}: [server]: ", f.path().display())),
                "{err}"
            );
            assert!(err.contains(message), "{err}");
        }
        // A variable that supplied the value is named, as for typed errors.
        let err = format!(
            "{:#}",
            load_env(Some(SERVER), &[("MANTA_SERVER_OPERATOR_LAT", "41.7")]).unwrap_err()
        );
        assert!(
            err.contains("(environment: MANTA_SERVER_OPERATOR_LAT)"),
            "{err}"
        );
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
    fn detector_guard_hz_is_rejected_as_source_derived() {
        let err = err_of("[detector]\nguard_hz = 900.0\n");
        assert!(
            err.contains(
                "detector.guard_hz is not configurable: it is derived from the source's audio \
                 front end (manta-dsp::hilbert, MAN-273)"
            ),
            "{err}"
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

    /// MAN-312: `crash_filter` is string-typed, so `MANTA_DETECTOR_CRASH_FILTER=off`
    /// reaches the loader as the string `"off"` and beats the file.
    #[test]
    fn env_crash_filter_overrides_the_file() {
        let loaded = load_file("[detector]\ncrash_filter = \"starts-only\"\n").unwrap();
        assert_eq!(
            loaded.detector.crash_filter,
            manta_engine::CrashFilter::StartsOnly
        );
        let loaded = load_env(
            Some("[detector]\ncrash_filter = \"starts-only\"\n"),
            &[("MANTA_DETECTOR_CRASH_FILTER", "off")],
        )
        .unwrap();
        assert_eq!(loaded.detector.crash_filter, manta_engine::CrashFilter::Off);
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
            "[spot]\nallowlist = [\"W1AW\"]\nblocklist_path = \"bad.txt\"\ncty_path = \"cty.dat\"\nscp_path = \"sub/MASTER.SCP\"\n",
        )
        .unwrap();
        let loaded = load(Some(&path), Env::Ignore).unwrap();
        assert_eq!(loaded.spot.allowlist, vec!["W1AW".to_string()]);
        assert_eq!(loaded.spot.blocklist_path, Some(dir.path().join("bad.txt")));
        assert_eq!(loaded.spot.notch_path, None);
        assert_eq!(loaded.spot.cty_path, Some(dir.path().join("cty.dat")));
        assert_eq!(
            loaded.spot.scp_path,
            Some(dir.path().join("sub/MASTER.SCP"))
        );
    }

    #[test]
    fn spot_unknown_key_is_rejected() {
        let err = err_of("[spot]\nblocklist = \"x\"\n");
        assert!(err.contains("unknown field"), "{err}");
        assert!(err.contains("blocklist_path"), "{err}");
        assert!(
            err.contains("cty_path") && err.contains("scp_path"),
            "{err}"
        );
    }

    // ---- MAN-316: list refresh and pins

    const PIN: &str = "e9ff91a3c3ab28c5f7dbcffb4280944a84645ed53d41cb4c68a41f132ee13fd1";

    #[test]
    fn spot_refresh_keys_parse_with_defaults() {
        let spot = load_file("[spot]\n").unwrap().spot;
        assert!(!spot.refresh_lists);
        assert_eq!(spot.list_cache_dir, None);
        assert_eq!(
            (spot.cty_url.as_deref(), spot.scp_url.as_deref()),
            (None, None)
        );
        assert_eq!(spot.cty_url(), DEFAULT_CTY_URL);
        assert_eq!(spot.scp_url(), DEFAULT_SCP_URL);
        assert_eq!(DEFAULT_CTY_URL, "http://www.country-files.com/cty/cty.dat");
        assert_eq!(
            DEFAULT_SCP_URL,
            "http://www.supercheckpartial.com/MASTER.SCP"
        );
        assert_eq!((spot.cty_sha256, spot.scp_sha256), (None, None));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manta.toml");
        std::fs::write(
            &path,
            "[spot]\nrefresh_lists = true\nlist_cache_dir = \"cache\"\n\
             cty_url = \"http://mirror.lan:8080/cty.dat\"\n\
             scp_url = \"http://mirror.lan/MASTER.SCP\"\n",
        )
        .unwrap();
        let spot = load(Some(&path), Env::Ignore).unwrap().spot;
        assert!(spot.refresh_lists);
        assert_eq!(spot.list_cache_dir, Some(dir.path().join("cache")));
        assert_eq!(spot.cty_url(), "http://mirror.lan:8080/cty.dat");
        assert_eq!(spot.scp_url(), "http://mirror.lan/MASTER.SCP");
    }

    /// Mirrors `env_cty_and_scp_paths_are_string_typed_and_cwd_relative`.
    #[test]
    fn env_refresh_keys_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manta.toml");
        std::fs::write(&path, "[spot]\nlist_cache_dir = 'from-file'\n").unwrap();
        let loaded = load(Some(&path), Env::Ignore).unwrap();
        assert_eq!(
            loaded.spot.list_cache_dir,
            Some(dir.path().join("from-file"))
        );

        let v = vars(&[
            ("MANTA_SPOT_REFRESH_LISTS", "true"),
            ("MANTA_SPOT_LIST_CACHE_DIR", "2026"),
            ("MANTA_SPOT_CTY_URL", "http://10.0.0.1/cty.dat"),
            ("MANTA_SPOT_SCP_URL", "http://10.0.0.1:8080/MASTER.SCP"),
        ]);
        let loaded = load(Some(&path), Env::Read(&v)).unwrap();
        assert!(loaded.spot.refresh_lists, "TOML-probed to a bool");
        // Verbatim and CWD-relative, not joined onto the config's directory.
        assert_eq!(loaded.spot.list_cache_dir, Some(PathBuf::from("2026")));
        assert_eq!(loaded.spot.cty_url(), "http://10.0.0.1/cty.dat");
        assert_eq!(loaded.spot.scp_url(), "http://10.0.0.1:8080/MASTER.SCP");
        assert_eq!(
            loaded.raw["spot"]["list_cache_dir"],
            toml::Value::String("2026".into())
        );
    }

    #[test]
    fn an_https_or_malformed_list_url_is_refused_before_io() {
        let f = write_cfg("[spot]\ncty_url = \"https://www.country-files.com/cty/cty.dat\"\n");
        let err = format!("{:#}", load(Some(f.path()), Env::Ignore).unwrap_err());
        assert_eq!(
            err,
            format!(
                "{}: [spot]: cty_url must be an http:// URL (manta has no TLS support), got \
                 \"https://www.country-files.com/cty/cty.dat\"",
                f.path().display()
            )
        );
        for bad in [
            "ftp://mirror.lan/MASTER.SCP",
            "www.supercheckpartial.com/MASTER.SCP",
            "",
        ] {
            let err = err_of(&format!("[spot]\nscp_url = \"{bad}\"\n"));
            assert!(
                err.contains(&format!(
                    "[spot]: scp_url must be an http:// URL (manta has no TLS support), got \
                     \"{bad}\""
                )),
                "{err}"
            );
        }
        // An http:// URL with no host still fails, with the parser's reason.
        let err = err_of("[spot]\ncty_url = \"http://\"\n");
        assert!(
            err.contains("[spot]: cty_url must be an http:// URL (manta has no TLS support), got \"http://\": "),
            "{err}"
        );
        // From the environment, the variable is named.
        let err = format!(
            "{:#}",
            load_env(None, &[("MANTA_SPOT_SCP_URL", "https://x/MASTER.SCP")]).unwrap_err()
        );
        assert!(
            err.starts_with("environment: [spot]: scp_url must be"),
            "{err}"
        );
        assert!(err.ends_with("(environment: MANTA_SPOT_SCP_URL)"), "{err}");
    }

    #[test]
    fn spot_pins_are_validated_lowercased_and_string_typed_from_env() {
        let upper = PIN.to_ascii_uppercase();
        let spot = load_file(&format!("[spot]\ncty_sha256 = \"{upper}\"\n"))
            .unwrap()
            .spot;
        assert_eq!(spot.cty_sha256.as_deref(), Some(PIN));
        assert_eq!(spot.scp_sha256, None);

        // An all-digit hash stays a string (it would TOML-probe as a number).
        let digits = "1".repeat(64);
        let loaded = load_env(
            None,
            &[
                ("MANTA_SPOT_CTY_SHA256", digits.as_str()),
                ("MANTA_SPOT_SCP_SHA256", PIN),
            ],
        )
        .unwrap();
        assert_eq!(loaded.spot.cty_sha256, Some(digits));
        assert_eq!(loaded.spot.scp_sha256.as_deref(), Some(PIN));

        let bad_pins = [
            "0069220".to_string(),
            format!("{PIN}0"),
            format!("{}g", &PIN[..63]),
        ];
        for bad in &bad_pins {
            let err = err_of(&format!("[spot]\nscp_sha256 = \"{bad}\"\n"));
            assert!(
                err.contains(&format!(
                    "[spot]: scp_sha256 must be 64 hexadecimal digits (a SHA-256), got \"{bad}\""
                )),
                "{err}"
            );
        }
        assert_eq!(parse_sha256(&upper), Ok(PIN.to_string()));
        assert_eq!(
            parse_sha256("abc"),
            Err("expected 64 hexadecimal digits (a SHA-256)".to_string())
        );
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
            ("type = \"hpsdr\"\nfreq_hz = 7e6\n", "rate_hz", "hpsdr"),
            ("type = \"hpsdr\"\nrate_hz = 192000.0\n", "freq_hz", "hpsdr"),
        ] {
            let err = err_of(&format!("[input]\n{body}"));
            // MAN-310: kiwi, soapy and hpsdr take freq_hz or bands.
            let key = match key {
                "freq_hz" => "freq_hz or input.bands",
                k => k,
            };
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

    const HPSDR_BODY: &str =
        "[input]\ntype = \"hpsdr\"\nhost = \"192.0.2.10\"\nfreq_hz = 7030000.0\nrate_hz = 192000.0\n";

    fn hpsdr_protocol_of(source: &Option<SourceFromFile>) -> manta_input::hpsdr::HpsdrProtocol {
        match source {
            Some(SourceFromFile::Hpsdr { protocol, .. }) => *protocol,
            other => panic!("expected an hpsdr input, got {other:?}"),
        }
    }

    #[test]
    fn hpsdr_protocol_key_accepts_auto_1_2_strings_and_integers() {
        use manta_input::hpsdr::HpsdrProtocol;
        for (value, want) in [
            ("\"auto\"", HpsdrProtocol::Auto),
            ("\"1\"", HpsdrProtocol::One),
            ("\"2\"", HpsdrProtocol::Two),
            ("1", HpsdrProtocol::One),
            ("2", HpsdrProtocol::Two),
        ] {
            let body = format!("{HPSDR_BODY}protocol = {value}\n");
            let loaded = load_file(&body).unwrap_or_else(|e| panic!("{value}: {e:#}"));
            assert_eq!(hpsdr_protocol_of(&loaded.input.source), want, "{value}");
        }
    }

    #[test]
    fn hpsdr_protocol_defaults_to_auto() {
        let loaded = load_file(HPSDR_BODY).unwrap();
        assert_eq!(
            loaded.input.source,
            Some(SourceFromFile::Hpsdr {
                host: Some("192.0.2.10".into()),
                mac: None,
                port: 1024,
                protocol: manta_input::hpsdr::HpsdrProtocol::Auto,
                freq_hz: 7_030_000.0,
                rate_hz: 192_000.0,
                receivers: None,
            })
        );
    }

    #[test]
    fn protocol_key_is_refused_for_other_input_types() {
        let err =
            err_of("[input]\ntype = \"kiwi\"\nhost = \"h\"\nfreq_hz = 7e6\nprotocol = \"2\"\n");
        assert!(
            err.contains("input.protocol does not apply to input.type = \"kiwi\""),
            "{err}"
        );
        let err = err_of("[input]\nprotocol = \"2\"\n");
        assert!(err.contains("input.protocol needs input.type"), "{err}");
    }

    #[test]
    fn manta_input_protocol_env_is_read_verbatim() {
        assert!(STRING_TYPED_ENV_KEYS.contains(&("input", "protocol")));
        let loaded = load_env(
            None,
            &[
                ("MANTA_INPUT_TYPE", "hpsdr"),
                ("MANTA_INPUT_HOST", "192.0.2.10"),
                ("MANTA_INPUT_FREQ_HZ", "7030000"),
                ("MANTA_INPUT_RATE_HZ", "192000"),
                ("MANTA_INPUT_PROTOCOL", "2"),
            ],
        )
        .unwrap();
        assert_eq!(
            hpsdr_protocol_of(&loaded.input.source),
            manta_input::hpsdr::HpsdrProtocol::Two
        );
    }

    #[test]
    fn invalid_protocol_value_names_the_choices() {
        for (value, got) in [
            ("\"3\"", "got \"3\""),
            ("3", "got 3"),
            ("\"two\"", "got \"two\""),
        ] {
            let err = err_of(&format!("{HPSDR_BODY}protocol = {value}\n"));
            assert!(
                err.contains(&format!(
                    "input.protocol must be \"auto\", \"1\" or \"2\", {got}"
                )),
                "{value}: {err}"
            );
        }
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

    // ---- MAN-140: type = "ka9q"

    const KA9Q: &str = "[input]\ntype = \"ka9q\"\ngroup = \"239.101.6.1\"\n";

    #[test]
    fn ka9q_input_defaults_port_5004_and_no_ssrc_or_interface() {
        let loaded = load_file(KA9Q).unwrap();
        assert_eq!(
            loaded.input.source,
            Some(SourceFromFile::Ka9q {
                group: "239.101.6.1".into(),
                port: 5004,
                ssrc: None,
                interface: None,
            })
        );
        let loaded = load_file(&format!(
            "{KA9Q}port = 5010\nssrc = 14100000\ninterface = \"192.168.1.20\"\n"
        ))
        .unwrap();
        assert_eq!(
            loaded.input.source,
            Some(SourceFromFile::Ka9q {
                group: "239.101.6.1".into(),
                port: 5010,
                ssrc: Some(14_100_000),
                interface: Some(Ipv4Addr::new(192, 168, 1, 20)),
            })
        );
    }

    #[test]
    fn ka9q_requires_group() {
        let err = err_of("[input]\ntype = \"ka9q\"\n");
        assert!(
            err.ends_with("[input]: input.group is required when input.type = \"ka9q\""),
            "{err}"
        );
    }

    #[test]
    fn ka9q_group_is_checked_at_load() {
        let group = |g: &str| format!("[input]\ntype = \"ka9q\"\ngroup = \"{g}\"\n");
        let err = err_of(&group("192.168.1.5"));
        assert!(
            err.ends_with(
                "[input]: input.group must be an IPv4 multicast address (224.0.0.0 to \
                 239.255.255.255) or a host name, got 192.168.1.5"
            ),
            "{err}"
        );
        let err = err_of(&group("ff02::1"));
        assert!(
            err.ends_with(
                "[input]: input.group: IPv6 groups are not supported yet; give radiod's IPv4 \
                 data group, got ff02::1"
            ),
            "{err}"
        );
        // A guide placeholder left in place is refused, not resolved.
        let err = err_of(&group("<your-radiod-data-group>"));
        assert!(
            err.contains("[input]: input.group must be an IPv4 multicast address"),
            "{err}"
        );
        for ok in ["239.101.6.1", "cw-20m.local"] {
            let loaded = load_file(&group(ok)).unwrap();
            let Some(SourceFromFile::Ka9q { group, .. }) = loaded.input.source else {
                panic!("expected a ka9q source for {ok}");
            };
            assert_eq!(group, ok);
        }
    }

    #[test]
    fn ka9q_interface_must_be_an_ipv4_address() {
        let err = err_of(&format!("{KA9Q}interface = \"eth0\"\n"));
        assert!(
            err.ends_with("[input]: input.interface must be an IPv4 address, got eth0"),
            "{err}"
        );
        let err = err_of(&format!("{KA9Q}interface = \"::1\"\n"));
        assert!(
            err.ends_with("[input]: input.interface must be an IPv4 address, got ::1"),
            "{err}"
        );
    }

    #[test]
    fn ka9q_ssrc_range_is_u32() {
        let err = err_of(&format!("{KA9Q}ssrc = -1\n"));
        assert!(
            err.ends_with("[input]: input.ssrc must be between 0 and 4294967295, got -1"),
            "{err}"
        );
        let err = err_of(&format!("{KA9Q}ssrc = 4294967296\n"));
        assert!(
            err.ends_with("[input]: input.ssrc must be between 0 and 4294967295, got 4294967296"),
            "{err}"
        );
        for (ssrc, want) in [("0", 0), ("4294967295", u32::MAX)] {
            let loaded = load_file(&format!("{KA9Q}ssrc = {ssrc}\n")).unwrap();
            let Some(SourceFromFile::Ka9q { ssrc, .. }) = loaded.input.source else {
                panic!("expected a ka9q source");
            };
            assert_eq!(ssrc, Some(want));
        }
    }

    #[test]
    fn ka9q_rejects_keys_of_other_types() {
        let err = err_of(&format!("{KA9Q}freq_hz = 14100000.0\n"));
        assert!(
            err.ends_with(
                "[input]: input.freq_hz does not apply to input.type = \"ka9q\" (ka9q takes \
                 group, port, ssrc, interface)"
            ),
            "{err}"
        );
        // And the ka9q keys belong to ka9q only.
        let err = err_of("[input]\ntype = \"kiwi\"\nhost = \"h\"\nfreq_hz = 7e6\nssrc = 1\n");
        assert!(
            err.contains("input.ssrc does not apply to input.type = \"kiwi\""),
            "{err}"
        );
    }

    #[test]
    fn keys_without_a_type_name_ka9q_among_the_types() {
        let err = err_of("[input]\ngroup = \"239.101.6.1\"\n");
        assert!(
            err.ends_with(
                "[input]: input.group needs input.type (one of audio, file, kiwi, soapy, hpsdr, \
                 ka9q)"
            ),
            "{err}"
        );
    }

    #[test]
    fn ka9q_group_and_interface_env_values_stay_strings() {
        // `127.0.0.1` and `239.101.6.1` are not TOML values, and a group
        // name could look like one; both keys are taken verbatim.
        let loaded = load_env(
            None,
            &[
                ("MANTA_INPUT_TYPE", "ka9q"),
                ("MANTA_INPUT_GROUP", "239.101.6.1"),
                ("MANTA_INPUT_INTERFACE", "127.0.0.1"),
                ("MANTA_INPUT_SSRC", "14100000"),
            ],
        )
        .unwrap();
        assert_eq!(
            loaded.input.source,
            Some(SourceFromFile::Ka9q {
                group: "239.101.6.1".into(),
                port: 5004,
                ssrc: Some(14_100_000),
                interface: Some(Ipv4Addr::LOCALHOST),
            })
        );
        assert!(STRING_TYPED_ENV_KEYS.contains(&("input", "group")));
        assert!(STRING_TYPED_ENV_KEYS.contains(&("input", "interface")));
        // A numeric-looking host name stays a string rather than a TOML
        // integer the loader would reject as the wrong type.
        let err = load_env(
            None,
            &[("MANTA_INPUT_TYPE", "ka9q"), ("MANTA_INPUT_GROUP", "1234")],
        )
        .err()
        .map(|e| format!("{e:#}"))
        .unwrap_or_default();
        assert!(!err.contains("invalid type"), "{err}");
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
                realtime: false,
                looped: false,
                raw: None,
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

    /// MAN-314: `MANTA_DETECTOR_RTTY_FILTER=false` overrides the file.
    #[test]
    fn env_rtty_filter_overrides_the_file() {
        let loaded = load_env(
            Some("[detector]\nrtty_filter = true\nrtty_dwell_ms = 30000\n"),
            &[("MANTA_DETECTOR_RTTY_FILTER", "false")],
        )
        .unwrap();
        assert!(!loaded.detector.rtty_filter);
        assert_eq!(loaded.detector.rtty_dwell_hops, 11250);
    }

    #[test]
    fn digital_window_env_overrides_the_file() {
        for (file, env, expected) in [("true", "false", false), ("false", "true", true)] {
            let loaded = load_env(
                Some(&format!("[detector]\ndigital_window_filter = {file}\n")),
                &[("MANTA_DETECTOR_DIGITAL_WINDOW_FILTER", env)],
            )
            .unwrap();
            assert_eq!(
                loaded.detector,
                DetectorConfig {
                    digital_window_filter: expected,
                    ..DetectorConfig::default()
                }
            );
        }
    }

    #[test]
    fn env_non_cw_filter_overrides_the_file() {
        let loaded = load_env(
            Some("[detector]\nnon_cw_filter = true\nunkeyed_close_ms = 20000\n"),
            &[("MANTA_DETECTOR_NON_CW_FILTER", "false")],
        )
        .unwrap();
        assert!(!loaded.detector.non_cw_filter);
        assert_eq!(loaded.detector.unkeyed_close_hops, 7500);
    }

    #[test]
    fn env_unkeyed_close_ms_overrides_the_file() {
        let loaded = load_env(
            Some("[detector]\nunkeyed_close_ms = 20000\n"),
            &[("MANTA_DETECTOR_UNKEYED_CLOSE_MS", "15000")],
        )
        .unwrap();
        assert!(loaded.detector.non_cw_filter);
        assert_eq!(loaded.detector.unkeyed_close_hops, 5625);
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
                realtime: false,
                looped: false,
                raw: None,
            })
        );
    }

    // ---- MAN-269: paced and looping file replay

    #[test]
    fn input_realtime_and_loop_are_file_keys() {
        // Absolute on every OS (a bare "/x.wav" is relative on Windows).
        let wav = std::env::temp_dir().join("x.wav");
        let loaded = load_file(&format!(
            "[input]\ntype = \"file\"\npath = '{}'\nrealtime = true\nloop = true\n",
            wav.display()
        ))
        .unwrap();
        assert_eq!(
            loaded.input.source,
            Some(SourceFromFile::File {
                path: wav,
                iq: false,
                realtime: true,
                looped: true,
                raw: None,
            })
        );
        let err = err_of("[input]\ntype = \"kiwi\"\nhost = \"h\"\nfreq_hz = 7e6\nloop = true\n");
        assert!(
            err.contains(
                "input.loop does not apply to input.type = \"kiwi\" (kiwi takes host, port, \
                 freq_hz, password, bands, tuning_range_hz)"
            ),
            "{err}"
        );
    }

    #[test]
    fn manta_input_loop_env_sets_the_loop_key() {
        let loaded = load_env(
            None,
            &[
                ("MANTA_INPUT_TYPE", "file"),
                ("MANTA_INPUT_PATH", "x.wav"),
                ("MANTA_INPUT_LOOP", "true"),
            ],
        )
        .unwrap();
        assert_eq!(
            loaded.input.source,
            Some(SourceFromFile::File {
                path: PathBuf::from("x.wav"),
                iq: false,
                realtime: false,
                looped: true,
                raw: None,
            })
        );
    }

    /// MAN-309: `iq_format`/`sample_rate_hz` describe a headerless IQ file
    /// together, on an IQ file input only, and the environment's
    /// `MANTA_INPUT_IQ_FORMAT` is a string.
    #[test]
    fn headerless_iq_keys_are_validated_together() {
        let loaded = load_env(
            None,
            &[
                ("MANTA_INPUT_TYPE", "file"),
                ("MANTA_INPUT_PATH", "x.ci16"),
                ("MANTA_INPUT_IQ", "true"),
                ("MANTA_INPUT_IQ_FORMAT", "ci16"),
                ("MANTA_INPUT_SAMPLE_RATE_HZ", "96000"),
            ],
        )
        .unwrap();
        assert_eq!(
            loaded.input.source,
            Some(SourceFromFile::File {
                path: PathBuf::from("x.ci16"),
                iq: true,
                realtime: false,
                looped: false,
                raw: Some((manta_input::RawIqFormat::Ci16, 96_000.0)),
            })
        );
        let file = "[input]\ntype = \"file\"\npath = \"x.cf32\"\n";
        for (extra, want) in [
            (
                "iq = true\niq_format = \"cf32\"\n",
                "input.iq_format needs input.sample_rate_hz",
            ),
            (
                "iq = true\nsample_rate_hz = 96000\n",
                "input.sample_rate_hz needs input.iq_format",
            ),
            (
                "iq_format = \"cf32\"\nsample_rate_hz = 96000\n",
                "input.iq_format needs input.iq = true",
            ),
            (
                "iq = true\niq_format = \"cs8\"\nsample_rate_hz = 96000\n",
                "unknown IQ format `cs8`; expected cf32 or ci16",
            ),
            (
                "iq = true\niq_format = \"cf32\"\nsample_rate_hz = 0\n",
                "input.sample_rate_hz must be above 0 and at most 10000000 Hz",
            ),
            (
                "iq = true\niq_format = \"cf32\"\nsample_rate_hz = 2e7\n",
                "input.sample_rate_hz must be above 0 and at most 10000000 Hz",
            ),
        ] {
            let err = err_of(&format!("{file}{extra}"));
            assert!(err.contains(want), "{extra}: {err}");
        }
        let err =
            err_of("[input]\ntype = \"kiwi\"\nhost = \"h\"\nfreq_hz = 7e6\niq_format = \"cf32\"\n");
        assert!(
            err.contains("input.iq_format does not apply to input.type = \"kiwi\""),
            "{err}"
        );
    }

    #[test]
    fn metrics_bind_addr_from_the_environment_is_a_string() {
        // "1234" would TOML-probe to an integer and fail serde if the key
        // were not string-typed.
        let loaded = load_env(
            None,
            &[
                ("MANTA_SERVER_STATION_CALLSIGN", "W1AW"),
                ("MANTA_SERVER_METRICS_BIND_ADDR", "1234"),
            ],
        )
        .unwrap();
        assert_eq!(loaded.server.unwrap().metrics_bind_addr, "1234");
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
    fn operator_text_env_values_stay_strings() {
        // MAN-86: free-text banner fields; `12345` and `true` are a legal
        // name or QTH, never a TOML integer or boolean.
        let loaded = load_env(
            None,
            &[
                ("MANTA_SERVER_STATION_CALLSIGN", "W1AW"),
                ("MANTA_SERVER_OPERATOR_NAME", "true"),
                ("MANTA_SERVER_OPERATOR_QTH", "12345"),
            ],
        )
        .unwrap();
        let server = loaded
            .server
            .expect("a [server] table from the environment");
        assert_eq!(server.operator_name.as_deref(), Some("true"));
        assert_eq!(server.operator_qth.as_deref(), Some("12345"));
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
                realtime: false,
                looped: false,
                raw: None,
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
    fn loaded_carries_the_overlaid_document_and_env_var_names() {
        let loaded = load_env(
            Some("[detector]\non_snr_db = 20.0\n"),
            &[("MANTA_DETECTOR_ON_SNR_DB", "15")],
        )
        .unwrap();
        assert_eq!(
            loaded.raw["detector"]["on_snr_db"],
            toml::Value::Integer(15)
        );
        assert_eq!(
            loaded.env_vars,
            vec!["MANTA_DETECTOR_ON_SNR_DB".to_string()]
        );
        let loaded = load_env(
            None,
            &[
                ("MANTA_CONFIG", "/etc/manta.toml"),
                ("MANTA_GIT_SHA", "abc123"),
            ],
        )
        .unwrap();
        assert!(loaded.env_vars.is_empty(), "{:?}", loaded.env_vars);
    }

    /// Both the names build.rs reads (`rerun-if-env-changed=`) and the ones
    /// it exports (`rustc-env=`, MAN-83): Cargo puts the latter into the
    /// environment of `cargo run`/`cargo test` children.
    #[test]
    fn every_build_rs_env_name_is_ignored() {
        let build_rs = include_str!("../build.rs");
        let names: Vec<&str> = ["rerun-if-env-changed=", "rustc-env="]
            .into_iter()
            .flat_map(|directive| build_rs.split(directive).skip(1))
            .filter_map(|rest| {
                rest.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .next()
            })
            .filter(|name| name.starts_with(ENV_PREFIX))
            .collect();
        assert!(names.contains(&"MANTA_FEATURES"), "{names:?}");
        assert!(!names.is_empty());
        for name in names {
            assert!(ENV_IGNORED.contains(&name), "{name} must be in ENV_IGNORED");
        }
    }

    // ---- MAN-301: [rbn_compare]

    #[test]
    fn rbn_compare_is_a_known_table() {
        assert!(KNOWN_TABLES.contains(&"rbn_compare"));
        let loaded = load_file(&format!(
            "{SERVER}[rbn_compare]\nenabled = true\nwindow_secs = 240\nspotters = [\"k5tr\"]\n"
        ))
        .unwrap();
        let rc = loaded.rbn_compare.expect("[rbn_compare] reaches Loaded");
        assert!(rc.enabled);
        assert_eq!(rc.window_secs, 240);
        assert_eq!(rc.spotters, vec!["K5TR".to_string()]);
        assert!(loaded.notes.is_empty(), "{:?}", loaded.notes);
        assert!(load_file(SERVER).unwrap().rbn_compare.is_none());
    }

    #[test]
    fn rbn_compare_enabled_without_server_is_rejected() {
        let f = write_cfg("[rbn_compare]\nenabled = true\n");
        let err = format!("{:#}", load(Some(f.path()), Env::Ignore).unwrap_err());
        assert_eq!(
            err,
            format!(
                "{}: [rbn_compare] needs a [server] table (grades are published on the JSON \
                 stream and /metrics, which only the daemon runs)",
                f.path().display()
            )
        );
        // The rbn_uplink refusal still applies alongside.
        let err = err_of("[[rbn_uplink]]\nenabled = false\ntarget_host = \"x\"\ntarget_port = 1\n");
        assert!(
            err.contains("[[rbn_uplink]] needs a [server] table"),
            "{err}"
        );
    }

    #[test]
    fn rbn_compare_disabled_without_server_is_a_note() {
        let loaded = load_file("[rbn_compare]\nenabled = false\n").unwrap();
        assert_eq!(
            loaded.notes,
            vec!["note: [rbn_compare] is off and ignored".to_string()]
        );
        assert!(loaded.server.is_none());
        assert!(loaded.rbn_compare.is_some_and(|rc| !rc.enabled));
        // A bad value in an inert table is still an error.
        let err = err_of("[rbn_compare]\nwindow_secs = 10\n");
        assert!(
            err.ends_with(": [rbn_compare].window_secs must be 30 to 3600, got 10"),
            "{err}"
        );
    }

    #[test]
    fn rbn_compare_env_overlay() {
        let body = format!("{SERVER}[rbn_compare]\nenabled = true\nwindow_secs = 120\n");
        let loaded = load_env(Some(&body), &[("MANTA_RBN_COMPARE_WINDOW_SECS", "240")]).unwrap();
        assert_eq!(loaded.rbn_compare.unwrap().window_secs, 240);
        assert_eq!(
            loaded.env_vars,
            vec!["MANTA_RBN_COMPARE_WINDOW_SECS".to_string()]
        );
        let loaded =
            load_env(Some(&body), &[("MANTA_RBN_COMPARE_FEED_HOST", "127.0.0.1")]).unwrap();
        assert_eq!(loaded.rbn_compare.unwrap().feed_host, "127.0.0.1");
        // String-typed: `12345` reaches the callsign check as a string, not
        // an integer type error, and is then refused as a callsign.
        let err = format!(
            "{:#}",
            load_env(
                Some(&body),
                &[("MANTA_RBN_COMPARE_LOGIN_CALLSIGN", "12345")]
            )
            .unwrap_err()
        );
        assert!(
            err.contains("login_callsign \"12345\" is not a plausible callsign"),
            "{err}"
        );
        assert!(err.contains("MANTA_RBN_COMPARE_LOGIN_CALLSIGN"), "{err}");
        let err = format!(
            "{:#}",
            load_env(Some(&body), &[("MANTA_RBN_COMPARE_FOO", "1")]).unwrap_err()
        );
        assert!(err.contains("unknown field `foo`"), "{err}");
        assert!(err.contains("MANTA_RBN_COMPARE_FOO"), "{err}");
        // The environment alone can turn it on, and then needs [server].
        let err = format!(
            "{:#}",
            load_env(None, &[("MANTA_RBN_COMPARE_ENABLED", "true")]).unwrap_err()
        );
        assert!(
            err.starts_with("environment: [rbn_compare] needs a [server] table"),
            "{err}"
        );
    }

    #[test]
    fn rbn_compare_spotters_must_not_be_own_call() {
        let body = "[server]\nstation_callsign = \"W3XYZ-1\"\n[rbn_compare]\nenabled = true\n";
        let f = write_cfg(&format!("{body}spotters = [\"K5TR\", \"w3xyz\"]\n"));
        let err = format!("{:#}", load(Some(f.path()), Env::Ignore).unwrap_err());
        assert_eq!(
            err,
            format!(
                "{}: [rbn_compare].spotters lists W3XYZ, this station's own callsign: its \
                 reports are always excluded, so every spot would grade none",
                f.path().display()
            )
        );
        let err = err_of(&format!("{body}spotters = [\"W3XYZ-1\"]\n"));
        assert!(err.contains("spotters lists W3XYZ-1"), "{err}");
        assert!(load_file(&format!("{body}spotters = [\"K5TR\"]\n")).is_ok());
    }

    #[test]
    fn unknown_table_message_lists_rbn_compare() {
        let err = err_of("[telemetry]\nx = 1\n");
        assert!(
            err.ends_with(
                "-- manta reads [server], [[rbn_uplink]], [rbn_compare], [input], [spot], \
                 [detector], [decode]"
            ),
            "{err}"
        );
        let err = err_of("[rbn_compar]\nenabled = true\n");
        assert!(err.contains("did you mean [rbn_compare]?"), "{err}");
    }

    #[test]
    fn env_overrides_server_history_minutes() {
        let loaded = load_env(
            Some("[server]\nstation_callsign = \"W1AW\"\nhistory_minutes = 30\n"),
            &[("MANTA_SERVER_HISTORY_MINUTES", "45")],
        )
        .unwrap();
        assert_eq!(loaded.server.unwrap().history_minutes, Some(45));
    }
}
