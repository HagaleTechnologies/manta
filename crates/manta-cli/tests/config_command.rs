//! MAN-76: `manta config check` / `manta config init`, end to end through
//! the real binary: exit codes, stdout vs stderr, and the guarantee that
//! check opens no source and binds no port.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn manta() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_manta"));
    // `config check` reads `MANTA_CONFIG` and `MANTA_<TABLE>_<KEY>` and
    // rejects unknown `MANTA_*` names, so a variable in the test runner's
    // own environment must never leak into the child. Tests that exercise
    // the env tier set theirs explicitly.
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"MANTA_") {
            cmd.env_remove(key);
        }
    }
    cmd
}

/// Writes `body` to `dir/name` and returns that path.
fn write_cfg(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    path
}

fn check_in(dir: &Path, args: &[&str]) -> Output {
    manta()
        .current_dir(dir)
        .args(["config", "check"])
        .args(args)
        .output()
        .unwrap()
}

fn check_file(path: &Path) -> Output {
    let dir = path.parent().unwrap();
    manta()
        .current_dir(dir)
        .args(["config", "check", "--config"])
        .arg(path)
        .output()
        .unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// Exit code 1 (an invalid config, not a usage error) and stderr.
fn fails(o: &Output) -> String {
    assert_eq!(
        o.status.code(),
        Some(1),
        "expected exit 1\nstdout: {}\nstderr: {}",
        stdout(o),
        stderr(o)
    );
    stderr(o)
}

fn succeeds(o: &Output) -> String {
    assert!(
        o.status.success(),
        "expected exit 0, got {:?}\nstdout: {}\nstderr: {}",
        o.status.code(),
        stdout(o),
        stderr(o)
    );
    stdout(o)
}

const SERVER_TOML: &str = "[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"127.0.0.1\"\n\
                           telnet_port = 0\njson_port = 0\nmetrics_port = 0\n";

// ---- Scenario 1: a valid config passes

#[test]
fn check_accepts_a_valid_config_and_exits_0() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_cfg(
        dir.path(),
        "manta.toml",
        &format!("{SERVER_TOML}[decode]\nengine = \"legacy\"\n"),
    );
    let out = succeeds(&check_file(&path));
    assert!(
        out.starts_with(&format!("{}: valid (from --config)\n", path.display())),
        "{out}"
    );
}

#[test]
fn check_binds_no_port_and_connects_to_no_receiver() {
    // Hold the three server ports: a bind attempt by check would fail
    // with EADDRINUSE. A fourth listener stands in for a KiwiSDR.
    let held: Vec<TcpListener> = (0..3)
        .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    let ports: Vec<u16> = held
        .iter()
        .map(|l| l.local_addr().unwrap().port())
        .collect();
    let fake_kiwi = TcpListener::bind("127.0.0.1:0").unwrap();
    fake_kiwi.set_nonblocking(true).unwrap();
    let kiwi_port = fake_kiwi.local_addr().unwrap().port();

    let dir = tempfile::tempdir().unwrap();
    let path = write_cfg(
        dir.path(),
        "manta.toml",
        &format!(
            "[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"127.0.0.1\"\n\
             telnet_port = {}\njson_port = {}\nmetrics_port = {}\n\
             [input]\ntype = \"kiwi\"\nhost = \"127.0.0.1\"\nport = {kiwi_port}\n\
             freq_hz = 7030000.0\n",
            ports[0], ports[1], ports[2]
        ),
    );
    let out = succeeds(&check_file(&path));
    assert!(
        out.contains(&format!("telnet_port={} ", ports[0])),
        "ports print as configured: {out}"
    );
    match fake_kiwi.accept() {
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
        Ok((_, peer)) => panic!("config check connected to the receiver from {peer}"),
        Err(e) => panic!("unexpected accept error: {e}"),
    }
}

#[test]
fn check_does_not_open_a_file_source() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_cfg(
        dir.path(),
        "manta.toml",
        "[input]\ntype = \"file\"\npath = \"missing.wav\"\niq = true\n",
    );
    let out = succeeds(&check_file(&path));
    assert!(out.contains("input: type=file path="), "{out}");
}

/// MAN-301: `config check` refuses what `run` refuses: `[rbn_compare]`
/// enabled while `[input]` replays a recording.
#[test]
fn check_refuses_rbn_compare_on_a_file_source() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_cfg(
        dir.path(),
        "manta.toml",
        &format!(
            "{SERVER_TOML}[rbn_compare]\nenabled = true\n\
             [input]\ntype = \"file\"\npath = \"missing.wav\"\n"
        ),
    );
    let err = fails(&check_file(&path));
    assert!(
        err.contains(
            "RBN comparison grades spots against the live RBN feed, but this run replays a \
             recording ([input] type = \"file\")."
        ),
        "{err}"
    );
}

// ---- Scenario 2: an invalid config is rejected naming field and problem

#[test]
fn check_rejects_an_unknown_field_naming_file_table_and_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_cfg(
        dir.path(),
        "manta.toml",
        "[server]\nstation_callsign = \"W1AW\"\ntelnet_prot = 7300\n",
    );
    let o = check_file(&path);
    let err = fails(&o);
    assert!(err.contains(&path.display().to_string()), "{err}");
    assert!(err.contains("[server]"), "{err}");
    assert!(err.contains("unknown field `telnet_prot`"), "{err}");
    assert!(stdout(&o).is_empty(), "no summary for an invalid config");
}

/// MAN-312: `crash_filter` is a closed set; a typo is refused before I/O.
#[test]
fn check_rejects_an_unknown_crash_filter() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_cfg(
        dir.path(),
        "bad.toml",
        "[detector]\ncrash_filter = \"sometimes\"\n",
    );
    let err = fails(&check_file(&path));
    assert!(err.contains("unknown variant"), "{err}");
    assert!(err.contains("sometimes"), "{err}");
    assert!(err.contains("starts-only"), "{err}");
}

/// MAN-312: a set `crash_filter` shows in the summary's `detector:` line.
#[test]
fn check_summary_shows_a_set_crash_filter() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_cfg(
        dir.path(),
        "good.toml",
        "[detector]\ncrash_filter = \"starts-only\"\n",
    );
    let out = succeeds(&check_file(&path));
    assert!(
        out.contains("detector: crash_filter=\"starts-only\" (other keys default)"),
        "{out}"
    );
}

