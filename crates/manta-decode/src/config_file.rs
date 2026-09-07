//! TOML deserialization for the `[decode]` config table (SPEC v2 §7,
//! additive over v1 §9).
//!
//! `manta-server`'s `DaemonConfigFile` deliberately does not model
//! `[decode]` (nor `[detector]`/`[input]`/`[spot]`) -- `manta-server` has no
//! dependency on `manta-decode` and isn't going to grow one just to parse a
//! table it never acts on. Consumers that DO care about `[decode]` (today:
//! `manta-cli`'s `Command::Run`/`Decode`/`Oracle`, when `--config` is given)
//! parse the same TOML text a SECOND time into `DecodeConfigFile`, independent of
//! `DaemonConfigFile` -- the same pattern that file's own doc comment
//! already establishes for keeping `[server]`/`[[rbn_uplink]]` parsing
//! self-contained.
//!
//! This lives in `manta-decode` (not in `manta-cli`, which is the only
//! current consumer) so that every exposed key's default is sourced
//! directly from the real `EvidenceConfig`/`NoiseConfig`/`HsmmConfig`
//! `Default` impls this crate already owns, rather than a second,
//! hand-copied set of constants in a downstream crate that could silently
//! drift from them. It also keeps the mapping reusable by any other
//! `manta-decode` consumer without a new `manta-cli` dependency, and avoids
//! adding `Deserialize` (and reconciling `#[serde(deny_unknown_fields)]`
//! with the several fields neither spec's `[decode]` table exposes, e.g.
//! `EvidenceConfig::tau_a_ms`/`u_init_hops`, `NoiseConfig::tau_ms`,
//! `HsmmConfig::u_min`/`u_max`, `DemodConfig::tau_hi_init_ms`) to those
//! runtime structs themselves. `demod`/`beam: BeamConfig`/
//! `DecodeConfig::flush_gap_dits` ARE exposed, carrying SPEC v1 §9's
//! pre-existing `timing_sigma`/`beam_width`/`debounce_ms`/`hyst_frac`/
//! `tau_lo_ms`/`tau_hi_bounds_ms`/`flush_gap_dits` keys (Codex review, PR
//! #161, two rounds -- `deny_unknown_fields` had silently rejected any
//! config file still using them, contradicting v2 §7's "additive over v1"
//! claim. Round 1 only caught 5 of v1 §9's 12 keys; round 2 caught 3 more
//! that have a real, simple backing field. `hyst_frac` replaces v1's
//! `hyst_up`/`hyst_down` pair (MAN-103 changed the keying decision from a
//! multiplicative geometric-mean threshold to an additive band about the
//! linear-amplitude midpoint -- the two old keys have no equivalent under
//! the new model, so there is no value in accepting-but-ignoring them; no
//! shipped config file used them). `char_gap_dits`/`word_gap_dits`/
//! `mu_ratio_bounds`/`cluster_alpha` are still not exposed -- see
//! `DecodeConfigToml`'s own doc comment on why those four are a real
//! fast-follow, not a config-plumbing fix).
//!
//! `engine = "hsmm"` parses here without complaint. As of Task 11,
//! `manta-cli`'s `parse_engine` no longer gates `--engine hsmm` either --
//! `Hsmm` is a fully implemented, reviewed engine (Task 8) reachable like
//! `legacy`/`edge-legacy` everywhere. `merge_cli_engine` in `manta-cli`
//! still applies SPEC v2 §7's CLI-wins-when-given precedence: an explicit
//! `--engine` overrides whatever this table's `engine` key says, hsmm or
//! not.

use crate::beam::BeamConfig;
use crate::decoder::{DecodeConfig, Engine};
use crate::envelope::DemodConfig;
use crate::evidence::EvidenceConfig;
use crate::hsmm::HsmmConfig;
use crate::noise::NoiseConfig;
use serde::Deserialize;

fn default_engine() -> Engine {
    Engine::Legacy
}

