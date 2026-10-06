//! TOML deserialization for the `[detector]` config table (SPEC §9), MAN-261.
//! Mirrors `manta-decode`'s `config_file.rs` for `[decode]`.
//!
//! `manta-server`'s `DaemonConfigFile` does not model `[detector]`, for the
//! same reason it skips `[decode]`: it has no dependency on this crate.
//! Consumers that act on `[detector]` (`manta-cli`, when `--config` is
//! given) deserialize the table into `DetectorConfigToml` and call
//! `into_detector_config`. This lives in `manta-engine`, not `manta-cli`,
//! so every absent key falls back to the real `DetectorConfig::default()`
//! this crate owns, rather than a hand-copied set of constants downstream
//! that could drift from it. That matters here more than for `[decode]`:
//! the default `on_snr_db` is 12.0, a measured deviation from SPEC §9's
//! literal 6.0 (see `DetectorConfig`'s `Default` impl), so a downstream copy
//! of SPEC's table would be wrong on day one.
//!
//! Keys are SPEC §9's `on_snr_db`, `off_snr_db`, `confirm_ms`, `hang_ms`,
//! `gc_ms` and `warmup_ms`, plus `track_cap` (ARCHITECTURE §4, configurable per §8) and
//! `silent_respawn_cooldown_ms` (MAN-171). Every key is an `Option`
//! overlaid on `DetectorConfig::default()`, so an absent key keeps the exact
//! default hop count instead of round-tripping it through milliseconds.
//! Millisecond keys convert with `manta_decode::ms_to_hops`, SPEC §1.1's
//! single normative rounding rule; SPEC's millisecond defaults
//! (50/5000/30000/2000/30000) reproduce the hop defaults
//! (19/1875/11250/750/11250) exactly.
//!
//! The bounds reject exactly the values the track manager treats as
//! degenerate: `gc_hops = 0` closes every ACTIVE track immediately,
//! `track_cap = 0` evicts every track, and `confirm_hops`/`hang_hops` are
//! compared with `>=`, so 0 makes the window meaningless. `warmup_ms` and
//! `silent_respawn_cooldown_ms` may be 0 (no warmup, no cooldown).
//! `into_detector_config` builds `DetectorConfig` with an exhaustive struct
//! literal (no `..DetectorConfig::default()`), so adding a field to
//! `DetectorConfig` is a compile error here until it gets a key or an
//! explicit decision not to have one.
//!
//! The constant-only SPEC §9 keys (`floor_quantile` and friends) are not
//! fields here; `manta-cli` rejects them with a specific "not configurable
//! yet" message before this struct ever sees them.

use crate::track::DetectorConfig;
use manta_decode::{ms_to_hops, HOP_MS};
use serde::Deserialize;

/// Upper bound for every `*_ms` key: one hour.
const MAX_MS: f64 = 3_600_000.0;
/// Upper bound for both dB keys.
const MAX_SNR_DB: f64 = 100.0;

/// The `[detector]` TOML table. Every key is optional; see the module doc
/// for the key list, defaults and bounds.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetectorConfigToml {
    pub on_snr_db: Option<f64>,
    pub off_snr_db: Option<f64>,
    pub confirm_ms: Option<f64>,
    pub hang_ms: Option<f64>,
    pub gc_ms: Option<f64>,
    pub warmup_ms: Option<f64>,
    pub track_cap: Option<usize>,
    pub silent_respawn_cooldown_ms: Option<f64>,
}

impl DetectorConfigToml {
    /// Validates this table and overlays it on `DetectorConfig::default()`.
    /// Every error names the offending key as `detector.<key>`.
    pub fn into_detector_config(self) -> Result<DetectorConfig, String> {
        let d = DetectorConfig::default();
        let on_snr_db = db("on_snr_db", self.on_snr_db, d.on_snr_db)?;
        let off_snr_db = db("off_snr_db", self.off_snr_db, d.off_snr_db)?;
        if off_snr_db > on_snr_db {
            return Err(format!(
                "detector.off_snr_db ({off_snr_db}) must not exceed detector.on_snr_db ({on_snr_db})"
            ));
        }
        Ok(DetectorConfig {
            on_snr_db,
            off_snr_db,
            confirm_hops: hops("confirm_ms", self.confirm_ms, d.confirm_hops, true)?,
            hang_hops: hops("hang_ms", self.hang_ms, d.hang_hops, true)?,
            gc_hops: hops("gc_ms", self.gc_ms, d.gc_hops, true)?,
            warmup_hops: hops("warmup_ms", self.warmup_ms, d.warmup_hops, false)?,
            track_cap: track_cap(self.track_cap, d.track_cap)?,
            silent_respawn_cooldown_hops: hops(
                "silent_respawn_cooldown_ms",
                self.silent_respawn_cooldown_ms,
                d.silent_respawn_cooldown_hops,
                false,
            )?,
        })
    }
}

/// A dB key: finite and in `[0, 100]`, stored as `f32`.
fn db(key: &str, value: Option<f64>, default: f32) -> Result<f32, String> {
    let Some(v) = value else {
        return Ok(default);
    };
    if !v.is_finite() || !(0.0..=MAX_SNR_DB).contains(&v) {
        return Err(format!(
            "detector.{key} must be finite and in [0, 100] dB, got {v}"
        ));
    }
    Ok(v as f32)
}