#[test]
fn check_rejects_out_of_range_values_naming_the_key() {
    let dir = tempfile::tempdir().unwrap();
    for (body, needle) in [
        (
            "[detector]\non_snr_db = 200.0\n",
            "detector.on_snr_db must be finite and in [0, 100] dB, got 200",
        ),
        (
            "[input]\ntype = \"kiwi\"\nhost = \"h\"\nport = 0\nfreq_hz = 7e6\n",
            "input.port must be between 1 and 65535, got 0",
        ),
        (
            "[server]\nstation_callsign = \"W1AW\"\ntelnet_port = 70000\n",
            "telnet_port",
        ),
    ] {
        let path = write_cfg(dir.path(), "manta.toml", body);
        let err = fails(&check_file(&path));
        assert!(err.contains(needle), "{body:?}: {err}");
        assert!(err.contains(&path.display().to_string()), "{err}");
    }
}

/// MAN-314: an RTTY dwell shorter than one detector block is refused.
#[test]
fn check_rejects_an_rtty_dwell_below_one_block() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_cfg(dir.path(), "bad.toml", "[detector]\nrtty_dwell_ms = 500\n");
    let err = fails(&check_file(&path));
    assert!(err.contains("detector.rtty_dwell_ms"), "{err}");
    assert!(err.contains("at least 1000"), "{err}");
}

