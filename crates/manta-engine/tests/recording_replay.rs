//! MAN-309: an IQ recording made while a source is decoded replays through
//! `decode_source` to the same report as the original source -- its gaps,
//! clock and source guards included -- whatever sizes the replay's reads
//! return, and recording does not change the decode it rides on.

use anyhow::Result;
use manta_engine::{decode_source, listen, DecodeReport, PipelineConfig};
use manta_input::{start_recording, IqSource, RecordedIqSource, RecordingOutcome};
use manta_testkit::keyer::{key_text_loop, KeyerSpec};
use num_complex::Complex32;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// A source of `parts`, never read across a part boundary; part `i`'s gap
/// is reported once with its first read, `terminal` with the EOF read.
struct Gapped {
    parts: Vec<(Option<u64>, Vec<Complex32>)>,
    terminal: Option<u64>,
    fs: f64,
    center: f64,
    part: usize,
    at: usize,
    pending: Option<u64>,
}

impl IqSource for Gapped {
    fn sample_rate(&self) -> f64 {
        self.fs
    }

    fn center_freq_hz(&self) -> f64 {
        self.center
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        loop {
            let Some((gap, iq)) = self.parts.get(self.part) else {
                self.pending = self.terminal.take();
                return Ok(0);
            };
            if self.at == iq.len() {
                self.part += 1;
                self.at = 0;
                continue;
            }
            if self.at == 0 {
                self.pending = *gap;
            }
            let n = buf.len().min(iq.len() - self.at);
            buf[..n].copy_from_slice(&iq[self.at..self.at + n]);
            self.at += n;
            return Ok(n);
        }
    }

    fn take_discontinuity(&mut self) -> Option<u64> {
        self.pending.take()
    }
}

/// Forwards `inner`, at most `step` samples per read, with every source
/// property and discontinuity.
struct Stepped<S> {
    inner: S,
    step: usize,
}

impl<S: IqSource> IqSource for Stepped<S> {
    fn sample_rate(&self) -> f64 {
        self.inner.sample_rate()
    }
    fn center_freq_hz(&self) -> f64 {
        self.inner.center_freq_hz()
    }
    fn rf_passband_hz(&self) -> (f64, f64) {
        self.inner.rf_passband_hz()
    }
    fn trusted_band_hz(&self) -> Option<(f64, f64)> {
        self.inner.trusted_band_hz()
    }
    fn analytic_guard_hz(&self) -> f64 {
        self.inner.analytic_guard_hz()
    }
    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        let n = self.step.min(buf.len());
        self.inner.read(&mut buf[..n])
    }
    fn take_discontinuity(&mut self) -> Option<u64> {
        self.inner.take_discontinuity()
    }
}

/// A fresh scratch directory for this process and test.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("manta-man309-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn report_text(report: &DecodeReport) -> String {
    format!("{report:?}")
}

/// Decodes `src` through a recorder writing `path`, finishes the recording
/// and returns the report.
fn decode_recorded(src: Box<dyn IqSource>, path: &std::path::Path) -> DecodeReport {
    let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_792_407_600);
    let (mut rec, handle) = start_recording(src, path, start, None).unwrap();
    let report = decode_source(&mut rec, &PipelineConfig::default()).unwrap();
    drop(rec);
    match handle.finish(Duration::from_secs(5)) {
        RecordingOutcome::Finished { .. } => {}
        other => panic!("recording did not finish: {other:?}"),
    }
    report
}

