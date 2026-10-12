//! MAN-315 golden gates VW1-VW5 (SPEC-decode-core.md §7.2): senders who
//! do not key ideal 1:3 / 3 / 7 timing -- heavy dashes, long letter and
//! word gaps, a bug key, a heavy hand key, and a keyboard sender beside a
//! heavy sender on an adjacent channel. Run on the default `legacy`
//! engine, whose per-track `SenderTiming` they exercise; each test also
//! checks the learned timing a track reports in its `TimingUpdate` events.
//! See docs/DECISIONS/2026-10-11-man315-sender-timing.md.

use std::collections::BTreeMap;
use std::process::Command;

fn decode_report(
    spec: &manta_testkit::vectors::VectorSpec,
) -> (serde_json::Value, manta_testkit::vectors::Manifest) {
    let dir = tempfile::tempdir().unwrap();
    let manifest = manta_testkit::vectors::write_fixture_set(spec, dir.path()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_manta"))
        .args(["decode", "--json", "--engine", "legacy"])
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

/// Group `report["events"]` by `track_id`: each track's decoded text and
/// its last-reported TrackMeta freq_hz. Same shape as golden_vr.rs's
/// helper of the same name, keyed by track id.
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

/// The id of the decoded track whose last-reported freq_hz is closest to
/// `expected_freq`, with that distance in Hz.
fn track_nearest_freq(
    tracks: &BTreeMap<u64, (String, Option<f64>)>,
    expected_freq: f64,
) -> (u64, f64) {
    tracks
        .iter()
        .map(|(&tid, (_, f))| (tid, (f.unwrap_or(f64::MAX) - expected_freq).abs()))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .unwrap()
}

/// The track's last `TimingUpdate` event (MAN-315), as JSON.
fn last_timing(report: &serde_json::Value, track_id: u64) -> serde_json::Value {
    report["events"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|ev| ev["event"] == "TimingUpdate" && ev["track_id"].as_u64() == Some(track_id))
        .unwrap_or_else(|| panic!("track {track_id} emitted no TimingUpdate"))
        .clone()
}

/// `timing[class].mean_dits`.
fn mean_dits(timing: &serde_json::Value, class: &str) -> f64 {
    timing[class]["mean_dits"]
        .as_f64()
        .unwrap_or_else(|| panic!("no {class} estimate in {timing}"))
}

/// The track id of the primary (lowest-id, decoded) track `report["text"]`
/// comes from.
fn primary_track(report: &serde_json::Value) -> u64 {
    let tracks = per_track(report);
    *tracks
        .iter()
        .find(|(_, (t, _))| !t.is_empty())
        .expect("no decoded track")
        .0
}

/// What a cheapest character-level alignment did with one position in `a`.
/// Copied from golden_vr.rs (see its doc comments for the derivation).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AlignOp {
    Match,
    Sub,
    Del,
}

fn char_alignment_ops(a: &[char], b: &[char]) -> Vec<AlignOp> {
    let (n, m) = (a.len(), b.len());
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in dp.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in dp[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let sub = dp[i - 1][j - 1] + usize::from(a[i - 1] != b[j - 1]);
            dp[i][j] = sub.min(dp[i - 1][j] + 1).min(dp[i][j - 1] + 1);
        }
    }
    let mut ops = vec![AlignOp::Del; n];
    let (mut i, mut j) = (n, m);
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && a[i - 1] == b[j - 1] && dp[i - 1][j - 1] == dp[i][j] {
            ops[i - 1] = AlignOp::Match;
            i -= 1;
            j -= 1;
        } else if i > 0 && j > 0 && dp[i - 1][j - 1] + 1 == dp[i][j] {
            ops[i - 1] = AlignOp::Sub;
            i -= 1;
            j -= 1;
        } else if i > 0 && dp[i - 1][j] + 1 == dp[i][j] {
            ops[i - 1] = AlignOp::Del;
            i -= 1;
        } else {
            j -= 1;
        }
    }
    ops
}

