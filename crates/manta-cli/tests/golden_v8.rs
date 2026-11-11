//! SPEC §7 V8 (AWGN pileup) golden gates: the legacy and `--engine hsmm`
//! recall/bogus bars and the MAN-317 prior and MAN-200 repetition-fusion
//! checks on the V8 scene. MAN-521 moved these from `golden_v8_v8w.rs`
//! so V8, V8w (`golden_v8w.rs`) and VC3 (`golden_vc3.rs`) run as separate
//! binaries on separate CI shards; the scene is rendered once per binary
//! (`common::pileup::fixture`).

use std::collections::HashSet;

mod common;
use common::pileup::*;

#[test]
fn v8_pileup_validates_at_least_45_of_50_with_no_bogus_calls() {
    let fx = fixture("V8");
    let report = decode_json(&fx.wav, &[]);
    let manifest = &fx.manifest;
    let known_calls: HashSet<&str> = manifest
        .keyed_texts
        .iter()
        .map(|t| call_from_keyed_text(t))
        .collect();
    assert_eq!(
        known_calls.len(),
        50,
        "V8 fixture must have 50 unique callsigns"
    );

    let spots = spotted_calls(&report);
    let spotted: HashSet<&str> = spots.iter().map(|(c, _)| c.as_str()).collect();

    let validated = known_calls.iter().filter(|c| spotted.contains(**c)).count();
    assert!(
        validated >= 45,
        "V8 must validate >= 45/50 callsigns, got {validated}/50 (spotted: {spotted:?})"
    );

    let bogus: Vec<&str> = spotted
        .iter()
        .filter(|c| !known_calls.contains(**c))
        .copied()
        .collect();
    assert!(
        bogus.is_empty(),
        "V8 must spot 0 bogus callsigns, got {bogus:?}"
    );
}

// --- SPEC v2 §8.4 Task 12: `--engine hsmm` copies of V8/V8w, at the same
// SPEC §7 bars as their legacy counterparts above. SPEC v2 §8.1 requires
// hsmm to clear V8w (legacy's known fading gap), not just V8. Measured
// 2026-09-09 -- see docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md.

// Passes since MAN-189 (was failing on the bogus spot K2ST, real call
// K2STA); see docs/DECISIONS/2026-10-13-man189-hsmm-commit-frontier.md.
#[test]
fn v8_pileup_validates_at_least_45_of_50_with_hsmm_engine() {
    let fx = fixture("V8");
    let report = decode_json(&fx.wav, &["--engine", "hsmm"]);
    let manifest = &fx.manifest;
    let known_calls: HashSet<&str> = manifest
        .keyed_texts
        .iter()
        .map(|t| call_from_keyed_text(t))
        .collect();
    assert_eq!(
        known_calls.len(),
        50,
        "V8 fixture must have 50 unique callsigns"
    );

    let spots = spotted_calls(&report);
    let spotted: HashSet<&str> = spots.iter().map(|(c, _)| c.as_str()).collect();

    let validated = known_calls.iter().filter(|c| spotted.contains(**c)).count();
    assert!(
        validated >= 45,
        "V8/hsmm must validate >= 45/50 callsigns, got {validated}/50 (spotted: {spotted:?})"
    );

    let bogus: Vec<&str> = spotted
        .iter()
        .filter(|c| !known_calls.contains(**c))
        .copied()
        .collect();
    assert!(
        bogus.is_empty(),
        "V8/hsmm must spot 0 bogus callsigns, got {bogus:?}"
    );
}

/// MAN-317 on V8; see `common::pileup::prior_on_stays_at_zero_bogus`.
#[test]
fn v8_with_prior_on_stays_at_zero_bogus() {
    prior_on_stays_at_zero_bogus("V8");
}

/// MAN-200 on V8; see `common::pileup::repetition_fusion_on_stays_at_zero_bogus`.
#[test]
fn v8_with_repetition_fusion_on_stays_at_zero_bogus() {
    repetition_fusion_on_stays_at_zero_bogus("V8");
}
