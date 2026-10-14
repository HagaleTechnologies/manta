//! MAN-119 against the real `manta decode` binary: 24-bit integer IQ WAVs
//! decode like their Float32 originals, a WAV shorter than its header says
//! fails naming the file, unsupported sample formats name the accepted ones,
//! and the decode streams instead of holding the recording in memory.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The binary with every `MANTA_*` variable removed, as in `cli.rs`.
fn manta() -> Command {
    without_manta_env(Command::new(env!("CARGO_BIN_EXE_manta")))
}

/// `cmd` with every `MANTA_*` variable removed from its environment.
fn without_manta_env(mut cmd: Command) -> Command {
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"MANTA_") {
            cmd.env_remove(key);
        }
    }
    cmd
}

/// V1 shortened to 30 s, written into `dir` as Float32 with its sidecar;
/// returns the WAV path. The same fixture as `cli.rs`'s `short_v1`: a 15 s
/// render decodes but emits no spot.
fn short_v1_wav(dir: &Path) -> PathBuf {
    let spec = manta_testkit::vectors::VectorSpec {
        duration_s: 30.0,
        ..manta_testkit::vectors::v1()
    };
    manta_testkit::vectors::write_fixture_set(&spec, dir).unwrap();
    dir.join(format!("{}.wav", spec.name))
}

/// Writes `json` as the `<stem>.json` sidecar of `wav`.
fn write_sidecar(wav: &Path, json: &str) {
    std::fs::write(wav.with_extension("json"), json).unwrap();
}

/// `float_wav` re-encoded as stereo PCM24 at `out`, with a
/// `center_freq_hz` 14 000 000 sidecar. hound writes it as
/// WAVE_FORMAT_EXTENSIBLE.
fn pcm24_copy(float_wav: &Path, out: &Path) {
    let mut reader = hound::WavReader::open(float_wav).unwrap();
    let spec = hound::WavSpec {
        bits_per_sample: 24,
        sample_format: hound::SampleFormat::Int,
        ..reader.spec()
    };
    let mut w = hound::WavWriter::create(out, spec).unwrap();
    for s in reader.samples::<f32>() {
        let v = (f64::from(s.unwrap()) * 8_388_608.0).round();
        w.write_sample(v.clamp(-8_388_608.0, 8_388_607.0) as i32)
            .unwrap();
    }
    w.finalize().unwrap();
    write_sidecar(out, r#"{"center_freq_hz": 14000000.0}"#);
}

/// `manta decode <wav> --json`, asserted to exit 0, as parsed JSON.
fn decode_json(wav: &Path) -> serde_json::Value {
    let out = manta()
        .arg("decode")
        .arg(wav)
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

/// The callsigns in a `decode --json` report's `spots`.
fn spot_calls(report: &serde_json::Value) -> Vec<String> {
    report["spots"]
        .as_array()
        .expect("decode --json output must include a 'spots' array")
        .iter()
        .map(|s| s["callsign"].as_str().unwrap().to_string())
        .collect()
}

/// Asserts `out` exited 1 and returns its stderr.
fn failed_stderr(out: &Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    stderr
}

#[test]
fn decode_reads_a_24_bit_iq_wav() {
    let dir = tempfile::tempdir().unwrap();
    let float_wav = short_v1_wav(dir.path());
    let pcm24_wav = dir.path().join("v1_24.wav");
    pcm24_copy(&float_wav, &pcm24_wav);

    let want = decode_json(&float_wav);
    let got = decode_json(&pcm24_wav);
    assert_eq!(got["text"], want["text"]);
    assert_eq!(spot_calls(&want), ["W1AW"]);
    assert_eq!(spot_calls(&got), spot_calls(&want));
}

#[test]
fn decode_reports_a_truncated_wav_naming_it() {
    // The header declares 1000 Float32 frames; the file holds 10.
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("trunc.wav");
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 96_000,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(&wav, spec).unwrap();
    for i in 0..1000 {
        w.write_sample(i as f32 / 1000.0).unwrap();
        w.write_sample(0.0f32).unwrap();
    }
    w.finalize().unwrap();
    let len = std::fs::metadata(&wav).unwrap().len();
    let file = std::fs::OpenOptions::new().write(true).open(&wav).unwrap();
    file.set_len(len - 990 * 8).unwrap();

    let out = manta()
        .arg("decode")
        .arg(&wav)
        .args(["--center-freq-hz", "14000000"])
        .output()
        .unwrap();
    let stderr = failed_stderr(&out);
    assert!(stderr.contains("read WAV "), "stderr: {stderr}");
    assert!(stderr.contains("trunc.wav"), "stderr: {stderr}");
}

#[test]
fn decode_rejects_an_8_bit_wav_naming_the_accepted_formats() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("fix8.wav");
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 96_000,
        bits_per_sample: 8,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(&wav, spec).unwrap();
    w.write_sample(64i8).unwrap();
    w.write_sample(-64i8).unwrap();
    w.finalize().unwrap();
    write_sidecar(&wav, r#"{"center_freq_hz": 14000000.0}"#);

    let out = manta().arg("decode").arg(&wav).output().unwrap();
    let stderr = failed_stderr(&out);
    assert!(
        stderr.contains("unsupported WAV format Int/8-bit (need Float32 or Int16/24/32)"),
        "stderr: {stderr}"
    );
}

/// MAN-119: `manta decode` holds no whole-file copy. 96 MiB of address space is
/// ~3x the streaming decode's measured need (completes under 32 MiB) and less
/// than one full Complex32 copy of the 120 s V1 render (92 MB) plus the binary;
/// measured: HEAD and a source-only-streaming build both abort under it.
#[cfg(target_os = "linux")]
#[test]
fn decode_runs_in_bounded_memory() {
    let dir = tempfile::tempdir().unwrap();
    // The full 120 s V1 render: Float32, 92 MB, plus its sidecar.
    let gen = manta()
        .args(["gen", "v1", "--out"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(
        gen.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&gen.stderr)
    );
    let wav = dir.path().join("v1.wav");

    // `sh -c 'ulimit -v'` (KiB) rather than an `unsafe` `pre_exec` setrlimit.
    // Each rayon worker reserves its own stack, so the default one-per-CPU
    // decoder pool alone can exceed the limit on a many-core machine
    // (measured: 32 CPUs fail to start the pool, EAGAIN, before any sample
    // is read). Two workers keep the limit measuring the input's footprint;
    // the output does not depend on the pool size.
    let out = without_manta_env(Command::new("sh"))
        .args(["-c", "ulimit -v 98304 && exec \"$0\" \"$@\""])
        .arg(env!("CARGO_BIN_EXE_manta"))
        .args(["decode", wav.to_str().unwrap(), "--json"])
        .env("RAYON_NUM_THREADS", "2")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "status: {:?}; stderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(spot_calls(&report), ["W1AW"]);
}
