//! MAN-301: `manta grade`, end to end through the real binary: the grade
//! records and summary for the four Gherkin scenarios
//! (`tests/fixtures/rbn_grade/`), determinism, input handling and
//! refusals.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rbn_grade")
}

fn fixture(name: &str) -> PathBuf {
    fixtures().join(name)
}

fn read_fixture(name: &str) -> String {
    std::fs::read_to_string(fixture(name)).unwrap()
}

/// The binary, with every `MANTA_*` variable of the test runner removed so
/// none can leak into the child. Tests that exercise the environment set
/// theirs explicitly.
fn manta() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"MANTA_") {
            cmd.env_remove(key);
        }
    }
    cmd
}

/// `manta grade --spots <fixture spots> --rbn <fixture csv>` plus `extra`.
fn grade_fixture(extra: &[&str]) -> Output {
    manta()
        .arg("grade")
        .arg("--spots")
        .arg(fixture("spots-2026-10-11.jsonl"))
        .arg("--rbn")
        .arg(fixture("rbn-20261011.csv"))
        .args(extra)
        .output()
        .unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[track_caller]
fn assert_success(out: &Output) {
    assert!(
        out.status.success(),
        "manta grade failed ({}):\nstdout:\n{}\nstderr:\n{}",
        out.status,
        stdout(out),
        stderr(out)
    );
}

#[track_caller]
fn assert_lines_eq(actual: &str, expected: &str) {
    let (a, e): (Vec<&str>, Vec<&str>) = (actual.lines().collect(), expected.lines().collect());
    for (i, (al, el)) in a.iter().zip(&e).enumerate() {
        assert_eq!(al, el, "line {} differs", i + 1);
    }
    assert_eq!(a.len(), e.len(), "line count differs:\n{actual}");
    assert_eq!(actual, expected);
}

#[test]
fn grade_json_matches_expected_records() {
    let out = grade_fixture(&["--json"]);
    assert_success(&out);
    assert_lines_eq(&stdout(&out), &read_fixture("expected-all.jsonl"));
}

#[test]
fn grade_with_spotter_k5tr_matches_expected_records() {
    let out = grade_fixture(&["--json", "--spotter", "K5TR"]);
    assert_success(&out);
    assert_lines_eq(&stdout(&out), &read_fixture("expected-k5tr.jsonl"));
}

#[test]
fn grade_summary_matches_expected_text() {
    let out = grade_fixture(&[]);
    assert_success(&out);
    assert_lines_eq(&stdout(&out), &read_fixture("expected-summary.txt"));
}

#[test]
fn grade_is_byte_identical_across_runs() {
    for extra in [&["--json"][..], &[][..]] {
        let first = grade_fixture(extra);
        let second = grade_fixture(extra);
        assert_success(&first);
        assert_success(&second);
        assert_eq!(first.stdout, second.stdout, "run {extra:?} twice");
    }
}

#[test]
fn grade_reads_spots_directories_and_skips_grade_records_and_a_torn_last_line() {
    // The fixture directory itself: only `spots-*.jsonl` is read (not the
    // expected-*.jsonl files beside it).
    let out = manta()
        .args(["grade", "--json", "--spots"])
        .arg(fixtures())
        .arg("--rbn")
        .arg(fixtures())
        .output()
        .unwrap();
    assert_success(&out);
    assert_lines_eq(&stdout(&out), &read_fixture("expected-all.jsonl"));

    // A node archive whose spot file also holds a grade record (skipped and
    // counted) and ends in a line torn by a kill (no newline; skipped and
    // counted), split across two daily files.
    let dir = tempfile::tempdir().unwrap();
    let spots = read_fixture("spots-2026-10-11.jsonl");
    let lines: Vec<&str> = spots.lines().collect();
    let grade_line = read_fixture("expected-all.jsonl")
        .lines()
        .next()
        .unwrap()
        .to_string();
    std::fs::write(
        dir.path().join("spots-2026-10-10.jsonl"),
        format!("{}\n", lines[0]),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("spots-2026-10-11.jsonl"),
        format!(
            "{}\n{}\n{}\n{}\n{}",
            lines[1],
            grade_line,
            lines[2],
            lines[3],
            &lines[0][..40]
        ),
    )
    .unwrap();
    std::fs::write(dir.path().join("notes.jsonl"), "not a spot file\n").unwrap();
    let json = manta()
        .args(["grade", "--json", "--spots"])
        .arg(dir.path())
        .arg("--rbn")
        .arg(fixture("rbn-20261011.csv"))
        .output()
        .unwrap();
    assert_success(&json);
    assert_lines_eq(&stdout(&json), &read_fixture("expected-all.jsonl"));
    let summary = manta()
        .args(["grade", "--spots"])
        .arg(dir.path())
        .arg("--rbn")
        .arg(fixture("rbn-20261011.csv"))
        .output()
        .unwrap();
    assert_success(&summary);
    let text = stdout(&summary);
    assert_eq!(
        text.lines().next().unwrap(),
        "spots: 4 from 2 files (1 grade record skipped, 1 torn line)"
    );
    let expected = read_fixture("expected-summary.txt");
    assert_eq!(
        text.lines().skip(1).collect::<Vec<_>>(),
        expected.lines().skip(1).collect::<Vec<_>>()
    );
}

#[test]
fn grade_refuses_zip_with_the_unzip_hint() {
    let dir = tempfile::tempdir().unwrap();
    let zip = dir.path().join("20261011.zip");
    std::fs::write(&zip, b"PK\x03\x04").unwrap();
    let out = manta()
        .arg("grade")
        .arg("--spots")
        .arg(fixture("spots-2026-10-11.jsonl"))
        .arg("--rbn")
        .arg(&zip)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    let expected = format!(
        "manta grade reads RBN's CSV; unzip {} first, or use \
         scripts/score-against-rbn.py --node-log, which unzips",
        zip.display()
    );
    assert!(stderr(&out).contains(&expected), "stderr: {}", stderr(&out));
}

/// A mistyped skimmer call is refused, as `[rbn_compare].spotters` and
/// `run --rbn-spotter` refuse it, rather than filtering out every report.
#[test]
fn grade_refuses_an_implausible_spotter_callsign() {
    for flag in ["--spotter", "--exclude-spotter"] {
        let out = grade_fixture(&["--json", flag, "K5T-R"]);
        assert!(!out.status.success(), "{flag} K5T-R was accepted");
        assert!(out.stdout.is_empty());
        let err = stderr(&out);
        assert!(err.contains(flag), "stderr: {err}");
        assert!(err.contains("not a plausible callsign"), "stderr: {err}");
    }
    // A lowercase call is still accepted, and uppercased.
    let out = grade_fixture(&["--json", "--spotter", "k5tr"]);
    assert_success(&out);
    assert_lines_eq(&stdout(&out), &read_fixture("expected-k5tr.jsonl"));
}

#[test]
fn grade_errors_on_a_malformed_middle_line() {
    let dir = tempfile::tempdir().unwrap();
    let spots = read_fixture("spots-2026-10-11.jsonl");
    let lines: Vec<&str> = spots.lines().collect();
    let path = dir.path().join("spots-2026-10-11.jsonl");
    // A torn line with more lines after it is not a torn final line.
    std::fs::write(
        &path,
        format!("{}\n{}\n{}\n", lines[0], &lines[1][..40], lines[2]),
    )
    .unwrap();
    let out = manta()
        .arg("grade")
        .arg("--spots")
        .arg(&path)
        .arg("--rbn")
        .arg(fixture("rbn-20261011.csv"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(
        err.contains(&format!(
            "{} line 2: malformed node spot record",
            path.display()
        )),
        "stderr: {err}"
    );

    // A malformed final line that does end in a newline was written whole,
    // so it is an error too.
    std::fs::write(&path, format!("{}\n{}\n", lines[0], &lines[1][..40])).unwrap();
    let out = manta()
        .arg("grade")
        .arg("--spots")
        .arg(&path)
        .arg("--rbn")
        .arg(fixture("rbn-20261011.csv"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains(&format!(
            "{} line 2: malformed node spot record",
            path.display()
        )),
        "stderr: {}",
        stderr(&out)
    );
}

/// The scenario-3 record's `"grade"` and `"disagreements"` values.
fn scenario3(json: &str) -> (String, String) {
    let line = json
        .lines()
        .find(|l| l.contains(":12:173760000:W1AW"))
        .unwrap_or_else(|| panic!("no scenario-3 record in:\n{json}"));
    let v: serde_json::Value = serde_json::from_str(line).unwrap();
    (v["grade"].to_string(), v["disagreements"].to_string())
}

#[test]
fn grade_config_file_tolerances_apply_and_flags_win() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("node.toml");
    std::fs::write(
        &cfg,
        "[server]\nstation_callsign = \"W3XYZ-1\"\n\n\
         [rbn_compare]\nfreq_tol_hz = 2000.0\nspeed_tol_wpm = 15.0\n",
    )
    .unwrap();
    let cfg_arg = cfg.to_str().unwrap();

    // The file's tolerances: scenario 3's 1400 Hz and 10 WPM gaps are both
    // inside them.
    let out = grade_fixture(&["--json", "--config", cfg_arg]);
    assert_success(&out);
    assert_eq!(
        scenario3(&stdout(&out)),
        ("\"match\"".to_string(), "[]".to_string())
    );
    let summary = grade_fixture(&["--config", cfg_arg]);
    assert_success(&summary);
    assert!(
        stdout(&summary).contains(
            "window: 180 s; frequency disagreement over 2000 Hz; speed disagreement over \
             15 WPM and 20% of RBN's median"
        ),
        "{}",
        stdout(&summary)
    );

    // A flag beats the file.
    let out = grade_fixture(&["--json", "--config", cfg_arg, "--speed-tol-wpm", "3"]);
    assert_success(&out);
    assert_eq!(
        scenario3(&stdout(&out)),
        ("\"disagree\"".to_string(), "[\"speed\"]".to_string())
    );
}

#[test]
fn grade_ignores_manta_env_vars() {
    let baseline = grade_fixture(&["--json"]);
    assert_success(&baseline);
    for extra in [&["--json"][..], &[][..]] {
        let with_env = manta()
            .arg("grade")
            .arg("--spots")
            .arg(fixture("spots-2026-10-11.jsonl"))
            .arg("--rbn")
            .arg(fixture("rbn-20261011.csv"))
            .args(extra)
            .env("MANTA_RBN_COMPARE_WINDOW_SECS", "30")
            .env("MANTA_RBN_COMPARE_SPOTTERS", "[\"K5TR\"]")
            .output()
            .unwrap();
        assert_success(&with_env);
        assert_eq!(stdout(&with_env), stdout(&grade_fixture(extra)));
    }
    assert_eq!(stdout(&baseline), read_fixture("expected-all.jsonl"));
}

#[test]
fn grade_rejects_rbn_csv_missing_required_columns() {
    let dir = tempfile::tempdir().unwrap();
    let csv = dir.path().join("rbn.csv");
    std::fs::write(
        &csv,
        "callsign,de_pfx,de_cont,frequency,band,dx,mode,db,speed,tx_mode\n\
         K5TR,K,NA,14025.0,20m,W1AW,CQ,21,22,CW\n",
    )
    .unwrap();
    let out = manta()
        .arg("grade")
        .arg("--spots")
        .arg(fixture("spots-2026-10-11.jsonl"))
        .arg("--rbn")
        .arg(&csv)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(
        err.contains(&format!(
            "{}: RBN CSV lacks column(s) freq, date",
            csv.display()
        )),
        "stderr: {err}"
    );
}