#[test]
fn check_with_no_flag_reads_manta_toml_in_the_current_directory() {
    let dir = tempfile::tempdir().unwrap();
    write_cfg(dir.path(), "manta.toml", "[detector]\nbogus_key = 1\n");
    let err = fails(&check_in(dir.path(), &[]));
    assert!(
        err.contains("manta.toml: [detector]: unknown field `bogus_key`"),
        "{err}"
    );

    write_cfg(dir.path(), "manta.toml", "[detector]\non_snr_db = 15\n");
    let o = check_in(dir.path(), &[]);
    let out = succeeds(&o);
    assert!(
        out.starts_with("manta.toml: valid (found in the current directory)\n"),
        "{out}"
    );
    // `run` never looks in the current directory, so check says how to
    // point it at the file it just checked.
    assert!(
        stderr(&o).contains("note: manta run does not read manta.toml from the current directory"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn check_prefers_config_flag_then_manta_config_then_cwd() {
    let dir = tempfile::tempdir().unwrap();
    write_cfg(dir.path(), "manta.toml", "");
    let env_file = write_cfg(dir.path(), "env.toml", "");
    let flag_file = write_cfg(dir.path(), "flag.toml", "");

    let first_line = |o: &Output| succeeds(o).lines().next().unwrap().to_string();
    let o = manta()
        .current_dir(dir.path())
        .env("MANTA_CONFIG", &env_file)
        .args(["config", "check", "--config"])
        .arg(&flag_file)
        .output()
        .unwrap();
    assert_eq!(
        first_line(&o),
        format!("{}: valid (from --config)", flag_file.display())
    );
    let o = manta()
        .current_dir(dir.path())
        .env("MANTA_CONFIG", &env_file)
        .args(["config", "check"])
        .output()
        .unwrap();
    assert_eq!(
        first_line(&o),
        format!("{}: valid (from MANTA_CONFIG)", env_file.display())
    );
    assert_eq!(
        first_line(&check_in(dir.path(), &[])),
        "manta.toml: valid (found in the current directory)"
    );
}

#[test]
fn check_with_no_config_anywhere_validates_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let o = check_in(dir.path(), &[]);
    let out = succeeds(&o);
    assert!(
        out.starts_with("no config file: valid (built-in defaults plus any MANTA_* variables)\n"),
        "{out}"
    );
    assert!(
        stderr(&o).contains("note: no config file given"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn check_reports_a_missing_config_file() {
    let dir = tempfile::tempdir().unwrap();
    let absent = dir.path().join("absent.toml");
    let err = fails(&check_in(
        dir.path(),
        &["--config", absent.to_str().unwrap()],
    ));
    assert!(
        err.contains(&format!("reading config file {}", absent.display())),
        "{err}"
    );
    let o = manta()
        .current_dir(dir.path())
        .env("MANTA_CONFIG", &absent)
        .args(["config", "check"])
        .output()
        .unwrap();
    let err = fails(&o);
    assert!(err.contains("reading config file"), "{err}");
}

#[test]
fn check_applies_and_validates_the_manta_environment() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_cfg(dir.path(), "manta.toml", SERVER_TOML);
    let o = manta()
        .current_dir(dir.path())
        .env("MANTA_SERVER_TELNET_PORT", "abc")
        .args(["config", "check", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    let err = fails(&o);
    assert!(err.contains("MANTA_SERVER_TELNET_PORT"), "{err}");
    let o = manta()
        .current_dir(dir.path())
        .env("MANTA_FOO", "1")
        .args(["config", "check", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    let err = fails(&o);
    assert!(
        err.contains("unrecognized environment variable MANTA_FOO"),
        "{err}"
    );
}

#[test]
fn check_reads_the_blocklist_and_notch_files() {
    let dir = tempfile::tempdir().unwrap();
    for (key, what) in [("blocklist_path", "blocklist"), ("notch_path", "notch")] {
        let path = write_cfg(
            dir.path(),
            "manta.toml",
            &format!("[spot]\n{key} = \"missing.txt\"\n"),
        );
        let err = fails(&check_file(&path));
        assert!(err.contains(&format!("reading {what} file")), "{err}");
    }
    write_cfg(dir.path(), "bad.txt", "N0BAD\n");
    let path = write_cfg(
        dir.path(),
        "manta.toml",
        "[spot]\nblocklist_path = \"bad.txt\"\n",
    );
    succeeds(&check_file(&path));
}

#[test]
fn check_rejects_duplicate_server_ports() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_cfg(
        dir.path(),
        "manta.toml",
        "[server]\nstation_callsign = \"W1AW\"\ntelnet_port = 17300\njson_port = 17300\n",
    );
    let err = fails(&check_file(&path));
    assert!(
        err.contains("json_port 17300 is the same as telnet_port"),
        "{err}"
    );
    let path = write_cfg(dir.path(), "manta.toml", SERVER_TOML);
    succeeds(&check_file(&path));
}

#[cfg(not(feature = "soapy"))]
#[test]
fn check_rejects_a_soapy_input_on_a_build_without_soapy() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_cfg(
        dir.path(),
        "manta.toml",
        "[input]\ntype = \"soapy\"\ndriver = \"driver=rtlsdr\"\nfreq_hz = 7e6\nrate_hz = 48000.0\n",
    );
    let err = fails(&check_file(&path));
    assert!(
        err.contains("input.type = \"soapy\" needs a manta built with --features soapy"),
        "{err}"
    );
}

#[test]
fn check_accepts_an_hpsdr_input_without_opening_the_receiver() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_cfg(
        dir.path(),
        "manta.toml",
        "[input]\ntype = \"hpsdr\"\nhost = \"receiver.invalid\"\nfreq_hz = 7030000.0\nrate_hz = 48000.0\n",
    );
    let out = succeeds(&check_file(&path));
    assert_eq!(
        out.lines().find(|line| line.starts_with("input:")),
        Some(
            "input: type=hpsdr host=receiver.invalid port=1024 protocol=auto freq_hz=7030000 \
             rate_hz=48000"
        )
    );
}

#[test]
fn config_without_a_subcommand_is_a_usage_error() {
    let o = manta().arg("config").output().unwrap();
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
}

// ---- The summary (stdout) and notes (stderr)

#[test]
fn check_prints_one_summary_line_per_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_cfg(dir.path(), "manta.toml", SERVER_TOML);
    let out = succeeds(&check_file(&path));
    let prefixes: Vec<&str> = out
        .lines()
        // `": "`, not `':'`: a Windows path's drive colon is followed by `\`.
        .map(|l| l.split_once(": ").map_or(l, |(p, _)| p))
        .collect();
    assert_eq!(
        prefixes,
        [
            path.display().to_string().as_str(),
            "environment",
            "server",
            "rbn_uplink",
            "rbn_compare",
            "input",
            "spot",
            "detector",
            "decode"
        ],
        "{out}"
    );
}

#[test]
fn check_summary_reflects_the_environment_overlay() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_cfg(dir.path(), "manta.toml", SERVER_TOML);
    let o = manta()
        .current_dir(dir.path())
        .env("MANTA_SERVER_TELNET_PORT", "9300")
        .args(["config", "check", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    let out = succeeds(&o);
    assert!(out.contains(" telnet_port=9300 "), "{out}");
    assert!(
        out.contains("\nenvironment: MANTA_SERVER_TELNET_PORT\n"),
        "{out}"
    );
}

#[test]
fn check_output_is_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_cfg(
        dir.path(),
        "manta.toml",
        &format!(
            "{SERVER_TOML}[input]\ntype = \"kiwi\"\nhost = \"rx.example.org\"\nfreq_hz = 7030000.0\n\
             [detector]\non_snr_db = 15\n[decode]\nengine = \"hsmm\"\nbeam = 16\n"
        ),
    );
    let a = check_file(&path);
    let b = check_file(&path);
    assert_eq!(succeeds(&a), succeeds(&b));
}

#[test]
fn check_notes_go_to_stderr_and_do_not_change_the_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_cfg(
        dir.path(),
        "manta.toml",
        "[server]\nstation_callsign = \"W1AW\"\n",
    );
    let o = check_file(&path);
    let out = succeeds(&o);
    assert!(!out.contains("note:"), "{out}");
    let err = stderr(&o);
    assert!(
        err.contains("note: manta run with this config needs your radio's dial frequency"),
        "{err}"
    );
    assert!(
        err.contains("note: the telnet and JSON servers listen on every network interface"),
        "{err}"
    );
}

/// MAN-90: an enabled uplink gets a stderr note that it is experimental; the
/// exit code and stdout summary are unchanged.
#[test]
fn check_notes_that_an_enabled_uplink_is_experimental() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_cfg(
        dir.path(),
        "manta.toml",
        "[server]\nstation_callsign = \"W1AW\"\n\
         [[rbn_uplink]]\nenabled = true\ntarget_host = \"a.example.org\"\ntarget_port = 7000\n",
    );
    let o = check_file(&path);
    let out = succeeds(&o);
    assert!(!out.contains("experimental"), "{out}");
    let err = stderr(&o);
    assert!(
        err.contains("note: [[rbn_uplink]] is experimental"),
        "{err}"
    );
}

/// MAN-319: a live uplink (`dry_run = false`) without a 6-character
/// `[server].operator_grid` is an invalid config: exit 1, the error on
/// stderr naming the target and the key to set, no `valid` on stdout.
const MAN319_LOCATOR_MSG: &str =
    "dry_run = false -- sending spots needs [server].operator_grid set to a 6-character \
     Maidenhead locator";

#[test]
fn config_check_refuses_a_live_uplink_without_a_six_character_locator() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_cfg(
        dir.path(),
        "nogrid-live.toml",
        "[server]\nstation_callsign = \"W1AW\"\n\
         [[rbn_uplink]]\nenabled = true\ntarget_host = \"127.0.0.1\"\ntarget_port = 7300\n\
         dry_run = false\n",
    );
    let o = check_file(&path);
    let err = fails(&o);
    assert!(err.contains(MAN319_LOCATOR_MSG), "{err}");
    assert!(err.contains("target 127.0.0.1:7300"), "{err}");
    assert!(!stdout(&o).contains(" valid"), "stdout: {}", stdout(&o));
}

#[test]
fn config_check_refuses_a_live_uplink_with_only_a_four_character_grid() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_cfg(
        dir.path(),
        "shortgrid-live.toml",
        "[server]\nstation_callsign = \"W1AW\"\noperator_grid = \"FN03\"\n\
         [[rbn_uplink]]\nenabled = true\ntarget_host = \"127.0.0.1\"\ntarget_port = 7300\n\
         dry_run = false\n",
    );
    let err = fails(&check_file(&path));
    assert!(err.contains(MAN319_LOCATOR_MSG), "{err}");
    assert!(err.contains("\"FN03\" is only 4 characters"), "{err}");
}

#[test]
fn config_check_accepts_a_live_uplink_with_a_six_character_grid() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_cfg(
        dir.path(),
        "grid-live.toml",
        "[server]\nstation_callsign = \"W1AW\"\noperator_grid = \"FN03ab\"\n\
         [[rbn_uplink]]\nenabled = true\ntarget_host = \"127.0.0.1\"\ntarget_port = 7300\n\
         dry_run = false\n",
    );
    let out = succeeds(&check_file(&path));
    assert!(out.contains("dry_run=false"), "{out}");
}

