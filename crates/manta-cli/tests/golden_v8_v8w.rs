//! SPEC §7 V8/V8w pileup golden gates. "Callsign validated"/"bogus
//! callsign"/"ghost decode" are measured against the real
//! `manta-spot::Validator`'s output (`report["spots"]`), wired into
//! `decode_samples` in M3's engine-wiring sub-project -- see
//! docs/superpowers/specs/2026-07-26-m3-engine-wiring-design.md. Previously
//! approximated with text-substring heuristics against raw decoder text,
//! the same way V5/V6 approximated "callsign validated" before this landed
//! -- see docs/superpowers/specs/2026-07-24-m2-pileup-cpu-budget-design.md.

use std::collections::{BTreeMap, HashSet};
use std::process::Command;

fn decode_report(
    spec: &manta_testkit::vectors::VectorSpec,
) -> (serde_json::Value, manta_testkit::vectors::Manifest) {
    let dir = tempfile::tempdir().unwrap();
    let manifest = manta_testkit::vectors::write_fixture_set(spec, dir.path()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_manta"))
        .args(["decode", "--json"])
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
/// golden_v7_v9_v10.rs's helper of the same name.
fn per_track(report: &serde_json::Value) -> BTreeMap<u64, (String, Option<f64>)> {
    let mut texts: BTreeMap<u64, String> = BTreeMap::new();
    let mut freqs: BTreeMap<u64, f64> = BTreeMap::new();
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

/// Extract the callsign from this project's SPEC §7 payload template
/// "CQ CQ DE <CALL> <CALL> K" (word index 3).
fn call_from_keyed_text(text: &str) -> &str {
    text.split_whitespace()
        .nth(3)
        .expect("keyed text must follow the 'CQ CQ DE <CALL> <CALL> K' template")
}

/// For each expected signal (`manifest.expected_freqs_hz` order), the
/// decoded track whose last-reported freq_hz is closest to that signal's
/// expected absolute frequency.
fn match_tracks_by_freq<'a>(
    manifest: &manta_testkit::vectors::Manifest,
    tracks: &'a BTreeMap<u64, (String, Option<f64>)>,
) -> Vec<(&'a str, Option<f64>)> {
    manifest
        .expected_freqs_hz
        .iter()
        .map(|&expected_freq| {
            tracks
                .values()
                .min_by(|(_, fa), (_, fb)| {
                    let da = (fa.unwrap_or(f64::MAX) - expected_freq).abs();
                    let db = (fb.unwrap_or(f64::MAX) - expected_freq).abs();
                    da.partial_cmp(&db).unwrap()
                })
                .map(|(text, freq)| (text.as_str(), *freq))
                .unwrap()
        })
        .collect()
}

/// `(callsign, track_id)` pairs from `report["spots"]` -- the real
/// `manta-spot::Validator`'s output, not a text-heuristic approximation.
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

#[test]
fn v8_pileup_validates_at_least_45_of_50_with_no_bogus_calls() {
    let spec = manta_testkit::vectors::v8();
    let (report, manifest) = decode_report(&spec);
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

/// Anti-vacuity floor for `v8w_pileup_fading_spots_no_bogus_callsigns`: a
/// change that simply stopped emitting spots would satisfy "0 bogus" while
/// destroying the skimmer. Measured baseline at e398d46 is 22/50 genuine
/// calls validated; MAN-100's arbitration fix measures 20/50. 15 leaves
/// room for a legitimate precision/recall trade while still catching a
/// collapse. This is NOT a SPEC criterion -- SPEC §7's V8w row states no
/// recall bar -- it is a guard on this test's own meaning.
const MIN_V8W_VALIDATED: usize = 15;

/// SPEC §7's V8w row states three independent pass criteria; this is the
/// second, "0 bogus callsigns". It is deliberately a test of its own, NOT
/// part of `v8w_pileup_fading_decodes_90pct_of_strong_signals_no_ghosts`
/// below: that test is `#[ignore]`d for the unrelated CER-accuracy gap
/// (issue #28 / MAN-107..MAN-113), and its CER `assert!` panics before the
/// bogus-call check is ever reached, so bundling them made this criterion
/// unrunnable in CI. Split out per MAN-101.
#[test]
fn v8w_pileup_fading_spots_no_bogus_callsigns() {
    let spec = manta_testkit::vectors::v8w();
    let (report, manifest) = decode_report(&spec);
    let known_calls: HashSet<&str> = manifest
        .keyed_texts
        .iter()
        .map(|t| call_from_keyed_text(t))
        .collect();
    assert_eq!(
        known_calls.len(),
        50,
        "V8w fixture must have 50 unique callsigns"
    );

    let spots = spotted_calls(&report);
    let spotted: HashSet<&str> = spots.iter().map(|(c, _)| c.as_str()).collect();
    let mut bogus: Vec<&str> = spotted
        .iter()
        .filter(|c| !known_calls.contains(**c))
        .copied()
        .collect();
    bogus.sort_unstable();
    let validated = known_calls.iter().filter(|c| spotted.contains(**c)).count();

    // Both conditions are evaluated and reported together, deliberately: two
    // sequential `assert!`s would make the second one unreachable whenever the
    // first fails -- exactly the defect MAN-101 exists to fix one level up.
    let mut failures: Vec<String> = Vec::new();
    if !bogus.is_empty() {
        failures.push(format!(
            "SPEC §7 V8w requires 0 bogus callsigns; got {} of {} distinct spotted: {bogus:?}",
            bogus.len(),
            spotted.len()
        ));
    }
    if validated < MIN_V8W_VALIDATED {
        failures.push(format!(
            "only {validated}/50 genuine callsigns validated (floor {MIN_V8W_VALIDATED}); \
             0 bogus is vacuous if recall has collapsed"
        ));
    }
    assert!(failures.is_empty(), "V8w: {}", failures.join(" | "));
}

/// Ignored: measured 1/34 (2.9%) of the >= +6 dB strong signals at
/// CER < 0.10 (need >= 90%, i.e. >= 31/34) -- sorted CERs across the 34:
/// 0.094 (the only pass), 0.108, 0.144, 0.154, 0.181, 0.187, 0.187, 0.199,
/// 0.200, 0.205, 0.207, 0.226, 0.232, 0.235, 0.244, 0.271, 0.275, 0.276,
/// 0.286, 0.298, 0.308, 0.327, 0.337, 0.373, 0.384, 0.393, 0.441, 0.521,
/// 0.526, 0.654, 0.686, 0.784, 0.844, 0.918 -- median ~0.276, ~2.76x the gate.
///
/// Ruled out as a harness/matching artifact two ways. First, the sibling
/// AWGN-only V8 test (identical 50-signal scene: same offsets/WPM/SNR/
/// jitter, same `match_tracks_by_freq` matching) passes 49/50 validated,
/// 0 bogus -- fading is the only variable that changed. Second, within V8w
/// itself, checked whether CER was inflated by the harness matching only a
/// fragment of a fading-split track (a deep fade causing drop/reacquire as
/// a new track_id, so `match_tracks_by_freq`'s nearest-single-track pick
/// captures less than the full transmission): for 31/34 strong signals,
/// `decoded_text.len() / expected_text.len()` is 1.0-1.3 (full-duration
/// capture, exactly one track within 300 Hz of the expected frequency) yet
/// CER still fails, ruling out fragmentation as the primary driver. Only
/// 3/34 signals (AC3AGO-band idx 25, W7QLO idx 41, W8SHR idx 44) show that
/// fragmentation pattern (len_ratio 0.09-0.22, 5-15 tracks within 300 Hz of
/// the expected frequency) -- a secondary, QSB-driven track-continuity
/// symptom (same family as issue #26), not the majority cause.
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
    let spec = manta_testkit::vectors::v8w();
    let (report, manifest) = decode_report(&spec);
    let tracks = per_track(&report);
    let known_calls: HashSet<&str> = manifest
        .keyed_texts
        .iter()
        .map(|t| call_from_keyed_text(t))
        .collect();

    let matched = match_tracks_by_freq(&manifest, &tracks);
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
        let (decoded_text, _freq) = matched[i];
        let cer = manta_testkit::cer::cer(&manifest.keyed_texts[i], decoded_text);
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

fn decode_report_hsmm(
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

#[test]
#[ignore = "stage-2 gate, un-ignored by Task 12 only if measured passing"]
fn v8_pileup_validates_at_least_45_of_50_with_hsmm_engine() {
    let spec = manta_testkit::vectors::v8();
    let (report, manifest) = decode_report_hsmm(&spec);
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

#[test]
#[ignore = "stage-2 gate, un-ignored by Task 12 only if measured passing"]
fn v8w_pileup_fading_decodes_90pct_of_strong_signals_with_hsmm_engine() {
    let spec = manta_testkit::vectors::v8w();
    let (report, manifest) = decode_report_hsmm(&spec);
    let tracks = per_track(&report);
    let known_calls: HashSet<&str> = manifest
        .keyed_texts
        .iter()
        .map(|t| call_from_keyed_text(t))
        .collect();

    let matched = match_tracks_by_freq(&manifest, &tracks);
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
        let (decoded_text, _freq) = matched[i];
        let cer = manta_testkit::cer::cer(&manifest.keyed_texts[i], decoded_text);
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
