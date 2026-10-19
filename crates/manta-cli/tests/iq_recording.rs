//! MAN-309 acceptance: `run --record-iq` records the IQ it decodes, and the
//! recording (or headerless cf32/ci16 IQ) replays through `decode` and
//! `run` to the same output on every run. Real binary, `manta gen`-shaped
//! V1 fixtures, no hardware.

mod common;

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
    s.as_chunks::<2>()
        .0
        .iter()
        .map(|&[re, im]| Complex32::new(re, im))
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
fn recorded_gaps_replay_through_run_and_decode_without_splicing() {
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

    // `decode` segments at the gap too: the same bytes on every run, a
    // spot in them, and not the gap-free recording's report.
    let decode = |p: &Path| run_ok(&["decode".as_ref(), p.as_os_str(), "--json".as_ref()]).stdout;
    let report = decode(&gapped);
    assert!(String::from_utf8_lossy(&report).contains("W1AW"));
    assert_eq!(decode(&gapped), report);
    assert_eq!(decode(&gapped), report);
    assert_ne!(decode(&smooth), report);

    // `oracle` reads a gap-free recording as it reads the WAV, and refuses
    // to splice a gapped one.
    let csv = dir.path().join("rbn.csv");
    std::fs::write(
        &csv,
        "callsign,de_pfx,de_cont,freq,band,dx,dx_pfx,dx_cont,mode,db,date,speed,tx_mode\n",
    )
    .unwrap();
    let oracle = |p: &Path| {
        manta()
            .args([
                "oracle".as_ref(),
                p.as_os_str(),
                csv.as_os_str(),
                "--capture-start".as_ref(),
                "2026-10-19T00:00:00Z".as_ref(),
            ])
            .output()
            .unwrap()
    };
    let from_wav = oracle(&wav);
    assert!(from_wav.status.success());
    assert_eq!(oracle(&smooth).stdout, from_wav.stdout);
    let out = oracle(&gapped);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("has input gaps"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
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

    // The same file described by `[input]` keys (relative to the config)
    // runs exactly as the flags describe it; `config check` reports them.
    let cfg = dir.path().join("manta.toml");
    std::fs::write(
        &cfg,
        format!(
            "[input]\ntype = \"file\"\npath = \"external.cf32\"\niq = true\n\
             iq_format = \"cf32\"\nsample_rate_hz = {rate}\ncenter_freq_hz = {centre}\n"
        ),
    )
    .unwrap();
    let from_config = run_ok(&[
        "run".as_ref(),
        "--config".as_ref(),
        cfg.as_os_str(),
        "--json".as_ref(),
    ]);
    assert_eq!(from_config.stdout, out.stdout);
    let check = run_ok(&[
        "config".as_ref(),
        "check".as_ref(),
        "--config".as_ref(),
        cfg.as_os_str(),
    ]);
    let check = String::from_utf8_lossy(&check.stdout).into_owned()
        + &String::from_utf8_lossy(&check.stderr);
    assert!(
        check.contains(&format!("iq_format=cf32 sample_rate_hz={rate}")),
        "{check}"
    );

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

#[test]
fn a_service_config_records_relative_to_its_file_and_config_check_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let svc = dir.path().join("svc");
    std::fs::create_dir(&svc).unwrap();
    let (wav, manifest) = v1_fixture(&svc);
    let toml = svc.join("manta.toml");
    std::fs::write(
        &toml,
        "[input]\ntype = \"file\"\npath = \"v1.wav\"\niq = true\nrecord_iq = \"cap.cf32\"\n",
    )
    .unwrap();
    let rec = svc.join("cap.cf32");

    let check = run_ok(&[
        "config".as_ref(),
        "check".as_ref(),
        "--config".as_ref(),
        toml.as_os_str(),
    ]);
    let summary = String::from_utf8_lossy(&check.stdout).into_owned()
        + &String::from_utf8_lossy(&check.stderr);
    assert!(
        summary.contains(&format!("record_iq={}", rec.display())),
        "{summary}"
    );
    assert!(!rec.exists());
    assert!(!manta_input::partial_metadata_path(&rec).exists());

    // Run from another directory: the path is the config file's, not the
    // working directory's.
    let out = manta()
        .current_dir(dir.path())
        .args(["run", "--json", "--config"])
        .arg(&toml)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let recs = records(&rec);
    assert_eq!(
        recs.last().unwrap(),
        &MetadataRecord::End {
            sample_count: (manifest.fs * manifest.duration_s) as u64,
            reason: EndReason::Eof
        }
    );
    assert_eq!(std::fs::read(&rec).unwrap(), cf32_bytes(&wav_samples(&wav)));
    // A restart with the same destination fails and touches nothing.
    let before = std::fs::read(metadata_path(&rec)).unwrap();
    let out = manta()
        .args(["run", "--json", "--config"])
        .arg(&toml)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(std::fs::read(metadata_path(&rec)).unwrap(), before);
}

#[test]
fn each_input_of_a_multi_input_daemon_records_its_own_file() {
    let dir = tempfile::tempdir().unwrap();
    let (wav, manifest) = v1_fixture(dir.path());
    let entry = |name: &str, rec: &str| {
        format!(
            "[[input]]\nname = \"{name}\"\ntype = \"file\"\npath = \"{}\"\niq = true\n\
             record_iq = \"{rec}\"\n\n",
            wav.display()
        )
    };
    let toml = dir.path().join("multi.toml");
    std::fs::write(&toml, entry("a", "a.cf32") + &entry("b", "b.cf32")).unwrap();
    let out = manta()
        .args(["run", "--json", "--config"])
        .arg(&toml)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    let n = (manifest.fs * manifest.duration_s) as u64;
    for rec in ["a.cf32", "b.cf32"] {
        let rec = dir.path().join(rec);
        assert!(
            stderr.contains(&format!(
                "recording finished: {}, {n} samples",
                rec.display()
            )),
            "{stderr}"
        );
        assert_eq!(std::fs::read(&rec).unwrap(), cf32_bytes(&wav_samples(&wav)));
    }

    // Two inputs on one destination, or --record-iq with several inputs,
    // fail before anything opens.
    let dup = dir.path().join("dup.toml");
    std::fs::write(&dup, entry("a", "same.cf32") + &entry("b", "same.cf32")).unwrap();
    let out = manta()
        .args(["run", "--config"])
        .arg(&dup)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("give each its own record_iq"));
    assert!(!dir.path().join("same.cf32").exists());
    let out = manta()
        .args(["run", "--record-iq", "x.cf32", "--config"])
        .arg(&toml)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--record-iq records one input"));
}