// ---- Scenario 3: a fresh config is scaffolded

fn init_in(dir: &Path, args: &[&str]) -> Output {
    manta()
        .current_dir(dir)
        .args(["config", "init"])
        .args(args)
        .output()
        .unwrap()
}

fn scaffold() -> String {
    let dir = tempfile::tempdir().unwrap();
    succeeds(&init_in(dir.path(), &["--out", "-"]))
}

#[test]
fn init_writes_manta_toml_in_the_current_directory() {
    let dir = tempfile::tempdir().unwrap();
    let o = init_in(dir.path(), &[]);
    assert!(succeeds(&o).is_empty(), "stdout stays empty");
    let written = std::fs::read_to_string(dir.path().join("manta.toml")).unwrap();
    assert_eq!(written, scaffold());
    let err = stderr(&o);
    assert!(err.contains("wrote manta.toml"), "{err}");
    assert!(err.contains("manta config check"), "{err}");
}

#[test]
fn init_refuses_to_overwrite_without_force() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_cfg(dir.path(), "manta.toml", "sentinel");
    let err = fails(&init_in(dir.path(), &[]));
    assert!(err.contains("already exists"), "{err}");
    assert!(err.contains("--force"), "{err}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "sentinel");
}

#[test]
fn init_force_replaces_an_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_cfg(dir.path(), "custom.toml", "sentinel");
    succeeds(&init_in(dir.path(), &["--out", "custom.toml", "--force"]));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), scaffold());
}

#[test]
fn init_out_dash_prints_to_stdout_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let out = succeeds(&init_in(dir.path(), &["--out", "-"]));
    assert!(out.starts_with("# manta configuration file"), "{out}");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn init_into_a_missing_directory_fails_naming_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("no-such-dir").join("manta.toml");
    let err = fails(&init_in(dir.path(), &["--out", target.to_str().unwrap()]));
    assert!(
        err.contains(&format!("writing {}", target.display())),
        "{err}"
    );
    assert!(!dir.path().join("no-such-dir").exists());
}

#[test]
fn init_then_check_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    succeeds(&init_in(dir.path(), &[]));
    let out = succeeds(&check_in(dir.path(), &[]));
    assert!(
        out.starts_with("manta.toml: valid (found in the current directory)\n"),
        "{out}"
    );
    assert!(out.contains("\nserver: none"), "{out}");
}

/// The init'd file with `[server]` and `station_callsign` uncommented and
/// the callsign set to `call`.
fn init_with_server(dir: &Path, call: &str) -> PathBuf {
    succeeds(&init_in(dir, &[]));
    let path = dir.join("manta.toml");
    // The scaffold is include_str!'d, so a CRLF (Windows) checkout writes CRLF.
    let text = std::fs::read_to_string(&path)
        .unwrap()
        .replace("\r\n", "\n");
    assert!(text.contains("\n#[server]\n") && text.contains("\n#station_callsign = \"N0CALL\"\n"));
    let edited = text.replacen("\n#[server]\n", "\n[server]\n", 1).replacen(
        "\n#station_callsign = \"N0CALL\"\n",
        &format!("\nstation_callsign = \"{call}\"\n"),
        1,
    );
    std::fs::write(&path, edited).unwrap();
    path
}

#[test]
fn uncommented_server_block_with_a_real_callsign_passes_check() {
    let dir = tempfile::tempdir().unwrap();
    let path = init_with_server(dir.path(), "W1AW");
    let o = check_file(&path);
    let out = succeeds(&o);
    assert!(
        out.contains(
            "\nserver: station_callsign=W1AW bind_addr=0.0.0.0 metrics_bind_addr=127.0.0.1 \
             telnet_port=7300 json_port=7301 metrics_port=7302 line_format=rbn\n"
        ),
        "{out}"
    );
    let err = stderr(&o);
    assert!(
        err.contains("note: manta run with this config needs your radio's dial frequency"),
        "{err}"
    );
    // MAN-132: telnet/JSON stay public by default; metrics stays local.
    assert!(
        err.contains("note: the telnet and JSON servers listen on every"),
        "{err}"
    );
    assert!(
        !err.contains("note: the metrics endpoint listens on every"),
        "{err}"
    );
}

#[test]
fn unedited_placeholder_fails_check_naming_the_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = init_with_server(dir.path(), "N0CALL");
    let err = fails(&check_file(&path));
    assert!(
        err.contains("server.station_callsign is still the example \"N0CALL\""),
        "{err}"
    );
}

#[test]
fn check_reports_an_unreadable_cty_path() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_cfg(
        dir.path(),
        "manta.toml",
        "[spot]\ncty_path = 'missing.dat'\n",
    );
    let err = fails(&check_file(&cfg));
    assert!(
        err.contains("reading cty.dat file") && err.contains("missing.dat"),
        "{err}"
    );
}

#[test]
fn check_summary_shows_the_resolved_cty_path() {
    let dir = tempfile::tempdir().unwrap();
    let cty = write_cfg(dir.path(), "cty.dat", manta_spot::CTY_DAT);
    let cfg = write_cfg(dir.path(), "manta.toml", "[spot]\ncty_path = 'cty.dat'\n");
    let out = succeeds(&check_file(&cfg));
    assert!(
        out.contains(&format!("cty_path={}", cty.display())),
        "{out}"
    );
    assert!(out.contains("scp_path=bundled"), "{out}");
}

use std::time::SystemTime;
/// Scenario 2 against the real clock, without depending on today's date:
/// the binary warns exactly when manta-spot's own age function says the
/// built-in table is stale. Evaluated before and after the spawn; if the
/// day rolls over in between, either answer is accepted.
#[test]
fn check_warns_about_the_built_in_cty_exactly_when_it_is_stale() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_cfg(dir.path(), "manta.toml", "");
    let before = manta_spot::vintage::stale_cty_dat_age_days(SystemTime::now());
    let o = check_file(&path);
    let after = manta_spot::vintage::stale_cty_dat_age_days(SystemTime::now());
    succeeds(&o);
    let err = stderr(&o);
    let warned = err.contains("warning: the built-in cty.dat is ");
    assert!(
        warned == before.is_some() || warned == after.is_some(),
        "{err}"
    );
    if warned {
        // The line names the age the library computes for the same moment.
        assert!(
            [before, after]
                .iter()
                .flatten()
                .any(|d| err.contains(&format!("is {d} days old"))),
            "{err}"
        );
    }
}

