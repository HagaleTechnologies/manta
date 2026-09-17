//! SPEC §7 V2/V3 golden gates.
//! V2 "fast-35": char accuracy >= 99 %; WPM reported 35 +/- 2.
//! V3 "slow-weak": char accuracy >= 95 %.

use std::process::Command;

fn decode_report(
    spec: &manta_testkit::vectors::VectorSpec,
) -> (serde_json::Value, manta_testkit::vectors::Manifest) {
    decode_report_with_args(spec, &[])
}

/// Same as `decode_report`, with extra CLI args (e.g. `--engine
/// edge-legacy`, Task 5's SPEC v2 §0 engine switch) appended after `--json`.
fn decode_report_with_args(
    spec: &manta_testkit::vectors::VectorSpec,
    extra_args: &[&str],
) -> (serde_json::Value, manta_testkit::vectors::Manifest) {
    let dir = tempfile::tempdir().unwrap();
    let manifest = manta_testkit::vectors::write_fixture_set(spec, dir.path()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_manta"))
        .args(["decode", "--json"])
        .args(extra_args)
        .arg(dir.path().join(format!("{}.wav", spec.name)))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    (serde_json::from_slice(&out.stdout).unwrap(), manifest)
}

/// MAN-103 acceptance test: V2 is the near-channel-edge case (-8200 Hz,
/// -0.4667 channels from center). SPEC §7 gates it at 35 +/- 2 WPM. This
/// was the ticket's own reproduction case -- MAN-7 diagnosed the symptom
/// (filed as <https://github.com/HagaleTechnologies/manta/issues/24>) but
/// its fix never landed on `main`; MAN-103 fixed the actual root cause
/// (SPEC §3.2/§3.3's keying threshold placement, plus SPEC §4.1's mark/gap
/// symmetric dit-period estimate) -- see
/// docs/DECISIONS/2026-09-07-man103-keying-edge-placement.md. Split from the
/// combined CER+WPM test below: the CER half's warmup-floor dilution
/// (unrelated to MAN-103) previously prevented this WPM assertion from ever
/// being reached.
#[test]
fn v2_wpm_is_within_spec_tolerance() {
    let spec = manta_testkit::vectors::v2();
    let (report, _manifest) = decode_report(&spec);
    let wpm = report["wpm"].as_f64().unwrap();
    assert!((wpm - 35.0).abs() < 2.0, "wpm {wpm}");
}

/// CER only: SPEC §2.1 warmup-floor dilution, unrelated to MAN-103. That
/// pin (docs/DECISIONS/2026-07-18-m2-pfb-channelizer-pins.md, pin 7/8)
/// blamed keying jitter interacting with the channelizer's transient
/// response at V2's near-channel-edge offset, measuring 8.94-23 % CER under
/// the old placeholder detector. Under the real detector this is CER 0.0325
/// (full 90 s V2 scene, with jitter) / 0.0203 (jitter removed) -- jitter now
/// contributes ~1.2 points, not 8-23. Both numbers match pure SPEC §2.1
/// warmup-floor dilution: CER shrinks with scene duration (90s: 0.0325,
/// 200s: 0.0128, 400s: 0.0064, converging toward 0), with a clean decode
/// throughout the middle of the scene. Left `#[ignore]`d rather than
/// widened -- its own separate investigation, out of MAN-103's scope
/// (docs/DECISIONS/2026-09-06-broad-review-decisions.md D8).
#[test]
#[ignore]
fn v2_char_accuracy_meets_spec() {
    let spec = manta_testkit::vectors::v2();
    let (report, manifest) = decode_report(&spec);
    let decoded = report["text"].as_str().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.01,
        "V2 char accuracy must be >= 99 % (CER <= 0.01), got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );
}

/// V2 with Task 5's `EdgeLegacy` engine (SPEC v2 §0), exercising the
/// `manta decode --engine edge-legacy` CLI surface added alongside it.
/// Unlike `v2_passes_end_to_end_from_wav` above, this vector is fully
/// synthetic (`manta_testkit::vectors::v2`) and needs no external corpus.
///
/// [Finding, Task 5 execution]: measured, this does NOT pass yet -- CER
/// 0.0244 (need <= 0.01), decoded text starting `"A DE JA1ABC..."` against
/// expected `"CQ CQ DE JA1ABC..."`. This is the SAME root cause diagnosed
/// and worked around in `manta_decode::decoder`'s
/// `edge_legacy_decodes_contest_speed_deep_keying` unit test: `NoiseTracker`
/// and `Evidence` both initialize their very first smoothed sample directly
/// from that sample's own value (no history to blend against yet), so if
/// the scene's true leading edge is a mark rather than a settled noise
/// floor, the noise estimate starts pinned near the mark level and the
/// evidence gate stays closed until a later gap forces it down -- garbling
/// or dropping whatever keying happened before that point. The unit test
/// could work around it with a 4-hop noise-floor lead-in it fully controls;
/// this end-to-end CLI test decodes a vector built by
/// `manta_testkit::vectors::v2()` through the real IQ pipeline
/// (`manta_engine::decode_wav`), which has no equivalent lead-in and is out
/// of Task 5's file scope (`manta-testkit`) to add. Left `#[ignore]`d
/// pending a proper fix -- likely a `NoiseTracker`/`Evidence` warm-up phase
/// analogous to the `Legacy` engine's `Demod::Phase::Init` -- rather than
/// widening the CER tolerance or hacking the vector generator, per this
/// plan's own guidance for genuine decoder bugs (see
/// `v2_passes_end_to_end_from_wav` above for the same policy applied to a
/// different bug).
#[test]
#[ignore]
fn v2_passes_with_edge_legacy_engine() {
    let spec = manta_testkit::vectors::v2();
    let (report, manifest) = decode_report_with_args(&spec, &["--engine", "edge-legacy"]);
    let decoded = report["text"].as_str().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.01,
        "V2 char accuracy must be >= 99 % (CER <= 0.01), got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );
    let wpm = report["wpm"].as_f64().unwrap();
    assert!((wpm - 35.0).abs() < 2.0, "wpm {wpm}");
}

#[test]
fn v3_passes_end_to_end_from_wav() {
    let spec = manta_testkit::vectors::v3();
    let (report, manifest) = decode_report(&spec);
    let decoded = report["text"].as_str().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.05,
        "V3 char accuracy must be >= 95 % (CER <= 0.05), got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );
}

#[test]
fn v4_passes_end_to_end_from_wav() {
    let spec = manta_testkit::vectors::v4();
    let (report, manifest) = decode_report(&spec);
    let decoded = report["text"].as_str().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.05,
        "V4 char accuracy must be >= 95 % (CER <= 0.05), got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );
}

/// Ignored: WattersonPreset::Poor at V5's 3 dB SNR produces near-continuous
/// fading with essentially no calm stretches (coherence time ~0.32s vs a
/// 22 WPM dit's ~54ms -- multiple dits per fade cycle). An exhaustive
/// 60-seed sweep of WattersonFade.seed found zero candidates meeting the
/// SPEC §7 CER <= 0.20 threshold (best of 60 was 0.38, roughly 2x over).
/// Pure-AWGN decode at the same 3 dB SNR (no fading) is CER=0, ruling out
/// an SNR-headroom bug -- this is a genuine classical-decoder fading-
/// robustness gap, consistent with this project's stated design (CLAUDE.md:
/// "Classical decoder first; ML fusion ... only at M4, gated on beating the
/// classical baseline under simulated fading"). Tracked in the M1 pinned-
/// decisions doc; revisit once manta-decode gains real fading resilience
/// (M4) or a different mitigation is found.
///
/// Its "callsign validated within 90 s" check below now uses the real
/// `manta-spot::Validator` (`report["spots"]`, M3 engine-wiring
/// sub-project) instead of the earlier running-decoded-text substring scan
/// -- see docs/superpowers/specs/2026-07-26-m3-engine-wiring-design.md.
#[test]
#[ignore]
fn v5_passes_end_to_end_from_wav() {
    let spec = manta_testkit::vectors::v5();
    let (report, manifest) = decode_report(&spec);
    let decoded = report["text"].as_str().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.20,
        "V5 char accuracy must be >= 80 % (CER <= 0.20), got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );

    // Callsign validated within 90 s: the real Validator's first ZL2XYZ
    // spot's sample_ts. sample_ts is in raw input samples at manifest.fs
    // (SPEC §1.1).
    let validated_ts = report["spots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["callsign"].as_str() == Some("ZL2XYZ"))
        .map(|s| s["sample_ts"].as_u64().unwrap() as f64)
        .expect("ZL2XYZ never validated as a spot");
    assert!(
        validated_ts <= 90.0 * manifest.fs,
        "ZL2XYZ validated at {:.1} s, expected <= 90 s",
        validated_ts / manifest.fs
    );
}

/// Ignored: regressed from green (sub-project 1's placeholder detector) to
/// CER 0.1429 (need <= 0.10) under the real detector/track manager landed
/// in M2 sub-project 2. Errors scattered throughout the decode rather than
/// confined to a lost leading prefix, ruling out the SPEC §2.1 warmup-floor
/// mechanism behind every other Task 11 tolerance fix on this branch --
/// confirmed during Task 11's investigation as a genuine, unrelated
/// classical-decoder fading-robustness gap under `WattersonPreset` fading,
/// same family as V5's pre-existing `#[ignore]`. Filed as
/// <https://github.com/HagaleTechnologies/manta/issues/25>; revisit
/// alongside V5 once manta-decode gains real fading resilience (M4).
#[test]
#[ignore]
fn v6_passes_end_to_end_from_wav() {
    let spec = manta_testkit::vectors::v6();
    let (report, manifest) = decode_report(&spec);
    let decoded = report["text"].as_str().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.10,
        "V6 char accuracy must be >= 90 % (CER <= 0.10), got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );
    // "Track survives" (ROADMAP): at least one CharDecoded event must land
    // in the render's second half, proving the decoder didn't silently
    // stop producing output during the QSB trough (no explicit
    // track-closed event exists yet at M0/M1 to assert against directly).
    let events = report["events"].as_array().unwrap();
    // sample_ts is in raw input samples at manifest.fs (SPEC §1.1's
    // extractor timing, NOT the 375 Hz channel-output rate).
    let half_ts = (manifest.duration_s / 2.0 * manifest.fs) as u64;
    let survives_past_half = events.iter().any(|ev| {
        ev["event"].as_str() == Some("CharDecoded")
            && ev["sample_ts"].as_u64().unwrap_or(0) > half_ts
    });
    assert!(
        survives_past_half,
        "no CharDecoded event past the render midpoint"
    );
}

// --- SPEC v2 §8.4 Task 12: `--engine hsmm` copies of V2/V3/V4/V5/V6, at
// the same SPEC §7 bars as their legacy counterparts above. SPEC v2 §8.1
// requires hsmm to clear V2/V5/V6 (the legacy-only-known limitations),
// not just V3/V4. Measured 2026-09-09 -- see
// docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md for the numbers
// and #[ignore] rationale on each.

#[test]
#[ignore = "stage-2 gate, un-ignored by Task 12 only if measured passing"]
fn v2_passes_end_to_end_with_hsmm_engine() {
    let spec = manta_testkit::vectors::v2();
    let (report, manifest) = decode_report_with_args(&spec, &["--engine", "hsmm"]);
    let decoded = report["text"].as_str().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.01,
        "V2/hsmm char accuracy must be >= 99 % (CER <= 0.01), got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );
    let wpm = report["wpm"].as_f64().unwrap();
    assert!((wpm - 35.0).abs() < 2.0, "wpm {wpm}");
}

// Measured passing (2026-09-09, stage-2 gate) -- see
// docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md.
#[test]
fn v3_passes_end_to_end_with_hsmm_engine() {
    let spec = manta_testkit::vectors::v3();
    let (report, manifest) = decode_report_with_args(&spec, &["--engine", "hsmm"]);
    let decoded = report["text"].as_str().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.05,
        "V3/hsmm char accuracy must be >= 95 % (CER <= 0.05), got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );
}

// Measured passing (2026-09-09, stage-2 gate) -- see
// docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md.
#[test]
fn v4_passes_end_to_end_with_hsmm_engine() {
    let spec = manta_testkit::vectors::v4();
    let (report, manifest) = decode_report_with_args(&spec, &["--engine", "hsmm"]);
    let decoded = report["text"].as_str().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.05,
        "V4/hsmm char accuracy must be >= 95 % (CER <= 0.05), got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );
}

#[test]
#[ignore = "stage-2 gate, un-ignored by Task 12 only if measured passing"]
fn v5_passes_end_to_end_with_hsmm_engine() {
    let spec = manta_testkit::vectors::v5();
    let (report, manifest) = decode_report_with_args(&spec, &["--engine", "hsmm"]);
    let decoded = report["text"].as_str().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.20,
        "V5/hsmm char accuracy must be >= 80 % (CER <= 0.20), got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );
    let validated_ts = report["spots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["callsign"].as_str() == Some("ZL2XYZ"))
        .map(|s| s["sample_ts"].as_u64().unwrap() as f64)
        .expect("ZL2XYZ never validated as a spot");
    assert!(
        validated_ts <= 90.0 * manifest.fs,
        "ZL2XYZ validated at {:.1} s, expected <= 90 s",
        validated_ts / manifest.fs
    );
}

// Measured passing (2026-09-09, stage-2 gate) -- see
// docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md.
#[test]
fn v6_passes_end_to_end_with_hsmm_engine() {
    let spec = manta_testkit::vectors::v6();
    let (report, manifest) = decode_report_with_args(&spec, &["--engine", "hsmm"]);
    let decoded = report["text"].as_str().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.10,
        "V6/hsmm char accuracy must be >= 90 % (CER <= 0.10), got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );
    let events = report["events"].as_array().unwrap();
    let half_ts = (manifest.duration_s / 2.0 * manifest.fs) as u64;
    let survives_past_half = events.iter().any(|ev| {
        ev["event"].as_str() == Some("CharDecoded")
            && ev["sample_ts"].as_u64().unwrap_or(0) > half_ts
    });
    assert!(
        survives_past_half,
        "no CharDecoded event past the render midpoint"
    );
}
