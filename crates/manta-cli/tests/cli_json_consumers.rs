//! MAN-139 scenario 4: the scripts that read the CLI JSON read what the real
//! binary prints. `scripts/summarize-listen-jsonl.sh`,
//! `scripts/score-transcript.py` and `scripts/score-against-rbn.py` run
//! against one `manta run --json` and `manta decode --json` capture of
//! `manta gen v1`, and the hand-written transcript fixture may use only keys
//! that capture holds. The Python suites under `scripts/tests/` feed the
//! scripts hand-written JSON, so on their own they stay green while the
//! scripts break on real output.
//!
//! Needs `bash`, `jq` and `python3`. A test whose tool is missing skips with
//! a message, except under GitHub Actions, where it fails: its `ubuntu-latest`
//! and `macos-latest` images ship all three, so a skip there is a bug.
#![cfg(unix)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::UNIX_EPOCH;

use regex::Regex;
use serde_json::Value;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn script(name: &str) -> PathBuf {
    repo_root().join("scripts").join(name)
}

/// The binary with every `MANTA_*` variable removed, as in `cli.rs`.
fn manta() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"MANTA_") {
            cmd.env_remove(key);
        }
    }
    cmd
}

fn assert_success(what: &str, out: &Output) {
    assert!(
        out.status.success(),
        "{what} exited {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// `manta run --json`'s stdout (`listen.jsonl`) and `manta decode --json`'s
/// (`decode.json`) for `manta gen v1`.
struct Capture {
    listen: PathBuf,
    decode: PathBuf,
}

/// One capture for every test in this binary, and for concurrent `cargo
/// test` runs: rendered into a process-unique staging directory under
/// `CARGO_TARGET_TMPDIR` and atomically renamed into place, as
/// `common::fixture_wav` does. The directory name carries the binary's size
/// and modification time, so a rebuilt `manta` never reads JSON an older
/// build printed. The 92 MB WAV is deleted before publishing.
fn capture() -> &'static Capture {
    static CAPTURE: OnceLock<Capture> = OnceLock::new();
    CAPTURE.get_or_init(|| {
        let meta = std::fs::metadata(env!("CARGO_BIN_EXE_manta")).expect("manta binary");
        let mtime = meta
            .modified()
            .expect("binary mtime")
            .duration_since(UNIX_EPOCH)
            .expect("mtime after 1970")
            .as_nanos();
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("man139-cli-json-{:x}-{mtime:x}", meta.len()));
        let capture = Capture {
            listen: root.join("listen.jsonl"),
            decode: root.join("decode.json"),
        };
        if root.exists() {
            return capture;
        }
        let staging = root.with_extension(format!("staging.{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging).unwrap();

        let out = manta()
            .args(["gen", "v1", "--out"])
            .arg(&staging)
            .output()
            .unwrap();
        assert_success("manta gen v1", &out);
        let wav = staging.join("v1.wav");

        let out = manta()
            .args(["run", "--source-iq", "--source"])
            .arg(&wav)
            .arg("--json")
            .output()
            .unwrap();
        assert_success("manta run --json", &out);
        std::fs::write(staging.join("listen.jsonl"), &out.stdout).unwrap();

        let out = manta()
            .arg("decode")
            .arg(&wav)
            .arg("--json")
            .output()
            .unwrap();
        assert_success("manta decode --json", &out);
        std::fs::write(staging.join("decode.json"), &out.stdout).unwrap();

        std::fs::remove_file(&wav).unwrap();
        // Loser of a rename race: another process already published one.
        if std::fs::rename(&staging, &root).is_err() {
            let _ = std::fs::remove_dir_all(&staging);
        }
        assert!(
            capture.listen.exists() && capture.decode.exists(),
            "capture missing after publish: {}",
            root.display()
        );
        capture
    })
}

/// Whether every tool in `names` answers `--version`. When one does not, the
/// test skips with a message, or panics naming the tool under GitHub Actions.
fn tools(names: &[&str]) -> bool {
    for name in names {
        let answers = Command::new(name)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if !answers {
            if std::env::var_os("GITHUB_ACTIONS").is_some() {
                panic!(
                    "{name} did not answer --version; CI images ship bash, jq and python3, \
                     so this test must not skip there"
                );
            }
            eprintln!("skipping: needs bash, jq and python3 ({name} did not answer --version)");
            return false;
        }
    }
    true
}

/// The rest of the line that starts with `label`, trimmed.
fn field<'a>(stdout: &'a str, label: &str) -> &'a str {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix(label))
        .unwrap_or_else(|| panic!("no {label:?} line in:\n{stdout}"))
        .trim()
}