// ---- MAN-268: `run` refuses the example callsigns, and only those

/// `run`'s guard for a `[server]` config whose audio/file source has no dial
/// frequency: the next check after the station guard, reached without I/O.
const DIAL_REQUIRED: &str = "--dial-freq-hz is required with --config";
const STATION_REFUSAL: &str =
    "server.station_callsign is still the example \"N0CALL\" -- set your own callsign";

fn run_with(dir: &Path, config: &Path, env: &[(&str, &str)], args: &[&str]) -> Output {
    let mut cmd = manta();
    cmd.current_dir(dir).args(["run", "--config"]).arg(config);
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

/// MAN-268: `run` checks the station after the `MANTA_*` overlay, so the
/// identity it refuses is the one it would have spotted as.
#[test]
fn run_checks_the_station_after_the_environment_overlay() {
    let dir = tempfile::tempdir().unwrap();
    let real = write_cfg(dir.path(), "real.toml", SERVER_TOML);
    let o = run_with(
        dir.path(),
        &real,
        &[("MANTA_SERVER_STATION_CALLSIGN", "N0CALL")],
        &[],
    );
    let err = fails(&o);
    assert!(err.contains(STATION_REFUSAL), "{err}");
    assert!(!err.contains(DIAL_REQUIRED), "{err}");
    assert!(stdout(&o).is_empty(), "{}", stdout(&o));

    let placeholder = write_cfg(
        dir.path(),
        "placeholder.toml",
        &SERVER_TOML.replace("W1AW", "N0CALL"),
    );
    let err = fails(&run_with(dir.path(), &placeholder, &[], &[]));
    assert!(err.contains(STATION_REFUSAL), "{err}");
    let err = fails(&run_with(
        dir.path(),
        &placeholder,
        &[("MANTA_SERVER_STATION_CALLSIGN", "W3XYZ")],
        &[],
    ));
    assert!(
        err.contains(DIAL_REQUIRED) && !err.contains("station_callsign"),
        "a real call from the environment must pass the station guard: {err}"
    );
}

/// MAN-268 refuses exactly N0CALL; every callsign form the loader already
/// accepts still reaches the same no-dial error as before.
#[test]
fn run_accepts_real_callsigns_including_portable_and_ssid_forms() {
    let dir = tempfile::tempdir().unwrap();
    for call in [
        "W3XYZ",
        "w3xyz",
        "W5AU-1",
        "K5ARH/QRP",
        "JW/LB2PG",
        "GB3LER/B",
    ] {
        let path = write_cfg(dir.path(), "manta.toml", &SERVER_TOML.replace("W1AW", call));
        let err = fails(&run_with(dir.path(), &path, &[], &[]));
        assert!(
            err.contains(DIAL_REQUIRED) && !err.contains("station_callsign"),
            "{call}: {err}"
        );
    }
}

#[test]
fn check_never_warns_about_cty_age_with_an_override() {
    // Holds on any date: an operator-supplied table is never age-checked.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("cty.dat"), manta_spot::CTY_DAT).unwrap();
    let path = write_cfg(dir.path(), "manta.toml", "[spot]\ncty_path = \"cty.dat\"\n");
    let o = check_file(&path);
    succeeds(&o);
    assert!(!stderr(&o).contains("built-in cty.dat"), "{}", stderr(&o));
}

/// A source flag replaces the whole `[input]` table (MAN-261), so `run`
/// must not refuse an unused placeholder there: the broad `<...>` scan stays
/// in `config check`, and only the station identity is checked at startup.
#[test]
fn run_lets_a_source_flag_replace_a_placeholder_receiver_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_cfg(
        dir.path(),
        "manta.toml",
        &format!(
            "{SERVER_TOML}[input]\ntype = \"kiwi\"\nhost = \"<your-receiver-host>\"\n\
             freq_hz = 7030000.0\n"
        ),
    );
    let err = fails(&check_file(&path));
    assert!(
        err.contains("input.host is still a placeholder (\"<your-receiver-host>\")"),
        "{err}"
    );

    // The missing WAV stops `run` at its first source read, after every
    // startup check, without contacting the placeholder host.
    let o = run_with(
        dir.path(),
        &path,
        &[],
        &["--source", "missing.wav", "--dial-freq-hz", "14060000"],
    );
    let err = fails(&o);
    assert!(
        err.contains("note: --source selects the source; ignoring [input] (type = \"kiwi\")"),
        "{err}"
    );
    assert!(err.contains("(os error 2)"), "{err}");
    assert!(!err.contains("placeholder"), "{err}");
    assert!(!err.contains("station_callsign"), "{err}");
}

/// MAN-268: an uncommented example uplink login is refused by `run` too --
/// no source flag replaces `[[rbn_uplink]]` -- before the no-dial guard.
#[test]
fn run_refuses_the_example_uplink_login() {
    let dir = tempfile::tempdir().unwrap();
    let uplink = "\n[[rbn_uplink]]\nenabled = true\ntarget_host = \"rbn.example.org\"\n\
                  target_port = 7000\ndry_run = true\n";
    let path = write_cfg(
        dir.path(),
        "manta.toml",
        &format!("{SERVER_TOML}{uplink}login_callsign = \"n0call\"\n"),
    );
    let o = run_with(dir.path(), &path, &[], &[]);
    let err = fails(&o);
    assert!(
        err.contains("rbn_uplink.login_callsign is still the example \"N0CALL\""),
        "{err}"
    );
    assert!(!err.contains(DIAL_REQUIRED), "{err}");
    assert!(stdout(&o).is_empty(), "{}", stdout(&o));

    let path = write_cfg(
        dir.path(),
        "manta.toml",
        &format!("{SERVER_TOML}{uplink}login_callsign = \"W3XYZ\"\n"),
    );
    let err = fails(&run_with(dir.path(), &path, &[], &[]));
    assert!(err.contains(DIAL_REQUIRED), "{err}");
}

