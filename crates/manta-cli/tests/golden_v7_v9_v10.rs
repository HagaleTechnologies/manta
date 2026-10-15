//! SPEC §7 V7/V9/V10 golden gates (M2 sub-project 2: real multi-track
//! detector). V10 is added in Task 10; V51 (MAN-210) is V9's scene
//! decoded with the narrowband refiner on.

use std::collections::{BTreeMap, BTreeSet};
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
/// text and its last-reported TrackMeta freq_hz.
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

/// The distinct `track_id`s of `report["events"]`'s `TrackPromoted`
/// events: every promoted track, including one merged or closed before it
/// decoded a character, which `per_track` never sees.
fn promoted_track_ids(report: &serde_json::Value) -> BTreeSet<u64> {
    report["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|ev| ev["event"] == "TrackPromoted")
        .map(|ev| ev["track_id"].as_u64().unwrap())
        .collect()
}

// Codex review, PR #303: a track promoted and then merged or closed
// before decoding a character is invisible to `per_track`, so V51's
// no-split gate also counts `promoted_track_ids`.
#[test]
fn promoted_track_ids_counts_a_track_that_decoded_nothing() {
    let report = serde_json::json!({ "events": [
        { "event": "TrackPromoted", "track_id": 1 },
        { "event": "TrackPromoted", "track_id": 2 },
        { "event": "CharDecoded", "track_id": 1, "glyph": { "Char": "E" } },
        { "event": "TrackClosed", "track_id": 2 },
    ] });
    assert_eq!(per_track(&report).len(), 1);
    assert_eq!(promoted_track_ids(&report), BTreeSet::from([1, 2]));
}

#[test]
fn v7_passes_end_to_end_from_wav() {
    let spec = manta_testkit::vectors::v7();
    let (report, manifest) = decode_report(&spec);
    let tracks = per_track(&report);
    assert_eq!(
        tracks.len(),
        2,
        "V7 must produce exactly 2 tracks, got {}",
        tracks.len()
    );

    for (i, expected_text) in manifest.keyed_texts.iter().enumerate() {
        let expected_freq = manifest.expected_freqs_hz[i];
        // Match each expected signal to whichever decoded track's freq is closest.
        let (_, (decoded_text, freq)) = tracks
            .iter()
            .min_by(|(_, (_, fa)), (_, (_, fb))| {
                let da = (fa.unwrap_or(f64::MAX) - expected_freq).abs();
                let db = (fb.unwrap_or(f64::MAX) - expected_freq).abs();
                da.partial_cmp(&db).unwrap()
            })
            .unwrap();
        let cer = manta_testkit::cer::cer(expected_text, decoded_text);
        assert!(
            cer <= 0.05,
            "signal {i} ({expected_text:?}) char accuracy must be >= 95%, got CER {cer} (decoded {decoded_text:?})"
        );
        let freq = freq.expect("TrackMeta freq_hz must have fired at least once in a 120s scene");
        assert!(
            (freq - expected_freq).abs() <= 15.0,
            "signal {i} freq {} expected {} (err {})",
            freq,
            expected_freq,
            (freq - expected_freq).abs()
        );
    }
}

#[test]
fn v9_passes_end_to_end_from_wav() {
    let spec = manta_testkit::vectors::v9();
    let dir = tempfile::tempdir().unwrap();
    let manifest = manta_testkit::vectors::write_v9_fixture_set(&spec, dir.path()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_manta"))
        .args(["decode", "--json"])
        .arg(dir.path().join("v9.wav"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();

    let tracks = per_track(&report);
    assert_eq!(
        tracks.len(),
        1,
        "V9 must not split into multiple tracks under drift"
    );
    let (_, (decoded_text, freq)) = tracks.iter().next().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded_text);
    assert!(
        cer <= 0.10,
        "V9 char accuracy must be >= 90%, got CER {cer}"
    );
    let freq = freq.expect("TrackMeta freq_hz must have fired at least once");
    assert!(
        (freq - manifest.expected_freq_hz).abs() <= 15.0,
        "final freq {} expected {} (err {})",
        freq,
        manifest.expected_freq_hz,
        (freq - manifest.expected_freq_hz).abs()
    );
}

#[test]
fn v10_passes_end_to_end_from_wav() {
    let spec = manta_testkit::vectors::v10();
    let (report, manifest) = decode_report(&spec);
    let decoded = report["text"].as_str().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.05,
        "V10 char accuracy must be >= 95%, got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );
    // Word boundaries 100% correct in steady state. `GapClassifier`'s
    // Farnsworth-adaptive char/word threshold (manta-decode::timing)
    // shares its 5-sample bootstrap with mark-speed clustering (not
    // Farnsworth-specific) -- until that bootstrap completes, a handful of
    // early inter-character gaps can be misclassified as word boundaries
    // on any real Farnsworth signal. This is a documented warmup floor
    // (M2 sub-project 2 close-out pins), not a bug: `FARNS_MIN_COUNT` is
    // already at its practical floor (5) per timing.rs's doc comment.
    // Tolerance sized from the measured V10 bootstrap window (3 extra
    // splits) plus headroom; word count must not drift further than this
    // once steady state is reached.
    const FARNSWORTH_BOOTSTRAP_WORD_TOLERANCE: usize = 4;
    let expected_words = manifest.keyed_texts[0].split(' ').count();
    let decoded_words = decoded.split(' ').filter(|w| !w.is_empty()).count();
    assert!(
        decoded_words >= expected_words
            && decoded_words <= expected_words + FARNSWORTH_BOOTSTRAP_WORD_TOLERANCE,
        "word boundary count outside the documented Farnsworth-bootstrap tolerance: expected {expected_words} (+0..={FARNSWORTH_BOOTSTRAP_WORD_TOLERANCE}), decoded {decoded_words}\nexpected: {:?}\ndecoded:  {decoded:?}",
        manifest.keyed_texts[0]
    );
}

/// Writes `body` to `dir/name` and returns that path (`cli.rs`'s
/// `write_cfg`, copied per test binary: `tests/common/mod.rs` is for
/// signal/daemon plumbing).
fn write_cfg(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    path
}

// MAN-210: the narrowband refiner (`refine_bw_hz > 0`) end-to-end.
// Same scene as V9 -- its +50 Hz/min drift crosses the 93.75 Hz
// channel grid mid-scene, firing `decoder_input`'s reset-drain branch
// (MAN-194) through the real channelizer, and every decode ends in
// `TrackManager::finish`'s end-of-stream drain. Refine-on must hold
// V9's own bars and must measurably change the pipeline's output.
#[test]
fn v51_drift_with_refiner_enabled_passes_end_to_end() {
    let spec = manta_testkit::vectors::v9();
    let dir = tempfile::tempdir().unwrap();
    let manifest = manta_testkit::vectors::write_v9_fixture_set(&spec, dir.path()).unwrap();
    let wav = dir.path().join("v9.wav");
    let control_cfg = write_cfg(dir.path(), "refine0.toml", "[decode]\nrefine_bw_hz = 0.0\n");
    let cfg = write_cfg(
        dir.path(),
        "refine30.toml",
        "[decode]\nrefine_bw_hz = 30.0\n",
    );

    let run = |extra: &[&str]| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
        cmd.args(["decode", "--json"]);
        for a in extra {
            cmd.arg(a);
        }
        let out = cmd.arg(&wav).output().unwrap();
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    };

    // Codex review, PR #303: the control is pinned to refinement off by an
    // explicit `refine_bw_hz = 0.0`, not by the key's default -- a future
    // default-on change would otherwise make both runs identical (the
    // decode is deterministic) and fail the assert below for exactly the
    // behavior it exists to green-light.
    let control = run(&["--config", control_cfg.to_str().unwrap()]);
    let refined = run(&["--config", cfg.to_str().unwrap()]);

    // Non-vacuity: decode is deterministic on file input, so if the
    // config were silently ignored the two reports would be identical.
    // (Probe-measured 2026-10-14: events 362 vs 363, reports differ.)
    // This differential proves the config reached the pipeline, not that
    // the MAN-194 drain protocols ran: ordinary 30 Hz filtering already
    // differs from the control, and a dropped `refiner_backlog` at a
    // channel reset or at EOF is too small to leave V51's other bars
    // (Codex review, PR #303). The drains are pinned on this same scene
    // one layer down, where they are observable:
    // `real_drift_scene_drains_refiner_backlog_at_resets_and_eof`
    // (crates/manta-engine/src/track.rs).
    assert_ne!(
        control, refined,
        "refine_bw_hz = 30.0 must reach the pipeline; identical reports \
         mean the config was dropped"
    );

    let promoted = promoted_track_ids(&refined);
    assert_eq!(
        promoted.len(),
        1,
        "V51 must not split into multiple tracks under drift with the refiner on: promoted {promoted:?}"
    );
    let tracks = per_track(&refined);
    assert_eq!(tracks.len(), 1, "V51's one promoted track must decode text");
    let (_, (decoded_text, freq)) = tracks.iter().next().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded_text);
    assert!(
        cer <= 0.10,
        "V51 char accuracy must be >= 90%, got CER {cer}"
    );
    let freq = freq.expect("TrackMeta freq_hz must have fired at least once");
    assert!(
        (freq - manifest.expected_freq_hz).abs() <= 15.0,
        "V51 final freq {freq} expected {} (err {})",
        manifest.expected_freq_hz,
        (freq - manifest.expected_freq_hz).abs()
    );
}

// --- SPEC v2 §8.4 Task 12: `--engine hsmm` copies of V7/V9/V10, at the
// same SPEC §7 bars as their legacy counterparts above. Measured
// 2026-09-09 -- see
// docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md.

// Passes since MAN-189 (was CER 0.0540 from a duplicated B, "N2BBB"); see
// docs/DECISIONS/2026-10-13-man189-hsmm-commit-frontier.md.
#[test]
fn v7_passes_end_to_end_with_hsmm_engine() {
    let spec = manta_testkit::vectors::v7();
    let dir = tempfile::tempdir().unwrap();
    let manifest = manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
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
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let tracks = per_track(&report);
    assert_eq!(
        tracks.len(),
        2,
        "V7/hsmm must produce exactly 2 tracks, got {}",
        tracks.len()
    );
    for (i, expected_text) in manifest.keyed_texts.iter().enumerate() {
        let expected_freq = manifest.expected_freqs_hz[i];
        let (_, (decoded_text, freq)) = tracks
            .iter()
            .min_by(|(_, (_, fa)), (_, (_, fb))| {
                let da = (fa.unwrap_or(f64::MAX) - expected_freq).abs();
                let db = (fb.unwrap_or(f64::MAX) - expected_freq).abs();
                da.partial_cmp(&db).unwrap()
            })
            .unwrap();
        let cer = manta_testkit::cer::cer(expected_text, decoded_text);
        assert!(
            cer <= 0.05,
            "signal {i} ({expected_text:?}) char accuracy must be >= 95%, got CER {cer} (decoded {decoded_text:?})"
        );
        let freq = freq.expect("TrackMeta freq_hz must have fired at least once in a 120s scene");
        assert!(
            (freq - expected_freq).abs() <= 15.0,
            "signal {i} freq {} expected {} (err {})",
            freq,
            expected_freq,
            (freq - expected_freq).abs()
        );
    }
}

// Measured passing (2026-09-09, stage-2 gate) -- see
// docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md.
#[test]
fn v9_passes_end_to_end_with_hsmm_engine() {
    let spec = manta_testkit::vectors::v9();
    let dir = tempfile::tempdir().unwrap();
    let manifest = manta_testkit::vectors::write_v9_fixture_set(&spec, dir.path()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_manta"))
        .args(["decode", "--json", "--engine", "hsmm"])
        .arg(dir.path().join("v9.wav"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let tracks = per_track(&report);
    assert_eq!(
        tracks.len(),
        1,
        "V9/hsmm must not split into multiple tracks under drift"
    );
    let (_, (decoded_text, freq)) = tracks.iter().next().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded_text);
    assert!(
        cer <= 0.10,
        "V9/hsmm char accuracy must be >= 90%, got CER {cer}"
    );
    let freq = freq.expect("TrackMeta freq_hz must have fired at least once");
    assert!(
        (freq - manifest.expected_freq_hz).abs() <= 15.0,
        "final freq {} expected {} (err {})",
        freq,
        manifest.expected_freq_hz,
        (freq - manifest.expected_freq_hz).abs()
    );
}

#[test]
#[ignore = "stage-2 gate, un-ignored by Task 12 only if measured passing"]
fn v10_passes_end_to_end_with_hsmm_engine() {
    let spec = manta_testkit::vectors::v10();
    let dir = tempfile::tempdir().unwrap();
    let manifest = manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
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
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let decoded = report["text"].as_str().unwrap();
    let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.05,
        "V10/hsmm char accuracy must be >= 95%, got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );
    const FARNSWORTH_BOOTSTRAP_WORD_TOLERANCE: usize = 4;
    let expected_words = manifest.keyed_texts[0].split(' ').count();
    let decoded_words = decoded.split(' ').filter(|w| !w.is_empty()).count();
    assert!(
        decoded_words >= expected_words
            && decoded_words <= expected_words + FARNSWORTH_BOOTSTRAP_WORD_TOLERANCE,
        "word boundary count outside the documented Farnsworth-bootstrap tolerance: expected {expected_words} (+0..={FARNSWORTH_BOOTSTRAP_WORD_TOLERANCE}), decoded {decoded_words}\nexpected: {:?}\ndecoded:  {decoded:?}",
        manifest.keyed_texts[0]
    );
}