fn default_sigma_u() -> f32 {
    EvidenceConfig::default().sigma_u
}
fn default_llr_clip() -> f32 {
    EvidenceConfig::default().llr_clip
}
fn default_hold_dits() -> f32 {
    EvidenceConfig::default().hold_dits
}
fn default_fallback_hops() -> u32 {
    EvidenceConfig::default().fallback_hops
}
fn default_noise_window_ms() -> f64 {
    NoiseConfig::default().noise_window_ms
}
fn default_noise_min_bias_db() -> f32 {
    NoiseConfig::default().noise_min_bias_db
}
fn default_spectral_min_bias_db() -> f32 {
    NoiseConfig::default().spectral_min_bias_db
}
fn default_spectral_beta() -> f32 {
    NoiseConfig::default().spectral_beta
}
/// SPEC v2 §3's narrowband refiner bandwidth in Hz (0 = off). Not sourced
/// from any `Default` impl -- the refiner itself (`manta-dsp::refine::
/// Refiner`) is a later task, and wiring it into the decode pipeline is
/// explicitly out of scope even there (MAN-168). Parsed and carried on
/// `DecodeConfig::refine_bw_hz` now so the full v2 §7 key list round-trips
/// through config today, ahead of MAN-168 having a field to read.
fn default_refine_bw_hz() -> f32 {
    0.0
}
fn default_dur_sigma() -> f32 {
    HsmmConfig::default().dur_sigma
}
fn default_mark_insert_penalty() -> f32 {
    HsmmConfig::default().mark_insert_penalty
}
fn default_hsmm_beam() -> usize {
    HsmmConfig::default().beam
}
fn default_lookahead_dits() -> f32 {
    HsmmConfig::default().lookahead_dits
}
fn default_speed_alpha() -> f32 {
    HsmmConfig::default().speed_alpha
}
fn default_seed_units_hops() -> Vec<f32> {
    HsmmConfig::default().seed_units_hops
}
fn default_conf_kappa() -> f32 {
    HsmmConfig::default().conf_kappa
}
// SPEC v1 §9's pre-existing `[decode]` keys (Codex review, PR #161): this
// table's `#[serde(deny_unknown_fields)]` otherwise silently rejects any
// config file still using these -- v2 §7 says its keys are additive over
// v1, not a replacement for it.
fn default_timing_sigma() -> f32 {
    BeamConfig::default().sigma
}
fn default_beam_width() -> usize {
    BeamConfig::default().width
}
fn default_debounce_ms() -> f64 {
    DemodConfig::default().debounce_ms
}
fn default_hyst_frac() -> f32 {
    DemodConfig::default().hyst_frac
}
fn default_tau_lo_ms() -> f64 {
    DemodConfig::default().tau_lo_ms
}
fn default_tau_hi_bounds_ms() -> [f64; 2] {
    let (lo, hi) = DemodConfig::default().tau_hi_bounds_ms;
    [lo, hi]
}
fn default_flush_gap_dits() -> f32 {
    DecodeConfig::default().flush_gap_dits
}