#[test]
fn check_summary_reports_pattern_level_and_resolved_path() {
    let dir = tempfile::tempdir().unwrap();
    let patterns = dir.path().join("patterns.lst");
    std::fs::write(&patterns, "+W1@@\n").unwrap();
    let cfg = write_cfg(
        dir.path(),
        "manta.toml",
        "[spot]\npattern_path = 'patterns.lst'\n",
    );
    let out = succeeds(&check_file(&cfg));
    assert!(
        out.lines().any(|line| line.ends_with(&format!(
            "validation_level=normal pattern_path={}",
            patterns.display()
        ))),
        "{out}"
    );
}

// ---- MAN-13: several [[input]] entries

/// Three ka9q channels of one radiod, the design section's topology B.
const THREE_INPUTS_TOML: &str =
    "[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"127.0.0.1\"\n\
     telnet_port = 0\njson_port = 0\nmetrics_port = 0\n\n\
     [[input]]\nname = \"40m\"\ntype = \"ka9q\"\ngroup = \"239.1.2.3\"\nssrc = 7030000\n\n\
     [[input]]\nname = \"20m\"\ntype = \"ka9q\"\ngroup = \"239.1.2.3\"\nssrc = 14030000\n\n\
     [[input]]\nname = \"15m\"\ntype = \"ka9q\"\ngroup = \"239.1.2.3\"\nssrc = 21030000\n\
     freq_correction_ppm = 0.8\n";

#[test]
fn check_prints_one_input_line_per_entry() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_cfg(dir.path(), "manta.toml", THREE_INPUTS_TOML);
    let out = succeeds(&check_file(&path));
    let inputs: Vec<&str> = out.lines().filter(|l| l.starts_with("input")).collect();
    assert_eq!(
        inputs,
        [
            "input 40m: type=ka9q group=239.1.2.3 port=5004 ssrc=7030000 interface=default",
            "input 20m: type=ka9q group=239.1.2.3 port=5004 ssrc=14030000 interface=default",
            "input 15m: type=ka9q group=239.1.2.3 port=5004 ssrc=21030000 interface=default \
             freq_correction_ppm=0.8",
        ],
        "{out}"
    );
}

#[test]
fn check_single_input_line_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_cfg(
        dir.path(),
        "manta.toml",
        "[input]\ntype = \"ka9q\"\ngroup = \"239.1.2.3\"\nssrc = 7030000\n\
         freq_correction_ppm = 0.8\n",
    );
    let out = succeeds(&check_file(&path));
    assert!(
        out.lines().any(|l| l
            == "input: type=ka9q group=239.1.2.3 port=5004 ssrc=7030000 interface=default \
                freq_correction_ppm=0.8"),
        "{out}"
    );
}

#[test]
fn check_binds_no_port_and_opens_no_receiver_with_several_inputs() {
    // Hold the three server ports, and two fake KiwiSDRs that must see no
    // connection.
    let held: Vec<TcpListener> = (0..3)
        .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    let ports: Vec<u16> = held
        .iter()
        .map(|l| l.local_addr().unwrap().port())
        .collect();
    let kiwis: Vec<TcpListener> = (0..2)
        .map(|_| {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.set_nonblocking(true).unwrap();
            l
        })
        .collect();
    let mut body = format!(
        "[server]\nstation_callsign = \"W1AW\"\nbind_addr = \"127.0.0.1\"\n\
         telnet_port = {}\njson_port = {}\nmetrics_port = {}\n",
        ports[0], ports[1], ports[2]
    );
    for (i, kiwi) in kiwis.iter().enumerate() {
        body.push_str(&format!(
            "\n[[input]]\nname = \"k{i}\"\ntype = \"kiwi\"\nhost = \"127.0.0.1\"\nport = {}\n\
             freq_hz = 7030000.0\n",
            kiwi.local_addr().unwrap().port()
        ));
    }
    let dir = tempfile::tempdir().unwrap();
    let path = write_cfg(dir.path(), "manta.toml", &body);
    let out = succeeds(&check_file(&path));
    assert!(out.contains("input k0: type=kiwi"), "{out}");
    assert!(out.contains("input k1: type=kiwi"), "{out}");
    for kiwi in &kiwis {
        match kiwi.accept() {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Ok((_, peer)) => panic!("config check connected to a receiver from {peer}"),
            Err(e) => panic!("unexpected accept error: {e}"),
        }
    }
}

#[test]
fn shared_hpsdr_check_is_offline_and_reports_declaration() {
    let dir = tempfile::tempdir().unwrap();
    let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    udp.set_nonblocking(true).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut body = format!(
        "[server]\nstation_callsign = \"W1AW\"\ntelnet_port = {}\n",
        listener.local_addr().unwrap().port()
    );
    for (i, name) in ["80m", "40m", "20m", "15m"].iter().enumerate() {
        body += &format!("[[input]]\nname = \"{name}\"\ntype = \"hpsdr\"\nhost = \"127.0.0.1\"\nport = {}\nfreq_hz = 7025000.0\nrate_hz = 192000.0\n{}", udp.local_addr().unwrap().port(), if i == 0 { "receivers = 4\n" } else { "" });
    }
    let path = write_cfg(dir.path(), "four.toml", &body);
    let output = check_file(&path);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("receivers=4"));
    assert!(stderr(&output).contains("is checked when manta run starts"));
    assert!(udp.recv_from(&mut [0; 100]).is_err());
    let body = body
        + "[[input]]\nname = \"10m\"\ntype = \"hpsdr\"\nhost = \"127.0.0.1\"\n"
        + &format!(
            "port = {}\nfreq_hz = 28025000.0\nrate_hz = 192000.0\n",
            udp.local_addr().unwrap().port()
        );
    std::fs::write(&path, body).unwrap();
    assert!(fails(&check_file(&path)).contains("declares 4 receivers; 5 bands are configured"));
}

// MAN-310: `bands` plans a receiver's streams from the CW segment table.

const KIWI: &str = "[input]\ntype = \"kiwi\"\nhost = \"kiwi.example.net\"\n";
const HL2: &str = "[input]\ntype = \"hpsdr\"\nhost = \"192.168.1.40\"\nrate_hz = 96000.0\n";

fn check_body(body: &str) -> Output {
    let dir = tempfile::tempdir().unwrap();
    check_file(&write_cfg(dir.path(), "m.toml", body))
}

fn input_lines(out: &str) -> Vec<&str> {
    out.lines().filter(|l| l.starts_with("input")).collect()
}

