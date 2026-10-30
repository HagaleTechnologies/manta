//! MAN-119: `decode_source` streams a source through the same pipeline as
//! `decode_samples`, with the same 4096-sample chunks of the zero-padded
//! stream whatever sizes the source's reads return. MAN-309: and segments
//! the decode at each discontinuity the source reports.

use anyhow::{bail, Result};
use manta_decode::events::{ClosureKind, DecoderEvent};
use manta_engine::{decode_samples, decode_source, PipelineConfig};
use manta_input::IqSource;
use num_complex::Complex32;

/// A source that returns at most `step` samples per read, and fails with
/// "stepped source failed" once `fail_after` samples have been read.
struct Stepped<'a> {
    iq: &'a [Complex32],
    fs: f64,
    center: f64,
    step: usize,
    fail_after: Option<usize>,
    read: usize,
}

impl<'a> Stepped<'a> {
    fn new(iq: &'a [Complex32], fs: f64, center: f64, step: usize) -> Self {
        Stepped {
            iq,
            fs,
            center,
            step,
            fail_after: None,
            read: 0,
        }
    }
}

impl IqSource for Stepped<'_> {
    fn sample_rate(&self) -> f64 {
        self.fs
    }

    fn center_freq_hz(&self) -> f64 {
        self.center
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        if self.fail_after.is_some_and(|limit| self.read >= limit) {
            bail!("stepped source failed");
        }
        let n = self.step.min(buf.len()).min(self.iq.len() - self.read);
        buf[..n].copy_from_slice(&self.iq[self.read..self.read + n]);
        self.read += n;
        Ok(n)
    }
}

/// The report as text. `serde_json` is not a manta-engine dependency;
/// `Debug` prints every field (the serde-skipped ones too) and each float
/// in its shortest round-trip form, so equal `Debug` text implies equal
/// JSON.
fn report_text(report: &manta_engine::DecodeReport) -> String {
    format!("{report:?}")
}

#[test]
fn decode_source_matches_decode_samples_for_any_read_size() {
    let mut spec = manta_testkit::vectors::v1();
    spec.duration_s = 30.0;
    let r = manta_testkit::vectors::render(&spec).unwrap();
    let cfg = PipelineConfig::default();
    let want =
        report_text(&decode_samples(&r.samples, spec.fs, spec.center_freq_hz, &cfg).unwrap());
    assert!(want.contains("W1AW"), "{want}");
    for step in [1, 7, 4095, 4096, 4097, 65_536] {
        let mut src = Stepped::new(&r.samples, spec.fs, spec.center_freq_hz, step);
        let got = decode_source(&mut src, &cfg).unwrap();
        assert_eq!(report_text(&got), want, "read size {step}");
    }
}

#[test]
fn decode_source_reports_digital_silence() {
    let iq = vec![Complex32::new(0.0, 0.0); 96_000];
    let mut src = Stepped::new(&iq, 96_000.0, 14_000_000.0, 4096);
    let err = format!(
        "{:#}",
        decode_source(&mut src, &PipelineConfig::default()).unwrap_err()
    );
    assert!(err.contains("input is digital silence"), "{err}");
}

#[test]
fn decode_source_reports_an_empty_source_as_digital_silence() {
    let cfg = PipelineConfig::default();
    let want = format!(
        "{:#}",
        decode_samples(&[], 96_000.0, 14_000_000.0, &cfg).unwrap_err()
    );
    assert_eq!(want, "input is digital silence");
    let mut src = Stepped::new(&[], 96_000.0, 14_000_000.0, 4096);
    let got = format!("{:#}", decode_source(&mut src, &cfg).unwrap_err());
    assert_eq!(got, want);
}

#[test]
fn decode_source_propagates_a_read_error() {
    let mut spec = manta_testkit::vectors::v1();
    spec.duration_s = 2.0;
    let r = manta_testkit::vectors::render(&spec).unwrap();
    let mut src = Stepped::new(&r.samples, spec.fs, spec.center_freq_hz, 4096);
    src.fail_after = Some(50_000);
    let err = format!(
        "{:#}",
        decode_source(&mut src, &PipelineConfig::default()).unwrap_err()
    );
    assert!(err.contains("stepped source failed"), "{err}");
}

/// MAN-309: a source of `parts` that returns at most `step` samples per
/// read and never reads across a part boundary. Part `i`'s gap is reported
/// once, with its first read; `terminal` is reported with the EOF read.
struct Gapped {
    parts: Vec<(Option<u64>, Vec<Complex32>)>,
    terminal: Option<u64>,
    fs: f64,
    center: f64,
    step: usize,
    part: usize,
    at: usize,
    pending: Option<u64>,
}

impl Gapped {
    fn new(parts: Vec<(Option<u64>, Vec<Complex32>)>, fs: f64, center: f64, step: usize) -> Self {
        Gapped {
            parts,
            terminal: None,
            fs,
            center,
            step,
            part: 0,
            at: 0,
            pending: None,
        }
    }
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
            let n = self.step.min(buf.len()).min(iq.len() - self.at);
            buf[..n].copy_from_slice(&iq[self.at..self.at + n]);
            self.at += n;
            return Ok(n);
        }
    }

    fn take_discontinuity(&mut self) -> Option<u64> {
        self.pending.take()
    }
}

