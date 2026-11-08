//! MAN-301: `manta grade`, end to end through the real binary: the grade
//! records and summary for the four Gherkin scenarios
//! (`tests/fixtures/rbn_grade/`), determinism, input handling and
//! refusals.

#[path = "common/launch.rs"]
mod launch;
use launch::manta;

use std::path::{Path, PathBuf};
use std::process::Output;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rbn_grade")
}

fn fixture(name: &str) -> PathBuf {
    fixtures().join(name)
}

fn read_fixture(name: &str) -> String {
    std::fs::read_to_string(fixture(name)).unwrap()
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

/// Each spot excludes only its own node's reports, as that node's live
/// comparator does: a second node's archive in `--spots` (here K5TR's,
/// itself an RBN skimmer) leaves the first node's grades unchanged, and the
/// summary does not list K5TR as excluded from every spot.
#[test]
fn grade_excludes_a_nodes_own_reports_only_from_that_nodes_spots() {
    let dir = tempfile::tempdir().unwrap();
    let spots = read_fixture("spots-2026-10-11.jsonl");
    let first = spots.lines().next().unwrap();
    let other = first
        .replace("\"deCall\":\"W3XYZ-1\"", "\"deCall\":\"K5TR\"")
        .replacen("W3XYZ-1:", "K5TR:", 1);
    assert!(other.contains("\"deCall\":\"K5TR\"") && other.contains("\"id\":\"K5TR:"));
    let other_node = dir.path().join("spots-k5tr.jsonl");
    std::fs::write(&other_node, format!("{other}\n")).unwrap();
    let grade = |json: bool| {
        let mut cmd = manta();
        cmd.arg("grade");
        if json {
            cmd.arg("--json");
        }
        let out = cmd
            .arg("--spots")
            .arg(fixture("spots-2026-10-11.jsonl"))
            .arg(&other_node)
            .arg("--rbn")
            .arg(fixture("rbn-20261011.csv"))
            .output()
            .unwrap();
        assert_success(&out);
        stdout(&out)
    };
    let json = grade(true);
    let own: Vec<&str> = json
        .lines()
        .filter(|l| l.contains("\"spotId\":\"W3XYZ-1:"))
        .collect();
    assert_eq!(
        own,
        read_fixture("expected-all.jsonl")
            .lines()
            .collect::<Vec<_>>()
    );
    assert_eq!(json.lines().count(), 5);
    let summary = grade(false);
    let header: Vec<&str> = summary.lines().take(4).collect();
    assert_eq!(header[2], "spotters: all");
    assert_eq!(
        header[3],
        "own calls, excluded only from that node's spots: W3XYZ-1, W3XYZ, K5TR"
    );
}

/// A spot line with no `deCall` excludes no own call, so the summary does
/// not list the other spots' node as excluded from every spot.
#[test]
fn grade_summary_lists_own_calls_apart_when_a_spot_has_no_de_call() {
    let dir = tempfile::tempdir().unwrap();
    let spots = read_fixture("spots-2026-10-11.jsonl");
    let first = spots.lines().next().unwrap();
    let anonymous = first
        .replace("\"deCall\":\"W3XYZ-1\",", "")
        .replacen("W3XYZ-1:", "ANON:", 1);
    assert!(!anonymous.contains("deCall"));
    let path = dir.path().join("spots-2026-10-11.jsonl");
    std::fs::write(&path, format!("{first}\n{anonymous}\n")).unwrap();
    let out = manta()
        .args(["grade", "--spots"])
        .arg(&path)
        .arg("--rbn")
        .arg(fixture("rbn-20261011.csv"))
        .output()
        .unwrap();
    assert_success(&out);
    let text = stdout(&out);
    let header: Vec<&str> = text.lines().take(4).collect();
    assert_eq!(header[2], "spotters: all");
    assert_eq!(
        header[3],
        "own calls, excluded only from that node's spots: W3XYZ-1, W3XYZ"
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

/// A spotter that is also excluded, by the flags or a node's own call,
/// would grade every spot it alone covers `none`, so it is refused.
#[test]
fn grade_refuses_a_spotter_that_is_also_excluded() {
    let out = grade_fixture(&["--json", "--spotter", "K5TR", "--exclude-spotter", "k5tr"]);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    let err = stderr(&out);
    assert!(
        err.contains(
            "spotter K5TR is both counted (--spotter or [rbn_compare].spotters) and excluded \
             (--exclude-spotter or [rbn_compare].exclude_spotters): its reports would never \
             count"
        ),
        "stderr: {err}"
    );
    for call in ["W3XYZ-1", "w3xyz"] {
        let out = grade_fixture(&["--json", "--spotter", call]);
        assert!(!out.status.success(), "--spotter {call} was accepted");
        assert!(out.stdout.is_empty());
        let err = stderr(&out);
        let upper = call.to_ascii_uppercase();
        assert!(
            err.contains(&format!(
                "spotter {upper} is node W3XYZ-1's own call: its reports never count for \
                 that node's spots, so every one would grade none"
            )),
            "stderr: {err}"
        );
    }
    // A disjoint pair still grades.
    let out = grade_fixture(&["--json", "--spotter", "K5TR", "--exclude-spotter", "N0ABC"]);
    assert_success(&out);
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

/// The fixture CSV split into two files with a 29-minute gap of no reports
/// between them (12:00:50 to 12:30:15), standing in for a day left out
/// between two daily dumps. Returns the directory and both paths.
fn split_rbn_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let csv = read_fixture("rbn-20261011.csv");
    let lines: Vec<&str> = csv.lines().collect();
    let (header, rows) = (lines[0], &lines[1..]);
    let first: Vec<&str> = rows
        .iter()
        .copied()
        .filter(|r| r.contains("2026-10-11 11:") || r.contains("2026-10-11 12:00:"))
        .collect();
    let second: Vec<&str> = rows
        .iter()
        .copied()
        .filter(|r| {
            r.contains("2026-10-11 12:3")
                || r.contains("2026-10-11 12:4")
                || r.contains("2026-10-11 13:")
        })
        .collect();
    let a = dir.path().join("rbn-a.csv");
    let b = dir.path().join("rbn-b.csv");
    std::fs::write(&a, format!("{header}\n{}\n", first.join("\n"))).unwrap();
    std::fs::write(&b, format!("{header}\n{}\n", second.join("\n"))).unwrap();
    (dir, a, b)
}

/// One record's `"grade"` value, found by the spot id fragment.
fn grade_of(json: &str, id_fragment: &str) -> String {
    let line = json
        .lines()
        .find(|l| l.contains(id_fragment))
        .unwrap_or_else(|| panic!("no {id_fragment} record in:\n{json}"));
    let v: serde_json::Value = serde_json::from_str(line).unwrap();
    v["grade"].to_string()
}

#[test]
fn grade_does_not_cover_a_gap_between_rbn_files() {
    let (_dir, a, b) = split_rbn_fixture();
    let out = manta()
        .args(["grade", "--json", "--spots"])
        .arg(fixture("spots-2026-10-11.jsonl"))
        .arg("--rbn")
        .arg(&a)
        .arg(&b)
        .output()
        .unwrap();
    assert_success(&out);
    let json = stdout(&out);
    // K5XYZ (12:10, window 12:07 to 12:13) lies in the gap: no file covers
    // it, so it is `unknown`, not `none`.
    assert_eq!(grade_of(&json, ":9:58560000:K5XYZ"), "\"unknown\"");
    // The other three grade as they do from the whole CSV: a match or a
    // disagreement needs no coverage, and DL1ABC's window lies inside the
    // second file's span.
    assert_eq!(grade_of(&json, ":7:960000:W1AW"), "\"match\"");
    assert_eq!(grade_of(&json, ":12:173760000:W1AW"), "\"disagree\"");
    assert_eq!(grade_of(&json, ":15:260160000:DL1ABC"), "\"elsewhere\"");

    // Files 45 s apart join. The first ends with an extra report of an
    // unspotted call at 12:09:45 and the second starts at 12:10:30, so the
    // join falls inside K5XYZ's window: only joined spans cover it, and its
    // `none`, like every other grade, is the one-file result.
    let dir = tempfile::tempdir().unwrap();
    let csv = read_fixture("rbn-20261011.csv");
    let lines: Vec<&str> = csv.lines().collect();
    let (head, tail) = lines[1..].split_at(4);
    assert!(tail[0].contains("2026-10-11 12:10:30"), "{}", tail[0]);
    let edge = "K5TR,K,NA,14010.0,20m,N0EDGE,K,NA,CQ,10,2026-10-11 12:09:45,20,CW";
    let c = dir.path().join("rbn-c.csv");
    let d = dir.path().join("rbn-d.csv");
    std::fs::write(&c, format!("{}\n{}\n{edge}\n", lines[0], head.join("\n"))).unwrap();
    std::fs::write(&d, format!("{}\n{}\n", lines[0], tail.join("\n"))).unwrap();
    let out = manta()
        .args(["grade", "--json", "--spots"])
        .arg(fixture("spots-2026-10-11.jsonl"))
        .arg("--rbn")
        .arg(&c)
        .arg(&d)
        .output()
        .unwrap();
    assert_success(&out);
    assert_lines_eq(&stdout(&out), &read_fixture("expected-all.jsonl"));
}

#[test]
fn grade_refuses_a_tolerance_flag_past_its_config_key_maximum() {
    for (flag, too_big, max) in [
        ("--freq-tol-hz", "10000.5", "10000"),
        ("--speed-tol-wpm", "20.5", "20"),
        ("--speed-tol-pct", "1000", "100"),
    ] {
        let out = grade_fixture(&["--json", flag, too_big]);
        assert!(!out.status.success(), "{flag} {too_big} was accepted");
        let err = stderr(&out);
        assert!(
            err.contains(&format!(
                "{flag} must be a number from 0 to {max}, got {too_big}"
            )),
            "{flag} {too_big}: stderr: {err}"
        );
        // The maximum itself is accepted, as the config key's is.
        assert_success(&grade_fixture(&["--json", flag, max]));
    }
    let out = grade_fixture(&["--json", "--speed-tol-wpm=-1"]);
    assert!(!out.status.success(), "--speed-tol-wpm -1 was accepted");
    assert!(
        stderr(&out).contains("--speed-tol-wpm must be a number from 0 to 20, got -1"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn grade_skips_hello_records_without_counting_them_as_grades() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("spots-hello.jsonl");
    std::fs::write(&path, format!("{}\n{}{}\n", r#"{"kind":"hello","schema":"stream.v1","session":"1","seq":0,"historyFrom":1,"resumed":false}"#,
        read_fixture("spots-2026-10-11.jsonl"), read_fixture("expected-all.jsonl").lines().next().unwrap())).unwrap();
    let out = manta()
        .args(["grade", "--spots"])
        .arg(&path)
        .arg("--rbn")
        .arg(fixture("rbn-20261011.csv"))
        .output()
        .unwrap();
    assert_success(&out);
    assert!(stdout(&out).contains("1 grade record skipped"));
    let out = manta()
        .args(["grade", "--json", "--spots"])
        .arg(&path)
        .arg("--rbn")
        .arg(fixture("rbn-20261011.csv"))
        .output()
        .unwrap();
    assert_success(&out);
    assert_lines_eq(&stdout(&out), &read_fixture("expected-all.jsonl"));
}

/// MAN-343: a tempdir holding `dumps/20261011.csv` and `spots.jsonl`,
/// copies of the fixture RBN CSV and spot log.
fn named_twice_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("dumps")).unwrap();
    std::fs::copy(
        fixture("rbn-20261011.csv"),
        dir.path().join("dumps/20261011.csv"),
    )
    .unwrap();
    std::fs::copy(
        fixture("spots-2026-10-11.jsonl"),
        dir.path().join("spots.jsonl"),
    )
    .unwrap();
    dir
}

/// `manta grade <args>` run in `dir`, once with `--json` and once without:
/// both stdouts, each from a successful run.
fn grade_in(dir: &Path, args: &[&str]) -> (String, String) {
    let run = |json: bool| {
        let mut cmd = manta();
        cmd.current_dir(dir).arg("grade").args(args);
        if json {
            cmd.arg("--json");
        }
        let out = cmd.output().unwrap();
        assert_success(&out);
        stdout(&out)
    };
    (run(true), run(false))
}

#[test]
fn grade_reads_a_file_named_directly_and_through_its_directory_once() {
    let dir = named_twice_dir();
    for args in [
        &[
            "--spots",
            "spots.jsonl",
            "--rbn",
            "dumps/",
            "--rbn",
            "dumps/20261011.csv",
        ][..],
        &[
            "--spots",
            "spots.jsonl",
            "--rbn",
            "dumps/20261011.csv",
            "dumps",
        ][..],
        &[
            "--spots",
            "spots.jsonl",
            "--rbn",
            "dumps",
            "./dumps/../dumps/20261011.csv",
        ][..],
        &[
            "--spots",
            "spots.jsonl",
            "--spots",
            "./spots.jsonl",
            "--rbn",
            "dumps/",
        ][..],
    ] {
        let (json, summary) = grade_in(dir.path(), args);
        println!("{args:?}");
        // One record per spot id, each the one-file grade; the summary
        // counts one spots file and one RBN file.
        assert_lines_eq(&json, &read_fixture("expected-all.jsonl"));
        assert_lines_eq(&summary, &read_fixture("expected-summary.txt"));
    }
}

#[cfg(unix)]
#[test]
fn grade_reads_a_symlinked_file_or_directory_once() {
    let dir = named_twice_dir();
    std::os::unix::fs::symlink("dumps/20261011.csv", dir.path().join("link.csv")).unwrap();
    std::os::unix::fs::symlink("dumps", dir.path().join("dumplink")).unwrap();
    for args in [
        &[
            "--spots",
            "spots.jsonl",
            "--rbn",
            "link.csv",
            "dumps/20261011.csv",
        ][..],
        &[
            "--spots",
            "spots.jsonl",
            "--rbn",
            "dumps/20261011.csv",
            "link.csv",
        ][..],
        &["--spots", "spots.jsonl", "--rbn", "dumps/", "dumplink/"][..],
    ] {
        let (json, summary) = grade_in(dir.path(), args);
        println!("{args:?}");
        assert_lines_eq(&json, &read_fixture("expected-all.jsonl"));
        assert_lines_eq(&summary, &read_fixture("expected-summary.txt"));
    }
}

#[test]
fn grade_does_not_reweigh_one_rbn_file_named_twice() {
    // Two skimmers hear W1AW at 14026.0 kHz and 22 WPM, three at 14025.0
    // kHz and 25 WPM: the node's 14025.1 kHz, 25 WPM spot is a match. Read
    // twice, the first file would outvote the second (four reports to
    // three) and flip it to a frequency disagreement.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("dumps")).unwrap();
    let csv = read_fixture("rbn-20261011.csv");
    let header = csv.lines().next().unwrap();
    let rows = |calls: &[&str], khz: &str, wpm: u32| {
        calls
            .iter()
            .map(|c| format!("{c},K,NA,{khz},20m,W1AW,K,NA,CQ,20,2026-10-11 12:00:10,{wpm},CW\n"))
            .collect::<String>()
    };
    std::fs::write(
        dir.path().join("dumps/rbn-a.csv"),
        format!("{header}\n{}", rows(&["K1AA", "K2BB"], "14026.0", 22)),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("dumps/rbn-b.csv"),
        format!(
            "{header}\n{}",
            rows(&["K3CC", "K4DD", "K5EE"], "14025.0", 25)
        ),
    )
    .unwrap();
    let spot = read_fixture("spots-2026-10-11.jsonl");
    let first = spot.lines().next().unwrap();
    assert!(first.contains(":7:960000:W1AW"), "{first}");
    std::fs::write(dir.path().join("spots.jsonl"), format!("{first}\n")).unwrap();

    let (once, _) = grade_in(dir.path(), &["--spots", "spots.jsonl", "--rbn", "dumps"]);
    assert_eq!(grade_of(&once, ":7:960000:W1AW"), "\"match\"");
    let (twice, _) = grade_in(
        dir.path(),
        &[
            "--spots",
            "spots.jsonl",
            "--rbn",
            "dumps",
            "dumps/rbn-a.csv",
        ],
    );
    assert_eq!(twice, once);
}

#[test]
fn grade_names_a_missing_input_as_typed_once() {
    let dir = named_twice_dir();
    let out = manta()
        .current_dir(dir.path())
        .args([
            "grade",
            "--spots",
            "spots.jsonl",
            "--rbn",
            "nope.csv",
            "nope.csv",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success(), "a missing --rbn file was accepted");
    let err = stderr(&out);
    assert_eq!(
        err.matches("cannot read nope.csv").count(),
        1,
        "stderr: {err}"
    );
}

// MAN-97: `--reference` (a skimmer's `DX de` transcript) and `--paired`
// (recall, misses and candidate false spots), against
// `tests/fixtures/skimsrv_pair/`: node W3XYZ-1 beside its SkimSrv W3XYZ.

fn pair_fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/skimsrv_pair")
        .join(name)
}

fn read_pair_fixture(name: &str) -> String {
    std::fs::read_to_string(pair_fixture(name)).unwrap()
}

/// `manta grade --spots <pair spots> --reference <pair transcript>` plus `extra`.
fn reference_fixture(extra: &[&str]) -> Output {
    manta()
        .arg("grade")
        .arg("--spots")
        .arg(pair_fixture("spots-2026-10-31.jsonl"))
        .arg("--reference")
        .arg(pair_fixture("reference-2026-10-31.txt"))
        .args(extra)
        .output()
        .unwrap()
}

/// The paired fixture run: the night 00:00-06:00Z on 14000-14070 kHz.
fn paired_fixture(extra: &[&str]) -> Output {
    let mut args = vec![
        "--paired",
        "--passband-khz",
        "14000-14070",
        "--start",
        "2026-10-31T00:00:00Z",
        "--end",
        "2026-10-31T06:00:00Z",
    ];
    args.extend_from_slice(extra);
    reference_fixture(&args)
}

fn paired_json(out: &Output) -> serde_json::Value {
    assert_success(out);
    let text = stdout(out);
    assert_eq!(text.lines().count(), 1, "one JSON line:\n{text}");
    serde_json::from_str(&text).unwrap()
}

/// The `grade` of the spot at `time` (Unix seconds), from `--json` lines.
fn grade_of_call(json: &str, time: i64) -> String {
    json.lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .find(|v| v["spotTime"] == time)
        .unwrap_or_else(|| panic!("no grade at {time} in:\n{json}"))["grade"]
        .as_str()
        .unwrap()
        .to_string()
}

/// 2026-10-31 00:06:00Z, the fixture's W1AW spot that R1 confirms.
const PAIR_W1AW_TIME: i64 = 1_793_405_160;

#[test]
fn grade_reference_summary_matches_expected_text() {
    let out = reference_fixture(&[]);
    assert_success(&out);
    assert_lines_eq(
        &stdout(&out),
        &read_pair_fixture("expected-reference-summary.txt"),
    );
}

#[test]
fn grade_reference_counts_the_nodes_own_base_call() {
    // The transcript's spotter is W3XYZ, the node W3XYZ-1's base call:
    // with --rbn it would be excluded and the spot would grade none.
    let out = reference_fixture(&["--json"]);
    assert_success(&out);
    assert_eq!(grade_of_call(&stdout(&out), PAIR_W1AW_TIME), "match");
}

#[test]
fn grade_reference_accepts_a_spotter_that_is_the_nodes_own_call() {
    let out = reference_fixture(&["--json", "--spotter", "W3XYZ"]);
    assert_success(&out);
    assert_eq!(grade_of_call(&stdout(&out), PAIR_W1AW_TIME), "match");
    // A spotter the transcript never names leaves every grade without a
    // report: the filter applies to `DX de` spotters. (K1ABC's spot at
    // 00:41:00Z, whose window the transcript's span covers.)
    let out = reference_fixture(&["--json", "--spotter", "K5TR"]);
    assert_success(&out);
    assert_eq!(grade_of_call(&stdout(&out), 1_793_407_260), "none");
}

#[test]
fn grade_reference_bare_file_takes_its_day_from_the_name() {
    // 2358Z then 0003Z in reference-bare-2026-10-30.txt: the second line is
    // 2026-10-31 00:03:30Z, which confirms the spot at 00:04:00Z.
    let out = manta()
        .args(["grade", "--json", "--spots"])
        .arg(pair_fixture("spots-bare-2026-10-31.jsonl"))
        .arg("--reference")
        .arg(pair_fixture("reference-bare-2026-10-30.txt"))
        .output()
        .unwrap();
    assert_success(&out);
    assert_eq!(grade_of_call(&stdout(&out), 1_793_405_040), "match");
}

#[test]
fn grade_reference_directory_reads_txt_and_log_files_once() {
    let dir = tempfile::tempdir().unwrap();
    let body = read_pair_fixture("reference-2026-10-31.txt");
    let (head, tail) = body.split_at(body.find("2026-10-31T02:00:00Z").unwrap());
    std::fs::write(dir.path().join("reference-2026-10-31.txt"), head).unwrap();
    std::fs::write(dir.path().join("reference-2026-10-31b.log"), tail).unwrap();
    std::fs::write(dir.path().join("notes.md"), "DX de nothing\n").unwrap();
    let out = manta()
        .args(["grade", "--spots"])
        .arg(pair_fixture("spots-2026-10-31.jsonl"))
        .arg("--reference")
        .arg(dir.path())
        .arg(dir.path().join("reference-2026-10-31.txt"))
        .output()
        .unwrap();
    assert_success(&out);
    let text = stdout(&out);
    assert!(
        text.contains(
            "reference: 8 CW reports from 2 files (1 non-CW line, 1 other line skipped), \
             2026-10-31 00:05:00Z to 06:02:00Z\n"
        ),
        "{text}"
    );
}

#[test]
fn grade_needs_rbn_or_reference_and_refuses_both() {
    let neither = manta()
        .args(["grade", "--spots"])
        .arg(pair_fixture("spots-2026-10-31.jsonl"))
        .output()
        .unwrap();
    assert_eq!(neither.status.code(), Some(2), "{}", stderr(&neither));
    assert!(stderr(&neither).contains("<--rbn <PATH>...|--reference <PATH>...>"));
    let both = reference_fixture(&["--rbn", fixture("rbn-20261011.csv").to_str().unwrap()]);
    assert_eq!(both.status.code(), Some(2), "{}", stderr(&both));
    assert!(
        stderr(&both).contains("cannot be used with"),
        "{}",
        stderr(&both)
    );
}

#[test]
fn grade_reference_error_names_file_and_line() {
    let dir = tempfile::tempdir().unwrap();
    let capture = dir.path().join("capture.txt");
    std::fs::write(
        &capture,
        "CW Skimmer Server\n\
         Please enter your callsign:\n\
         DX de W3XYZ-#:  14025.0  W1AW           CW    21 dB  25 WPM  CQ      0005Z\n",
    )
    .unwrap();
    let out = manta()
        .args(["grade", "--spots"])
        .arg(pair_fixture("spots-2026-10-31.jsonl"))
        .arg("--reference")
        .arg(&capture)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains(&format!(
            "{} line 3: a DX de line with no UTC stamp needs its UTC day in the file name \
             (as reference-2026-10-31.txt)",
            capture.display()
        )),
        "{}",
        stderr(&out)
    );

    let dated = dir.path().join("reference-2026-10-31.txt");
    std::fs::write(
        &dated,
        "2026-10-31T25:00:00Z DX de W3XYZ-#:  14025.0  W1AW           CW    21 dB  25 WPM  CQ      0005Z\n",
    )
    .unwrap();
    let out = manta()
        .args(["grade", "--spots"])
        .arg(pair_fixture("spots-2026-10-31.jsonl"))
        .arg("--reference")
        .arg(&dated)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains(&format!(
            "{} line 1: bad UTC stamp \"2026-10-31T25:00:00Z\" before DX de",
            dated.display()
        )),
        "{}",
        stderr(&out)
    );
}

#[test]
fn paired_text_matches_expected() {
    let out = paired_fixture(&[]);
    assert_success(&out);
    assert_lines_eq(&stdout(&out), &read_pair_fixture("expected-paired.txt"));
}

#[test]
fn paired_json_matches_expected() {
    let out = paired_fixture(&["--json"]);
    assert_success(&out);
    assert_lines_eq(&stdout(&out), &read_pair_fixture("expected-paired.json"));
}

#[test]
fn paired_is_byte_identical_across_runs() {
    for extra in [&[][..], &["--json"][..]] {
        let first = paired_fixture(extra);
        let second = paired_fixture(extra);
        assert_success(&first);
        assert_success(&second);
        assert_eq!(first.stdout, second.stdout, "run {extra:?} twice");
    }
}

#[test]
fn paired_counts_add_up() {
    for extra in [&["--json"][..], &["--json", "--window-secs", "180"][..]] {
        let v = paired_json(&paired_fixture(extra));
        let (m, r) = (&v["mantaSpots"], &v["referenceReports"]);
        let n = |v: &serde_json::Value, k: &str| v[k].as_u64().unwrap();
        assert_eq!(n(r, "found") + n(r, "missed"), n(r, "counted"), "{extra:?}");
        assert_eq!(
            ["match", "disagree", "elsewhere", "none", "unknown"]
                .iter()
                .map(|k| n(m, k))
                .sum::<u64>(),
            n(m, "counted"),
            "{extra:?}"
        );
        assert_eq!(
            n(m, "outOfPassband") + n(m, "outOfWindow") + n(m, "counted"),
            n(m, "loaded")
        );
        assert_eq!(
            [
                "otherSpotter",
                "outOfPassband",
                "outOfWindow",
                "implausibleCall",
                "counted"
            ]
            .iter()
            .map(|k| n(r, k))
            .sum::<u64>(),
            n(r, "loaded")
        );
        assert_eq!(v["missed"].as_array().unwrap().len() as u64, n(r, "missed"));
        assert_eq!(
            v["candidateFalse"].as_array().unwrap().len() as u64,
            n(m, "none") + n(m, "elsewhere")
        );
    }
}

#[test]
fn paired_window_defaults_to_600_and_the_flag_wins() {
    let out = paired_fixture(&[]);
    assert_success(&out);
    assert!(
        stdout(&out).contains("\nwindow: 600 s; "),
        "{}",
        stdout(&out)
    );
    let out = paired_fixture(&["--window-secs", "180"]);
    assert_success(&out);
    assert!(
        stdout(&out).contains("\nwindow: 180 s; "),
        "{}",
        stdout(&out)
    );
}

#[test]
fn paired_ignores_the_config_window_but_keeps_its_tolerances() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("node.toml");
    std::fs::write(
        &cfg,
        "[server]\nstation_callsign = \"W3XYZ-1\"\n\n\
         [rbn_compare]\nwindow_secs = 180\nfreq_tol_hz = 2000.0\n",
    )
    .unwrap();
    let out = paired_fixture(&["--config", cfg.to_str().unwrap()]);
    assert_success(&out);
    let text = stdout(&out);
    assert!(
        text.contains("\nwindow: 600 s; frequency disagreement over 2000 Hz; "),
        "{text}"
    );
    // K1ABC's 1.4 kHz gap is inside the file's 2000 Hz: match, not disagree.
    assert!(
        text.contains("confirmed: 3 manta spots the reference also reported (match 3, disagree 0)"),
        "{text}"
    );
}