#[test]
fn a_recorded_gapped_source_replays_to_the_same_report() {
    let mut spec = manta_testkit::vectors::v1();
    spec.duration_s = 30.0;
    let r = manta_testkit::vectors::render(&spec).unwrap();
    let split = 1_440_123;
    let missing = (5.0 * spec.fs) as u64;
    let source = || Gapped {
        parts: vec![
            (Some(0), r.samples[..split].to_vec()),
            (Some(missing), r.samples[split..].to_vec()),
        ],
        terminal: Some(9_600),
        fs: spec.fs,
        center: spec.center_freq_hz,
        part: 0,
        at: 0,
        pending: None,
    };
    let cfg = PipelineConfig::default();
    let want = report_text(&decode_source(&mut source(), &cfg).unwrap());
    assert!(want.contains("W1AW"), "{want}");

    let dir = scratch("gapped");
    let path = dir.join("session.cf32");
    // Recording does not change the decode it rides on.
    assert_eq!(
        report_text(&decode_recorded(Box::new(source()), &path)),
        want
    );
    let summary = manta_input::read_recording_summary(&path).unwrap();
    assert_eq!(summary.sample_count, r.samples.len() as u64);
    assert!(summary.has_gaps());
    for step in [1, 4095, 4096, 4097, 65_536] {
        let replay = Stepped {
            inner: RecordedIqSource::open(&path).unwrap(),
            step,
        };
        let got = decode_source(&mut { replay }, &cfg).unwrap();
        assert_eq!(report_text(&got), want, "replay read size {step}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The MAN-273 fixture: a keyed tone in +20 dB noise through the Hilbert
/// front end, optionally decimated.
fn keyed_audio(tone_hz: f64, capture_rate_hz: Option<f64>) -> Box<dyn IqSource> {
    let fs = 48_000.0;
    let spec = KeyerSpec::new(20.0);
    let (env, _) = key_text_loop("CQ CQ DE W1AW W1AW K", &spec, fs, 15.0).unwrap();
    let mut real = vec![0.0f32; env.len()];
    let dphi = std::f64::consts::TAU * tone_hz / fs;
    let mut phi = 0.0f64;
    for (i, r) in real.iter_mut().enumerate() {
        *r = env.get(i).copied().unwrap_or(0.0) * phi.cos() as f32;
        phi += dphi;
    }
    let sigma = manta_testkit::noise::real_awgn_sigma_for_snr_2500(20.0, fs);
    manta_testkit::noise::add_real_awgn(&mut real, sigma, 0x1234_5678);
    let audio = manta_input::AudioIqSource::new(Box::new(coppa_audio::WavSource::from_samples(
        real, 48_000,
    )))
    .unwrap();
    match capture_rate_hz {
        Some(rate) => Box::new(manta_input::DecimatingSource::new(Box::new(audio), rate).unwrap()),
        None => Box::new(audio),
    }
}

fn emitting_tracks(report: &DecodeReport) -> std::collections::BTreeSet<u32> {
    report
        .events
        .iter()
        .filter_map(|e| match e {
            manta_decode::events::DecoderEvent::CharDecoded { track_id, .. } => Some(*track_id),
            _ => None,
        })
        .collect()
}

/// A recording of rig audio keeps its analytic guard, passband and (when
/// decimated) trusted band, and the offline decode applies them as
/// `listen` does: the replay makes the same spawn and mirror-veto
/// decisions as the live source, one emitting track per clean tone.
#[test]
fn a_recording_keeps_the_audio_source_guards() {
    for (name, tone_hz, rate) in [("audio", 750.0, None), ("decimated", 2600.0, Some(6_000.0))] {
        let original = keyed_audio(tone_hz, rate);
        let bands = (
            original.sample_rate(),
            original.rf_passband_hz(),
            original.trusted_band_hz(),
            original.analytic_guard_hz(),
        );
        assert!(bands.3 > 0.0);
        let cfg = PipelineConfig::default();
        let want = decode_source(keyed_audio(tone_hz, rate).as_mut(), &cfg).unwrap();
        assert!(want.text.contains("W1AW"), "{name}: {}", want.text);
        assert_eq!(emitting_tracks(&want).len(), 1, "{name}: {want:?}");

        let dir = scratch(name);
        let path = dir.join("rig.cf32");
        assert_eq!(
            report_text(&decode_recorded(original, &path)),
            report_text(&want),
            "{name}"
        );
        let replay = RecordedIqSource::open(&path).unwrap();
        assert_eq!(
            (
                replay.sample_rate(),
                replay.rf_passband_hz(),
                replay.trusted_band_hz(),
                replay.analytic_guard_hz()
            ),
            bands,
            "{name}"
        );
        let got = decode_source(&mut { replay }, &cfg).unwrap();
        assert_eq!(report_text(&got), report_text(&want), "{name}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

/// Every event `listen` emits for `src`, as text.
fn listen_events(src: Box<dyn IqSource>) -> Vec<String> {
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = events.clone();
    listen(
        src,
        &PipelineConfig::default(),
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        move |ev| sink.lock().unwrap().push(format!("{ev:?}")),
        |_spot| {},
    )
    .unwrap();
    let events = events.lock().unwrap().clone();
    events
}

/// The live path: `listen` over a source with a post-calibration gap and an
/// outage at EOF emits the same events with recording on or off, and the
/// recording replays through `listen` to those events again.
#[test]
fn listen_is_unchanged_by_recording_and_its_replay_matches() {
    listen_round_trip(1_440_123, "listen", true);
}

/// As above, with the outage inside `listen`'s 2 s calibration window
/// (1 s in), whose samples the live path discards.
#[test]
fn a_gap_during_calibration_is_unchanged_by_recording_and_replays() {
    listen_round_trip(96_000, "listen-calibration", false);
}

fn listen_round_trip(split: usize, name: &str, expect_closure: bool) {
    let mut spec = manta_testkit::vectors::v1();
    spec.duration_s = 30.0;
    let r = manta_testkit::vectors::render(&spec).unwrap();
    let source = || Gapped {
        parts: vec![
            (None, r.samples[..split].to_vec()),
            (Some(4 * spec.fs as u64), r.samples[split..].to_vec()),
        ],
        terminal: Some(9_600),
        fs: spec.fs,
        center: spec.center_freq_hz,
        part: 0,
        at: 0,
        pending: None,
    };
    let want = listen_events(Box::new(source()));
    assert!(!want.is_empty(), "no events");
    if expect_closure {
        assert!(
            want.iter().any(|e| e.contains("Bookkeeping")),
            "no discontinuity closure"
        );
    }

    let dir = scratch(name);
    let path = dir.join("live.cf32");
    let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_792_407_600);
    let (rec, handle) = start_recording(Box::new(source()), &path, start, None).unwrap();
    assert_eq!(listen_events(Box::new(rec)), want);
    match handle.finish(Duration::from_secs(5)) {
        RecordingOutcome::Finished { samples, .. } => {
            assert_eq!(samples, r.samples.len() as u64)
        }
        other => panic!("recording did not finish: {other:?}"),
    }
    let replay = RecordedIqSource::open(&path).unwrap();
    assert_eq!(listen_events(Box::new(replay)), want);
    std::fs::remove_dir_all(&dir).unwrap();
}
