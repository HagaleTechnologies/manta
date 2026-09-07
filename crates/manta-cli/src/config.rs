//! The single operator-facing TOML config surface (MAN-74).
//!
//! Lives in `manta-cli`, not `manta-server` (home of the old
//! `DaemonConfigFile`, now removed): `manta-server` has no dependency on
//! `manta-engine`/`manta-decode`/`manta-input`, so it cannot hold a
//! `detector: manta_engine::DetectorConfig` field next to `[server]`
//! without inverting its current position as a leaf-ish output-layer
//! crate. `manta-cli` is the only crate with edges to all four
//! (`manta-server`, `manta-engine`, `manta-decode`, `manta-input`), so the
//! one place that parses the whole daemon TOML lives here, composing
//! per-table sub-structs that stay defined in whichever crate already owns
//! that data (`[server]`/`[[rbn_uplink]]` stay `manta_server::config`
//! types, unchanged).
//!
//! `deny_unknown_fields` at the TOP level here is deliberate, and is the
//! reversal of a round-11 fix on the now-deleted
//! `manta_server::config::DaemonConfigFile`: that type had to tolerate
//! unknown top-level tables because it modeled only `[server]`/
//! `[[rbn_uplink]]` of the six tables `docs/SPEC-decode-core.md` §9
//! documents. All six are modeled here, so tolerating an unrecognized
//! table is no longer "don't break real configs" -- it is silently
//! swallowing settings an operator believes are in effect (MAN-74's
//! reported bug: `[input].freq_correction_ppm = 999999`, out of the range
//! the equivalent CLI flag enforces, loaded with no error at all).
//!
//! Precedence, per key: CLI flag > `MANTA_<TABLE>_<KEY>` environment
//! variable > this file > the compiled-in `Default`. The environment tier
//! is implemented as a TOML-document overlay (`apply_env_overlay`): env
//! values are spliced into the parsed document BEFORE typed
//! deserialization, so they are validated by exactly the same
//! `deny_unknown_fields` + per-field checks a file value is -- there is no
//! second, divergent validation path for env input.
//!
//! `manta decode`/`manta gen` never call anything in this module -- SPEC's
//! "file input -> byte-identical spot logs" determinism contract runs
//! through `decode`, and an ambient config file or `MANTA_*` variable that
//! silently retuned the decoder would make that contract depend on the
//! machine's environment.

use anyhow::{bail, Context, Result};
use manta_decode::decoder::DecodeConfig;
use manta_engine::PipelineConfig;
use manta_server::config::{RbnUplinkConfig, ServerConfig};
use serde::{Deserialize, Deserializer};
use std::path::{Path, PathBuf};

fn default_kiwi_port() -> u16 {
    8073
}

/// HPSDR/Hermes's standard Metis discovery/control port. Duplicated as a
/// plain constant (rather than reusing `manta_input::hpsdr::CONTROL_PORT`)
/// because `[input]` must parse regardless of which `manta-cli` features
/// are built -- `manta_input::hpsdr` only exists behind the `hpsdr`
/// feature (see `open_source_spec`'s own feature gate for where the
/// feature actually matters: opening the source, not parsing the table).
const DEFAULT_HPSDR_PORT: u16 = 1024;

fn default_hpsdr_port() -> u16 {
    DEFAULT_HPSDR_PORT
}

fn validate_ppm<E: serde::de::Error>(ppm: f64) -> std::result::Result<f64, E> {
    manta_spot::calibration_factor_from_ppm(ppm).map_err(E::custom)?;
    Ok(ppm)
}

/// Shared by every `InputSource` variant's `freq_correction_ppm` field --
/// reuses `manta_spot::calibration_factor_from_ppm`, the SAME validation
/// the CLI's `--freq-correction-ppm` flag applies (`parse_freq_correction_ppm`
/// in `main.rs`), so a file value and a flag value can never disagree about
/// what is in range. This is MAN-74's headline repro: `999999` inside
/// `[input]` used to parse with no error at all.
fn de_opt_ppm<'de, D>(deserializer: D) -> std::result::Result<Option<f64>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw: Option<f64> = Option::deserialize(deserializer)?;
    match raw {
        Some(ppm) => Ok(Some(validate_ppm(ppm)?)),
        None => Ok(None),
    }
}

/// Shared by every `InputSource` variant's `dial_freq_hz` field -- mirrors
/// the CLI's `--dial-freq-hz` value_parser (`parse_dial_freq_hz` in
/// `main.rs`).
fn de_opt_dial_freq<'de, D>(deserializer: D) -> std::result::Result<Option<f64>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw: Option<f64> = Option::deserialize(deserializer)?;
    if let Some(hz) = raw {
        if !hz.is_finite() || hz <= 0.0 {
            return Err(serde::de::Error::custom(format!(
                "dial_freq_hz must be a finite, positive number of Hz, got {hz}"
            )));
        }
    }
    Ok(raw)
}

/// The `[input]` table: an internally-tagged union, one variant per source
/// type. `deny_unknown_fields` is on the enum itself, applying to whichever
/// variant `type` selects -- NOT implemented via `#[serde(flatten)]`,
/// which serde documents (and this was confirmed live while designing this
/// module) silently disables `deny_unknown_fields` entirely.
///
/// `type` is required whenever `[input]` is present. `type = "audio"` with
/// no `device` is the "let the CLI/OS pick the default input device"
/// spelling -- not a degenerate case, but exactly what an operator running
/// against a rig's sound card writes.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum InputSource {
    Audio {
        #[serde(default)]
        device: Option<String>,
        #[serde(default, deserialize_with = "de_opt_ppm")]
        freq_correction_ppm: Option<f64>,
        #[serde(default, deserialize_with = "de_opt_dial_freq")]
        dial_freq_hz: Option<f64>,
    },
    File {
        path: PathBuf,
        #[serde(default, deserialize_with = "de_opt_ppm")]
        freq_correction_ppm: Option<f64>,
        #[serde(default, deserialize_with = "de_opt_dial_freq")]
        dial_freq_hz: Option<f64>,
    },
    Kiwi {
        host: String,
        #[serde(default = "default_kiwi_port")]
        port: u16,
        freq_hz: f64,
        #[serde(default)]
        password: String,
        #[serde(default, deserialize_with = "de_opt_ppm")]
        freq_correction_ppm: Option<f64>,
        #[serde(default, deserialize_with = "de_opt_dial_freq")]
        dial_freq_hz: Option<f64>,
    },
    Soapy {
        driver: String,
        freq_hz: f64,
        rate_hz: f64,
        #[serde(default)]
        gain_db: Option<f64>,
        #[serde(default, deserialize_with = "de_opt_ppm")]
        freq_correction_ppm: Option<f64>,
        #[serde(default, deserialize_with = "de_opt_dial_freq")]
        dial_freq_hz: Option<f64>,
    },
    Hpsdr {
        host: String,
        #[serde(default = "default_hpsdr_port")]
        port: u16,
        freq_hz: f64,
        rate_hz: f64,
        #[serde(default, deserialize_with = "de_opt_ppm")]
        freq_correction_ppm: Option<f64>,
        #[serde(default, deserialize_with = "de_opt_dial_freq")]
        dial_freq_hz: Option<f64>,
    },
}

impl InputSource {
    pub fn freq_correction_ppm(&self) -> Option<f64> {
        match self {
            InputSource::Audio {
                freq_correction_ppm,
                ..
            }
            | InputSource::File {
                freq_correction_ppm,
                ..
            }
            | InputSource::Kiwi {
                freq_correction_ppm,
                ..
            }
            | InputSource::Soapy {
                freq_correction_ppm,
                ..
            }
            | InputSource::Hpsdr {
                freq_correction_ppm,
                ..
            } => *freq_correction_ppm,
        }
    }