/// An event's own sample timestamp; `None` for the kinds without one
/// (`TrackMeta` may carry 0, "no ordering claim").
fn event_ts(e: &DecoderEvent) -> Option<u64> {
    match e {
        DecoderEvent::CharDecoded { sample_ts, .. }
        | DecoderEvent::WordBoundary { sample_ts, .. }
        | DecoderEvent::TrackPromoted { sample_ts, .. }
        | DecoderEvent::ReadingsHeld { sample_ts, .. }
        | DecoderEvent::TimingUpdate { sample_ts, .. } => Some(*sample_ts),
        _ => None,
    }
}

fn event_track(e: &DecoderEvent) -> u32 {
    match e {
        DecoderEvent::CharDecoded { track_id, .. }
        | DecoderEvent::WordBoundary { track_id, .. }
        | DecoderEvent::SpeedUpdate { track_id, .. }
        | DecoderEvent::TrackMeta { track_id, .. }
        | DecoderEvent::ReadingsHeld { track_id, .. }
        | DecoderEvent::TimingUpdate { track_id, .. }
        | DecoderEvent::TrackPromoted { track_id, .. }
        | DecoderEvent::TrackClosed { track_id, .. } => *track_id,
    }
}

fn v1_samples(seconds: f64) -> (Vec<Complex32>, f64, f64) {
    let mut spec = manta_testkit::vectors::v1();
    spec.duration_s = seconds;
    let r = manta_testkit::vectors::render(&spec).unwrap();
    (r.samples, spec.fs, spec.center_freq_hz)
}

/// MAN-309: a gap closes the segment at the boundary (a `Bookkeeping`
/// closure, as `listen` does), and the next segment's events are timed
/// after the missing samples, on fresh track ids -- wherever the boundary
/// falls in the 4096-sample chunking, and whatever the read size.
#[test]
fn a_gap_restarts_the_segment_after_the_missing_samples() {
    let (iq, fs, center) = v1_samples(30.0);
    let missing = (5.0 * fs) as u64;
    let cfg = PipelineConfig::default();
    // Inside a canonical chunk, and on a chunk boundary of the source's
    // samples.
    for split in [1_440_123usize, 4096 * 350] {
        let mut want = None;
        for step in [1, 4095, 4096, 4097, 65_536] {
            let parts = vec![
                (None, iq[..split].to_vec()),
                (Some(missing), iq[split..].to_vec()),
            ];
            let report = decode_source(&mut Gapped::new(parts, fs, center, step), &cfg).unwrap();
            let text = report_text(&report);
            match &want {
                None => want = Some(text),
                Some(w) => assert_eq!(&text, w, "split {split}, read size {step}"),
            }
            let resumed = split as u64 + missing;
            let (mut before, mut after) = (Vec::new(), Vec::new());
            for e in &report.events {
                if let Some(ts) = event_ts(e) {
                    assert!(
                        ts <= split as u64 || ts >= resumed,
                        "event inside the outage at {ts}: {e:?}"
                    );
                    if ts < resumed {
                        before.push(event_track(e));
                    } else {
                        after.push(event_track(e));
                    }
                }
            }
            assert!(!before.is_empty() && !after.is_empty(), "split {split}");
            let last_before = *before.iter().max().unwrap();
            assert!(
                after.iter().all(|id| *id > last_before),
                "a track id spans the gap: {before:?} / {after:?}"
            );
            assert!(
                report.events.iter().any(|e| matches!(
                    e,
                    DecoderEvent::TrackClosed {
                        track_id,
                        closure: ClosureKind::Bookkeeping {
                            survivor_track_id: None
                        },
                    } if before.contains(track_id)
                )),
                "no discontinuity closure before the gap"
            );
            assert!(report.text.contains("W1AW"), "{}", report.text);
        }
    }
}

/// MAN-309: a gap before the first sample moves every timestamp by the
/// missing samples and changes nothing else.
#[test]
fn a_gap_before_any_sample_only_moves_the_clock() {
    let (iq, fs, center) = v1_samples(30.0);
    let cfg = PipelineConfig::default();
    let plain = decode_source(&mut Stepped::new(&iq, fs, center, 4096), &cfg).unwrap();
    let missing = 123_457u64;
    for step in [7, 4096, 65_536] {
        let parts = vec![(Some(missing), iq.clone())];
        let gapped = decode_source(&mut Gapped::new(parts, fs, center, step), &cfg).unwrap();
        assert_eq!(gapped.text, plain.text);
        assert_eq!(gapped.freq_hz, plain.freq_hz);
        assert_eq!(gapped.events.len(), plain.events.len());
        for (g, p) in gapped.events.iter().zip(&plain.events) {
            assert_eq!(
                event_ts(g),
                event_ts(p).map(|ts| ts + missing),
                "{g:?} / {p:?}"
            );
            assert_eq!(event_track(g), event_track(p));
        }
        let shifted: Vec<u64> = plain.spots.iter().map(|s| s.sample_ts + missing).collect();
        let got: Vec<u64> = gapped.spots.iter().map(|s| s.sample_ts).collect();
        assert_eq!(got, shifted);
        assert!(!got.is_empty());
    }
}

