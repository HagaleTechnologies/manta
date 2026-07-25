//! SPEC §7 V8/V8w pileup golden gates. "Callsign validated"/"bogus
//! callsign"/"ghost decode" approximate the future skimmer-spot validator
//! (M3) the same way V5/V6 approximate "callsign validated" today -- see
//! docs/superpowers/specs/2026-07-24-m2-pileup-cpu-budget-design.md.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::process::Command;

fn decode_report(
    spec: &skimmer_testkit::vectors::VectorSpec,
) -> (serde_json::Value, skimmer_testkit::vectors::Manifest) {
    let dir = tempfile::tempdir().unwrap();
    let manifest = skimmer_testkit::vectors::write_fixture_set(spec, dir.path()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_skimmer"))
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
    manifest: &skimmer_testkit::vectors::Manifest,
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

/// Callsign-shaped, >=2-rep tokens in `decoded_text` that are not in
/// `known_calls` -- SPEC §7 V8/V8w's "0 bogus callsigns spotted".
fn bogus_calls(decoded_text: &str, known_calls: &HashSet<&str>) -> Vec<String> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for word in decoded_text.split_whitespace() {
        if (3..=7).contains(&word.len())
            && word.chars().all(|c| c.is_ascii_alphanumeric())
            && word.chars().any(|c| c.is_ascii_digit())
            && word.chars().any(|c| c.is_ascii_alphabetic())
        {
            *counts.entry(word).or_insert(0) += 1;
        }
    }
    counts
        .into_iter()
        .filter(|&(word, n)| n >= 2 && !known_calls.contains(word))
        .map(|(word, _)| word.to_string())
        .collect()
}

#[test]
fn v8_pileup_validates_at_least_45_of_50_with_no_bogus_calls() {
    let spec = skimmer_testkit::vectors::v8();
    let (report, manifest) = decode_report(&spec);
    let tracks = per_track(&report);
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

    let matched = match_tracks_by_freq(&manifest, &tracks);
    let mut validated = 0;
    for (i, keyed_text) in manifest.keyed_texts.iter().enumerate() {
        let call = call_from_keyed_text(keyed_text);
        let (decoded_text, _freq) = matched[i];
        if decoded_text.matches(call).count() >= 2 {
            validated += 1;
        }
    }
    assert!(
        validated >= 45,
        "V8 must validate >= 45/50 callsigns, got {validated}/50"
    );

    let mut bogus = Vec::new();
    for (decoded_text, _freq) in tracks.values() {
        bogus.extend(bogus_calls(decoded_text, &known_calls));
    }
    assert!(
        bogus.is_empty(),
        "V8 must spot 0 bogus callsigns, got {bogus:?}"
    );
}