    pub fn dial_freq_hz(&self) -> Option<f64> {
        match self {
            InputSource::Audio { dial_freq_hz, .. }
            | InputSource::File { dial_freq_hz, .. }
            | InputSource::Kiwi { dial_freq_hz, .. }
            | InputSource::Soapy { dial_freq_hz, .. }
            | InputSource::Hpsdr { dial_freq_hz, .. } => *dial_freq_hz,
        }
    }
}

/// One fully-resolved source, after `[input]` -> CLI merging
/// (`resolve_source`). A separate type from `InputSource` because the CLI
/// side has no `type` tag or shared-key deserializer concerns -- just the
/// per-source-kind data `open_source_spec` (`main.rs`) needs to actually
/// open a connection.
#[derive(Debug, Clone, PartialEq)]
pub enum SourceSpec {
    Audio {
        device: Option<String>,
    },
    File {
        path: PathBuf,
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

impl SourceSpec {
    /// Kiwi/Soapy/Hpsdr sources report their own real RF center frequency;
    /// Audio/File (a sound card or a WAV replay) do not -- mirrors
    /// `main.rs`'s existing `has_rf_aware_source` check, which decides
    /// whether `--dial-freq-hz` is required alongside a daemon config.
    pub fn is_rf_aware(&self) -> bool {
        matches!(
            self,
            SourceSpec::Kiwi { .. } | SourceSpec::Soapy { .. } | SourceSpec::Hpsdr { .. }
        )
    }
}

/// The `[spot]` table: the MAN-28 Watch List (`allowlist`, an inline TOML
/// array, matching `PipelineConfig.allowlist` directly) plus MAN-31's
/// operator suppression lists. `blocklist_path`/`notch_path` point at the
/// SAME flat-text file format `--blocklist`/`--notch` already read
/// (`manta_spot::{Blocklist, NotchList}::parse`) -- SPEC §9 documents only
/// `allowlist` as an inline array and is silent on how the other two
/// operator-editable files enter TOML at all; pointing at a path keeps the
/// existing "drop in a text file, restart" workflow CW Skimmer/Aggregator
/// operators already know, and needs no changes to `Blocklist`/`NotchList`
/// themselves.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpotTable {
    #[serde(default)]
    pub allowlist: Option<Vec<String>>,
    #[serde(default)]
    pub blocklist_path: Option<PathBuf>,
    #[serde(default)]
    pub notch_path: Option<PathBuf>,
}

/// `[detector]`, overlaid onto `manta_engine::DetectorConfig::default()`
/// (never onto SPEC §9's literal text -- see `resolve_detector`'s doc
/// comment for why that distinction matters for `on_snr_db` specifically).
/// SPEC §9 states `confirm_ms`/`hang_ms`/`gc_ms`/`warmup_ms` in
/// milliseconds; `DetectorConfig` stores them in hops at the channelizer's
/// fixed 375 Hz rate -- `ms_to_hops_checked` below is the conversion,
/// reusing `manta_decode::ms_to_hops`, the crate's single normative
/// rounding rule (SPEC §1.1), so a TOML value and the spec's own worked
/// examples can never round differently.
///
/// `floor_quantile`/`floor_window_ms`/`block_channels`/`block_allowance_db`
/// are documented in SPEC §9 but compiled in as `const`s in
/// `manta-dsp::floor` with no struct field to bind to at all --
/// `floor_window_ms`/`block_channels` size fixed per-channel arrays in the
/// channel-hop hot path (`RING_LEN`, `BLOCK_CHANNELS`), and turning either
/// into a runtime value means heap-allocating there, directly against the
/// Pi-4 CPU budget this repo enforces with criterion benches. These are
/// accepted by the parser (so the error can explain WHY, not just
/// "unknown field", which would contradict the spec page an operator is
/// reading) and rejected by `apply` with an actionable error naming where
/// the constant actually lives.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetectorTable {
    #[serde(default)]
    pub on_snr_db: Option<f32>,
    #[serde(default)]
    pub off_snr_db: Option<f32>,
    #[serde(default)]
    pub confirm_ms: Option<f64>,
    #[serde(default)]
    pub hang_ms: Option<f64>,
    #[serde(default)]
    pub gc_ms: Option<f64>,
    #[serde(default)]
    pub warmup_ms: Option<f64>,
    #[serde(default)]
    pub track_cap: Option<usize>,
    #[serde(default)]
    pub floor_quantile: Option<f64>,
    #[serde(default)]
    pub floor_window_ms: Option<f64>,
    #[serde(default)]
    pub block_channels: Option<usize>,
    #[serde(default)]
    pub block_allowance_db: Option<f64>,
}

/// Upper bound accepted for any `*_ms` key in `[detector]`/`[decode]`: one
/// hour. Purely a sanity ceiling -- generous above any real value, tight
/// enough to reject a stray extra zero instead of silently accepting it.
const MAX_PLAUSIBLE_MS: f64 = 3_600_000.0;

/// The smallest `*_ms` value that rounds up to a nonzero hop count at the
/// channelizer's fixed 375 Hz rate, per `manta_decode::ms_to_hops`'s
/// round-half-up rule (`floor(ms * 0.375 + 0.5)`): solving
/// `floor(ms * 0.375 + 0.5) >= 1` for `ms` gives `ms >= 4.0 / 3.0`. Below
/// this, `ms_to_hops` silently rounds to 0 -- and `TrackManager::on_hop`
/// (`manta-engine::track`) compares `silent_count >= gc_hops`/
/// `confirm_count >= confirm_hops` etc., both of which are trivially true
/// at 0, so every track would close (or promote) on its very first hop.
/// The daemon would run and silently spot nothing (code-review finding 2).
const MIN_MS_FOR_ONE_HOP: f64 = 4.0 / 3.0;

fn ms_to_hops_checked(key: &str, ms: f64) -> Result<u64> {
    if !ms.is_finite() || ms <= 0.0 || ms > MAX_PLAUSIBLE_MS {
        bail!(
            "{key} must be a finite number of milliseconds between 0 (exclusive) and {MAX_PLAUSIBLE_MS}, got {ms}"
        );
    }
    let hops = manta_decode::ms_to_hops(ms);
    if hops == 0 {
        bail!(
            "{key} = {ms} rounds to 0 hops at the channelizer's fixed 375 Hz rate -- the smallest \
             value that rounds up to 1 hop is {MIN_MS_FOR_ONE_HOP} ms"
        );
    }
    Ok(u64::from(hops))
}

fn positive_ms(key: &str, ms: f64) -> Result<f64> {
    if !ms.is_finite() || ms <= 0.0 || ms > MAX_PLAUSIBLE_MS {
        bail!(
            "{key} must be a finite number of milliseconds between 0 (exclusive) and {MAX_PLAUSIBLE_MS}, got {ms}"
        );
    }
    Ok(ms)
}

/// Documented in SPEC §9 but backed by a compile-time constant with no
/// struct field -- see `DetectorTable`'s doc comment for the CPU-budget
/// rationale. Filing a follow-up ticket for these (rather than silently
/// ignoring them, the ticket's whole complaint) is this ticket's own
/// explicit scope decision.
fn reject_not_yet_configurable<T>(key: &str, v: Option<T>, home: &str) -> Result<()> {
    if v.is_some() {
        bail!(
            "{key} is documented in docs/SPEC-decode-core.md \u{a7}9 but is not yet configurable \
             -- it is a compile-time constant in {home}. Remove the key from your config; making \
             it settable is tracked as a MAN-74 follow-up."
        );
    }
    Ok(())
}

