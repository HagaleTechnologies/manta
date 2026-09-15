//! SPEC v2 §10 determinism gate: the `Hsmm` engine must produce byte-
//! identical `decode --json` output across repeated runs of the same input.
//! Requires `--engine hsmm` to actually be accepted and to run the real
//! pipeline (Task 11's CLI-level fix lifting the earlier defensive
//! rejection) -- this test is the CI enforcement of that requirement, not
//! just a compile-time exercise.

use sha2::{Digest, Sha256};

#[test]
fn hsmm_three_runs_identical() {
    let dir = tempfile::tempdir().unwrap();
    let spec = manta_testkit::vectors::v8();
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
    let mut hashes = vec![];
    for _ in 0..3 {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_manta"))
            .args(["decode", "--json", "--engine", "hsmm"])
            .arg(dir.path().join("v8.wav"))
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        hashes.push(format!("{:x}", Sha256::digest(&out.stdout)));
    }
    assert!(hashes.iter().all(|h| h == &hashes[0]), "{hashes:?}");
}