/// `dir/v1.wav` rendered for `seconds`.
#[cfg(unix)]
fn v1_fixture_secs(dir: &Path, seconds: f64) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let spec = manta_testkit::vectors::VectorSpec {
        duration_s: seconds,
        ..manta_testkit::vectors::v1()
    };
    manta_testkit::vectors::write_fixture_set(&spec, dir).unwrap();
    dir.join("v1.wav")
}

/// `manta` limited to writing files of at most `bytes` (a write past it
/// fails with EFBIG instead of raising SIGXFSZ): a storage failure for the
/// recorder only, since stdout and stderr are pipes.
#[cfg(unix)]
fn manta_with_file_size_limit(bytes: u64) -> Command {
    use std::os::unix::process::CommandExt;
    let mut cmd = manta();
    // SAFETY: async-signal-safe libc calls only, between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
            let limit = libc::rlimit {
                rlim_cur: bytes as libc::rlim_t,
                rlim_max: bytes as libc::rlim_t,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd
}

/// A recording that cannot be written warns once, leaves decoding exactly
/// as without it, publishes no complete pair and makes `run` exit 1.
#[cfg(unix)]
#[test]
fn a_recording_write_failure_keeps_decoding_and_fails_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let wav = v1_fixture_secs(&dir.path().join("long"), 30.0);
    let args = |extra: &[&std::ffi::OsStr]| {
        let mut a: Vec<std::ffi::OsString> = vec![
            "run".into(),
            "--source".into(),
            wav.as_os_str().into(),
            "--source-iq".into(),
            "--json".into(),
        ];
        a.extend(extra.iter().map(|s| s.to_os_string()));
        a
    };
    let plain = run_ok(&args(&[]).iter().map(|a| a.as_os_str()).collect::<Vec<_>>());
    assert!(String::from_utf8_lossy(&plain.stdout).contains("W1AW"));

    let rec = dir.path().join("session.cf32");
    let out = manta_with_file_size_limit(1 << 20)
        .args(args(&["--record-iq".as_ref(), rec.as_os_str()]))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert_eq!(out.stdout, plain.stdout, "decoding changed: {stderr}");
    let warning = format!("IQ recording stopped: write failed for {}", rec.display());
    assert_eq!(stderr.matches(&warning).count(), 1, "{stderr}");
    assert!(stderr.contains("decoding continues"), "{stderr}");
    assert!(!stderr.contains("recording finished:"), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "error: IQ recording {} is incomplete: it stopped early",
            rec.display()
        )),
        "{stderr}"
    );
    assert!(!metadata_path(&rec).exists());
    assert!(manta_input::partial_metadata_path(&rec).exists());
}

