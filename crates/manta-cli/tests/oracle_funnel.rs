//! MAN-198 acceptance: `manta oracle --funnel PATH` writes a stage-attributed
//! loss funnel (header, one record per RBN episode, summary) and leaves the
//! oracle's own stdout and `--jsonl` byte-identical. Real binary, a short
//! `manta gen`-shaped V1 fixture, no hardware.

use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Output};

/// `manta` with no `MANTA_*` variable from the test runner's environment.
fn manta() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"MANTA_") {
            cmd.env_remove(key);
        }
    }
    cmd
}

/// Four K5TR rows against V1 (W1AW at 14012.34 kHz, 96 kS/s around
/// 14.000 MHz): W1AW itself, an empty in-band frequency, one beyond the
/// +-48 kHz passband and one after the recording ends. The N0ABC row is
/// another spotter's and is not truth.
const RBN_CSV: &str = "\
callsign,de_pfx,de_cont,freq,band,dx,dx_pfx,dx_cont,mode,db,date,speed,tx_mode
K5TR,K,NA,14012.3,20m,W1AW,K,NA,CW,25,2026-10-19 00:00:25,20,CQ
K5TR,K,NA,14030.0,20m,OH2XYZ,OH,EU,CW,20,2026-10-19 00:00:25,20,CQ
K5TR,K,NA,14060.0,20m,VE3OUT,VE,NA,CW,20,2026-10-19 00:00:25,20,CQ
K5TR,K,NA,14012.3,20m,W9LATE,K,NA,CW,20,2026-10-19 00:11:00,20,CQ
N0ABC,K,NA,14012.3,20m,K1ZZZ,K,NA,CW,20,2026-10-19 00:00:25,20,CQ
";

fn oracle(wav: &Path, csv: &Path, extra: &[&OsStr]) -> Output {
    let mut cmd = manta();
    cmd.arg("oracle")
        .arg(wav)
        .arg(csv)
        .args([
            "--spotter",
            "K5TR",
            "--capture-start",
            "2026-10-19T00:00:00Z",
        ])
        .args(extra);
    cmd.output().unwrap()
}

