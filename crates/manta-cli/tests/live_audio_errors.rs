//! MAN-131 scenario 1 against the real `manta` binary: a live audio input
//! that cannot be found is reported with what was requested, the 48000 Hz
//! requirement and the command that lists the selectable inputs, by every
//! command that opens one.
//!
//! Safe on both CI runners: enumerating devices needs no microphone
//! permission (`source_check.rs`'s `devices_json_smoke…` already does it),
//! and a name that matches nothing never opens a stream. The one test that
//! opens a stream is Linux-only and uses ALSA's software null device.

use std::process::Command;

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

#[test]
fn a_missing_audio_input_names_itself_the_rate_and_manta_devices_in_every_command() {
    let name = format!("manta-man131-no-such-input-{}", std::process::id());
    let name = name.as_str();
    for args in [
        vec!["run", "--device", name, "--dial-freq-hz", "14030000"],
        vec!["listen", "--device", name, "--dial-freq-hz", "14030000"],
        vec![
            "soak",
            "--device",
            name,
            "--dial-freq-hz",
            "14030000",
            "--duration",
            "3",
        ],
        vec![
            "doctor",
            "--device",
            name,
            "--dial-freq-hz",
            "14030000",
            "--duration",
            "3",
        ],
        vec!["check", "--device", name],
    ] {
        let out = manta().args(&args).output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {stderr}");
        let want = format!(
            "no audio input matching \"{name}\" (manta needs a 48000 Hz input) -- \
             run `manta devices` to list the inputs --device can select"
        );
        assert!(
            stderr.contains(&want),
            "{args:?}: {want:?} missing from {stderr}"
        );
    }
}

/// ALSA's software null capture device, which delivers only exact zeros,
/// as `manta devices` names it.
#[cfg(target_os = "linux")]
const ALSA_NULL_INPUT: &str =
    "\"Discard all samples (playback) or generate zero samples (capture)\"";

/// MAN-131: `run`'s digital-silence warning goes through MAN-124's logging
/// helper, so under `--log-format json` it is a WARN record, and under
/// `text` it is the same plain line as before. Only the warning line is
/// parsed: alsa-lib may write its own diagnostics straight to stderr.
/// Linux only: it needs ALSA's null capture device, and returns early when
/// `manta devices` does not list one.
#[cfg(target_os = "linux")]
#[test]
fn run_writes_the_digital_silence_warning_in_the_chosen_log_format() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let devices = manta().arg("devices").output().unwrap();
    if !String::from_utf8_lossy(&devices.stdout).contains(ALSA_NULL_INPUT) {
        eprintln!("skipping: no ALSA null capture device listed by `manta devices`");
        return;
    }
    let want = format!("warning: audio input {ALSA_NULL_INPUT} is delivering digital silence");
    for format in ["json", "text"] {
        let mut child = manta()
            .args([
                "run",
                "--log-format",
                format,
                "--cty",
                concat!(env!("CARGO_MANIFEST_DIR"), "/../manta-spot/data/cty.dat"),
                "--device",
                "generate zero samples",
                "--dial-freq-hz",
                "14030000",
            ])
            .env_remove("NO_COLOR")
            .env_remove("RUST_LOG")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                if tx.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut lines = Vec::new();
        let found = loop {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(line) => {
                    let hit = line.contains("digital silence");
                    lines.push(line);
                    if hit {
                        break true;
                    }
                }
                Err(_) => break false,
            }
        };
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(found, "{format}: no silence warning in {lines:#?}");
        let warning = lines.last().unwrap();
        if format == "json" {
            let record: serde_json::Value = serde_json::from_str(warning)
                .unwrap_or_else(|e| panic!("json: not JSON ({e}): {warning:?}"));
            assert_eq!(record["level"], "WARN", "{warning}");
            let message = record["message"].as_str().unwrap();
            assert!(message.starts_with(&want), "json: {message}");
        } else {
            assert!(warning.starts_with(&want), "text: {warning}");
        }
    }
}
