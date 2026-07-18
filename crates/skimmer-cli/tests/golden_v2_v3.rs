//! SPEC §7 V2/V3 golden gates.
//! V2 "fast-35": char accuracy >= 99 %; WPM reported 35 +/- 2.
//! V3 "slow-weak": char accuracy >= 95 %.

use std::process::Command;

fn decode_report(spec: &skimmer_testkit::vectors::VectorSpec) -> (serde_json::Value, skimmer_testkit::vectors::Manifest) {
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

#[test]
fn v2_passes_end_to_end_from_wav() {
    let spec = skimmer_testkit::vectors::v2();
    let (report, manifest) = decode_report(&spec);
    let decoded = report["text"].as_str().unwrap();
    let cer = skimmer_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
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
    let spec = skimmer_testkit::vectors::v3();
    let (report, manifest) = decode_report(&spec);
    let decoded = report["text"].as_str().unwrap();
    let cer = skimmer_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
    assert!(
        cer <= 0.05,
        "V3 char accuracy must be >= 95 % (CER <= 0.05), got CER {cer}\nexpected: {}\ndecoded:  {}",
        manifest.keyed_texts[0],
        decoded
    );
}