/// A `*_ms` key: finite and in `[0, 3600000]`, converted to hops. When
/// `at_least_one_hop` is set, a value that rounds to 0 hops is rejected.
fn hops(
    key: &str,
    value: Option<f64>,
    default: u64,
    at_least_one_hop: bool,
) -> Result<u64, String> {
    let Some(ms) = value else {
        return Ok(default);
    };
    if !ms.is_finite() || !(0.0..=MAX_MS).contains(&ms) {
        return Err(format!(
            "detector.{key} must be finite and in [0, 3600000] ms, got {ms}"
        ));
    }
    let h = ms_to_hops(ms);
    if at_least_one_hop && h == 0 {
        return Err(format!(
            "detector.{key} = {ms} rounds to 0 hops (1 hop = {HOP_MS:.3} ms); it must give at least 1 hop"
        ));
    }
    Ok(u64::from(h))
}

/// `track_cap` must be at least 1: 0 evicts every track.
fn track_cap(value: Option<usize>, default: usize) -> Result<usize, String> {
    match value {
        None => Ok(default),
        Some(0) => {
            Err("detector.track_cap must be at least 1 (0 evicts every track), got 0".into())
        }
        Some(n) => Ok(n),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<DetectorConfig, String> {
        toml::from_str::<DetectorConfigToml>(s)
            .map_err(|e| e.to_string())?
            .into_detector_config()
    }

    fn err(s: &str) -> String {
        match parse(s) {
            Ok(cfg) => panic!("{s:?} should be rejected, got {cfg:?}"),
            Err(e) => e,
        }
    }

    #[test]
    fn empty_table_is_the_default() {
        assert_eq!(parse("").unwrap(), DetectorConfig::default());
    }

    #[test]
    fn spec_millisecond_defaults_reproduce_the_hop_defaults() {
        let cfg = parse(
            r#"
            on_snr_db = 12.0
            off_snr_db = 3.0
            confirm_ms = 50.0
            hang_ms = 5000.0
            gc_ms = 30000.0
            warmup_ms = 2000.0
            track_cap = 1200
            silent_respawn_cooldown_ms = 30000.0
            "#,
        )
        .unwrap();
        assert_eq!(cfg, DetectorConfig::default());
        assert_eq!(cfg.confirm_hops, 19);
        assert_eq!(cfg.hang_hops, 1875);
        assert_eq!(cfg.gc_hops, 11250);
        assert_eq!(cfg.warmup_hops, 750);
        assert_eq!(cfg.silent_respawn_cooldown_hops, 11250);
    }

    #[test]
    fn integer_toml_values_are_accepted_for_float_keys() {
        let cfg = parse("confirm_ms = 50").unwrap();
        assert_eq!(cfg.confirm_hops, 19);
    }

    #[test]
    fn non_finite_db_values_are_rejected() {
        let e = err("on_snr_db = nan");
        assert!(e.contains("detector.on_snr_db must be finite"), "{e}");
        let e = err("off_snr_db = inf");
        assert!(e.contains("detector.off_snr_db must be finite"), "{e}");
    }

    #[test]
    fn off_above_default_on_is_rejected() {
        let e = err("off_snr_db = 13.0");
        assert_eq!(
            e,
            "detector.off_snr_db (13) must not exceed detector.on_snr_db (12)"
        );
    }

    #[test]
    fn on_snr_db_above_100_is_rejected() {
        let e = err("on_snr_db = 101.0");
        assert!(e.contains("detector.on_snr_db"), "{e}");
        assert!(e.contains("[0, 100]"), "{e}");
    }

    #[test]
    fn confirm_ms_that_rounds_to_zero_hops_is_rejected() {
        let e = err("confirm_ms = 1");
        assert!(e.contains("detector.confirm_ms"), "{e}");
        assert!(e.contains("rounds to 0 hops (1 hop = 2.667 ms)"), "{e}");
    }

    #[test]
    fn negative_gc_ms_is_rejected() {
        let e = err("gc_ms = -5.0");
        assert!(e.contains("detector.gc_ms"), "{e}");
        assert!(e.contains("[0, 3600000]"), "{e}");
    }

    #[test]
    fn hang_ms_above_one_hour_is_rejected() {
        let e = err("hang_ms = 3600001");
        assert!(e.contains("detector.hang_ms"), "{e}");
        assert!(e.contains("[0, 3600000]"), "{e}");
    }

    #[test]
    fn zero_track_cap_is_rejected() {
        let e = err("track_cap = 0");
        assert!(e.contains("detector.track_cap must be at least 1"), "{e}");
    }

    #[test]
    fn negative_track_cap_is_a_type_error() {
        let e = err("track_cap = -1");
        assert!(e.contains("track_cap"), "{e}");
        assert!(e.contains("expected usize"), "{e}");
    }

    #[test]
    fn unknown_key_is_rejected() {
        let e = err("onsnr_db = 1.0");
        assert!(e.contains("unknown field"), "{e}");
        assert!(e.contains("onsnr_db"), "{e}");
    }

    #[test]
    fn warmup_and_cooldown_may_be_zero() {
        let cfg = parse("warmup_ms = 0\nsilent_respawn_cooldown_ms = 0.0").unwrap();
        assert_eq!(cfg.warmup_hops, 0);
        assert_eq!(cfg.silent_respawn_cooldown_hops, 0);
    }
}
