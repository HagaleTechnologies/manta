//! SPEC v2 §8.2 "real-conditions" golden gates VR1-VR8. Conditions the
//! original V1-V10 vectors never exercised (contest speed, deep keying, hard
//! transmitter edges, co-channel occupancy, non-standard weighting, tight
//! Farnsworth-free spacing) -- the gap that let the real-signal timing
//! defect motivating this redesign go undetected.
//!
//! All nine tests here run `manta decode --json --engine hsmm` (SPEC v2's
//! stage-2 engine). Task 12 (2026-09-09) measured the real `HsmmDecoder`
//! against every one and un-ignored the three that clear their bars
//! (vr6a, vr6b, vr7); the other six (vr1-vr5, vr8) stay
//! `#[ignore = "stage-2 gate, un-ignored by Task 12"]` -- see
//! docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md for the
//! measured CER/WPM/word-boundary numbers and failure-kind analysis. The
//! overall stage-2 gate (SPEC v2 §8.4) is FAIL.

use std::collections::HashSet;
use std::process::Command;

fn decode_report(
    spec: &manta_testkit::vectors::VectorSpec,
) -> (serde_json::Value, manta_testkit::vectors::Manifest) {
    let dir = tempfile::tempdir().unwrap();
    let manifest = manta_testkit::vectors::write_fixture_set(spec, dir.path()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_manta"))
        .args(["decode", "--json", "--engine", "hsmm"])
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

/// Group `report["events"]` by `track_id`, returning each track's decoded
/// text and its last-reported TrackMeta freq_hz. Same shape as
/// golden_v7_v9_v10.rs's/golden_v8_v8w.rs's helper of the same name.
fn per_track(report: &serde_json::Value) -> std::collections::BTreeMap<u64, (String, Option<f64>)> {
    let mut texts: std::collections::BTreeMap<u64, String> = std::collections::BTreeMap::new();
    let mut freqs: std::collections::BTreeMap<u64, f64> = std::collections::BTreeMap::new();
    for ev in report["events"].as_array().unwrap() {
        let tid = ev["track_id"].as_u64().unwrap();
        match ev["event"].as_str().unwrap() {
            "CharDecoded" => {
                if let Some(c) = ev["glyph"]["Char"].as_str() {
                    texts.entry(tid).or_default().push_str(c);
                }
            }
            "WordBoundary" => {
                let t = texts.entry(tid).or_default();
                if !t.is_empty() && !t.ends_with(' ') {
                    t.push(' ');
                }
            }
            "TrackMeta" => {
                freqs.insert(tid, ev["freq_hz"].as_f64().unwrap());
            }
            _ => {}
        }
    }
    texts
        .into_iter()
        .map(|(tid, t)| (tid, (t.trim().to_string(), freqs.get(&tid).copied())))
        .collect()
}

/// The decoded track whose last-reported freq_hz is closest to
/// `expected_freq`.
fn track_nearest_freq(
    tracks: &std::collections::BTreeMap<u64, (String, Option<f64>)>,
    expected_freq: f64,
) -> &str {
    tracks
        .values()
        .min_by(|(_, fa), (_, fb)| {
            let da = (fa.unwrap_or(f64::MAX) - expected_freq).abs();
            let db = (fb.unwrap_or(f64::MAX) - expected_freq).abs();
            da.partial_cmp(&db).unwrap()
        })
        .map(|(text, _)| text.as_str())
        .unwrap()
}

/// `(callsign, track_id)` pairs from `report["spots"]` -- the real
/// `manta-spot::Validator`'s output. Same shape as golden_v8_v8w.rs.
fn spotted_calls(report: &serde_json::Value) -> Vec<(String, u64)> {
    report["spots"]
        .as_array()
        .expect("decode --json output must include a 'spots' array")
        .iter()
        .map(|s| {
            (
                s["callsign"].as_str().unwrap().to_string(),
                s["track_id"].as_u64().unwrap(),
            )
        })
        .collect()
}

/// Extract the callsign from this project's SPEC v2 §8.2 payload template
/// "CQ TEST <CALL> <CALL> TEST" (word index 2).
fn call_from_keyed_text(text: &str) -> &str {
    text.split_whitespace()
        .nth(2)
        .expect("keyed text must follow the 'CQ TEST <CALL> <CALL> TEST' template")
}

/// Word-boundary accuracy per the Task 10 brief: the fraction of expected
/// word boundaries present in the decoded text, comparing
/// `split_whitespace()` word counts. Word count is used as a direct proxy
/// for boundary count (boundaries = words - 1; for the many-dozens-of-words
/// repeated-loop scenes VR1-VR8 render over 120 s, the constant -1 offset is
/// immaterial to the fraction). Allows the SPEC §2.1 mandatory warm-up floor
/// to cost exactly the first word (same documented mechanism as
/// golden_v1.rs's CER note and golden_v7_v9_v10.rs's V10 word-count
/// tolerance) by crediting the decoded count one word higher before
/// comparing.
fn word_boundary_fraction(expected: &str, decoded: &str) -> f64 {
    let expected_words = expected.split_whitespace().count();
    let decoded_words = decoded.split_whitespace().count();
    if expected_words == 0 {
        return 1.0;
    }
    let credited_decoded = decoded_words + 1;
    credited_decoded.min(expected_words) as f64 / expected_words as f64
}

#[test]
#[ignore = "stage-2 gate, un-ignored by Task 12"]
fn vr1_contest_40_passes_end_to_end() {
    let spec = manta_testkit::vectors::vr1();
    let (report, manifest) = decode_report(&spec);
    let decoded = report["text"].as_str().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.05,
        "VR1 char accuracy must be >= 95% (CER <= 0.05), got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );
    let wpm = report["wpm"].as_f64().unwrap();
    assert!((wpm - 40.0).abs() < 2.0, "wpm {wpm}");
    let wb = word_boundary_fraction(&manifest.keyed_texts[0], decoded);
    assert!(wb >= 0.95, "VR1 word boundaries must be >= 95%, got {wb}");
}

#[test]
#[ignore = "stage-2 gate, un-ignored by Task 12"]
fn vr2_contest_45_passes_end_to_end() {
    let spec = manta_testkit::vectors::vr2();
    let (report, manifest) = decode_report(&spec);
    let decoded = report["text"].as_str().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.10,
        "VR2 char accuracy must be >= 90% (CER <= 0.10), got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );
    let wpm = report["wpm"].as_f64().unwrap();
    assert!((wpm - 45.0).abs() < 3.0, "wpm {wpm}");
}

#[test]
#[ignore = "stage-2 gate, un-ignored by Task 12"]
fn vr3_deep_keying_passes_end_to_end() {
    let spec = manta_testkit::vectors::vr3();
    let (report, manifest) = decode_report(&spec);
    let decoded = report["text"].as_str().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.02,
        "VR3 char accuracy must be >= 98% (CER <= 0.02), got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );
    // "Measured dit within +/-10% of 40 ms" (VR3 is 30 WPM: nominal dit =
    // 1200/30 = 40 ms): derived from the reported WPM (dit_ms = 1200/wpm),
    // since `decode --json` has no direct per-element timing field.
    let wpm = report["wpm"].as_f64().unwrap();
    let dit_ms = 1200.0 / wpm;
    assert!(
        (dit_ms - 40.0).abs() <= 4.0,
        "measured dit {dit_ms:.2} ms (from wpm {wpm}) outside +/-10% of 40 ms"
    );
}

#[test]
#[ignore = "stage-2 gate, un-ignored by Task 12"]
fn vr4_hard_edges_passes_end_to_end() {
    let spec = manta_testkit::vectors::vr4();
    let (report, manifest) = decode_report(&spec);
    let decoded = report["text"].as_str().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.10,
        "VR4 char accuracy must be >= 90% (CER <= 0.10), got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );
}

#[test]
#[ignore = "stage-2 gate, un-ignored by Task 12"]
fn vr5_co_channel_passes_end_to_end() {
    let spec = manta_testkit::vectors::vr5();
    let (report, manifest) = decode_report(&spec);
    let tracks = per_track(&report);

    // Signal A (30 WPM, +25 dB, W1AA) is `manifest.keyed_texts[0]` /
    // `manifest.expected_freqs_hz[0]` -- SPEC v2 §8.2 only gates signal A.
    let decoded_a = track_nearest_freq(&tracks, manifest.expected_freqs_hz[0]);
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded_a);
    assert!(
        cer <= 0.10,
        "VR5 signal A char accuracy must be >= 90% (CER <= 0.10), got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded_a
    );

    // 0 bogus callsigns: every spotted call must be one of the two known
    // co-channel signals' callsigns (W1AA, W2BB).
    let known_calls: HashSet<&str> = manifest
        .keyed_texts
        .iter()
        .map(|t| call_from_keyed_text(t))
        .collect();
    let spots = spotted_calls(&report);
    let bogus: Vec<&str> = spots
        .iter()
        .map(|(c, _)| c.as_str())
        .filter(|c| !known_calls.contains(c))
        .collect();
    assert!(
        bogus.is_empty(),
        "VR5 must spot 0 bogus callsigns, got {bogus:?}"
    );
}

/// Shared VR6 assertion: char >= 95% at the given dah:dit weight.
fn assert_vr6_passes(spec: &manta_testkit::vectors::VectorSpec) {
    let (report, manifest) = decode_report(spec);
    let decoded = report["text"].as_str().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.05,
        "{} char accuracy must be >= 95% (CER <= 0.05), got CER {cer}\nexpected: {}\ndecoded:  {}",
        spec.name,
        manifest.keyed_texts[0],
        decoded
    );
}