/// In a two-input daemon one recording failing leaves the other input's
/// recording to finish, and the run still exits 1.
#[cfg(unix)]
#[test]
fn one_failing_recording_of_a_multi_input_daemon_leaves_the_other() {
    let dir = tempfile::tempdir().unwrap();
    let short = v1_fixture_secs(&dir.path().join("short"), 2.0);
    let long = v1_fixture_secs(&dir.path().join("long"), 30.0);
    let entry = |name: &str, wav: &Path, rec: &str| {
        format!(
            "[[input]]\nname = \"{name}\"\ntype = \"file\"\npath = \"{}\"\niq = true\n\
             record_iq = \"{rec}\"\n\n",
            wav.display()
        )
    };
    let toml = dir.path().join("multi.toml");
    std::fs::write(
        &toml,
        entry("a", &short, "a.cf32") + &entry("b", &long, "b.cf32"),
    )
    .unwrap();
    // 2 s at 96 kS/s is 1.5 MB of cf32; 30 s is 23 MB.
    let out = manta_with_file_size_limit(4 << 20)
        .args(["run", "--json", "--config"])
        .arg(&toml)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    let (a, b) = (dir.path().join("a.cf32"), dir.path().join("b.cf32"));
    assert!(
        stderr.contains(&format!(
            "recording finished: {}, 192000 samples",
            a.display()
        )),
        "{stderr}"
    );
    assert_eq!(std::fs::read(&a).unwrap(), cf32_bytes(&wav_samples(&short)));
    assert!(
        stderr.contains(&format!(
            "IQ recording stopped: write failed for {}",
            b.display()
        )),
        "{stderr}"
    );
    assert!(!metadata_path(&b).exists());
}