#[test]
fn summarize_listen_jsonl_reads_real_run_json() {
    if !tools(&["bash", "jq"]) {
        return;
    }
    let capture = capture();
    let out = Command::new("bash")
        .arg(script("summarize-listen-jsonl.sh"))
        .arg(&capture.listen)
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout.clone()).unwrap();
    assert_success("summarize-listen-jsonl.sh", &out);
    assert!(
        out.stderr.is_empty(),
        "stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let chars = read(&capture.listen)
        .lines()
        .filter(|line| line.contains(r#""event":"CharDecoded""#))
        .count();
    assert!(chars > 0, "the capture decoded no characters");
    assert_eq!(
        field(&stdout, "charsDecoded:"),
        chars.to_string(),
        "{stdout}"
    );
    let distinct: usize = field(&stdout, "distinctChars:").parse().unwrap();
    assert!(distinct >= 1, "{stdout}");

    let number = r"-?\d+(\.\d+)?";
    let snr = Regex::new(&format!(
        r"(?m)^snr \(2500 Hz reference\): min={number} median={number} max={number}$"
    ))
    .unwrap();
    assert!(snr.is_match(&stdout), "no SNR line in:\n{stdout}");

    let spot = stdout
        .lines()
        .find(|line| line.contains("W1AW"))
        .unwrap_or_else(|| panic!("no W1AW spot line in:\n{stdout}"));
    for pattern in [
        r"\tspotType=cq(\t|$)".to_string(),
        r"\ttrackId=\d+(\t|$)".to_string(),
        format!(r"\tsnr={number}(\t|$)"),
    ] {
        assert!(
            Regex::new(&pattern).unwrap().is_match(spot),
            "spot line {spot:?} does not match {pattern}"
        );
    }
    assert!(!stdout.contains("null"), "a field read as null:\n{stdout}");
}

#[test]
fn score_transcript_reads_real_run_json() {
    if !tools(&["python3"]) {
        return;
    }
    let capture = capture();
    let dir = tempfile::tempdir().unwrap();
    let transcript = dir.path().join("tr.csv");
    std::fs::write(
        &transcript,
        "start_s,end_s,pitch_hz,text\n0,120,12340,CQ CQ DE W1AW W1AW K\n",
    )
    .unwrap();
    let out = Command::new("python3")
        .arg(script("score-transcript.py"))
        .arg(&capture.listen)
        .arg(&transcript)
        .args(["--sample-rate-hz", "96000", "--dial-freq-hz", "14000000"])
        .output()
        .unwrap();
    assert_success("score-transcript.py", &out);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("spot precision: 100.0% (1 of 1)"),
        "{stdout}"
    );
    assert!(stdout.contains("call recall: 100.0% (1 of 1)"), "{stdout}");
    // Words come from the CharDecoded glyphs, which no spot field covers.
    let words = Regex::new(r"(?m)^word recall: \d+\.\d% \(([1-9]\d*) of 6\)$").unwrap();
    assert!(
        words.is_match(&stdout),
        "no decoded word matched:\n{stdout}"
    );
}

#[test]
fn score_against_rbn_reads_real_decode_json() {
    if !tools(&["python3"]) {
        return;
    }
    let capture = capture();
    let dir = tempfile::tempdir().unwrap();
    let rbn = dir.path().join("rbn.csv");
    std::fs::write(
        &rbn,
        "callsign,de_pfx,de_cont,freq,band,dx,dx_pfx,dx_cont,mode,db,date,speed,tx_mode\n\
         K5TR,K,NA,14012.3,20m,W1AW,K,NA,CQ,21,2026-10-11 12:00:20,20,CW\n",
    )
    .unwrap();
    let out = Command::new("python3")
        .arg(script("score-against-rbn.py"))
        .arg(&capture.decode)
        .arg(&rbn)
        .args([
            "--capture-start",
            "2026-10-11T12:00:00Z",
            "--sample-rate-hz",
            "96000",
            "--show-samples",
            "0",
        ])
        .output()
        .unwrap();
    assert_success("score-against-rbn.py", &out);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("true positives: 1\n"), "{stdout}");
}

/// A JSON Lines record's kind (its `event` value, or `spot`) and its keys
/// (the inner object's, for a spot).
fn record(what: &str, line: &str) -> (String, BTreeSet<String>) {
    let value: Value =
        serde_json::from_str(line).unwrap_or_else(|e| panic!("{what}: not JSON ({e}): {line}"));
    if let Some(spot) = value.get("spot") {
        return ("spot".to_string(), keys(what, spot));
    }
    let kind = value
        .get("event")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("{what}: neither an event nor a spot: {line}"));
    (kind.to_string(), keys(what, &value))
}

fn keys(what: &str, value: &Value) -> BTreeSet<String> {
    value
        .as_object()
        .unwrap_or_else(|| panic!("{what}: not an object: {value}"))
        .keys()
        .cloned()
        .collect()
}

#[test]
fn transcript_fixture_uses_only_keys_run_json_emits() {
    let capture = capture();
    let mut emitted: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (i, line) in read(&capture.listen).lines().enumerate() {
        let (kind, keys) = record(&format!("listen.jsonl:{}", i + 1), line);
        emitted.entry(kind).or_default().extend(keys);
    }
    let decode: Value = serde_json::from_str(&read(&capture.decode)).expect("decode.json");
    let spots = decode["spots"].as_array().expect("decode.json spots");
    assert!(!spots.is_empty(), "decode --json reported no spot");
    let decode_spot_keys: BTreeSet<String> = spots
        .iter()
        .flat_map(|spot| keys("decode.json spots[]", spot))
        .collect();

    let dir = repo_root().join("scripts/tests/fixtures/transcript");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let name = path
            .strip_prefix(repo_root())
            .unwrap()
            .display()
            .to_string();
        for (i, line) in read(&path).lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let what = format!("{name}:{}", i + 1);
            let (kind, keys) = record(&what, line);
            let real = emitted.get(&kind).unwrap_or_else(|| {
                panic!(
                    "{what}: the real run --json capture has no {kind} record to check it against"
                )
            });
            let extra: Vec<_> = keys.difference(real).collect();
            assert!(
                extra.is_empty(),
                "{what}: {kind} keys {extra:?} are not in what run --json emits ({real:?})"
            );
            if kind == "spot" {
                let extra: Vec<_> = keys.difference(&decode_spot_keys).collect();
                assert!(
                    extra.is_empty(),
                    "{what}: spot keys {extra:?} are not in decode --json's spots \
                     ({decode_spot_keys:?})"
                );
            }
            checked += 1;
        }
    }
    assert!(checked > 0, "no JSON Lines fixture under {}", dir.display());
}
