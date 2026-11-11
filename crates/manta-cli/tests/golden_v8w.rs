//! SPEC §7 V8w (Watterson-faded pileup) golden gates: the legacy bogus
//! gate, the two ignored CER gates (legacy and `--engine hsmm`), and the
//! MAN-317 prior and MAN-200 repetition-fusion checks on the V8w scene.
//! MAN-521 moved these from `golden_v8_v8w.rs` so V8 (`golden_v8.rs`), V8w
//! and VC3 (`golden_vc3.rs`) run as separate binaries on separate CI
//! shards; the scene is rendered once per binary
//! (`common::pileup::fixture`).

use std::collections::HashSet;

mod common;
use common::decode::per_track;
use common::pileup::*;

/// SPEC §7's V8w row states three independent pass criteria; this is the
/// second, "0 bogus callsigns". It is deliberately a test of its own, NOT
/// part of `v8w_pileup_fading_decodes_90pct_of_strong_signals_no_ghosts`
/// below: that test is `#[ignore]`d for the unrelated CER-accuracy gap
/// (issue #28 / MAN-107..MAN-113), and its CER `assert!` panics before the
/// bogus-call check is ever reached, so bundling them made this criterion
/// unrunnable in CI. Split out per MAN-101.
#[test]
fn v8w_pileup_fading_spots_no_bogus_callsigns() {
    let fx = fixture("V8w");
    let report = decode_json(&fx.wav, &[]);
    let failures = v8w_scene_failures(&report, &fx.manifest, "V8w");
    assert!(failures.is_empty(), "V8w: {}", failures.join(" | "));
}

/// Ignored: measured 1/34 (2.9%) of the >= +6 dB strong signals at
/// CER < 0.10 (need >= 90%, i.e. >= 31/34) -- sorted CERs across the 34:
/// 0.094 (the only pass), 0.108, 0.144, 0.154, 0.181, 0.187, 0.187, 0.199,
/// 0.200, 0.205, 0.207, 0.226, 0.232, 0.235, 0.244, 0.271, 0.275, 0.276,
/// 0.286, 0.298, 0.308, 0.327, 0.337, 0.373, 0.384, 0.393, 0.441, 0.521,
/// 0.526, 0.654, 0.686, 0.784, 0.844, 0.918 -- median ~0.276, ~2.76x the gate.
/// (CERs recorded under the pre-MAN-120 nearest-single-track pick; MAN-120
/// moved scoring onto the frequency-joined copy, which selects the same
/// single track on every one of these scenes -- see below -- so the
/// recorded numbers carry over unchanged.)
///
/// Ruled out as a harness/matching artifact two ways. First, the sibling
/// AWGN-only V8 test (`golden_v8.rs`; identical 50-signal scene: same offsets/WPM/SNR/
/// jitter, same frequency-join matching) passes 49/50 validated,
/// 0 bogus -- fading is the only variable that changed. Second, within V8w
/// itself, checked whether CER was inflated by the harness scoring only a
/// fragment of a fading-split track (a deep fade causing drop/reacquire as
/// a new track_id, so the nearest-single-track pick -- the rule here
/// before MAN-120 moved CER onto the frequency-joined copy -- captures
/// less than the full transmission): for 31/34 strong signals,
/// `decoded_text.len() / expected_text.len()` is 1.0-1.3 (full-duration
/// capture, exactly one track within 300 Hz of the expected frequency) yet
/// CER still fails, ruling out fragmentation as the primary driver. That
/// sentence once counted 3/34 strong signals split into 5-15 tracks within
/// 300 Hz; on `35deb42` none was, and with MAN-108's DORMANT re-acquisition
/// still 0/34 have more than one track within 300 Hz of the expected
/// frequency (docs/DECISIONS/2026-10-17-man108-dormant-track-reacquisition.md),
/// so fragmentation plays no part in this CER gap -- and since MAN-120 the
/// scoring is structurally immune to it anyway: CER is scored over the
/// texts of all tracks within `CER_JOIN_HZ` joined in id order
/// (manta-testkit's `joined_cer`, the `manta bench sensitivity`
/// convention), so a future fragmentation regression can no longer
/// inflate these numbers either.
///
/// The dominant pattern for the other 31 is scattered character-level
/// corruption throughout an otherwise full-length decode, e.g. signal 1
/// (AC3AGO, SNR 12 dB, CER 0.521): expected `CQ CQ DE AC3AGO AC3AGO K ...`
/// vs decoded `U C7 DE ANI3AGO E RIAT ATIO K CNU NNQ IE ACSMAG M ...` --
/// call recognizable but heavily corrupted at matched length. This is the
/// same classical-decoder fading-robustness gap already tracked for V5
/// (docs/DECISIONS/2026-07-17-m1-implementation-pins.md) and V6 (issue
/// #25), now demonstrated at scale (34 independent fading realizations in
/// one scene, vs V5/V6's one signal each). Filed as
/// <https://github.com/HagaleTechnologies/manta/issues/28>; revisit
/// alongside V5/V6 once manta-decode gains real fading resilience (M4).
#[test]
#[ignore]
fn v8w_pileup_fading_decodes_90pct_of_strong_signals_no_ghosts() {
    let fx = fixture("V8w");
    let report = decode_json(&fx.wav, &[]);
    let spec = &fx.spec;
    let manifest = &fx.manifest;
    let tracks = per_track(&report);
    let known_calls: HashSet<&str> = manifest
        .keyed_texts
        .iter()
        .map(|t| call_from_keyed_text(t))
        .collect();

    let refs = track_refs(&tracks);
    let strong: Vec<usize> = spec
        .signals
        .iter()
        .enumerate()
        .filter(|(_, s)| s.snr_2500_db >= 6.0)
        .map(|(i, _)| i)
        .collect();
    assert!(
        !strong.is_empty(),
        "V8w must have at least one >= +6 dB signal"
    );

    let mut good = 0;
    for &i in &strong {
        let cer = manta_testkit::cer::joined_cer(
            &manifest.keyed_texts[i],
            &refs,
            manifest.expected_freqs_hz[i],
            CER_JOIN_HZ,
        );
        if cer < 0.10 {
            good += 1;
        }
    }
    let pct = good as f64 / strong.len() as f64;
    assert!(
        pct >= 0.90,
        "V8w must decode >= 90% of >= +6 dB signals at CER < 10%, got {good}/{} ({:.1}%)",
        strong.len(),
        pct * 100.0
    );

    // NOTE: SPEC §7's second V8w criterion, "0 bogus callsigns", is NOT
    // asserted here -- it lives in the non-ignored
    // `v8w_pileup_fading_spots_no_bogus_callsigns` above, so it runs in CI
    // independently of this CER gate (MAN-101).
    let spots = spotted_calls(&report);

    // 0 cross-channel ghost decodes: no known call's spots span more than
    // one distinct track_id.
    for call in &known_calls {
        let track_ids: HashSet<u64> = spots
            .iter()
            .filter(|(c, _)| c == call)
            .map(|(_, tid)| *tid)
            .collect();
        assert!(
            track_ids.len() <= 1,
            "callsign {call} spotted from {} distinct tracks, expected <= 1 (ghost decode)",
            track_ids.len()
        );
    }
}