impl DetectorTable {
    fn apply(&self, base: &mut manta_engine::DetectorConfig) -> Result<()> {
        reject_not_yet_configurable(
            "detector.floor_quantile",
            self.floor_quantile,
            "manta-dsp::floor",
        )?;
        reject_not_yet_configurable(
            "detector.floor_window_ms",
            self.floor_window_ms,
            "manta-dsp::floor",
        )?;
        reject_not_yet_configurable(
            "detector.block_channels",
            self.block_channels,
            "manta-dsp::floor",
        )?;
        reject_not_yet_configurable(
            "detector.block_allowance_db",
            self.block_allowance_db,
            "manta-dsp::floor",
        )?;

        if let Some(v) = self.on_snr_db {
            if !v.is_finite() {
                bail!("detector.on_snr_db must be finite, got {v}");
            }
            base.on_snr_db = v;
        }
        if let Some(v) = self.off_snr_db {
            if !v.is_finite() {
                bail!("detector.off_snr_db must be finite, got {v}");
            }
            base.off_snr_db = v;
        }
        if base.off_snr_db > base.on_snr_db {
            bail!(
                "detector.off_snr_db ({}) must not exceed detector.on_snr_db ({})",
                base.off_snr_db,
                base.on_snr_db
            );
        }
        if let Some(v) = self.confirm_ms {
            base.confirm_hops = ms_to_hops_checked("detector.confirm_ms", v)?;
        }
        if let Some(v) = self.hang_ms {
            base.hang_hops = ms_to_hops_checked("detector.hang_ms", v)?;
        }
        if let Some(v) = self.gc_ms {
            base.gc_hops = ms_to_hops_checked("detector.gc_ms", v)?;
        }
        if let Some(v) = self.warmup_ms {
            base.warmup_hops = ms_to_hops_checked("detector.warmup_ms", v)?;
        }
        if let Some(v) = self.track_cap {
            if v == 0 {
                bail!("detector.track_cap must be at least 1");
            }
            base.track_cap = v;
        }
        Ok(())
    }
}

/// `[decode]`, overlaid onto `manta_decode::decoder::DecodeConfig::default()`.
/// SPEC §9 states `[decode]` as one flat table; the Rust side nests
/// `DemodConfig`/`BeamConfig` inside `DecodeConfig` -- this struct is that
/// flattening, spelled out explicitly (field by field) rather than via
/// `#[serde(flatten)]`, for the same `deny_unknown_fields` reason
/// `InputSource` avoids it (see that type's doc comment).
///
/// `cluster_alpha`/`mu_ratio_bounds`/`char_gap_dits`/`word_gap_dits` are
/// documented in SPEC §9 but compiled in as `const`s in
/// `manta-decode::timing` with no struct field -- same treatment as
/// `DetectorTable`'s four const-only keys.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecodeTable {
    #[serde(default)]
    pub timing_sigma: Option<f32>,
    #[serde(default)]
    pub beam_width: Option<usize>,
    #[serde(default)]
    pub debounce_ms: Option<f64>,
    #[serde(default)]
    pub hyst_up: Option<f32>,
    #[serde(default)]
    pub hyst_down: Option<f32>,
    #[serde(default)]
    pub tau_lo_ms: Option<f64>,
    #[serde(default)]
    pub tau_hi_init_ms: Option<f64>,
    #[serde(default)]
    pub tau_hi_bounds_ms: Option<(f64, f64)>,
    #[serde(default)]
    pub flush_gap_dits: Option<f32>,
    #[serde(default)]
    pub cluster_alpha: Option<f64>,
    #[serde(default)]
    pub mu_ratio_bounds: Option<(f64, f64)>,
    #[serde(default)]
    pub char_gap_dits: Option<f64>,
    #[serde(default)]
    pub word_gap_dits: Option<f64>,
}

impl DecodeTable {
    fn apply(&self, base: &mut DecodeConfig) -> Result<()> {
        reject_not_yet_configurable(
            "decode.cluster_alpha",
            self.cluster_alpha,
            "manta-decode::timing",
        )?;
        reject_not_yet_configurable(
            "decode.mu_ratio_bounds",
            self.mu_ratio_bounds,
            "manta-decode::timing",
        )?;
        reject_not_yet_configurable(
            "decode.char_gap_dits",
            self.char_gap_dits,
            "manta-decode::timing",
        )?;
        reject_not_yet_configurable(
            "decode.word_gap_dits",
            self.word_gap_dits,
            "manta-decode::timing",
        )?;

        if let Some(v) = self.timing_sigma {
            if !(v.is_finite() && v > 0.0) {
                bail!("decode.timing_sigma must be a positive, finite number, got {v}");
            }
            base.beam.sigma = v;
        }
        if let Some(v) = self.beam_width {
            if v == 0 {
                bail!("decode.beam_width must be at least 1");
            }
            base.beam.width = v;
        }
        if let Some(v) = self.debounce_ms {
            base.demod.debounce_ms = positive_ms("decode.debounce_ms", v)?;
        }
        if let Some(v) = self.hyst_up {
            if !(v.is_finite() && v > 0.0) {
                bail!("decode.hyst_up must be a positive, finite number, got {v}");
            }
            base.demod.hyst_up = v;
        }
        if let Some(v) = self.hyst_down {
            if !(v.is_finite() && v > 0.0) {
                bail!("decode.hyst_down must be a positive, finite number, got {v}");
            }
            base.demod.hyst_down = v;
        }
        if base.demod.hyst_down > base.demod.hyst_up {
            bail!(
                "decode.hyst_down ({}) must not exceed decode.hyst_up ({})",
                base.demod.hyst_down,
                base.demod.hyst_up
            );
        }
        if let Some(v) = self.tau_lo_ms {
            base.demod.tau_lo_ms = positive_ms("decode.tau_lo_ms", v)?;
        }
        if let Some(v) = self.tau_hi_init_ms {
            base.demod.tau_hi_init_ms = positive_ms("decode.tau_hi_init_ms", v)?;
        }
        if let Some((lo, hi)) = self.tau_hi_bounds_ms {
            if !(lo.is_finite() && hi.is_finite() && lo > 0.0 && hi > lo) {
                bail!(
                    "decode.tau_hi_bounds_ms must be an increasing pair of positive numbers, got ({lo}, {hi})"
                );
            }
            base.demod.tau_hi_bounds_ms = (lo, hi);
        }
        if let Some(v) = self.flush_gap_dits {
            if !(v.is_finite() && v > 0.0) {
                bail!("decode.flush_gap_dits must be a positive, finite number, got {v}");
            }
            base.flush_gap_dits = v;
        }
        Ok(())
    }
}

/// The whole daemon TOML: `[server]`/`[[rbn_uplink]]` (unchanged from the
/// retired `manta_server::config::DaemonConfigFile`) plus the four tables
/// that used to parse and do nothing at all (MAN-74). See the module doc
/// comment for the `deny_unknown_fields`-at-this-level rationale.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    #[serde(default)]
    pub server: Option<ServerConfig>,
    #[serde(default)]
    pub rbn_uplink: Vec<RbnUplinkConfig>,
    #[serde(default)]
    pub input: Option<InputSource>,
    #[serde(default)]
    pub spot: Option<SpotTable>,
    #[serde(default)]
    pub detector: Option<DetectorTable>,
    #[serde(default)]
    pub decode: Option<DecodeTable>,
}

impl ConfigFile {
    /// `[detector]` overlaid onto `manta_engine::DetectorConfig::default()`
    /// -- **never** onto SPEC §9's literal table text. `DetectorConfig`'s
    /// own `Default` impl deliberately deviates from SPEC §9's stated
    /// `on_snr_db = 6.0` (empirically retuned to 12.0 -- see that impl's
    /// doc comment: 6.0 produced 298 spurious tracks against a single
    /// clean signal in the V1 golden vector). Starting from the Rust
    /// default and overriding only named keys means an operator who omits
    /// `on_snr_db` gets the production-safe 12.0, never a silent
    /// regression to the stale spec value.
    pub fn resolve_detector(&self) -> Result<manta_engine::DetectorConfig> {
        let mut cfg = manta_engine::DetectorConfig::default();
        if let Some(table) = &self.detector {
            table.apply(&mut cfg)?;
        }
        Ok(cfg)
    }

    /// `[decode]` overlaid onto `DecodeConfig::default()`, same reasoning
    /// as `resolve_detector`.
    pub fn resolve_decode(&self) -> Result<DecodeConfig> {
        let mut cfg = DecodeConfig::default();
        if let Some(table) = &self.decode {
            table.apply(&mut cfg)?;
        }
        Ok(cfg)
    }
}

