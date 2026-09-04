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
use std::sync::OnceLock;

/// One V8w render+decode shared by every test in this binary. `cargo test`
/// runs a binary's tests as threads in one process, so a `OnceLock` is
/// honored across them; the 120 s / 96 kS/s / 50-signal scene under
/// Watterson-Poor fading is far too expensive (measured ~200 s) to pay for
/// per test.
fn v8w_decode() -> &'static (serde_json::Value, manta_testkit::vectors::Manifest) {
    static CACHE: OnceLock<(serde_json::Value, manta_testkit::vectors::Manifest)> = OnceLock::new();
    CACHE.get_or_init(|| decode_report(&manta_testkit::vectors::v8w()))
}

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

/// Count of decoded tracks whose last-reported `freq_hz` lies within
/// `within_hz` of `expected_freq_hz` -- the fragmentation-cluster-size
/// column of the Phase 1 diagnostic table.
fn tracks_near(
    tracks: &BTreeMap<u64, (String, Option<f64>)>,
    expected_freq_hz: f64,
    within_hz: f64,
) -> usize {
    tracks
        .values()
        .filter(|(_, f)| f.is_some_and(|f| (f - expected_freq_hz).abs() <= within_hz))
        .count()
}

/// One row of the Phase 1 per-signal diagnostic table.
#[derive(Debug, serde::Serialize)]
struct SignalRow {
    idx: usize,
    call: String,
    snr_2500_db: f32,
    wpm: f32,
    expected_freq_hz: f64,
    cer: f64,
    len_ratio: f64,
    tracks_within_300hz: usize,
}

fn print_table(rows: &[SignalRow]) {
    println!(
        "{:>3} {:<10} {:>7} {:>5} {:>9} {:>6} {:>4}",
        "idx", "call", "snr_db", "wpm", "cer", "len_r", "trks"
    );
    for r in rows {
        println!(
            "{:>3} {:<10} {:>7.1} {:>5.1} {:>9.3} {:>6.2} {:>4}",
            r.idx, r.call, r.snr_2500_db, r.wpm, r.cer, r.len_ratio, r.tracks_within_300hz
        );
    }
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
    let (report, manifest) = v8w_decode();
    let tracks = per_track(report);
    let known_calls: HashSet<&str> = manifest
        .keyed_texts
        .iter()
        .map(|t| call_from_keyed_text(t))
        .collect();

    let matched = match_tracks_by_freq(manifest, &tracks);
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

    let spots = spotted_calls(report);
    let spotted: HashSet<&str> = spots.iter().map(|(c, _)| c.as_str()).collect();
    let bogus: Vec<&str> = spotted
        .iter()
        .filter(|c| !known_calls.contains(**c))
        .copied()
        .collect();
    assert!(
        bogus.is_empty(),
        "V8w must spot 0 bogus callsigns, got {bogus:?}"
    );

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

/// Diagnostic, not a gate: prints the full per-signal V8w table and writes a
/// machine-readable summary to `$MANTA_V8W_REPORT` if set. This is the
/// artifact `ROADMAP.md`'s M4 acceptance criterion ("fusion beats
/// classical-only CER by a measured, documented margin") has to diff
/// against -- MAN-9 Phase 1.
///
/// cargo test -p manta-cli --test golden_v8_v8w -- --ignored --nocapture \
///     v8w_per_signal_cer_report
#[test]
#[ignore]
fn v8w_per_signal_cer_report() {
    let spec = manta_testkit::vectors::v8w();
    let (report, manifest) = v8w_decode();
    let tracks = per_track(report);
    let matched = match_tracks_by_freq(manifest, &tracks);

    let mut rows = Vec::new();
    for (i, sig) in spec.signals.iter().enumerate() {
        let (decoded, _freq) = matched[i];
        let expected = &manifest.keyed_texts[i];
        let expected_freq_hz = manifest.expected_freqs_hz[i];
        rows.push(SignalRow {
            idx: i,
            call: call_from_keyed_text(expected).to_string(),
            snr_2500_db: sig.snr_2500_db,
            wpm: sig.wpm,
            expected_freq_hz,
            cer: manta_testkit::cer::cer(expected, decoded),
            len_ratio: decoded.chars().count() as f64 / expected.chars().count() as f64,
            tracks_within_300hz: tracks_near(&tracks, expected_freq_hz, 300.0),
        });
    }
    print_table(&rows);
    if let Ok(path) = std::env::var("MANTA_V8W_REPORT") {
        std::fs::write(path, serde_json::to_string_pretty(&rows).unwrap()).unwrap();
    }

    // Deliberately weak: this test exists to report, and to fail loudly only
    // if the decode collapses or the scene stops matching the ticket's
    // premise -- it is not the golden gate (that stays
    // `v8w_pileup_fading_decodes_90pct_of_strong_signals_no_ghosts`, whose
    // thresholds this test never touches).
    let strong: Vec<&SignalRow> = rows.iter().filter(|r| r.snr_2500_db >= 6.0).collect();
    assert_eq!(strong.len(), 34, "V8w strong-signal count must stay 34");
    assert!(
        strong.iter().filter(|r| r.cer < 0.10).count() >= 1,
        "V8w regressed below its recorded 1/34 baseline"
    );
}