/// The `[decode]` TOML table, exactly SPEC v2 §7's key list. Every key
/// defaults to its real `Default` impl's value (or, for `refine_bw_hz`,
/// SPEC v2 §3's stated off-value) so an absent key is indistinguishable
/// from one explicitly set to the default -- and so a missing `[decode]`
/// table entirely (`DecodeConfigFile`'s own `#[serde(default)]`) parses to
/// the same `DecodeConfigToml::default()`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecodeConfigToml {
    #[serde(default = "default_engine")]
    pub engine: Engine,
    // SPEC v2 §1 (EvidenceConfig)
    #[serde(default = "default_sigma_u")]
    pub sigma_u: f32,
    #[serde(default = "default_llr_clip")]
    pub llr_clip: f32,
    #[serde(default = "default_hold_dits")]
    pub hold_dits: f32,
    #[serde(default = "default_fallback_hops")]
    pub fallback_hops: u32,
    // SPEC v2 §2 (NoiseConfig)
    #[serde(default = "default_noise_window_ms")]
    pub noise_window_ms: f64,
    #[serde(default = "default_noise_min_bias_db")]
    pub noise_min_bias_db: f32,
    #[serde(default = "default_spectral_min_bias_db")]
    pub spectral_min_bias_db: f32,
    #[serde(default = "default_spectral_beta")]
    pub spectral_beta: f32,
    // SPEC v2 §3 (narrowband refiner; not yet wired to any effect, see
    // `default_refine_bw_hz`)
    #[serde(default = "default_refine_bw_hz")]
    pub refine_bw_hz: f32,
    // SPEC v2 §4 (HsmmConfig)
    #[serde(default = "default_dur_sigma")]
    pub dur_sigma: f32,
    #[serde(default = "default_mark_insert_penalty")]
    pub mark_insert_penalty: f32,
    #[serde(default = "default_hsmm_beam")]
    pub beam: usize,
    #[serde(default = "default_lookahead_dits")]
    pub lookahead_dits: f32,
    #[serde(default = "default_speed_alpha")]
    pub speed_alpha: f32,
    #[serde(default = "default_seed_units_hops")]
    pub seed_units_hops: Vec<f32>,
    #[serde(default = "default_conf_kappa")]
    pub conf_kappa: f32,
    // SPEC v1 §9 (preserved additively over v2 -- see the default_* fns above)
    #[serde(default = "default_timing_sigma")]
    pub timing_sigma: f32,
    #[serde(default = "default_beam_width")]
    pub beam_width: usize,
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: f64,
    #[serde(default = "default_hyst_frac")]
    pub hyst_frac: f32,
    #[serde(default = "default_tau_lo_ms")]
    pub tau_lo_ms: f64,
    #[serde(default = "default_tau_hi_bounds_ms")]
    pub tau_hi_bounds_ms: [f64; 2],
    #[serde(default = "default_flush_gap_dits")]
    pub flush_gap_dits: f32,
    // NOT exposed here despite being in SPEC v1 §9's table --
    // `char_gap_dits`, `word_gap_dits`, `mu_ratio_bounds`, `cluster_alpha`
    // are hardcoded constants in `crates/manta-decode/src/timing.rs`
    // (`CHAR_GAP_DITS`/`WORD_GAP_DITS`/`RATIO_MIN`+`RATIO_MAX`/
    // `CLUSTER_ALPHA`), not fields on any `DecodeConfig`-reachable struct.
    // Making these four genuinely configurable is real engineering in
    // `manta-decode`'s core timing/gap-classification logic, not a config-
    // plumbing fix -- tracked as a fast-follow rather than attempted here
    // (Codex review, PR #161).
}

impl Default for DecodeConfigToml {
    fn default() -> Self {
        DecodeConfigToml {
            engine: default_engine(),
            sigma_u: default_sigma_u(),
            llr_clip: default_llr_clip(),
            hold_dits: default_hold_dits(),
            fallback_hops: default_fallback_hops(),
            noise_window_ms: default_noise_window_ms(),
            noise_min_bias_db: default_noise_min_bias_db(),
            spectral_min_bias_db: default_spectral_min_bias_db(),
            spectral_beta: default_spectral_beta(),
            refine_bw_hz: default_refine_bw_hz(),
            dur_sigma: default_dur_sigma(),
            mark_insert_penalty: default_mark_insert_penalty(),
            beam: default_hsmm_beam(),
            lookahead_dits: default_lookahead_dits(),
            speed_alpha: default_speed_alpha(),
            seed_units_hops: default_seed_units_hops(),
            conf_kappa: default_conf_kappa(),
            timing_sigma: default_timing_sigma(),
            beam_width: default_beam_width(),
            debounce_ms: default_debounce_ms(),
            hyst_frac: default_hyst_frac(),
            tau_lo_ms: default_tau_lo_ms(),
            tau_hi_bounds_ms: default_tau_hi_bounds_ms(),
            flush_gap_dits: default_flush_gap_dits(),
        }
    }
}