// Measured passing (2026-09-09, stage-2 gate) -- see
// docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md.
#[test]
fn vr6a_weighting_2_6_passes_end_to_end() {
    assert_vr6_passes(&manta_testkit::vectors::vr6a());
}

// Measured passing (2026-09-09, stage-2 gate) -- see
// docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md.
#[test]
fn vr6b_weighting_3_4_passes_end_to_end() {
    assert_vr6_passes(&manta_testkit::vectors::vr6b());
}

// Measured passing (2026-09-09, stage-2 gate) -- see
// docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md.
#[test]
fn vr7_tight_spacing_passes_end_to_end() {
    let spec = manta_testkit::vectors::vr7();
    let (report, manifest) = decode_report(&spec);
    let decoded = report["text"].as_str().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.05,
        "VR7 char accuracy must be >= 95% (CER <= 0.05), got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );
    let wb = word_boundary_fraction(&manifest.keyed_texts[0], decoded);
    assert!(wb >= 0.90, "VR7 word boundaries must be >= 90%, got {wb}");
}

#[test]
#[ignore = "stage-2 gate, un-ignored by Task 12"]
fn vr8_qsb_fast_passes_end_to_end() {
    let spec = manta_testkit::vectors::vr8();
    let (report, manifest) = decode_report(&spec);
    let decoded = report["text"].as_str().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.15,
        "VR8 char accuracy must be >= 85% (CER <= 0.15), got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );
}