/// A non-empty `[[rbn_uplink]]` with no `[server]` table can never actually
/// connect -- `RbnUplinkConfig::effective_login_callsign` falls back to
/// `[server].station_callsign`, which does not exist. Checked once, here,
/// rather than deep inside `main.rs`'s daemon-startup path, so the error
/// surfaces at config-load time regardless of which subcommand loaded it.
fn validate(cfg: &ConfigFile) -> Result<()> {
    if !cfg.rbn_uplink.is_empty() && cfg.server.is_none() {
        bail!("[[rbn_uplink]] requires a [server] table (for station_callsign)");
    }
    Ok(())
}

/// Recognized `MANTA_<TABLE>_<KEY>` table prefixes. `rbn_uplink` is
/// deliberately excluded: it is a TOML array-of-tables, and there is no
/// unambiguous `MANTA_RBN_UPLINK_*` spelling for "the second configured
/// target" -- an attempt is a hard error (`apply_env_overlay`) rather than
/// a silent no-op, so an operator relying on it finds out immediately, not
/// by wondering why a second uplink never connects.
const ENV_TABLES: [&str; 5] = ["server", "input", "spot", "detector", "decode"];
const ENV_PREFIX: &str = "MANTA_";
/// The config file PATH itself, not a table key -- read directly by
/// `main.rs` as the `--config` fallback, so it is excluded from the
/// table-overlay treatment every other `MANTA_*` variable gets.
pub const ENV_CONFIG_PATH: &str = "MANTA_CONFIG";

/// `(table, key)` pairs whose target field is always `String`/`PathBuf` --
/// forced to a bare TOML string unconditionally in `apply_env_overlay`,
/// bypassing `env_value_to_toml`'s TOML-typed probe entirely. Without this,
/// a numeric-looking value -- a KiwiSDR password (MAN-73's secret-injection
/// use case), a `MANTA_INPUT_DEVICE` that happens to be all digits, a
/// numeric-looking blocklist/notch path -- parses as an integer instead of
/// a string, with no documented way to force the string type (round-2
/// finding C-5). `[spot].allowlist` is deliberately absent: it is a
/// `Vec<String>`, which genuinely needs the TOML-array parse
/// (`a_typed_env_value_is_parsed_as_toml`).
const STRING_TYPED_ENV_KEYS: &[(&str, &str)] = &[
    ("server", "station_callsign"),
    ("server", "bind_addr"),
    ("input", "device"),
    ("input", "path"),
    ("input", "host"),
    ("input", "password"),
    ("input", "driver"),
    ("spot", "blocklist_path"),
    ("spot", "notch_path"),
];

/// Best-effort TOML-typed parse of one environment variable's raw text:
/// tried first as a TOML value (so `9300`, `true`, `1.5`, `["W1AW"]` come
/// through typed), falling back to a bare string so
/// `MANTA_SERVER_BIND_ADDR=0.0.0.0` or `MANTA_SERVER_STATION_CALLSIGN=K1ABC`
/// need no shell quoting. Callers that know the target field is always a
/// string (`STRING_TYPED_ENV_KEYS`) skip this probe entirely rather than
/// relying on it -- see that constant's doc comment.
fn env_value_to_toml(raw: &str) -> toml::Value {
    let probe = format!("x = {raw}");
    match toml::from_str::<toml::Table>(&probe) {
        Ok(mut t) => t
            .remove("x")
            .unwrap_or_else(|| toml::Value::String(raw.to_string())),
        Err(_) => toml::Value::String(raw.to_string()),
    }
}

/// Splices `MANTA_<TABLE>_<KEY>` variables into the parsed TOML document
/// BEFORE typed deserialization -- an env value is therefore checked by
/// exactly the same `deny_unknown_fields` + per-field validators a file
/// value is; there is no separate, potentially-divergent validation path
/// for environment input. `vars` is an explicit slice (never
/// `std::env::vars()` read directly in here) so unit tests stay hermetic;
/// `load` is the one real call site that reads the actual process
/// environment.
fn apply_env_overlay(doc: &mut toml::Table, vars: &[(String, String)]) -> Result<()> {
    for (name, value) in vars {
        let Some(rest) = name.strip_prefix(ENV_PREFIX) else {
            continue;
        };
        let mut parts = rest.splitn(2, '_');
        let table = parts.next().unwrap_or_default().to_lowercase();
        let Some(key) = parts.next() else {
            bail!("unrecognized environment variable {name} (expected MANTA_<TABLE>_<KEY>)");
        };
        if !ENV_TABLES.contains(&table.as_str()) {
            bail!(
                "unrecognized environment variable {name} (unknown table {table:?}; expected \
                 one of server/input/spot/detector/decode)"
            );
        }
        let key = key.to_lowercase();
        let entry = doc
            .entry(table.clone())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let Some(table_mut) = entry.as_table_mut() else {
            bail!("{name}: [{table}] is not a table in the config file");
        };
        let toml_value = if STRING_TYPED_ENV_KEYS.contains(&(table.as_str(), key.as_str())) {
            toml::Value::String(value.clone())
        } else {
            env_value_to_toml(value)
        };
        table_mut.insert(key, toml_value);
    }
    Ok(())
}

fn load_from_text_and_env(text: &str, vars: &[(String, String)]) -> Result<ConfigFile> {
    let mut doc: toml::Table = toml::from_str(text).map_err(|e| anyhow::anyhow!("{e}"))?;
    apply_env_overlay(&mut doc, vars)?;
    let cfg: ConfigFile = doc.try_into().map_err(|e| anyhow::anyhow!("{e}"))?;
    validate(&cfg)?;
    Ok(cfg)
}

/// Parses `text` alone -- no environment overlay. Used by every hermetic
/// unit test in this module, so a stray `MANTA_*` variable in the test
/// runner's own environment can never affect them; `#[cfg(test)]` since
/// this bin-only crate has no non-test caller (`load`/`load_str_with_env`
/// below cover the real, environment-aware entry points).
#[cfg(test)]
pub fn load_str(text: &str) -> Result<ConfigFile> {
    load_from_text_and_env(text, &[])
}

/// Parses `text` with an explicit, caller-supplied set of `MANTA_*`
/// variables overlaid -- the hermetic counterpart to `load`'s real
/// `std::env::vars()` read, used by this module's own env-tier tests.
pub fn load_str_with_env(text: &str, vars: &[(String, String)]) -> Result<ConfigFile> {
    load_from_text_and_env(text, vars)
}

/// The table-overlay-eligible subset of a raw variable list: every
/// `MANTA_*`-prefixed name except [`ENV_CONFIG_PATH`] itself, which
/// `main.rs` reads directly as the `--config` fallback, not as a table key.
/// Split out from `load` so the exclusion can be asserted directly against
/// a plain `Vec`, with no `std::env::set_var` needed (`load`'s own
/// `std::env::vars()` read is the one real call site that touches the
/// process environment).
fn env_overlay_vars(vars: impl IntoIterator<Item = (String, String)>) -> Vec<(String, String)> {
    vars.into_iter()
        .filter(|(k, _)| k.starts_with(ENV_PREFIX) && k != ENV_CONFIG_PATH)
        .collect()
}

