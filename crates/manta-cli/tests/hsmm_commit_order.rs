//! MAN-189: the HSMM engine's emitted characters and word boundaries
//! never move backwards in time on a track. On main before the fix, VR6a
//! decoded "TEEST" (E@39819 then E@39818) and V7 decoded "N2BBB".

use std::collections::BTreeMap;
use std::process::Command;

/// Every place a track's `CharDecoded`/`WordBoundary` `sample_ts` is lower
/// than the previous such event's on that track, as `track N: X@ts after
/// Y@ts` (`_` for a word boundary).
fn backwards_commits(spec: &manta_testkit::vectors::VectorSpec) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    manta_testkit::vectors::write_fixture_set(spec, dir.path()).unwrap();
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
    let mut last: BTreeMap<u64, (String, u64)> = BTreeMap::new();
    let mut bad = Vec::new();
    for ev in report["events"].as_array().unwrap() {
        let label = match ev["event"].as_str().unwrap() {
            "CharDecoded" => ev["glyph"].as_str().unwrap().to_string(),
            "WordBoundary" => "_".to_string(),
            _ => continue,
        };
        let tid = ev["trackId"].as_u64().unwrap();
        let ts = ev["sampleTs"].as_u64().unwrap();
        if let Some((prev, prev_ts)) = last.get(&tid) {
            if ts < *prev_ts {
                bad.push(format!("track {tid}: {label}@{ts} after {prev}@{prev_ts}"));
            }
        }
        last.insert(tid, (label, ts));
    }
    bad
}

#[test]
fn hsmm_commits_never_move_backwards_in_time() {
    for spec in [manta_testkit::vectors::vr6a(), manta_testkit::vectors::v7()] {
        let bad = backwards_commits(&spec);
        assert!(bad.is_empty(), "{}: {bad:?}", spec.name);
    }
}