/// A stop signal ends a never-ending (looping, paced) input through `run`'s
/// shutdown path, which finalizes the recording as `stopped`.
#[cfg(unix)]
#[test]
fn a_stop_signal_finalizes_the_recording() {
    use std::io::BufRead;
    let dir = tempfile::tempdir().unwrap();
    let wav = v1_fixture_secs(&dir.path().join("fixture"), 30.0);
    let rec = dir.path().join("live.cf32");
    let mut child = manta()
        .args([
            "run".as_ref(),
            "--source".as_ref(),
            wav.as_os_str(),
            "--source-iq".as_ref(),
            "--loop".as_ref(),
            "--json".as_ref(),
            "--record-iq".as_ref(),
            rec.as_os_str(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let stderr = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut all = String::new();
        for line in std::io::BufReader::new(stderr).lines() {
            let line = line.unwrap();
            let _ = tx.send(line.clone());
            all.push_str(&line);
            all.push('\n');
        }
        all
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let line = rx
            .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            .expect("run never started recording");
        if line.contains("recording IQ to") {
            break;
        }
    }
    // Some paced samples, then a stop.
    std::thread::sleep(std::time::Duration::from_secs(2));
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
    let status = child.wait().unwrap();
    let stderr = reader.join().unwrap();
    assert!(status.success(), "{status:?}: {stderr}");
    assert!(
        stderr.contains(&format!("recording finished: {}", rec.display())),
        "{stderr}"
    );
    let records = records(&rec);
    let Some(MetadataRecord::End {
        sample_count,
        reason,
    }) = records.last()
    else {
        panic!("no end record: {records:?}");
    };
    assert_eq!(*reason, EndReason::Stopped);
    assert!(*sample_count > 0);
    assert_eq!(std::fs::metadata(&rec).unwrap().len(), sample_count * 8);
}

/// The live path: a (fake) KiwiSDR drops its connection and comes back.
/// `run` keeps decoding through the reconnect, and its recording holds the
/// receiver's rate, centre and passband and the outage as a gap record at
/// the stored sample where the stream resumed.
#[cfg(target_os = "linux")]
#[test]
fn a_live_input_outage_is_recorded_as_a_gap() {
    use common::{
        accept_kiwi_session, expect_line, line_channel, send_snd_frames, wait_bounded, KillOnDrop,
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const WAIT: Duration = Duration::from_secs(60);
    let dir = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let done = Arc::new(AtomicBool::new(false));
    let fake = std::thread::spawn({
        let done = done.clone();
        move || -> Result<(), String> {
            // Past the 2 s calibration, then a hang-up; the second session
            // streams until the test ends.
            for secs in [4, 120] {
                let mut ws = accept_kiwi_session(&listener, &done)?;
                let until = Instant::now() + Duration::from_secs(secs);
                send_snd_frames(&mut ws, || {
                    done.load(Ordering::Relaxed) || Instant::now() >= until
                });
            }
            Ok(())
        }
    });
    let rec = dir.path().join("kiwi.cf32");
    let mut child = KillOnDrop(
        manta()
            .args([
                "run".as_ref(),
                "--kiwi-host".as_ref(),
                "127.0.0.1".as_ref(),
                "--kiwi-port".as_ref(),
                port.as_ref(),
                "--kiwi-freq-hz".as_ref(),
                "14025000".as_ref(),
                "--json".as_ref(),
                "--record-iq".as_ref(),
                rec.as_os_str(),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stderr = line_channel(child.0.stderr.take().unwrap());
    expect_line(&stderr, &["recording IQ to"], WAIT);
    expect_line(&stderr, &["reconnected after"], WAIT);
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(unsafe { libc::kill(child.0.id() as i32, libc::SIGINT) }, 0);
    let finished = expect_line(&stderr, &["recording finished:"], WAIT);
    assert!(finished.contains(&rec.display().to_string()), "{finished}");
    let status = wait_bounded(&mut child.0, WAIT).expect("run did not exit");
    assert!(status.success(), "{status:?}");
    done.store(true, Ordering::Relaxed);
    let _ = fake.join();

    let records = records(&rec);
    let Some(MetadataRecord::Header(header)) = records.first() else {
        panic!("no header: {records:?}");
    };
    assert_eq!(header.center_freq_hz, 14_025_000.0);
    assert!(header.sample_rate_hz > 0.0);
    let (lo, hi) = (header.rf_passband_hz[0], header.rf_passband_hz[1]);
    assert!(
        hi - lo < header.sample_rate_hz,
        "a KiwiSDR carries less than its Nyquist span: {header:?}"
    );
    let gaps: Vec<_> = records
        .iter()
        .filter_map(|r| match r {
            MetadataRecord::Gap { before_sample, .. } => Some(*before_sample),
            _ => None,
        })
        .collect();
    let Some(MetadataRecord::End {
        sample_count,
        reason,
    }) = records.last()
    else {
        panic!("no end record: {records:?}");
    };
    assert_eq!(*reason, EndReason::Stopped);
    // The reconnect inside the stream; a stop can add an outage at the end
    // (MAN-258), recorded as a terminal gap at `sample_count`.
    let inside = gaps.iter().filter(|&&g| g > 0 && g < *sample_count).count();
    assert_eq!(inside, 1, "{records:?}");
    assert!(
        gaps.iter().all(|&g| g > 0 && g <= *sample_count),
        "{records:?}"
    );
    assert_eq!(std::fs::metadata(&rec).unwrap().len(), sample_count * 8);
}