/// `std::env::vars()` PANICS if any variable in the process environment --
/// including one entirely unrelated to manta -- has a non-Unicode name or
/// value. `load` is on the unconditional `listen`/`soak` startup path (it
/// runs even with no `--config`), so that panic would turn a clean startup
/// into a crash over a stray environment entry manta never reads
/// (code-review finding 4). A name that isn't valid Unicode can never
/// match `MANTA_<TABLE>_<KEY>` anyway, so it's dropped outright; a value
/// that isn't is converted lossily (`\u{FFFD}` in place of invalid bytes)
/// so a genuinely-relevant `MANTA_*` variable still reaches the ordinary
/// per-field validation instead of crashing the process.
fn os_vars_to_string_lossy(
    vars: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> Vec<(String, String)> {
    vars.into_iter()
        .filter_map(|(k, v)| {
            let k = k.into_string().ok()?;
            Some((k, v.to_string_lossy().into_owned()))
        })
        .collect()
}

/// The real entry point: reads `path` (if given), overlays the process's
/// actual `MANTA_*` environment variables (excluding [`ENV_CONFIG_PATH`],
/// which `main.rs` reads directly as the `--config` fallback, not as a
/// table key), and deserializes. `path: None` still applies the
/// environment tier and defaults -- a config-file-free deployment
/// controlled entirely by `MANTA_*` variables is a valid, if unusual,
/// pattern this doesn't need to special-case.
pub fn load(path: Option<&Path>) -> Result<ConfigFile> {
    let text = match path {
        Some(p) => std::fs::read_to_string(p)
            .with_context(|| format!("reading config file {}", p.display()))?,
        None => String::new(),
    };
    let vars = env_overlay_vars(os_vars_to_string_lossy(std::env::vars_os()));
    load_str_with_env(&text, &vars).with_context(|| match path {
        Some(p) => format!("parsing config file {}", p.display()),
        None => "parsing MANTA_* environment overrides".to_string(),
    })
}

fn resolve_relative(base_dir: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base_dir.join(p)
    }
}

/// CLI values that participate in file/env merging. `None`/empty means
/// "not given, fall through" -- which is why `--freq-correction-ppm` sheds
/// its old `default_value_t = 0.0` (see `main.rs`): with a hardcoded
/// default, `Some(0.0)` (the user explicitly typed 0) and "flag absent"
/// were indistinguishable, and CLI-wins-over-file precedence needs to tell
/// them apart.
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    pub freq_correction_ppm: Option<f64>,
    /// Empty means "not given" -- a non-empty CLI `--allowlist` replaces
    /// `[spot].allowlist` wholesale, it does not merge with it.
    pub allowlist: Vec<String>,
    pub blocklist: Option<PathBuf>,
    pub notch: Option<PathBuf>,
    /// True when a CLI source-selection flag (`--kiwi-host`/`--soapy-*`/
    /// `--hpsdr-*`) chose an RF-aware live source that discards `[input]`
    /// wholesale (MAN-74 Decision 3/5) -- in that case `[input]`'s shared
    /// per-source keys describe a DIFFERENT, now-unused source and must
    /// not silently carry over. `main.rs` already applies exactly this
    /// condition to suppress a stale `[input].dial_freq_hz` (round-2
    /// finding C-2); `freq_correction_ppm` used to skip that same check
    /// (code-review finding 1: a WAV's 25 ppm calibration silently
    /// applying to a live Kiwi source the operator switched to via
    /// `--kiwi-host`) -- both shared keys now use this one flag.
    pub suppress_file_input_shared_keys: bool,
}

/// Merges a parsed `ConfigFile` with CLI overrides into one
/// `PipelineConfig`, per key: `cli` > `file` > `PipelineConfig::default()`.
/// `[spot].blocklist_path`/`notch_path` resolve relative to `base_dir`
/// (the config file's own directory) when given via the file, not the CLI
/// flags -- see `resolve_relative`'s callers below; a `--blocklist` CLI
/// flag stays relative to the process's CWD, matching its existing
/// behavior before this ticket.
pub fn resolve_pipeline(
    file: &ConfigFile,
    base_dir: &Path,
    cli: &CliOverrides,
) -> Result<PipelineConfig> {
    let mut cfg = PipelineConfig {
        detector: file.resolve_detector()?,
        decode: file.resolve_decode()?,
        ..PipelineConfig::default()
    };

    cfg.freq_correction_ppm = cli
        .freq_correction_ppm
        .or_else(|| {
            if cli.suppress_file_input_shared_keys {
                None
            } else {
                file.input
                    .as_ref()
                    .and_then(InputSource::freq_correction_ppm)
            }
        })
        .unwrap_or(0.0);

    let spot = file.spot.clone().unwrap_or_default();
    cfg.allowlist = if !cli.allowlist.is_empty() {
        cli.allowlist.clone()
    } else {
        spot.allowlist.clone().unwrap_or_default()
    };

    let blocklist_path = cli.blocklist.clone().or_else(|| {
        spot.blocklist_path
            .as_ref()
            .map(|p| resolve_relative(base_dir, p))
    });
    if let Some(path) = blocklist_path {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading blocklist file {}", path.display()))?;
        cfg.blocklist = manta_engine::Blocklist::parse(crate::strip_bom(&text));
    }

    let notch_path = cli.notch.clone().or_else(|| {
        spot.notch_path
            .as_ref()
            .map(|p| resolve_relative(base_dir, p))
    });
    if let Some(path) = notch_path {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading notch file {}", path.display()))?;
        cfg.notch = manta_engine::NotchList::parse(crate::strip_bom(&text));
    }

    Ok(cfg)
}

