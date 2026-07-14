//! SPEC §7 V1 golden gate: 120 s, 20 WPM, +20 dB, offset +12.34 kHz, W1AW.
//! Pass criteria: char accuracy = 100 %; 1 track; freq error <= 10 Hz.

use std::process::Command;

#[test]
fn v1_passes_end_to_end_from_wav() {
    let dir = tempfile::tempdir().unwrap();
    let spec = skimmer_testkit::vectors::v1();
    let manifest = skimmer_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_skimmer"))
        .args(["decode", "--json"])
        .arg(dir.path().join("v1.wav"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();

    // char accuracy = 100 %
    let decoded = report["text"].as_str().unwrap();
    assert_eq!(
        skimmer_testkit::cer::cer(&manifest.keyed_texts[0], decoded),
        0.0,
        "V1 char accuracy must be 100 %\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );

    // freq error <= 10 Hz
    let freq = report["freq_hz"].as_f64().unwrap();
    assert!(
        (freq - manifest.expected_freq_hz).abs() <= 10.0,
        "freq {} expected {} (err {})",
        freq,
        manifest.expected_freq_hz,
        (freq - manifest.expected_freq_hz).abs()
    );

    // 1 track: every event carries track_id 1 (single hardwired channel).
    for ev in report["events"].as_array().unwrap() {
        assert_eq!(ev["track_id"].as_u64(), Some(1));
    }

    // WPM sanity (V1 is 20 WPM; SPEC only gates WPM at V2 but it's free here).
    let wpm = report["wpm"].as_f64().unwrap();
    assert!((wpm - 20.0).abs() < 2.0, "wpm {wpm}");
}
