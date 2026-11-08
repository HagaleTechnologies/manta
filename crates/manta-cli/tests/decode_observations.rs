//! MAN-207 against the real `manta decode` binary: `--observations PATH`
//! writes why each callsign candidate was or was not spotted
//! (docs/OBSERVATIONS.md), and leaves stdout unchanged.

#[path = "common/launch.rs"]
mod launch;
use launch::manta;

use std::path::{Path, PathBuf};
use std::process::Output;

/// `CQ DE K5ARH K` sent once in 12 s: decoded, never repeated, so the
/// repetition gate withholds K5ARH.
fn once_sent_fixture(dir: &Path) -> PathBuf {
    let mut spec = manta_testkit::vectors::VectorSpec {
        name: "man207",
        duration_s: 12.0,
        ..manta_testkit::vectors::v1()
    };
    spec.signals[0].text = "CQ DE K5ARH K".into();
    spec.signals[0].loop_text = false;
    manta_testkit::vectors::write_fixture_set(&spec, dir).unwrap();
    dir.join("man207.wav")
}

fn decode(args: &[&str]) -> Output {
    manta().arg("decode").args(args).output().unwrap()
}

fn lines(path: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn a_withheld_candidate_is_in_the_observation_file() {
    let dir = tempfile::tempdir().unwrap();
    let wav = once_sent_fixture(dir.path());
    let obs = dir.path().join("obs.jsonl");
    let out = decode(&[
        wav.to_str().unwrap(),
        "--observations",
        obs.to_str().unwrap(),
    ]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains("observations: 1 record written to"),
        "{stderr}"
    );

    let lines = lines(&obs);
    assert_eq!(lines[0]["schema"], "manta-observations.v1");
    assert_eq!(lines[0]["detail"], "records");
    let withheld: Vec<&serde_json::Value> = lines
        .iter()
        .filter(|l| l["kind"] == "observation" && l["call"] == "K5ARH")
        .collect();
    assert_eq!(withheld.len(), 1, "{lines:#?}");
    let r = withheld[0];
    assert_eq!(r["decision"], "withheld");
    assert_eq!(r["reason"], "repetition");
    assert_eq!(r["reps"], 1);
    assert_eq!(r["requiredReps"], 2);
    let id = format!("t{}.w{}", r["trackId"], r["wordSeq"]);
    assert_eq!(r["observation"], id.as_str());
    let evidence = r["evidence"].as_array().unwrap();
    assert!(evidence.iter().any(|e| e["observation"] == id.as_str()));
    assert!(r.get("trace").is_none());

    let summary = lines.last().unwrap();
    assert_eq!(summary["kind"], "summary");
    assert_eq!(summary["records"], 1);
    assert_eq!(
        summary["decisions"],
        serde_json::json!([{"decision": "withheld", "reason": "repetition", "count": 1}])
    );
}

#[test]
fn stdout_is_unchanged_by_the_flag() {
    let dir = tempfile::tempdir().unwrap();
    let wav = once_sent_fixture(dir.path());
    let obs = dir.path().join("obs.jsonl");
    let plain = decode(&[wav.to_str().unwrap()]);
    let observed = decode(&[
        wav.to_str().unwrap(),
        "--observations",
        obs.to_str().unwrap(),
    ]);
    assert!(plain.status.success() && observed.status.success());
    assert_eq!(plain.stdout, observed.stdout);
    let json_plain = decode(&["--json", wav.to_str().unwrap()]);
    let obs2 = dir.path().join("obs2.jsonl");
    let json_observed = decode(&[
        "--json",
        wav.to_str().unwrap(),
        "--observations",
        obs2.to_str().unwrap(),
    ]);
    assert_eq!(json_plain.stdout, json_observed.stdout);
}

#[test]
fn the_file_is_byte_identical_across_runs() {
    let dir = tempfile::tempdir().unwrap();
    let wav = once_sent_fixture(dir.path());
    let a = dir.path().join("a.jsonl");
    let b = dir.path().join("b.jsonl");
    for path in [&a, &b] {
        let out = decode(&[
            wav.to_str().unwrap(),
            "--observations",
            path.to_str().unwrap(),
        ]);
        assert!(out.status.success());
    }
    assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
}

#[test]
fn text_detail_adds_only_the_trace() {
    let dir = tempfile::tempdir().unwrap();
    let wav = once_sent_fixture(dir.path());
    let records = dir.path().join("records.jsonl");
    let text = dir.path().join("text.jsonl");
    let wav = wav.to_str().unwrap();
    assert!(decode(&[wav, "--observations", records.to_str().unwrap()])
        .status
        .success());
    assert!(decode(&[
        wav,
        "--observations",
        text.to_str().unwrap(),
        "--observation-text",
    ])
    .status
    .success());
    let (records, text) = (lines(&records), lines(&text));
    assert_eq!(text[0]["detail"], "text");
    assert_eq!(records.len(), text.len());
    let pairs = records.iter().zip(&text).skip(1);
    for (r, t) in pairs.filter(|(r, _)| r["kind"] == "observation") {
        let mut t = t.clone();
        let trace = t.as_object_mut().unwrap().remove("trace").expect("a trace");
        assert_eq!(trace["text"], r["call"]);
        assert_eq!(&t, r);
    }
}

#[test]
fn an_existing_path_is_refused_before_decoding() {
    let dir = tempfile::tempdir().unwrap();
    let wav = once_sent_fixture(dir.path());
    let obs = dir.path().join("obs.jsonl");
    std::fs::write(&obs, "keep me\n").unwrap();
    let out = decode(&[
        wav.to_str().unwrap(),
        "--observations",
        obs.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("obs.jsonl already exists"), "{stderr}");
    assert!(out.stdout.is_empty(), "nothing was decoded");
    assert_eq!(std::fs::read_to_string(&obs).unwrap(), "keep me\n");
}

#[test]
fn a_failed_decode_leaves_no_file() {
    let dir = tempfile::tempdir().unwrap();
    let obs = dir.path().join("obs.jsonl");
    let missing = dir.path().join("missing.wav");
    let out = decode(&[
        missing.to_str().unwrap(),
        "--observations",
        obs.to_str().unwrap(),
    ]);
    assert!(!out.status.success());
    assert!(!obs.exists());
}

#[test]
fn observation_text_requires_observations() {
    let dir = tempfile::tempdir().unwrap();
    let wav = once_sent_fixture(dir.path());
    let out = decode(&[wav.to_str().unwrap(), "--observation-text"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--observations <PATH>"), "{stderr}");
}