impl DecodeConfigToml {
    /// Builds a real `DecodeConfig` from this table. `demod`/`beam`/
    /// `flush_gap_dits` carry SPEC v1 §9's pre-existing keys (Codex review,
    /// PR #161 -- v2 §7's keys are additive over v1, not a replacement;
    /// `hyst_frac` is MAN-103's replacement for v1's `hyst_up`/`hyst_down`).
    /// Every field neither spec exposes (`evidence.tau_a_ms`/`u_init_hops`,
    /// `noise.tau_ms`, `hsmm.u_min`/`u_max`, `demod.tau_hi_init_ms`) is
    /// left at `DecodeConfig::default()`'s value.
    pub fn into_decode_config(self) -> DecodeConfig {
        DecodeConfig {
            engine: self.engine,
            demod: DemodConfig {
                hyst_frac: self.hyst_frac,
                debounce_ms: self.debounce_ms,
                tau_lo_ms: self.tau_lo_ms,
                tau_hi_bounds_ms: (self.tau_hi_bounds_ms[0], self.tau_hi_bounds_ms[1]),
                ..DemodConfig::default()
            },
            beam: BeamConfig {
                width: self.beam_width,
                sigma: self.timing_sigma,
            },
            flush_gap_dits: self.flush_gap_dits,
            evidence: EvidenceConfig {
                sigma_u: self.sigma_u,
                llr_clip: self.llr_clip,
                hold_dits: self.hold_dits,
                fallback_hops: self.fallback_hops,
                ..EvidenceConfig::default()
            },
            noise: NoiseConfig {
                noise_window_ms: self.noise_window_ms,
                noise_min_bias_db: self.noise_min_bias_db,
                spectral_min_bias_db: self.spectral_min_bias_db,
                spectral_beta: self.spectral_beta,
                ..NoiseConfig::default()
            },
            hsmm: HsmmConfig {
                dur_sigma: self.dur_sigma,
                mark_insert_penalty: self.mark_insert_penalty,
                beam: self.beam,
                lookahead_dits: self.lookahead_dits,
                speed_alpha: self.speed_alpha,
                seed_units_hops: self.seed_units_hops,
                conf_kappa: self.conf_kappa,
                ..HsmmConfig::default()
            },
            refine_bw_hz: self.refine_bw_hz,
        }
    }
}