/// Scenario 4: a bare centre in kHz is one stream on that frequency.
#[test]
fn a_bare_centre_in_bands_is_one_stream_on_that_frequency() {
    for bands in ["[\"7030\"]", "\"7030\"", "7030"] {
        let o = check_body(&format!("{KIWI}bands = {bands}\n"));
        let out = succeeds(&o);
        assert_eq!(
            input_lines(&out),
            ["input: type=kiwi host=kiwi.example.net port=8073 freq_hz=7030000 password=none"],
            "bands = {bands}"
        );
        assert!(
            stderr(&o).contains("note: [input]: bands = [\"7030\"] plans 1 stream: 7030 kHz\n"),
            "{}",
            stderr(&o)
        );
    }
}

/// Scenario 2: a band outside the tuning range is skipped, with a note.
#[test]
fn a_band_outside_the_tuning_range_is_skipped_with_a_note() {
    let o = check_body(&format!(
        "{KIWI}bands = [\"160m\", \"60m\"]\ntuning_range_hz = [3000000.0, 30000000.0]\n"
    ));
    let out = succeeds(&o);
    assert_eq!(
        input_lines(&out),
        [
            "input kiwi-60m-1: type=kiwi host=kiwi.example.net port=8073 freq_hz=5355250 \
             password=none",
            "input kiwi-60m-2: type=kiwi host=kiwi.example.net port=8073 freq_hz=5362750 \
             password=none",
        ]
    );
    assert!(
        stderr(&o).starts_with(
            "note: [input]: bands = [\"160m\", \"60m\"] plans 2 streams: 60m (2)\n\
             note: [input]: 160m skipped: 1810 to 1840 kHz is outside the receiver's tuning \
             range, 3000 to 30000 kHz\n"
        ),
        "{}",
        stderr(&o)
    );
}

/// Scenario 3, config side: the split streams print exactly what two
/// hand-written entries at the same centres print.
#[test]
fn a_band_wider_than_one_stream_is_split_into_named_streams() {
    let planned = succeeds(&check_body(&format!("{KIWI}bands = [\"60m\"]\n")));
    let by_hand = succeeds(&check_body(
        "[[input]]\nname = \"kiwi-60m-1\"\ntype = \"kiwi\"\nhost = \"kiwi.example.net\"\n\
         freq_hz = 5355250.0\n\n[[input]]\nname = \"kiwi-60m-2\"\ntype = \"kiwi\"\n\
         host = \"kiwi.example.net\"\nfreq_hz = 5362750.0\n",
    ));
    assert_eq!(input_lines(&planned), input_lines(&by_hand));
    assert_eq!(input_lines(&planned).len(), 2);
}

/// Scenario 1, config side: "all" on one HPSDR radio names the plan.
#[test]
fn bands_all_on_one_hpsdr_radio_is_refused_naming_the_14_stream_plan() {
    let err = fails(&check_body(&format!("{HL2}bands = [\"all\"]\n")));
    assert!(
        err.ends_with(
            "[input]: input.bands plans 14 streams (160m, 80m, 60m, 40m, 30m, 20m (2), 17m, \
             15m (2), 12m, 10m (3)), but bands opens one stream on an HPSDR radio; list bands \
             that fit one stream (a 96000 Hz stream skims 86.899 kHz), or give each its own \
             [[input]] on that radio (several receivers on one radio are one entry each)\n"
        ),
        "{err}"
    );
}

#[test]
fn one_band_that_fits_an_hpsdr_stream_is_that_stream() {
    let o = check_body(&format!("{HL2}bands = [\"40m\"]\n"));
    assert_eq!(
        input_lines(&succeeds(&o)),
        ["input: type=hpsdr host=192.168.1.40 port=1024 protocol=auto freq_hz=7020000 rate_hz=96000"]
    );
    assert!(stderr(&o).contains("note: [input]: bands = [\"40m\"] plans 1 stream: 40m\n"));
}

#[test]
fn a_soapy_device_refuses_a_two_stream_plan() {
    let err = fails(&check_body(
        "[input]\ntype = \"soapy\"\ndriver = \"driver=rtlsdr\"\nrate_hz = 96000.0\n\
         bands = \"15m\"\n",
    ));
    assert!(
        err.ends_with(
            "[input]: input.bands plans 2 streams (15m (2)), but manta opens one stream from a \
             SoapySDR device; list bands that fit one stream (a 96000 Hz stream skims 86.899 \
             kHz), or give each its own device\n"
        ),
        "{err}"
    );
}

#[test]
fn bands_all_on_a_kiwi_exceeds_the_input_cap_naming_each_band() {
    let err = fails(&check_body(&format!("{KIWI}bands = \"all\"\n")));
    assert!(
        err.ends_with(
            "m.toml: input.bands plans 86 streams for [input] (160m (4), 80m (9), 60m (2), \
             40m (5), 30m (4), 20m (12), 17m (5), 15m (17), 12m (5), 10m (23)), and manta runs \
             at most 16 inputs; list fewer bands\n"
        ),
        "{err}"
    );
}

#[test]
fn stream_plans_across_entries_share_the_input_cap() {
    let entry = |name: &str| {
        format!(
            "[[input]]\nname = \"{name}\"\ntype = \"kiwi\"\nhost = \"kiwi.example.net\"\n\
             bands = [\"80m\"]\n"
        )
    };
    let err = fails(&check_body(&format!(
        "{}\n{}",
        entry("kiwi-a"),
        entry("kiwi-b")
    )));
    assert!(
        err.ends_with(
            "m.toml: the [[input]] entries plan 18 streams (\"kiwi-a\" 9, \"kiwi-b\" 9), and \
             manta runs at most 16 inputs; list fewer bands\n"
        ),
        "{err}"
    );
}

#[test]
fn bands_and_freq_hz_together_are_refused() {
    let err = fails(&check_body(&format!(
        "{KIWI}freq_hz = 7030000.0\nbands = \"40m\"\n"
    )));
    assert!(
        err.ends_with(
            "[input]: input.bands and input.freq_hz both set the receiver's centre; keep one\n"
        ),
        "{err}"
    );
}