// --- SPEC v2 §8.4 Task 12: `--engine hsmm` copies of V8/V8w, at the same
// SPEC §7 bars as their legacy counterparts above. SPEC v2 §8.1 requires
// hsmm to clear V8w (legacy's known fading gap), not just V8. Measured
// 2026-09-09 -- see docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md.

#[test]
#[ignore = "stage-2 gate, un-ignored by Task 12 only if measured passing"]
fn v8w_pileup_fading_decodes_90pct_of_strong_signals_with_hsmm_engine() {
    let fx = fixture("V8w");
    let report = decode_json(&fx.wav, &["--engine", "hsmm"]);
    let spec = &fx.spec;
    let manifest = &fx.manifest;
    let tracks = per_track(&report);
    let known_calls: HashSet<&str> = manifest
        .keyed_texts
        .iter()
        .map(|t| call_from_keyed_text(t))
        .collect();

    let refs = track_refs(&tracks);
    let strong: Vec<usize> = spec
        .signals
        .iter()
        .enumerate()
        .filter(|(_, s)| s.snr_2500_db >= 6.0)
        .map(|(i, _)| i)
        .collect();
    assert!(
        !strong.is_empty(),
        "V8w must have at least one >= +6 dB signal"
    );

    let mut good = 0;
    for &i in &strong {
        let cer = manta_testkit::cer::joined_cer(
            &manifest.keyed_texts[i],
            &refs,
            manifest.expected_freqs_hz[i],
            CER_JOIN_HZ,
        );
        if cer < 0.10 {
            good += 1;
        }
    }
    let pct = good as f64 / strong.len() as f64;
    assert!(
        pct >= 0.90,
        "V8w/hsmm must decode >= 90% of >= +6 dB signals at CER < 10%, got {good}/{} ({:.1}%)",
        strong.len(),
        pct * 100.0
    );

    let spots = spotted_calls(&report);
    let spotted: HashSet<&str> = spots.iter().map(|(c, _)| c.as_str()).collect();
    let bogus: Vec<&str> = spotted
        .iter()
        .filter(|c| !known_calls.contains(**c))
        .copied()
        .collect();
    assert!(
        bogus.is_empty(),
        "V8w/hsmm must spot 0 bogus callsigns, got {bogus:?}"
    );

    for call in &known_calls {
        let track_ids: HashSet<u64> = spots
            .iter()
            .filter(|(c, _)| c == call)
            .map(|(_, tid)| *tid)
            .collect();
        assert!(
            track_ids.len() <= 1,
            "callsign {call} spotted from {} distinct tracks, expected <= 1 (ghost decode)",
            track_ids.len()
        );
    }
}

/// MAN-317 on V8w; see `common::pileup::prior_on_stays_at_zero_bogus`.
#[test]
fn v8w_with_prior_on_stays_at_zero_bogus() {
    prior_on_stays_at_zero_bogus("V8w");
}

/// MAN-200 on V8w; see `common::pileup::repetition_fusion_on_stays_at_zero_bogus`.
#[test]
fn v8w_with_repetition_fusion_on_stays_at_zero_bogus() {
    repetition_fusion_on_stays_at_zero_bogus("V8w");
}
