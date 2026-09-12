//! MAN-8 Phase 6 scratch harness: V6 CER across noise_seed+1..=5.
//! TEMPORARY -- delete before merge (plan Phase 6: "the frozen-seed gate is
//! the permanent artifact"). Not wired into any CI gate.

use std::process::Command;

fn decode_report(spec: &manta_testkit::vectors::VectorSpec) -> (serde_json::Value, manta_testkit::vectors::Manifest) {
    let dir = tempfile::tempdir().unwrap();
    let manifest = manta_testkit::vectors::write_fixture_set(spec, dir.path()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_manta"))
        .args(["decode", "--json"])
        .arg(dir.path().join(format!("{}.wav", spec.name)))
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    (serde_json::from_slice(&out.stdout).unwrap(), manifest)
}

#[test]
#[ignore]
fn v6_five_seed_robustness_sweep() {
    let base = manta_testkit::vectors::v6();
    for delta in 1..=5u64 {
        let spec = manta_testkit::vectors::VectorSpec {
            noise_seed: base.noise_seed + delta,
            ..base.clone()
        };
        let (report, manifest) = decode_report(&spec);
        let decoded = report["text"].as_str().unwrap();
        let cer = manta_testkit::cer::cer(&manifest.keyed_texts[0], decoded);
        println!("seed+{delta}: CER {cer:.6}");
    }
}