/// Fraction of `expected`'s word boundaries reproduced in `decoded`, the
/// first forgiven when the whole first word is genuinely absent (SPEC
/// §2.1 warm-up). Copied from golden_vr.rs.
fn word_boundary_fraction(expected: &str, decoded: &str) -> f64 {
    let normalize = |s: &str| -> Vec<char> {
        s.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .collect()
    };
    let e = normalize(expected);
    let d = normalize(decoded);
    let boundaries: Vec<usize> = e
        .iter()
        .enumerate()
        .filter(|&(_, &c)| c == ' ')
        .map(|(i, _)| i)
        .collect();
    if boundaries.is_empty() {
        return 1.0;
    }
    let ops = char_alignment_ops(&e, &d);
    let boundary0_genuinely_dropped = ops[..=boundaries[0]].iter().all(|&op| op == AlignOp::Del);
    let correct = boundaries
        .iter()
        .enumerate()
        .filter(|&(idx, &p)| ops[p] == AlignOp::Match || (idx == 0 && boundary0_genuinely_dropped))
        .count();
    correct as f64 / boundaries.len() as f64
}

/// Decode a single-signal VW vector; returns `(report, keyed, decoded,
/// cer, last TimingUpdate of the primary track)`.
fn decode_single(
    spec: &manta_testkit::vectors::VectorSpec,
) -> (serde_json::Value, String, String, f64, serde_json::Value) {
    let (report, manifest) = decode_report(spec);
    let keyed = manifest.keyed_texts[0].clone();
    let decoded = report["text"].as_str().unwrap().to_string();
    let cer = manta_testkit::cer::cer(&keyed, &decoded);
    let timing = last_timing(&report, primary_track(&report));
    eprintln!(
        "{}: CER {cer:.4}\nkeyed:   {keyed}\ndecoded: {decoded}\ntiming: {timing}",
        spec.name
    );
    // `decode --json`'s top-level `timing` is the primary track's last
    // TimingUpdate, without its track_id/sample_ts.
    for key in [
        "dit_ms",
        "dot",
        "dash",
        "element_gap",
        "char_gap",
        "word_gap",
    ] {
        assert_eq!(report["timing"][key], timing[key], "top-level timing.{key}");
    }
    (report, keyed, decoded, cer, timing)
}

/// MAN-315 scenario 1: a 25 WPM sender with 4-dit dashes, on a noise seed
/// where a noise fragment seeds the dit cluster (base decoder, measured:
/// real dits decoded as dashes past the 20th word, CER 0.517; now exact
/// from the 10th word, CER 0.203).
#[test]
fn vw1_heavy_dash_decodes_without_dash_dot_confusion() {
    let (_, _, decoded, cer, timing) = decode_single(&manta_testkit::vectors::vw1());
    let words: Vec<&str> = decoded.split_whitespace().collect();
    let first = words
        .iter()
        .position(|w| *w == "K1ABC")
        .unwrap_or_else(|| panic!("no exact K1ABC in {decoded:?}"));
    assert!(
        first < 10,
        "first exact K1ABC is word {first}, want within the first 10: {decoded:?}"
    );
    // After it, no dash/dot confusion: every word is one keyed word (the
    // last is cut off by the scene's end).
    let settled = &words[first..words.len() - 1];
    assert!(
        settled.iter().all(|w| ["CQ", "TEST", "K1ABC"].contains(w)),
        "decode after the first K1ABC is not exact: {decoded:?}"
    );
    // CER counts the warm-up before the scale heals (base decoder 0.517;
    // measured 0.203 -- see the MAN-315 decision doc).
    assert!(cer <= 0.25, "VW1 CER {cer} > 0.25");
    let dash = mean_dits(&timing, "dash");
    assert!((3.6..=4.4).contains(&dash), "VW1 learned dash {dash}");
}

/// MAN-315 scenario 2: letter gaps 4.5 dits and word gaps 9 do not split
/// words.
#[test]
fn vw2_long_gaps_keep_word_boundaries() {
    let (_, keyed, decoded, cer, timing) = decode_single(&manta_testkit::vectors::vw2());
    assert!(cer <= 0.06, "VW2 CER {cer} > 0.06");
    let wb = word_boundary_fraction(&keyed, &decoded);
    assert!(wb >= 0.95, "VW2 word boundaries {wb} < 0.95");
    let cg = mean_dits(&timing, "char_gap");
    assert!((4.0..=5.0).contains(&cg), "VW2 learned char gap {cg}");
    let wg = mean_dits(&timing, "word_gap");
    assert!((8.0..=10.0).contains(&wg), "VW2 learned word gap {wg}");
}

