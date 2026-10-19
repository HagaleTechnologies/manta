//! MAN-309 acceptance: `run --record-iq` records the IQ it decodes, and the
//! recording (or headerless cf32/ci16 IQ) replays through `decode` and
//! `run` to the same output on every run. Real binary, `manta gen`-shaped
//! V1 fixtures, no hardware.

use manta_input::recording_format::MetadataRecord;
use manta_input::{metadata_path, EndReason, RecordingHeader};
use num_complex::Complex32;
use std::path::{Path, PathBuf};
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

fn run_ok(args: &[&std::ffi::OsStr]) -> Output {
    let out = manta().args(args).output().unwrap();
    assert!(
        out.status.success(),
        "manta {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn stderr_of(args: &[&std::ffi::OsStr]) -> (i32, String) {
    let out = manta().args(args).output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// `dir/v1.wav` (+ sidecar, centre 14 MHz): V1 rendered for 30 s.
fn v1_fixture(dir: &Path) -> (PathBuf, manta_testkit::vectors::Manifest) {
    let spec = manta_testkit::vectors::VectorSpec {
        duration_s: 30.0,
        ..manta_testkit::vectors::v1()
    };
    let manifest = manta_testkit::vectors::write_fixture_set(&spec, dir).unwrap();
    (dir.join("v1.wav"), manifest)
}

fn wav_samples(path: &Path) -> Vec<Complex32> {
    let mut r = hound::WavReader::open(path).unwrap();
    let s: Vec<f32> = r.samples::<f32>().map(Result::unwrap).collect();
    s.chunks_exact(2)
        .map(|c| Complex32::new(c[0], c[1]))
        .collect()
}

fn cf32_bytes(samples: &[Complex32]) -> Vec<u8> {
    let mut b = Vec::with_capacity(samples.len() * 8);
    for s in samples {
        b.extend_from_slice(&s.re.to_le_bytes());
        b.extend_from_slice(&s.im.to_le_bytes());
    }
    b
}

/// The metadata records of a recording.
fn records(payload: &Path) -> Vec<MetadataRecord> {
    std::fs::read_to_string(metadata_path(payload))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn a_run_records_its_input_and_decodes_exactly_as_without_recording() {
    let dir = tempfile::tempdir().unwrap();
    let (wav, manifest) = v1_fixture(dir.path());
    let rec = dir.path().join("session.cf32");
    let wav = wav.as_os_str();
    let plain = run_ok(&[
        "run".as_ref(),
        "--source".as_ref(),
        wav,
        "--source-iq".as_ref(),
        "--json".as_ref(),
    ]);
    let recorded = run_ok(&[
        "run".as_ref(),
        "--source".as_ref(),
        wav,
        "--source-iq".as_ref(),
        "--json".as_ref(),
        "--record-iq".as_ref(),
        rec.as_os_str(),
    ]);
    // Decoding continues unaffected: the same stdout, byte for byte.
    assert!(!plain.stdout.is_empty());
    assert_eq!(plain.stdout, recorded.stdout);
    let stderr = String::from_utf8_lossy(&recorded.stderr);
    let n = (manifest.fs * manifest.duration_s) as u64;
    assert!(
        stderr.contains(&format!(
            "recording IQ to {}; metadata: {}",
            rec.display(),
            metadata_path(&rec).display()
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "recording finished: {}, {n} samples",
            rec.display()
        )),
        "{stderr}"
    );
    // Rate, centre and start time; the replay's epoch is the WAV's mtime.
    let mtime = std::fs::metadata(wav).unwrap().modified().unwrap();
    let start = mtime.duration_since(std::time::UNIX_EPOCH).unwrap();
    let recs = records(&rec);
    let MetadataRecord::Header(h) = &recs[0] else {
        panic!("{recs:?}")
    };
    assert_eq!(
        h,
        &RecordingHeader {
            schema: "manta-iq.v1".into(),
            sample_format: "cf32_le".into(),
            sample_rate_hz: manifest.fs,
            center_freq_hz: manifest.center_freq_hz,
            start_unix_seconds: start.as_secs() as i64,
            start_nanoseconds: start.subsec_nanos(),
            rf_passband_hz: [-manifest.fs / 2.0, manifest.fs / 2.0],
            trusted_band_hz: None,
            analytic_guard_hz: 0.0,
        }
    );
    assert_eq!(
        recs.last().unwrap(),
        &MetadataRecord::End {
            sample_count: n,
            reason: EndReason::Eof
        }
    );
    // The payload is the decoded samples, bit for bit.
    assert_eq!(
        std::fs::read(&rec).unwrap(),
        cf32_bytes(&wav_samples(Path::new(wav)))
    );
}

#[test]
fn a_recording_replays_to_the_same_spots_on_every_run() {
    let dir = tempfile::tempdir().unwrap();
    let (wav, _) = v1_fixture(dir.path());
    let rec = dir.path().join("session.cf32");
    run_ok(&[
        "run".as_ref(),
        "--source".as_ref(),
        wav.as_os_str(),
        "--source-iq".as_ref(),
        "--json".as_ref(),
        "--record-iq".as_ref(),
        rec.as_os_str(),
    ]);
    // decode: three runs, identical, with the known callsign; and the same
    // report as the WAV it was recorded from.
    let decode = |p: &Path| run_ok(&["decode".as_ref(), p.as_os_str(), "--json".as_ref()]).stdout;
    let first = decode(&rec);
    assert!(String::from_utf8_lossy(&first).contains("W1AW"));
    assert_eq!(decode(&rec), first);
    assert_eq!(decode(&rec), first);
    assert_eq!(decode(&wav), first);
    // run: three runs, identical, with a spot.
    let replay = |p: &Path| {
        run_ok(&[
            "run".as_ref(),
            "--source".as_ref(),
            p.as_os_str(),
            "--source-iq".as_ref(),
            "--json".as_ref(),
        ])
        .stdout
    };
    let first = replay(&rec);
    assert!(String::from_utf8_lossy(&first).contains("W1AW"));
    assert_eq!(replay(&rec), first);
    assert_eq!(replay(&rec), first);
    // A copied pair (new mtime) replays the same.
    let copy = dir.path().join("copy.cf32");
    std::fs::copy(&rec, &copy).unwrap();
    std::fs::copy(metadata_path(&rec), metadata_path(&copy)).unwrap();
    assert_eq!(replay(&copy), first);
}

#[test]
fn recorded_gaps_replay_through_run_and_decode_refuses_to_splice_them() {
    let dir = tempfile::tempdir().unwrap();
    let (wav, manifest) = v1_fixture(dir.path());
    let samples = wav_samples(&wav);
    let header = RecordingHeader {
        schema: "manta-iq.v1".into(),
        sample_format: "cf32_le".into(),
        sample_rate_hz: manifest.fs,
        center_freq_hz: manifest.center_freq_hz,
        start_unix_seconds: 1_792_407_600,
        start_nanoseconds: 0,
        rf_passband_hz: [-manifest.fs / 2.0, manifest.fs / 2.0],
        trusted_band_hz: None,
        analytic_guard_hz: 0.0,
    };
    let write = |name: &str, gap: Option<(u64, u64)>| {
        let p = dir.path().join(name);
        std::fs::write(&p, cf32_bytes(&samples)).unwrap();
        let mut meta = MetadataRecord::Header(header.clone()).to_line();
        if let Some((before_sample, missing_samples)) = gap {
            meta += &MetadataRecord::Gap {
                before_sample,
                missing_samples,
            }
            .to_line();
        }
        meta += &MetadataRecord::End {
            sample_count: samples.len() as u64,
            reason: EndReason::Stopped,
        }
        .to_line();
        std::fs::write(metadata_path(&p), meta).unwrap();
        p
    };
    let fs = manifest.fs as u64;
    let gapped = write("gapped.cf32", Some((15 * fs, 10 * fs)));
    let smooth = write("smooth.cf32", None);
    let replay = |p: &Path| {
        run_ok(&[
            "run".as_ref(),
            "--source".as_ref(),
            p.as_os_str(),
            "--source-iq".as_ref(),
            "--json".as_ref(),
        ])
        .stdout
    };
    let first = replay(&gapped);
    assert!(!first.is_empty());
    assert_eq!(replay(&gapped), first);
    assert_eq!(replay(&gapped), first);
    // The outage is not spliced away: the events differ from the gap-free
    // recording of the same samples.
    assert_ne!(replay(&smooth), first);

    let (code, stderr) = stderr_of(&["decode".as_ref(), gapped.as_os_str()]);
    assert_eq!(code, 1);
    assert!(stderr.contains("has input gaps"), "{stderr}");
    let (code, stderr) = stderr_of(&[
        "run".as_ref(),
        "--source".as_ref(),
        gapped.as_os_str(),
        "--source-iq".as_ref(),
        "--capture-rate-hz".as_ref(),
        "48000".as_ref(),
    ]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains(
            "cannot decimate an IQ recording with gaps; replay at its recorded sample rate"
        ),
        "{stderr}"
    );
}

#[test]
fn an_existing_destination_fails_startup_and_is_left_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let (wav, _) = v1_fixture(dir.path());
    let rec = dir.path().join("session.cf32");
    std::fs::write(&rec, b"keep").unwrap();
    let (code, stderr) = stderr_of(&[
        "run".as_ref(),
        "--source".as_ref(),
        wav.as_os_str(),
        "--source-iq".as_ref(),
        "--record-iq".as_ref(),
        rec.as_os_str(),
    ]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains(&format!(
            "error: IQ recording destination already exists: {}",
            rec.display()
        )),
        "{stderr}"
    );
    assert_eq!(std::fs::read(&rec).unwrap(), b"keep");
    assert!(!metadata_path(&rec).exists());
}

#[test]
fn headerless_cf32_and_ci16_decode_like_the_equivalent_wavs() {
    let dir = tempfile::tempdir().unwrap();
    let (wav, manifest) = v1_fixture(dir.path());
    let samples = wav_samples(&wav);
    let rate = format!("{}", manifest.fs);
    let centre = format!("{}", manifest.center_freq_hz);

    let raw = dir.path().join("external.cf32");
    std::fs::write(&raw, cf32_bytes(&samples)).unwrap();
    let decode_raw = |p: &Path, format: &str| {
        run_ok(&[
            "decode".as_ref(),
            p.as_os_str(),
            "--iq-format".as_ref(),
            format.as_ref(),
            "--sample-rate-hz".as_ref(),
            rate.as_ref(),
            "--center-freq-hz".as_ref(),
            centre.as_ref(),
            "--json".as_ref(),
        ])
        .stdout
    };
    let wav_report = run_ok(&[
        "decode".as_ref(),
        wav.as_os_str(),
        "--center-freq-hz".as_ref(),
        centre.as_ref(),
        "--json".as_ref(),
    ])
    .stdout;
    assert!(String::from_utf8_lossy(&wav_report).contains("W1AW"));
    assert_eq!(decode_raw(&raw, "cf32"), wav_report);

    // ci16 against the same samples quantized into a PCM16 WAV.
    let q = |v: f32| (v * 32768.0).round().clamp(-32768.0, 32767.0) as i16;
    let pcm = dir.path().join("pcm16.wav");
    let mut w = hound::WavWriter::create(
        &pcm,
        hound::WavSpec {
            channels: 2,
            sample_rate: manifest.fs as u32,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        },
    )
    .unwrap();
    let mut ci16 = Vec::new();
    for s in &samples {
        for v in [q(s.re), q(s.im)] {
            w.write_sample(v).unwrap();
            ci16.extend_from_slice(&v.to_le_bytes());
        }
    }
    w.finalize().unwrap();
    let raw16 = dir.path().join("external.ci16");
    std::fs::write(&raw16, ci16).unwrap();
    let pcm_report = run_ok(&[
        "decode".as_ref(),
        pcm.as_os_str(),
        "--center-freq-hz".as_ref(),
        centre.as_ref(),
        "--json".as_ref(),
    ])
    .stdout;
    assert_eq!(decode_raw(&raw16, "ci16"), pcm_report);

    // run takes the same file with --dial-freq-hz as its centre.
    let run_raw = |extra: &[&std::ffi::OsStr]| {
        let mut args: Vec<&std::ffi::OsStr> = vec![
            "run".as_ref(),
            "--source".as_ref(),
            raw.as_os_str(),
            "--source-iq".as_ref(),
            "--iq-format".as_ref(),
            "cf32".as_ref(),
            "--sample-rate-hz".as_ref(),
            rate.as_ref(),
            "--json".as_ref(),
        ];
        args.extend_from_slice(extra);
        args.iter().map(|a| a.to_os_string()).collect::<Vec<_>>()
    };
    let with_dial = run_raw(&["--dial-freq-hz".as_ref(), centre.as_ref()]);
    let out = manta().args(&with_dial).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("W1AW"));

    // Missing metadata: the command-specific message.
    let (code, stderr) = stderr_of(&[
        "decode".as_ref(),
        raw.as_os_str(),
        "--iq-format".as_ref(),
        "cf32".as_ref(),
    ]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("error: headerless IQ requires --sample-rate-hz and --center-freq-hz"),
        "{stderr}"
    );
    let out = manta().args(run_raw(&[])).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("error: headerless IQ requires --sample-rate-hz and --dial-freq-hz"),
        "{stderr}"
    );
}