/// Top-level wrapper for the `[decode]` table alone. Deliberately NOT
/// `#[serde(deny_unknown_fields)]` -- see `manta_server::config::
/// DaemonConfigFile`'s doc comment for why: a real `--server-config` file
/// has `[server]`, `[[rbn_uplink]]`, and potentially `[detector]`/`[input]`/
/// `[spot]` tables alongside `[decode]`, and this struct is parsed from the
/// SAME file text a second time, independently of `DaemonConfigFile` --
/// denying unknown fields at this level would reject every one of those
/// other real, valid tables.
#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct DecodeConfigFile {
    #[serde(default)]
    pub decode: DecodeConfigToml,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_decode_table_parses_to_all_defaults() {
        let file: DecodeConfigFile = toml::from_str("").unwrap();
        assert_eq!(file.decode, DecodeConfigToml::default());
    }

    #[test]
    fn explicit_keys_override_only_themselves() {
        let file: DecodeConfigFile = toml::from_str(
            r#"
            [decode]
            sigma_u = 0.25
            beam = 8
            "#,
        )
        .unwrap();
        let expected = DecodeConfigToml {
            sigma_u: 0.25,
            beam: 8,
            ..DecodeConfigToml::default()
        };
        assert_eq!(file.decode, expected);
    }

    #[test]
    fn every_spec_v2_section7_key_round_trips() {
        let file: DecodeConfigFile = toml::from_str(
            r#"
            [decode]
            engine = "edge-legacy"
            sigma_u = 0.31
            llr_clip = 9.0
            hold_dits = 5.0
            fallback_hops = 9
            noise_window_ms = 1600.0
            noise_min_bias_db = 2.0
            spectral_min_bias_db = 1.4
            spectral_beta = 0.6
            refine_bw_hz = 30.0
            dur_sigma = 0.25
            mark_insert_penalty = -1.4
            beam = 10
            lookahead_dits = 20.0
            speed_alpha = 0.25
            seed_units_hops = [8.0, 12.0, 17.0, 25.0, 37.0]
            conf_kappa = 5.5
            "#,
        )
        .unwrap();
        assert_eq!(
            file.decode,
            DecodeConfigToml {
                engine: Engine::EdgeLegacy,
                sigma_u: 0.31,
                llr_clip: 9.0,
                hold_dits: 5.0,
                fallback_hops: 9,
                noise_window_ms: 1600.0,
                noise_min_bias_db: 2.0,
                spectral_min_bias_db: 1.4,
                spectral_beta: 0.6,
                refine_bw_hz: 30.0,
                dur_sigma: 0.25,
                mark_insert_penalty: -1.4,
                beam: 10,
                lookahead_dits: 20.0,
                speed_alpha: 0.25,
                seed_units_hops: vec![8.0, 12.0, 17.0, 25.0, 37.0],
                conf_kappa: 5.5,
                ..DecodeConfigToml::default()
            }
        );
    }

    #[test]
    fn spec_v1_section9_keys_still_parse_and_are_not_rejected_as_unknown() {
        // Codex review, PR #161 (two rounds): `deny_unknown_fields` had
        // silently rejected any config file still using SPEC v1 §9's
        // pre-existing keys, even though v2 §7 documents its own keys as
        // additive over v1, not a replacement. Round 1 covered 5 keys;
        // round 2 added the 3 more that have a real, simple backing field
        // (`char_gap_dits`/`word_gap_dits`/`mu_ratio_bounds`/
        // `cluster_alpha` remain unexposed -- see `DecodeConfigToml`'s doc
        // comment). `hyst_frac` is MAN-103's replacement for v1's
        // `hyst_up`/`hyst_down` (no equivalent under the new additive-band
        // keying model, so the old pair is not accepted here).
        let file: DecodeConfigFile = toml::from_str(
            r#"
            [decode]
            timing_sigma = 0.30
            beam_width = 6
            debounce_ms = 15.0
            hyst_frac = 0.2
            tau_lo_ms = 450.0
            tau_hi_bounds_ms = [120.0, 380.0]
            flush_gap_dits = 9.0
            "#,
        )
        .unwrap();
        let expected = DecodeConfigToml {
            timing_sigma: 0.30,
            beam_width: 6,
            debounce_ms: 15.0,
            hyst_frac: 0.2,
            tau_lo_ms: 450.0,
            tau_hi_bounds_ms: [120.0, 380.0],
            flush_gap_dits: 9.0,
            ..DecodeConfigToml::default()
        };
        assert_eq!(file.decode, expected);

        let cfg = file.decode.into_decode_config();
        assert_eq!(cfg.beam.sigma, 0.30);
        assert_eq!(cfg.beam.width, 6);
        assert_eq!(cfg.demod.debounce_ms, 15.0);
        assert_eq!(cfg.demod.hyst_frac, 0.2);
        assert_eq!(cfg.demod.tau_lo_ms, 450.0);
        assert_eq!(cfg.demod.tau_hi_bounds_ms, (120.0, 380.0));
        assert_eq!(cfg.flush_gap_dits, 9.0);
    }

    #[test]
    fn hsmm_engine_parses_permissively_at_this_layer() {
        // Engine::Hsmm has no CLI-level gate anywhere as of Task 11, but
        // this layer never depended on that gate to begin with -- it just
        // parses whatever `engine` value is given, letting the CALLER
        // (manta-cli's merge_cli_engine) apply SPEC v2 §7's
        // CLI-wins-when-given precedence over this file's value.
        let file: DecodeConfigFile = toml::from_str(
            r#"
            [decode]
            engine = "hsmm"
            "#,
        )
        .unwrap();
        assert_eq!(file.decode.engine, Engine::Hsmm);
    }

    #[test]
    fn unknown_key_in_decode_table_is_rejected() {
        let result: Result<DecodeConfigFile, _> = toml::from_str(
            r#"
            [decode]
            sigma_u = 0.25
            sigma_you = 0.25
            "#,
        );
        assert!(result.is_err(), "a typo'd key must not silently no-op");
    }

    #[test]
    fn into_decode_config_leaves_non_exposed_fields_at_their_defaults() {
        let cfg = DecodeConfigToml {
            sigma_u: 0.31,
            beam: 10,
            ..DecodeConfigToml::default()
        }
        .into_decode_config();
        let default = DecodeConfig::default();
        assert_eq!(cfg.evidence.sigma_u, 0.31);
        assert_eq!(cfg.hsmm.beam, 10);
        assert_eq!(cfg.evidence.tau_a_ms, default.evidence.tau_a_ms);
        assert_eq!(cfg.evidence.u_init_hops, default.evidence.u_init_hops);
        assert_eq!(cfg.noise.tau_ms, default.noise.tau_ms);
        assert_eq!(cfg.hsmm.u_min, default.hsmm.u_min);
        assert_eq!(cfg.hsmm.u_max, default.hsmm.u_max);
        assert_eq!(cfg.demod.tau_hi_init_ms, default.demod.tau_hi_init_ms);
        // flush_gap_dits/beam.width are exposed now (Codex review, PR #161)
        // but weren't overridden in this instance -- still equal defaults.
        assert_eq!(cfg.flush_gap_dits, default.flush_gap_dits);
        assert_eq!(cfg.beam.width, default.beam.width);
    }
}