/// A bug key: short automatic dots, long hand-made dashes.
#[test]
fn vw3_bug_key_decodes() {
    let (_, _, _, cer, timing) = decode_single(&manta_testkit::vectors::vw3());
    assert!(cer <= 0.08, "VW3 CER {cer} > 0.08");
    // Not worse than the base decoder (measured 0.0349, MAN-315 decision doc).
    assert!(
        cer <= 0.035,
        "VW3 CER {cer} worse than the base decoder's 0.0349"
    );
    let dash = mean_dits(&timing, "dash");
    assert!(dash > 4.0, "VW3 learned dash {dash}");
}

/// A heavy hand key: long dots, short element gaps, long letter and word
/// gaps.
#[test]
fn vw4_heavy_hand_key_decodes() {
    let (_, _, _, cer, _) = decode_single(&manta_testkit::vectors::vw4());
    assert!(cer <= 0.08, "VW4 CER {cer} > 0.08");
    // Not worse than the base decoder (measured 0.0375, MAN-315 decision doc).
    assert!(
        cer <= 0.038,
        "VW4 CER {cer} worse than the base decoder's 0.0375"
    );
}

/// MAN-315 scenario 3: two senders three channels apart, one keyboard and
/// one heavy, each keep their own timing estimates, visible per track in
/// `TimingUpdate`.
#[test]
fn vw5_adjacent_senders_keep_their_own_timing() {
    let spec = manta_testkit::vectors::vw5();
    let (report, manifest) = decode_report(&spec);
    let tracks = per_track(&report);
    let with_text: Vec<_> = tracks.iter().filter(|(_, (t, _))| !t.is_empty()).collect();
    assert_eq!(
        with_text.len(),
        2,
        "want exactly two decoded tracks: {tracks:?}"
    );
    let mut ids = Vec::new();
    for (i, (want_dash, want_cg)) in [((2.6, 3.4), (2.5, 3.5)), ((3.6, 4.4), (4.0, 5.0))]
        .into_iter()
        .enumerate()
    {
        let keyed = &manifest.keyed_texts[i];
        let expected_freq = manifest.expected_freqs_hz[i];
        let (tid, df) = track_nearest_freq(&tracks, expected_freq);
        // Half a 93.75 Hz channel: an unambiguous match for signals three
        // channels apart. Both tracks' reported frequency sits ~20 Hz high
        // on this pair (measured 19.3 and 22.2 Hz, base decoder alike), a
        // frequency-estimate matter outside MAN-315's timing scope.
        assert!(
            df <= 46.875,
            "signal {i}: nearest track {tid} is {df} Hz off: {tracks:?}"
        );
        let decoded = &tracks[&tid].0;
        let cer = manta_testkit::cer::cer(keyed, decoded);
        let timing = last_timing(&report, tid);
        eprintln!(
            "vw5 signal {i} (track {tid}): CER {cer:.4}\nkeyed:   {keyed}\ndecoded: {decoded}\ntiming: {timing}"
        );
        assert!(cer <= 0.06, "VW5 signal {i} CER {cer} > 0.06");
        let dash = mean_dits(&timing, "dash");
        assert!(
            (want_dash.0..=want_dash.1).contains(&dash),
            "VW5 signal {i} learned dash {dash}"
        );
        let cg = mean_dits(&timing, "char_gap");
        assert!(
            (want_cg.0..=want_cg.1).contains(&cg),
            "VW5 signal {i} learned char gap {cg}"
        );
        for key in ["dot", "element_gap", "word_gap"] {
            assert!(
                timing[key]["n"].as_u64().unwrap_or(0) > 0,
                "signal {i}: no {key}"
            );
        }
        ids.push(tid);
    }
    assert_ne!(ids[0], ids[1], "both signals matched one track");
}