/// `[input]` -> `SourceSpec`, when the file has one. `None` (no `[input]`
/// table) tells the caller to fall through to its existing CLI-flag-driven
/// source resolution unchanged. Whether this result is actually USED --
/// versus a CLI source flag overriding it entirely -- is `main.rs`'s call
/// (Decision: any CLI source-selection flag wins over `[input]` as a
/// whole; mixing fields across source TYPES field-by-field has no coherent
/// meaning).
///
/// `type = "file"`'s `path` resolves relative to `base_dir` (the config
/// file's own directory, Decision 7) -- same reasoning as
/// `[spot].blocklist_path`/`notch_path` in `resolve_pipeline`: a systemd
/// unit with no `WorkingDirectory` set must replay the WAV sitting next to
/// `manta.toml`, not one relative to the daemon's arbitrary CWD.
pub fn resolve_source(file: &ConfigFile, base_dir: &Path) -> Option<SourceSpec> {
    file.input.as_ref().map(|input| match input.clone() {
        InputSource::Audio { device, .. } => SourceSpec::Audio { device },
        InputSource::File { path, .. } => SourceSpec::File {
            path: resolve_relative(base_dir, &path),
        },
        InputSource::Kiwi {
            host,
            port,
            freq_hz,
            password,
            ..
        } => SourceSpec::Kiwi {
            host,
            port,
            freq_hz,
            password,
        },
        InputSource::Soapy {
            driver,
            freq_hz,
            rate_hz,
            gain_db,
            ..
        } => SourceSpec::Soapy {
            driver,
            freq_hz,
            rate_hz,
            gain_db,
        },
        InputSource::Hpsdr {
            host,
            port,
            freq_hz,
            rate_hz,
            ..
        } => SourceSpec::Hpsdr {
            host,
            port,
            freq_hz,
            rate_hz,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_SIX_TABLE_TOML: &str = r#"
        [server]
        station_callsign = "W3XYZ"

        [[rbn_uplink]]
        enabled = true
        target_host = "telnet.reversebeacon.net"
        target_port = 7000

        [input]
        type = "kiwi"
        host = "kiwi.example.com"
        freq_hz = 14025000.0

        [spot]
        allowlist = ["W1AW"]

        [detector]
        on_snr_db = 10.0

        [decode]
        timing_sigma = 0.3
    "#;

    // Phase 1: strict six-table ConfigFile.

    #[test]
    fn unknown_top_level_table_is_rejected_and_named() {
        let err = load_str(
            r#"
                [server]
                station_callsign = "W3XYZ"
                [completely_bogus_table]
                nonsense = 42
            "#,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("completely_bogus_table"),
            "error must name the table: {err}"
        );
    }

    #[test]
    fn all_six_daemon_tables_parse_together() {
        let cfg = load_str(FULL_SIX_TABLE_TOML).unwrap();
        assert_eq!(cfg.server.as_ref().unwrap().station_callsign, "W3XYZ");
        assert_eq!(cfg.rbn_uplink.len(), 1);
        assert!(cfg.input.is_some());
        assert!(cfg.spot.is_some());
        assert!(cfg.detector.is_some());
        assert!(cfg.decode.is_some());
    }

    #[test]
    fn a_typo_inside_the_server_table_is_still_rejected() {
        let err =
            load_str("[server]\nstation_callsign = \"W3XYZ\"\nbind_address = \"127.0.0.1\"\n")
                .unwrap_err()
                .to_string();
        assert!(err.contains("bind_address"), "{err}");
    }

    #[test]
    fn out_of_range_input_freq_correction_ppm_is_rejected() {
        let err = load_str("[input]\ntype = \"audio\"\nfreq_correction_ppm = 999999\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("[-1000, 1000]"), "{err}");
        assert!(load_str("[input]\ntype = \"audio\"\nfreq_correction_ppm = -1.5\n").is_ok());
    }

    #[test]
    fn server_table_is_optional_but_rbn_uplink_without_it_is_an_error() {
        assert!(load_str("[decode]\ntiming_sigma = 0.3\n")
            .unwrap()
            .server
            .is_none());
        let err =
            load_str("[[rbn_uplink]]\nenabled = true\ntarget_host = \"h\"\ntarget_port = 7000\n")
                .unwrap_err()
                .to_string();
        assert!(err.contains("[server]"), "{err}");
    }

    #[test]
    fn an_empty_config_is_valid() {
        assert!(load_str("").is_ok());
    }

    #[test]
    fn unknown_input_type_names_the_valid_alternatives() {
        let err = load_str("[input]\ntype = \"rtl\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("rtl"), "{err}");
    }

    #[test]
    fn unknown_key_inside_an_input_variant_is_rejected() {
        let err = load_str("[input]\ntype = \"audio\"\nbogus = 3\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("bogus"), "{err}");
    }

    // Ported from manta-server's old DaemonConfigFile tests -- same
    // behavior, now exercised through ConfigFile.

    #[test]
    fn uplink_table_omitted_parses_as_empty_vec() {
        let cfg = load_str("[server]\nstation_callsign = \"W3XYZ\"\n").unwrap();
        assert!(cfg.rbn_uplink.is_empty());
    }

    #[test]
    fn uplink_table_requires_target_when_present() {
        let result =
            load_str("[server]\nstation_callsign = \"W3XYZ\"\n[[rbn_uplink]]\nenabled = true\n");
        assert!(result.is_err(), "enabled=true with no target should fail");
    }

    #[test]
    fn uplink_table_parses_with_defaults() {
        let cfg = load_str(
            r#"
                [server]
                station_callsign = "W3XYZ"
                [[rbn_uplink]]
                enabled = true
                target_host = "example.invalid"
                target_port = 7300
            "#,
        )
        .unwrap();
        assert_eq!(cfg.rbn_uplink.len(), 1);
        let uplink = &cfg.rbn_uplink[0];
        assert!(uplink.enabled);
        assert_eq!(uplink.target_host, "example.invalid");
        assert!(!uplink.dry_run);
        assert_eq!(uplink.login_callsign, None);
    }

    #[test]
    fn two_rbn_uplink_tables_parse_into_a_vec_of_two() {
        let cfg = load_str(
            r#"
                [server]
                station_callsign = "W3XYZ"
                [[rbn_uplink]]
                enabled = true
                target_host = "rbn1.example"
                target_port = 7300
                [[rbn_uplink]]
                enabled = true
                target_host = "rbn2.example"
                target_port = 7301
            "#,
        )
        .unwrap();
        assert_eq!(cfg.rbn_uplink.len(), 2);
        assert_eq!(cfg.rbn_uplink[0].target_host, "rbn1.example");
        assert_eq!(cfg.rbn_uplink[1].target_host, "rbn2.example");
    }

    #[test]
    fn single_bracket_rbn_uplink_table_is_a_parse_error() {
        let result = load_str(
            r#"
                [server]
                station_callsign = "W3XYZ"
                [rbn_uplink]
                enabled = true
                target_host = "example.invalid"
                target_port = 7300
            "#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn uplink_unknown_key_is_a_parse_error() {
        let result = load_str(
            r#"
                [server]
                station_callsign = "W3XYZ"
                [[rbn_uplink]]
                enabled = true
                target_host = "example.invalid"
                target_port = 7300
                dry-run = true
            "#,
        );
        assert!(result.is_err(), "unknown key should have been rejected");
    }

    #[test]
    fn uplink_rejects_implausible_login_callsign() {
        let result = load_str(
            r#"
                [server]
                station_callsign = "W3XYZ"
                [[rbn_uplink]]
                enabled = true
                target_host = "example.invalid"
                target_port = 7300
                login_callsign = "W3XYZ\r\nEVIL LINE"
            "#,
        );
        assert!(result.is_err());
    }

    // Phase 2: [detector]/[decode] overlays.

    #[test]
    fn an_omitted_detector_table_is_exactly_the_rust_default() {
        let cfg = load_str("").unwrap().resolve_detector().unwrap();
        assert_eq!(
            cfg.on_snr_db,
            manta_engine::DetectorConfig::default().on_snr_db
        );
        assert_eq!(cfg.on_snr_db, 12.0);
    }

    #[test]
    fn spec_section_9_milliseconds_round_trip_to_the_current_hop_defaults() {
        let cfg = load_str(
            "[detector]\nconfirm_ms = 50\nhang_ms = 5000\ngc_ms = 30000\nwarmup_ms = 2000\n",
        )
        .unwrap()
        .resolve_detector()
        .unwrap();
        assert_eq!(cfg, manta_engine::DetectorConfig::default());
    }

    #[test]
    fn a_partial_detector_table_overrides_only_what_it_names() {
        let cfg = load_str("[detector]\non_snr_db = 6.0\n")
            .unwrap()
            .resolve_detector()
            .unwrap();
        assert_eq!(cfg.on_snr_db, 6.0);
        assert_eq!(
            cfg.hang_hops,
            manta_engine::DetectorConfig::default().hang_hops
        );
    }

    #[test]
    fn flat_decode_table_reaches_the_nested_demod_and_beam_configs() {
        let cfg = load_str(
            "[decode]\ntiming_sigma = 0.4\nbeam_width = 8\nhyst_up = 1.5\ntau_hi_bounds_ms = [120, 380]\n",
        )
        .unwrap()
        .resolve_decode()
        .unwrap();
        assert_eq!(cfg.beam.sigma, 0.4);
        assert_eq!(cfg.beam.width, 8);
        assert_eq!(cfg.demod.hyst_up, 1.5);
        assert_eq!(cfg.demod.tau_hi_bounds_ms, (120.0, 380.0));
        assert_eq!(cfg.flush_gap_dits, DecodeConfig::default().flush_gap_dits);
    }

    /// The rejection lives in `DetectorTable::apply`/`DecodeTable::apply`,
    /// run by `resolve_detector`/`resolve_decode` -- `load_str` alone only
    /// parses the table (every one of these keys is a syntactically valid
    /// `Option<f64>`/`Option<usize>` field), matching `PipelineConfig`
    /// resolution's own two-step shape (parse, then apply-with-validation).
    #[test]
    fn a_documented_but_not_yet_configurable_key_fails_with_an_actionable_error() {
        for (table, key, home) in [
            ("detector", "floor_quantile = 0.3", "manta-dsp::floor"),
            ("detector", "floor_window_ms = 5000", "manta-dsp::floor"),
            ("detector", "block_channels = 16", "manta-dsp::floor"),
            ("detector", "block_allowance_db = 2.0", "manta-dsp::floor"),
            ("decode", "cluster_alpha = 0.2", "manta-decode::timing"),
            (
                "decode",
                "mu_ratio_bounds = [2.0, 5.0]",
                "manta-decode::timing",
            ),
            ("decode", "char_gap_dits = 2.0", "manta-decode::timing"),
            ("decode", "word_gap_dits = 5.0", "manta-decode::timing"),
        ] {
            let file = load_str(&format!("[{table}]\n{key}\n")).unwrap();
            let err = if table == "detector" {
                file.resolve_detector().unwrap_err().to_string()
            } else {
                file.resolve_decode().unwrap_err().to_string()
            };
            assert!(err.contains("not yet configurable"), "{err}");
            assert!(
                err.contains(home),
                "error must name where the constant lives: {err}"
            );
        }
    }

    #[test]
    fn out_of_range_detector_and_decode_values_are_rejected() {
        for (table, bad) in [
            (
                "detector",
                "[detector]\non_snr_db = 3.0\noff_snr_db = 9.0\n",
            ),
            ("detector", "[detector]\nhang_ms = -1\n"),
            ("detector", "[detector]\nhang_ms = 0\n"),
            ("detector", "[detector]\ngc_ms = nan\n"),
            ("detector", "[detector]\ngc_ms = 0\n"),
            ("detector", "[detector]\ntrack_cap = 0\n"),
            ("decode", "[decode]\nbeam_width = 0\n"),
            ("decode", "[decode]\ntiming_sigma = 0\n"),
            ("decode", "[decode]\nhyst_down = 2.0\nhyst_up = 1.0\n"),
            ("decode", "[decode]\ntau_hi_bounds_ms = [400, 100]\n"),
        ] {
            let file = load_str(bad).unwrap();
            let result = if table == "detector" {
                file.resolve_detector().map(|_| ())
            } else {
                file.resolve_decode().map(|_| ())
            };
            assert!(result.is_err(), "should have been rejected: {bad}");
        }
    }

    /// Code-review finding 2: below `MIN_MS_FOR_ONE_HOP`, `ms_to_hops`
    /// silently rounds to 0 -- and a 0 `gc_hops`/`confirm_hops` closes (or
    /// promotes) every track on its very first hop, since
    /// `manta-engine::track`'s `>= 0` comparisons are trivially true. A
    /// config with a stray-small `*_ms` value must be rejected, not
    /// silently produce a daemon that decodes nothing.
    #[test]
    fn a_ms_value_that_rounds_to_zero_hops_is_rejected() {
        for bad in ["[detector]\ngc_ms = 1\n", "[detector]\nconfirm_ms = 1\n"] {
            let err = load_str(bad)
                .unwrap()
                .resolve_detector()
                .unwrap_err()
                .to_string();
            assert!(err.contains("0 hops"), "{bad} -> {err}");
        }
        // Comfortably above the ~1.333 ms floor: must still be accepted.
        assert!(load_str("[detector]\ngc_ms = 50\n")
            .unwrap()
            .resolve_detector()
            .is_ok());
    }

    /// Code-review finding 3: `MAX_PLAUSIBLE_MS`'s own doc comment claims it
    /// bounds "any `*_ms` key in `[detector]`/`[decode]`", but `positive_ms`
    /// (used by `decode.debounce_ms`/`tau_lo_ms`/`tau_hi_init_ms`) never
    /// applied it -- the same stray-extra-zero typo was accepted in
    /// `[decode]` while the mirrored `[detector]` key was rejected.
    #[test]
    fn an_implausibly_large_decode_ms_value_is_rejected() {
        let err = load_str("[decode]\ndebounce_ms = 4000000\n")
            .unwrap()
            .resolve_decode()
            .unwrap_err()
            .to_string();
        assert!(err.contains("3600000"), "{err}");
    }

    /// Code-review finding 4: `std::env::vars()` panics on any non-Unicode
    /// entry in the WHOLE process environment, including one unrelated to
    /// manta -- `load` is on the unconditional startup path, so that used
    /// to turn a clean `listen`/`soak` invocation into a crash. Exercised
    /// against the pure `os_vars_to_string_lossy` helper (not real process
    /// env, matching this module's existing hermetic-test convention) so
    /// the fix is proven without mutating `std::env` and racing sibling
    /// tests in this shared bin target.
    #[test]
    #[cfg(unix)]
    fn os_vars_to_string_lossy_never_panics_on_non_utf8_entries() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let non_utf8_name = OsString::from_vec(vec![0xff, 0xfe]);
        let non_utf8_value = OsString::from_vec(vec![b'x', 0xff, b'y']);
        let result = os_vars_to_string_lossy([
            (non_utf8_name, OsString::from("ignored")),
            (OsString::from("MANTA_INPUT_DEVICE"), non_utf8_value),
            (
                OsString::from("MANTA_SERVER_STATION_CALLSIGN"),
                OsString::from("K1ABC"),
            ),
        ]);

        // The non-UTF-8-named entry is dropped outright (never matches
        // MANTA_<TABLE>_<KEY> anyway); the non-UTF-8 value is kept, lossily
        // converted rather than panicking; an ordinary entry passes through
        // untouched.
        assert_eq!(result.len(), 2);
        assert!(result
            .iter()
            .any(|(k, v)| k == "MANTA_INPUT_DEVICE" && v.contains('\u{FFFD}')));
        assert!(result.contains(&(
            "MANTA_SERVER_STATION_CALLSIGN".to_string(),
            "K1ABC".to_string()
        )));
    }

    // Phase 3: [input]/[spot] -> pipeline/source resolution.

    #[test]
    fn a_kiwi_source_is_fully_specified_by_the_input_table_alone() {
        let file = load_str(
            r#"
                [input]
                type = "kiwi"
                host = "kiwi.example.com"
                freq_hz = 14025000.0
            "#,
        )
        .unwrap();
        let spec = resolve_source(&file, Path::new(".")).unwrap();
        assert_eq!(
            spec,
            SourceSpec::Kiwi {
                host: "kiwi.example.com".to_string(),
                port: 8073,
                freq_hz: 14_025_000.0,
                password: String::new(),
            }
        );
    }

    #[test]
    fn a_file_source_is_specified_by_the_input_table_relative_to_the_config_dir() {
        let file = load_str("[input]\ntype = \"file\"\npath = \"cw48k.wav\"\n").unwrap();
        assert_eq!(
            resolve_source(&file, Path::new("/etc/manta")).unwrap(),
            SourceSpec::File {
                path: PathBuf::from("/etc/manta/cw48k.wav")
            }
        );
    }

    #[test]
    fn a_file_source_with_an_absolute_path_is_used_as_is() {
        let file = load_str("[input]\ntype = \"file\"\npath = \"/opt/cw48k.wav\"\n").unwrap();
        assert_eq!(
            resolve_source(&file, Path::new("/etc/manta")).unwrap(),
            SourceSpec::File {
                path: PathBuf::from("/opt/cw48k.wav")
            }
        );
    }

    #[test]
    fn no_input_table_resolves_to_no_source_spec() {
        assert!(resolve_source(&load_str("").unwrap(), Path::new(".")).is_none());
    }

    #[test]
    fn a_cli_freq_correction_ppm_beats_the_file_including_an_explicit_zero() {
        // The `default_value_t = 0.0` trap: `--freq-correction-ppm 0` must
        // WIN over a file value of 3.0, not be mistaken for "flag absent".
        let file = load_str("[input]\ntype = \"audio\"\nfreq_correction_ppm = 3.0\n").unwrap();
        let cli = CliOverrides {
            freq_correction_ppm: Some(0.0),
            ..CliOverrides::default()
        };
        assert_eq!(
            resolve_pipeline(&file, Path::new("."), &cli)
                .unwrap()
                .freq_correction_ppm,
            0.0
        );
    }

    /// Code-review finding 1: a CLI source flag that discards `[input]`
    /// wholesale (`suppress_file_input_shared_keys`) must also discard the
    /// stale `[input].freq_correction_ppm` it carries -- otherwise a WAV
    /// recording's calibration silently applies to a completely different,
    /// CLI-selected source (e.g. a live KiwiSDR), matching the existing
    /// `dial_freq_hz` suppression rule (round-2 finding C-2).
    #[test]
    fn a_suppressed_file_input_does_not_leak_its_freq_correction_ppm() {
        let file =
            load_str("[input]\ntype = \"file\"\npath = \"x.wav\"\nfreq_correction_ppm = 25.0\n")
                .unwrap();
        let cli = CliOverrides {
            suppress_file_input_shared_keys: true,
            ..CliOverrides::default()
        };
        assert_eq!(
            resolve_pipeline(&file, Path::new("."), &cli)
                .unwrap()
                .freq_correction_ppm,
            0.0
        );
    }

    #[test]
    fn an_absent_cli_ppm_falls_back_to_the_file_value() {
        let file = load_str("[input]\ntype = \"audio\"\nfreq_correction_ppm = 3.0\n").unwrap();
        assert_eq!(
            resolve_pipeline(&file, Path::new("."), &CliOverrides::default())
                .unwrap()
                .freq_correction_ppm,
            3.0
        );
    }

    #[test]
    fn a_cli_allowlist_replaces_the_file_allowlist_and_an_empty_one_does_not() {
        let file = load_str("[spot]\nallowlist = [\"W1AW\"]\n").unwrap();
        let replaced = resolve_pipeline(
            &file,
            Path::new("."),
            &CliOverrides {
                allowlist: vec!["K1ABC".to_string()],
                ..CliOverrides::default()
            },
        )
        .unwrap();
        assert_eq!(replaced.allowlist, vec!["K1ABC".to_string()]);

        let fallen_through =
            resolve_pipeline(&file, Path::new("."), &CliOverrides::default()).unwrap();
        assert_eq!(fallen_through.allowlist, vec!["W1AW".to_string()]);
    }

    #[test]
    fn spot_paths_resolve_against_the_config_files_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("blocklist.txt"), "K1BAD\n").unwrap();
        let cfg_path = dir.path().join("manta.toml");
        std::fs::write(&cfg_path, "[spot]\nblocklist_path = \"blocklist.txt\"\n").unwrap();

        let file = load(Some(&cfg_path)).unwrap();
        let pipeline = resolve_pipeline(&file, dir.path(), &CliOverrides::default()).unwrap();
        assert!(pipeline.blocklist.contains("K1BAD"));
    }

    // Phase 4: MANTA_<TABLE>_<KEY> environment tier.

    #[test]
    fn an_env_var_overrides_the_config_file() {
        let cfg = load_str_with_env(
            "[server]\nstation_callsign = \"W3XYZ\"\ntelnet_port = 7300\n",
            &[("MANTA_SERVER_TELNET_PORT".to_string(), "9300".to_string())],
        )
        .unwrap();
        assert_eq!(cfg.server.unwrap().telnet_port, 9300);
    }

    #[test]
    fn an_env_var_creates_a_table_the_file_omitted() {
        let cfg = load_str_with_env(
            "",
            &[(
                "MANTA_SERVER_STATION_CALLSIGN".to_string(),
                "K1ABC".to_string(),
            )],
        )
        .unwrap();
        assert_eq!(cfg.server.unwrap().station_callsign, "K1ABC");
    }

    #[test]
    fn an_unquoted_env_string_value_needs_no_toml_quoting() {
        let cfg = load_str_with_env(
            "",
            &[
                (
                    "MANTA_SERVER_STATION_CALLSIGN".to_string(),
                    "K1ABC".to_string(),
                ),
                ("MANTA_SERVER_BIND_ADDR".to_string(), "0.0.0.0".to_string()),
            ],
        )
        .unwrap();
        let server = cfg.server.unwrap();
        assert_eq!(server.station_callsign, "K1ABC");
        assert_eq!(server.bind_addr, "0.0.0.0");
    }

    #[test]
    fn a_typed_env_value_is_parsed_as_toml() {
        let cfg = load_str_with_env(
            "[spot]\n",
            &[(
                "MANTA_SPOT_ALLOWLIST".to_string(),
                "[\"W1AW\", \"K1ABC\"]".to_string(),
            )],
        )
        .unwrap();
        assert_eq!(
            cfg.spot.unwrap().allowlist.unwrap(),
            vec!["W1AW".to_string(), "K1ABC".to_string()]
        );
    }

    #[test]
    fn an_unrecognized_manta_env_var_is_a_hard_error() {
        let err = load_str_with_env("", &[("MANTA_NOPE".to_string(), "1".to_string())])
            .unwrap_err()
            .to_string();
        assert!(err.contains("MANTA_NOPE"), "{err}");
    }

    #[test]
    fn an_unknown_key_set_via_env_is_still_rejected() {
        let err = load_str_with_env(
            "[server]\nstation_callsign = \"W3XYZ\"\n",
            &[("MANTA_SERVER_BOGUS".to_string(), "1".to_string())],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("bogus"), "{err}");
    }

    #[test]
    fn an_env_only_input_table_with_no_type_anywhere_is_a_missing_field_error() {
        // `[input]` is a tagged union: MANTA_INPUT_* with no `type` key,
        // in the file or via MANTA_INPUT_TYPE, cannot select a variant --
        // this must be an error, never a silent accept of the (also
        // out-of-range) ppm value (round-2 finding C-4, SPEC §9).
        let err = load_str_with_env(
            "",
            &[(
                "MANTA_INPUT_FREQ_CORRECTION_PPM".to_string(),
                "999999".to_string(),
            )],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("missing field `type`"), "{err}");
    }

    #[test]
    fn an_env_value_is_validated_by_the_same_typed_layer_once_type_is_known() {
        // With MANTA_INPUT_TYPE supplying the tag the earlier test lacked,
        // the out-of-range ppm value now reaches the real typed validator.
        let err = load_str_with_env(
            "",
            &[
                ("MANTA_INPUT_TYPE".to_string(), "audio".to_string()),
                (
                    "MANTA_INPUT_FREQ_CORRECTION_PPM".to_string(),
                    "999999".to_string(),
                ),
            ],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("[-1000, 1000]"), "{err}");
    }

    #[test]
    fn a_numeric_looking_env_string_value_is_not_mistyped_as_a_number() {
        // MAN-73's secret-injection use case: a KiwiSDR password that
        // happens to be all digits must stay a string, not become a TOML
        // integer with no documented way to force the type back (round-2
        // finding C-5).
        let cfg = load_str_with_env(
            "[input]\ntype = \"kiwi\"\nhost = \"kiwi.example.com\"\nfreq_hz = 14025000.0\n",
            &[("MANTA_INPUT_PASSWORD".to_string(), "12345678".to_string())],
        )
        .unwrap();
        match cfg.input.unwrap() {
            InputSource::Kiwi { password, .. } => assert_eq!(password, "12345678"),
            other => panic!("expected a Kiwi input source, got {other:?}"),
        }
    }

    #[test]
    fn rbn_uplink_is_not_env_addressable() {
        let err = load_str_with_env(
            "[server]\nstation_callsign = \"W3XYZ\"\n",
            &[("MANTA_RBN_UPLINK_ENABLED".to_string(), "true".to_string())],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("MANTA_RBN_UPLINK_ENABLED"), "{err}");
    }

    #[test]
    fn manta_config_env_var_name_is_excluded_from_table_overlay() {
        // `load`'s env-collection filter must not treat MANTA_CONFIG as a
        // table-key overlay attempt -- main.rs reads it directly as the
        // --config fallback. Asserted against `env_overlay_vars` directly
        // rather than via `std::env::set_var`, which raced `load`'s own
        // `std::env::vars()` read against sibling tests sharing this bin
        // target's single test process (round-2 finding C-6).
        let vars = env_overlay_vars([
            (
                ENV_CONFIG_PATH.to_string(),
                "/should/not/be/read/as/a/table.toml".to_string(),
            ),
            (
                "MANTA_SERVER_STATION_CALLSIGN".to_string(),
                "W3XYZ".to_string(),
            ),
        ]);
        assert_eq!(
            vars,
            vec![(
                "MANTA_SERVER_STATION_CALLSIGN".to_string(),
                "W3XYZ".to_string()
            )]
        );
    }
}