#[test]
fn paired_without_start_and_end_counts_everything_loaded() {
    let out = reference_fixture(&["--paired", "--passband-khz", "14000-14070", "--json"]);
    let v = paired_json(&out);
    assert_eq!(v["start"], serde_json::Value::Null);
    assert_eq!(v["end"], serde_json::Value::Null);
    assert_eq!(v["mantaSpots"]["outOfWindow"], 0);
    // M8 (06:30) and R9 (06:02) now count.
    assert_eq!(v["mantaSpots"]["counted"], 7);
    assert_eq!(v["referenceReports"]["outOfWindow"], 0);
    assert_eq!(v["referenceReports"]["counted"], 6);
    let text = stdout(&reference_fixture(&[
        "--paired",
        "--passband-khz",
        "14000-14070",
    ]));
    assert!(
        text.contains("paired: node W3XYZ-1, all times, passband 14000-14070 kHz\n"),
        "{text}"
    );
}

#[test]
fn paired_needs_a_passband() {
    let out = reference_fixture(&["--paired"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains(
            "error: the following required arguments were not provided:\n  --passband-khz <LO-HI>"
        ),
        "{}",
        stderr(&out)
    );
}

#[test]
fn paired_only_flags_need_paired() {
    for extra in [
        &["--passband-khz", "14000-14070"][..],
        &["--start", "2026-10-31T00:00:00Z"][..],
        &["--end", "2026-10-31T06:00:00Z"][..],
    ] {
        let out = reference_fixture(extra);
        assert_eq!(out.status.code(), Some(2), "{extra:?}: {}", stderr(&out));
        let err = stderr(&out);
        assert!(
            err.contains("required arguments were not provided:")
                && err.lines().any(|l| l == "  --paired"),
            "{extra:?}: {err}"
        );
    }
}

#[test]
fn paired_with_rbn_needs_a_spotter() {
    let out = grade_fixture(&["--paired", "--passband-khz", "14000-14070"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains(
            "error: --paired with --rbn needs --spotter (the co-located skimmer): against \
             every RBN skimmer, each call heard anywhere would count as a miss"
        ),
        "{}",
        stderr(&out)
    );
}

#[test]
fn paired_with_rbn_and_a_spotter_reports_recall() {
    let args = [
        "--paired",
        "--spotter",
        "K5TR",
        "--passband-khz",
        "7000-7300",
        "--passband-khz",
        "14000-14350",
    ];
    let mut json_args = args.to_vec();
    json_args.push("--json");
    let v = paired_json(&grade_fixture(&json_args));
    assert_eq!(v["referenceSource"], "rbn");
    assert_eq!(v["spotters"], serde_json::json!(["K5TR"]));
    assert_eq!(
        v["passbandsKhz"],
        serde_json::json!([[7000.0, 7300.0], [14000.0, 14350.0]])
    );
    let (m, r) = (&v["mantaSpots"], &v["referenceReports"]);
    let n = |v: &serde_json::Value, k: &str| v[k].as_u64().unwrap();
    assert!(n(r, "counted") > 0, "{v}");
    assert!(n(r, "otherSpotter") > 0, "{v}");
    assert_eq!(n(r, "found") + n(r, "missed"), n(r, "counted"));
    assert_eq!(
        ["match", "disagree", "elsewhere", "none", "unknown"]
            .iter()
            .map(|k| n(m, k))
            .sum::<u64>(),
        n(m, "counted")
    );
    // Own calls stay excluded with --rbn, as in the one-way header.
    let out = grade_fixture(&args);
    assert_success(&out);
    let text = stdout(&out);
    assert!(text.contains("\nrbn: "), "{text}");
    assert!(
        text.contains("\nspotters: K5TR (excluding W3XYZ-1, W3XYZ)\n"),
        "{text}"
    );
    assert!(!text.contains("own calls are not excluded"), "{text}");
    assert!(
        text.contains("passbands 7000-7300, 14000-14350 kHz"),
        "{text}"
    );
}

#[test]
fn paired_refuses_spots_from_two_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let spots = read_pair_fixture("spots-2026-10-31.jsonl");
    let second = spots
        .lines()
        .next()
        .unwrap()
        .replace("\"deCall\":\"W3XYZ-1\"", "\"deCall\":\"K1ABC\"")
        .replace("W3XYZ-1:", "K1ABC:");
    let path = dir.path().join("spots-2026-10-31.jsonl");
    std::fs::write(&path, format!("{spots}{second}\n")).unwrap();
    let out = manta()
        .args([
            "grade",
            "--paired",
            "--passband-khz",
            "14000-14070",
            "--spots",
        ])
        .arg(&path)
        .arg("--reference")
        .arg(pair_fixture("reference-2026-10-31.txt"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains(
            "error: --paired compares one node with its reference; the spots carry 2 nodes \
             (W3XYZ-1, K1ABC): grade each node's archive apart"
        ),
        "{}",
        stderr(&out)
    );
}

#[test]
fn paired_refuses_end_not_after_start() {
    let out = reference_fixture(&[
        "--paired",
        "--passband-khz",
        "14000-14070",
        "--start",
        "2026-10-31T06:00:00Z",
        "--end",
        "2026-10-31T00:00:00Z",
    ]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains(
            "error: --end (2026-10-31 00:00:00Z) must be after --start (2026-10-31 06:00:00Z)"
        ),
        "{}",
        stderr(&out)
    );
    let bad = reference_fixture(&["--paired", "--passband-khz", "14070-14000"]);
    assert_eq!(bad.status.code(), Some(2), "{}", stderr(&bad));
    assert!(
        stderr(&bad).contains(
            "error: invalid value '14070-14000' for '--passband-khz <LO-HI>': passband needs \
             finite LO < HI in kHz, got \"14070-14000\""
        ),
        "{}",
        stderr(&bad)
    );
}

#[test]
fn paired_ignores_manta_env_vars() {
    let out = manta()
        .arg("grade")
        .arg("--spots")
        .arg(pair_fixture("spots-2026-10-31.jsonl"))
        .arg("--reference")
        .arg(pair_fixture("reference-2026-10-31.txt"))
        .args([
            "--paired",
            "--passband-khz",
            "14000-14070",
            "--start",
            "2026-10-31T00:00:00Z",
            "--end",
            "2026-10-31T06:00:00Z",
        ])
        .env("MANTA_RBN_COMPARE_WINDOW_SECS", "30")
        .env("MANTA_RBN_COMPARE_SPOTTERS", "[\"K5TR\"]")
        .env("MANTA_RBN_COMPARE_FREQ_TOL_HZ", "5000")
        .output()
        .unwrap();
    assert_success(&out);
    assert_eq!(stdout(&out), read_pair_fixture("expected-paired.txt"));
}

/// Every key of a JSON line, in wire order, as `a` and `a.b` for objects
/// and `a[].b` for arrays of objects (each array's first element names its
/// keys). Scans the raw text: `serde_json::Value` keeps no key order.
fn wire_key_paths(text: &str) -> Vec<String> {
    enum Frame {
        Obj {
            prefix: String,
            emit: bool,
            key: String,
        },
        Arr {
            path: String,
            index: usize,
            emit: bool,
        },
    }
    let mut out = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();
    let mut last_string: Option<String> = None;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                let mut s = String::new();
                while let Some(c) = chars.next() {
                    match c {
                        '\\' => s.extend(chars.next()),
                        '"' => break,
                        _ => s.push(c),
                    }
                }
                last_string = Some(s);
                continue;
            }
            ':' => {
                if let (Some(Frame::Obj { prefix, emit, key }), Some(k)) =
                    (stack.last_mut(), last_string.take())
                {
                    if *emit {
                        out.push(format!("{prefix}{k}"));
                    }
                    *key = k;
                }
            }
            '{' | '[' => {
                let (path, emit) = match stack.last() {
                    None => (String::new(), true),
                    Some(Frame::Obj { prefix, emit, key }) => (format!("{prefix}{key}"), *emit),
                    Some(Frame::Arr { path, index, emit }) => {
                        (format!("{path}[]"), *emit && *index == 0)
                    }
                };
                stack.push(if c == '{' {
                    let prefix = if path.is_empty() {
                        path
                    } else {
                        format!("{path}.")
                    };
                    Frame::Obj {
                        prefix,
                        emit,
                        key: String::new(),
                    }
                } else {
                    Frame::Arr {
                        path,
                        index: 0,
                        emit,
                    }
                });
            }
            '}' | ']' => {
                stack.pop();
            }
            ',' => {
                if let Some(Frame::Arr { index, .. }) = stack.last_mut() {
                    *index += 1;
                }
            }
            _ => {}
        }
        last_string = None;
    }
    out
}

#[test]
fn paired_json_keys_are_documented_in_the_runbook() {
    let out = paired_fixture(&["--json"]);
    assert_success(&out);
    let line = stdout(&out);
    let keys = wire_key_paths(&line);
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/RUNBOOKS/rbn-comparison.md"
    );
    let runbook = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("reading {path}: {e}"))
        .replace("\r\n", "\n");
    let (_, rest) = runbook
        .split_once("\n### The paired report's JSON\n")
        .expect("docs/RUNBOOKS/rbn-comparison.md has no `### The paired report's JSON` section");
    let section = rest.split("\n#").next().unwrap_or(rest);
    let rows: Vec<&str> = section
        .lines()
        .filter_map(|l| l.strip_prefix("- `"))
        .filter_map(|l| l.split_once("` ("))
        .map(|(field, _)| field)
        .collect();
    assert_eq!(
        rows, keys,
        "the runbook's paired JSON list must name every key, in wire order"
    );
    assert!(
        section.contains(&format!("all {} keys", keys.len())),
        "the section must say \"all {} keys\"",
        keys.len()
    );
}
