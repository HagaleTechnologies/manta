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

#[cfg(not(feature = "hpsdr"))]
#[test]
fn check_rejects_an_hpsdr_input_on_a_build_without_hpsdr() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_cfg(
        dir.path(),
        "manta.toml",
        "[input]\ntype = \"hpsdr\"\nhost = \"h\"\nfreq_hz = 7e6\nrate_hz = 48000.0\n",
    );
    let err = fails(&check_file(&path));
    assert!(
        err.contains("input.type = \"hpsdr\" needs a manta built with --features hpsdr"),
        "{err}"
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
        .map(|l| l.split_once(':').map_or(l, |(p, _)| p))
        .collect();
    assert_eq!(
        prefixes,
        [
            path.display().to_string().as_str(),
            "environment",
            "server",
            "rbn_uplink",
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
    let text = std::fs::read_to_string(&path).unwrap();
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