fn lines(p: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(p)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn funnel_writes_one_stage_per_episode_and_leaves_the_oracle_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = manta_testkit::vectors::v1();
    // W1AW's first CQ spot lands ~21 s in; 45 s keeps the test short.
    spec.duration_s = 45.0;
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
    let wav = dir.path().join(format!("{}.wav", spec.name));
    let csv = dir.path().join("rbn.csv");
    std::fs::write(&csv, RBN_CSV).unwrap();
    let (j0, j1) = (dir.path().join("j0.jsonl"), dir.path().join("j1.jsonl"));
    let (f1, f2) = (dir.path().join("f1.jsonl"), dir.path().join("f2.jsonl"));

    let plain = oracle(&wav, &csv, &["--jsonl".as_ref(), j0.as_os_str()]);
    let with_funnel = oracle(
        &wav,
        &csv,
        &[
            "--jsonl".as_ref(),
            j1.as_os_str(),
            "--funnel".as_ref(),
            f1.as_os_str(),
        ],
    );
    let again = oracle(&wav, &csv, &["--funnel".as_ref(), f2.as_os_str()]);
    for out in [&plain, &with_funnel, &again] {
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // The oracle's own output does not move.
    assert_eq!(plain.stdout, with_funnel.stdout);
    assert_eq!(plain.stdout, again.stdout);
    assert_eq!(std::fs::read(&j0).unwrap(), std::fs::read(&j1).unwrap());

    // One note on stderr, and none without --funnel.
    let notes = |out: &Output| {
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .filter(|l| l.starts_with("note: loss funnel:"))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert!(notes(&plain).is_empty());
    let note = notes(&with_funnel);
    assert_eq!(note.len(), 1, "{note:?}");
    assert!(
        note[0].starts_with("note: loss funnel: 4 opportunities (rbn-single-spotter truth), 1 delivered; lost at inputCoverage 2, detectableEnergy 1; written to "),
        "{}",
        note[0]
    );

    // Deterministic: two runs write the same bytes.
    assert_eq!(std::fs::read(&f1).unwrap(), std::fs::read(&f2).unwrap());

    let v = lines(&f1);
    assert_eq!(v.len(), 6, "header, 4 records, summary");
    let header = &v[0];
    assert_eq!(header["kind"], "lossFunnelHeader");
    assert_eq!(header["schema"], 1);
    assert_eq!(header["truthLayer"], "rbn-single-spotter");
    assert_eq!(header["spotter"], "K5TR");
    assert_eq!(header["captureStart"], "2026-10-19T00:00:00Z");
    assert_eq!(header["recordingFile"], "v1.wav");
    assert_eq!(header["referenceFile"], "rbn.csv");
    for key in ["recordingSha256", "sidecarSha256", "referenceSha256"] {
        let h = header[key].as_str().unwrap_or_default();
        assert_eq!(h.len(), 64, "{key}: {h}");
        assert!(h.bytes().all(|b| b.is_ascii_hexdigit()), "{key}: {h}");
    }
    assert!(header["configSha256"].is_null());
    assert!(header["build"].as_str().unwrap().starts_with(concat!(
        "manta-",
        env!("CARGO_PKG_VERSION"),
        "+"
    )));
    assert_eq!(header["engine"], "legacy");
    assert_eq!(header["prior"], "off");
    assert_eq!(header["bypasses"], serde_json::json!([]));
    assert_eq!(header["stages"].as_array().unwrap().len(), 9);

    // Records sorted by call; each charged to exactly one stage.
    let rec = |i: usize| &v[i];
    let calls: Vec<&str> = (1..5).map(|i| rec(i)["call"].as_str().unwrap()).collect();
    assert_eq!(calls, ["OH2XYZ", "VE3OUT", "W1AW", "W9LATE"]);
    for i in 1..5 {
        assert_eq!(rec(i)["kind"], "lossFunnel");
    }
    assert_eq!(rec(1)["lostAt"], "detectableEnergy");
    assert_eq!(rec(1)["detail"], "noRise");
    assert_eq!(rec(1)["tracks"]["candidates"], 0);
    assert_eq!(rec(2)["lostAt"], "inputCoverage");
    assert_eq!(rec(2)["detail"], "outsidePassband");
    assert_eq!(rec(3)["reached"], "delivered");
    assert!(rec(3)["lostAt"].is_null());
    assert!(!rec(3)["spots"].as_array().unwrap().is_empty());
    assert_eq!(rec(4)["lostAt"], "inputCoverage");
    assert_eq!(rec(4)["detail"], "outsideRecording");

    let summary = &v[5];
    assert_eq!(summary["kind"], "lossFunnelSummary");
    assert_eq!(summary["opportunities"], 4);
    assert_eq!(summary["delivered"], 1);
    assert_eq!(summary["lostAt"]["inputCoverage"], 2);
    assert_eq!(summary["lostAt"]["detectableEnergy"], 1);
    let births = &summary["births"];
    let n = |k: &str| births[k].as_u64().unwrap();
    assert!(n("candidates") >= n("confirmed"));
    assert!(n("confirmed") >= n("leased"));
    assert!(n("leased") >= 1);

    // Production lines only: no oracle-assisted number.
    assert!(!std::fs::read_to_string(&f1)
        .unwrap()
        .contains("oracleAssisted"));
}

#[test]
fn bad_funnel_tolerance_exits_2() {
    for flag in [
        "--funnel-track-tol-hz=-1",
        "--funnel-spot-tol-hz=NaN",
        "--funnel-spot-window-s=inf",
    ] {
        let out = manta()
            .args([
                "oracle",
                "missing.wav",
                "missing.csv",
                "--spotter",
                "K5TR",
                "--capture-start",
                "2026-10-19T00:00:00Z",
                flag,
            ])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{flag}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("must be finite and non-negative"),
            "{flag}: {stderr}"
        );
    }
}

/// `--funnel-bypass known-frequency` appends oracle-assisted lines after the
/// production summary; the production lines before them are the same bytes
/// as without the bypass.
#[test]
fn funnel_bypass_known_frequency_appends_after_summary() {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = manta_testkit::vectors::v1();
    spec.duration_s = 45.0;
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
    let wav = dir.path().join(format!("{}.wav", spec.name));
    let csv = dir.path().join("rbn.csv");
    std::fs::write(&csv, RBN_CSV).unwrap();
    let (plain, bypassed) = (dir.path().join("p.jsonl"), dir.path().join("b.jsonl"));
    let a = oracle(&wav, &csv, &["--funnel".as_ref(), plain.as_os_str()]);
    let b = oracle(
        &wav,
        &csv,
        &[
            "--funnel".as_ref(),
            bypassed.as_os_str(),
            "--funnel-bypass".as_ref(),
            "known-frequency".as_ref(),
        ],
    );
    for out in [&a, &b] {
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert_eq!(a.stdout, b.stdout);

    let p = std::fs::read_to_string(&plain).unwrap();
    let q = std::fs::read_to_string(&bypassed).unwrap();
    let (p_lines, q_lines): (Vec<&str>, Vec<&str>) = (p.lines().collect(), q.lines().collect());
    // The header names the bypass; every production line after it is
    // unchanged.
    assert_eq!(p_lines.len(), 6);
    assert_eq!(&q_lines[1..6], &p_lines[1..6]);
    let header: serde_json::Value = serde_json::from_str(q_lines[0]).unwrap();
    assert_eq!(header["bypasses"], serde_json::json!(["knownFrequency"]));

    let extra: Vec<serde_json::Value> = q_lines[6..]
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(extra.len(), 5, "4 bypass lines and their summary");
    for (i, v) in extra.iter().enumerate() {
        let kind = if i < 4 {
            "lossFunnelBypass"
        } else {
            "lossFunnelBypassSummary"
        };
        assert_eq!(v["kind"], kind);
        assert_eq!(v["bypass"], "knownFrequency");
        assert_eq!(v["oracleAssisted"], true);
        let o = v.as_object().unwrap();
        for key in ["dxCall", "frequency", "spots"] {
            assert!(!o.contains_key(key), "{key} in {v}");
        }
    }
    assert_eq!(
        extra[0]["forcedStages"],
        serde_json::json!(["detectableEnergy", "admittedTrack"])
    );
    let summary = &extra[4];
    assert_eq!(summary["opportunities"], 4);
    assert_eq!(summary["available"], 2);
    assert_eq!(summary["unavailable"]["outsidePassband"], 1);
    assert_eq!(summary["unavailable"]["outsideRecording"], 1);
    assert!(summary["upperBound"].as_u64().unwrap() >= 1);
}

#[test]
fn funnel_bypass_rejects_unbuilt_names_and_needs_funnel() {
    let base = [
        "oracle",
        "missing.wav",
        "missing.csv",
        "--spotter",
        "K5TR",
        "--capture-start",
        "2026-10-19T00:00:00Z",
    ];
    for extra in [
        &[
            "--funnel",
            "f.jsonl",
            "--funnel-bypass",
            "known-transitions",
        ][..],
        &[
            "--funnel",
            "f.jsonl",
            "--funnel-bypass",
            "known-frequency,bogus",
        ][..],
        &["--funnel-bypass", "known-frequency"][..],
    ] {
        let out = manta().args(base).args(extra).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{extra:?}");
    }
}