#[test]
fn tuning_range_needs_bands_and_a_low_below_high() {
    let err = fails(&check_body(&format!(
        "{KIWI}freq_hz = 7030000.0\ntuning_range_hz = [0.0, 30000000.0]\n"
    )));
    assert!(
        err.ends_with("[input]: input.tuning_range_hz applies only with input.bands\n"),
        "{err}"
    );
    let err = fails(&check_body(&format!(
        "{KIWI}bands = \"40m\"\ntuning_range_hz = [30000000.0, 3000000.0]\n"
    )));
    assert!(
        err.ends_with(
            "[input]: input.tuning_range_hz must be [low, high] in Hz with 0 <= low < high, \
             got [30000000.0, 3000000.0]\n"
        ),
        "{err}"
    );
}

#[test]
fn an_unknown_band_lists_the_vocabulary() {
    let err = fails(&check_body(&format!("{KIWI}bands = [\"6m\"]\n")));
    assert!(
        err.ends_with(
            "[input]: input.bands: \"6m\" is not \"all\", a band (160m, 80m, 60m, 40m, 30m, \
             20m, 17m, 15m, 12m or 10m) or a centre frequency in kHz such as \"7030\"\n"
        ),
        "{err}"
    );
    let err = fails(&check_body(&format!("{KIWI}bands = true\n")));
    assert!(
        err.ends_with(
            "[input]: input.bands must be \"all\", a band such as \"40m\", a centre frequency \
             in kHz such as \"7030\", or a list of them; got true\n"
        ),
        "{err}"
    );
}

#[test]
fn bands_on_a_ka9q_input_does_not_apply() {
    let err = fails(&check_body(
        "[input]\ntype = \"ka9q\"\ngroup = \"239.101.6.1\"\nbands = \"40m\"\n",
    ));
    assert!(
        err.ends_with(
            "[input]: input.bands does not apply to input.type = \"ka9q\" (ka9q takes group, \
             port, ssrc, interface)\n"
        ),
        "{err}"
    );
}

#[test]
fn a_kiwi_without_freq_hz_or_bands_names_both() {
    let err = fails(&check_body(KIWI));
    assert!(
        err.ends_with(
            "[input]: input.freq_hz or input.bands is required when input.type = \"kiwi\"\n"
        ),
        "{err}"
    );
}

#[test]
fn generated_stream_names_must_fit_and_be_unique() {
    let err = fails(&check_body(
        "[input]\nname = \"a-very-long-kiwi-receiver-name\"\ntype = \"kiwi\"\n\
         host = \"kiwi.example.net\"\nbands = \"60m\"\n",
    ));
    assert!(
        err.ends_with(
            "m.toml: the stream name \"a-very-long-kiwi-receiver-name-60m-1\" is longer than 32 \
             characters; shorten the entry's name\n"
        ),
        "{err}"
    );
    let err = fails(&check_body(
        "[[input]]\nname = \"kiwi-60m-1\"\ntype = \"kiwi\"\nhost = \"other.example.net\"\n\
         freq_hz = 7030000.0\n\n[[input]]\nname = \"kiwi\"\ntype = \"kiwi\"\n\
         host = \"kiwi.example.net\"\nbands = \"60m\"\n",
    ));
    assert!(
        err.ends_with("m.toml: the stream name \"kiwi-60m-1\" is already input \"kiwi-60m-1\"\n"),
        "{err}"
    );
}

#[test]
fn nothing_in_range_is_refused() {
    let err = fails(&check_body(&format!(
        "{KIWI}bands = \"160m\"\ntuning_range_hz = [3000000.0, 30000000.0]\n"
    )));
    assert!(
        err.ends_with(
            "[input]: input.bands leaves nothing to skim inside the receiver's tuning range, \
             3000 to 30000 kHz: 160m (1810 to 1840 kHz) is outside it\n"
        ),
        "{err}"
    );
}

#[test]
fn bands_from_the_environment() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_cfg(dir.path(), "m.toml", KIWI);
    let run = |value: &str| {
        manta()
            .current_dir(dir.path())
            .env("MANTA_INPUT_BANDS", value)
            .args(["config", "check", "--config"])
            .arg(&cfg)
            .output()
            .unwrap()
    };
    let out = succeeds(&run("7030"));
    assert!(out.contains("freq_hz=7030000"), "{out}");
    let err = fails(&run("all"));
    assert!(
        err.contains("and manta runs at most 16 inputs; list fewer bands"),
        "{err}"
    );
}

#[test]
fn environment_bands_replace_the_file_frequency() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_cfg(dir.path(), "m.toml", &format!("{KIWI}freq_hz = 7040000\n"));
    let checked = manta()
        .current_dir(dir.path())
        .env("MANTA_INPUT_BANDS", "7030")
        .args(["config", "check", "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    let out = succeeds(&checked);
    assert!(out.contains("freq_hz=7030000"), "{out}");
    assert!(out.contains("environment: MANTA_INPUT_BANDS\n"), "{out}");
    assert_eq!(
        stderr(&checked),
        "note: [input]: bands = [\"7030\"] plans 1 stream: 7030 kHz\n"
    );
}

#[test]
fn environment_frequency_replaces_the_file_bands_and_range() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_cfg(
        dir.path(),
        "m.toml",
        &format!("{KIWI}bands = \"7030\"\ntuning_range_hz = [3000000, 30000000]\n"),
    );
    let checked = manta()
        .current_dir(dir.path())
        .env("MANTA_INPUT_FREQ_HZ", "7040000")
        .args(["config", "check", "--config"])
        .arg(&cfg)
        .output()
        .unwrap();
    let out = succeeds(&checked);
    assert!(out.contains("freq_hz=7040000"), "{out}");
    assert!(out.contains("environment: MANTA_INPUT_FREQ_HZ\n"), "{out}");
    assert!(stderr(&checked).is_empty(), "{}", stderr(&checked));
}

/// MAN-306: config check prints the canonical MAC without opening a radio.
#[test]
fn hpsdr_discovery_config_check_normalizes_mac_without_opening_a_radio() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_cfg(dir.path(), "manta.toml", "[input]\ntype = \"hpsdr\"\nfreq_hz = 7030000.0\nrate_hz = 192000.0\nmac = \"aa-bbcc-ddee-ff\"\n");
    let checked = check_file(&path);
    assert!(succeeds(&checked).contains("mac=AA:BB:CC:DD:EE:FF"));
    let overridden = manta()
        .args(["config", "check", "--config"])
        .arg(&path)
        .env("MANTA_INPUT_MAC", "001122334455")
        .output()
        .unwrap();
    assert!(succeeds(&overridden).contains("mac=00:11:22:33:44:55"));
}