/// MAN-309 (MAN-258): an outage pending at EOF closes the last tracks as a
/// discontinuity, not an end of signal.
#[test]
fn a_terminal_gap_closes_the_last_tracks_as_bookkeeping() {
    let (iq, fs, center) = v1_samples(30.0);
    let cfg = PipelineConfig::default();
    let closures = |terminal: Option<u64>| {
        let mut src = Gapped::new(vec![(None, iq.clone())], fs, center, 4096);
        src.terminal = terminal;
        let report = decode_source(&mut src, &cfg).unwrap();
        report
            .events
            .iter()
            .rev()
            .find_map(|e| match e {
                DecoderEvent::TrackClosed { closure, .. } => Some(*closure),
                _ => None,
            })
            .unwrap()
    };
    assert_eq!(closures(None), ClosureKind::SignalEnded);
    for terminal in [0, 96_000] {
        assert_eq!(
            closures(Some(terminal)),
            ClosureKind::Bookkeeping {
                survivor_track_id: None
            }
        );
    }
}

/// MAN-309: the validator spans every segment, so a repeat of the same
/// call across a gap is suppressed inside the 600 s dedupe window and
/// spotted again past it, at its true time.
#[test]
fn the_dedupe_window_spans_a_gap() {
    let (iq, fs, center) = v1_samples(30.0);
    let cfg = PipelineConfig::default();
    let spots = |missing_s: f64| {
        let missing = (missing_s * fs) as u64;
        let parts = vec![(None, iq.clone()), (Some(missing), iq.clone())];
        let report = decode_source(&mut Gapped::new(parts, fs, center, 4096), &cfg).unwrap();
        report
            .spots
            .iter()
            .filter(|s| s.callsign == "W1AW")
            .map(|s| s.sample_ts)
            .collect::<Vec<_>>()
    };
    let near = spots(10.0);
    assert_eq!(near.len(), 1, "{near:?}");
    assert!(near[0] < iq.len() as u64);
    let far = spots(700.0);
    assert_eq!(far.len(), 2, "{far:?}");
    assert!(far[0] < iq.len() as u64);
    assert!(far[1] >= iq.len() as u64 + (700.0 * fs) as u64, "{far:?}");
}

/// MAN-309: the report's bad-sample total covers every segment, and the
/// zero pre-roll of each new segment counts none.
#[test]
fn bad_samples_are_totalled_across_segments() {
    let (mut iq, fs, center) = v1_samples(10.0);
    let split = iq.len() / 2;
    for i in [100, split - 1, split, iq.len() - 5] {
        iq[i] = Complex32::new(f32::NAN, 0.0);
    }
    let cfg = PipelineConfig::default();
    let plain = decode_source(&mut Stepped::new(&iq, fs, center, 4096), &cfg).unwrap();
    assert_eq!(plain.bad_samples, 4);
    let parts = vec![
        (None, iq[..split].to_vec()),
        (Some(48_000), iq[split..].to_vec()),
    ];
    let gapped = decode_source(&mut Gapped::new(parts, fs, center, 4096), &cfg).unwrap();
    assert_eq!(gapped.bad_samples, 4);
}

/// MAN-309: RTTY marked on both sides of a gap gives one report row per
/// segment, timed on the source's clock, with distinct ids.
#[test]
fn rtty_rows_span_a_gap_with_distinct_ids() {
    let (fs, center) = (96_000.0, 14_070_000.0);
    let (mut iq, _) = manta_testkit::scene::render_scene(&[], fs, 25.0, Some(11)).unwrap();
    manta_testkit::fsk::add_fsk(
        &mut iq,
        fs,
        &manta_testkit::fsk::FskSpec {
            mark_offset_hz: 15_000.0,
            shift_hz: 170.0,
            baud: 45.45,
            snr_2500_db: 30.0,
            text: "CQ TEST DE W1AW W1AW RYRYRY 599 001 ".into(),
            start_s: 0.0,
            watterson: None,
            seed: 15_000,
        },
    );
    let missing = 96_000u64;
    let parts = vec![(None, iq.clone()), (Some(missing), iq.clone())];
    let report = decode_source(
        &mut Gapped::new(parts, fs, center, 4096),
        &PipelineConfig::default(),
    )
    .unwrap();
    assert_eq!(report.rtty.len(), 2, "{:?}", report.rtty);
    assert_ne!(report.rtty[0].id, report.rtty[1].id);
    assert!(report.rtty[0].marked_sample_ts < iq.len() as u64);
    assert!(report.rtty[1].marked_sample_ts >= iq.len() as u64 + missing);
}
