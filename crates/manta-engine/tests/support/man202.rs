//! MAN-202 test support: an in-memory `IqSource` that serves a rendered
//! scene in a repeating read-size schedule and can drop a span of samples
//! with or without reporting it, plus the event skeleton and text helpers
//! `replay_live_parity.rs` and `unknown_input_gap.rs` share.
#![allow(dead_code)]

use manta_decode::events::DecoderEvent;
use manta_engine::{
    decode_source, listen, listen_inputs, open_inputs, InputSpec, PipelineConfig, SessionClock,
    SharedObservers,
};
use manta_input::IqSource;
use manta_testkit::scene::SignalSpec;
use manta_testkit::vectors::{render, v1};
use num_complex::Complex32;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// A rendered scene: samples, rate and centre.
#[derive(Clone)]
pub struct Scene {
    pub samples: Arc<Vec<Complex32>>,
    pub fs: f64,
    pub center_freq_hz: f64,
}

/// V1 (W1AW, 20 WPM, 96 kS/s) cut to 30 s.
pub fn v1_30s() -> Scene {
    let mut spec = v1();
    spec.duration_s = 30.0;
    scene_of(spec)
}

/// V1 plus a second, faster and weaker station at -21 kHz.
pub fn two_signal_30s() -> Scene {
    let mut spec = v1();
    spec.duration_s = 30.0;
    let mut second: SignalSpec = spec.signals[0].clone();
    second.text = "CQ TEST DE K1ABC K1ABC K".into();
    second.wpm = 25.0;
    second.offset_hz = -21_000.0;
    second.snr_2500_db = 16.0;
    spec.signals.push(second);
    scene_of(spec)
}

fn scene_of(spec: manta_testkit::vectors::VectorSpec) -> Scene {
    let rendered = render(&spec).unwrap();
    Scene {
        samples: Arc::new(rendered.samples),
        fs: spec.fs,
        center_freq_hz: spec.center_freq_hz,
    }
}

/// How a dropped span is reported to the engine.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Report {
    /// Nothing: today's SoapySDR overflow shape.
    Silent,
    /// `take_unknown_gap()`.
    Unknown,
    /// `take_discontinuity(Some(g))`.
    Disc(u64),
    /// Both `take_discontinuity(Some(g))` and `take_unknown_gap()`.
    DiscAndUnknown(u64),
}

/// Drop `len` samples starting at data sample `at`, reported as `report`
/// on the read that returns the first post-gap sample.
#[derive(Clone, Copy, Debug)]
pub struct Cut {
    pub at: usize,
    pub len: usize,
    pub report: Report,
}

impl Cut {
    pub fn ms(at_s: f64, len_ms: f64, fs: f64, report: Report) -> Self {
        Cut {
            at: (at_s * fs) as usize,
            len: (len_ms * 1e-3 * fs) as usize,
            report,
        }
    }
}

/// Serves `scene` in reads sized by `schedule` (repeating), applying `cut`.
pub struct Chunked {
    scene: Scene,
    cursor: usize,
    schedule: Vec<usize>,
    step: usize,
    cut: Option<Cut>,
    unknown_at_end: bool,
    pending_unknown: bool,
    pending_disc: Option<u64>,
}

impl Chunked {
    pub fn new(scene: &Scene, schedule: &[usize], cut: Option<Cut>) -> Self {
        Chunked {
            scene: scene.clone(),
            cursor: 0,
            schedule: schedule.to_vec(),
            step: 0,
            cut,
            unknown_at_end: false,
            pending_unknown: false,
            pending_disc: None,
        }
    }

    /// Leave an unknown gap pending when the stream ends.
    pub fn with_unknown_gap_at_end(mut self) -> Self {
        self.unknown_at_end = true;
        self
    }
}

impl IqSource for Chunked {
    fn sample_rate(&self) -> f64 {
        self.scene.fs
    }
    fn center_freq_hz(&self) -> f64 {
        self.scene.center_freq_hz
    }
    fn read(&mut self, buf: &mut [Complex32]) -> anyhow::Result<usize> {
        let mut limit = self.scene.samples.len();
        if let Some(cut) = self.cut {
            if self.cursor == cut.at {
                self.cursor += cut.len;
                self.cut = None;
                match cut.report {
                    Report::Silent => {}
                    Report::Unknown => self.pending_unknown = true,
                    Report::Disc(g) => self.pending_disc = Some(g),
                    Report::DiscAndUnknown(g) => {
                        self.pending_disc = Some(g);
                        self.pending_unknown = true;
                    }
                }
            } else if self.cursor < cut.at {
                limit = cut.at;
            }
        }
        let want = self.schedule[self.step % self.schedule.len()];
        self.step += 1;
        let n = buf.len().min(want).min(limit.saturating_sub(self.cursor));
        buf[..n].copy_from_slice(&self.scene.samples[self.cursor..self.cursor + n]);
        self.cursor += n;
        if n == 0 && self.unknown_at_end {
            self.unknown_at_end = false;
            self.pending_unknown = true;
        }
        Ok(n)
    }
    fn take_discontinuity(&mut self) -> Option<u64> {
        self.pending_disc.take()
    }
    fn take_unknown_gap(&mut self) -> bool {
        std::mem::take(&mut self.pending_unknown)
    }
}

/// Every event `listen` emits for `src`.
pub fn listen_events(src: Chunked) -> Vec<DecoderEvent> {
    let mut events = Vec::new();
    listen(
        Box::new(src),
        &PipelineConfig::default(),
        Arc::new(AtomicBool::new(false)),
        |e| events.push(e.clone()),
        |_| {},
    )
    .unwrap();
    events
}

/// Every event `decode_source` (the file path) emits for `src`.
pub fn decode_events(mut src: impl IqSource) -> Vec<DecoderEvent> {
    decode_source(&mut src, &PipelineConfig::default())
        .unwrap()
        .events
}

/// Every event `listen_inputs` emits with `src` as its one input, under
/// the replay clock.
pub fn listen_inputs_events(src: Chunked) -> Vec<DecoderEvent> {
    let stop = Arc::new(AtomicBool::new(false));
    let slot = std::sync::Mutex::new(Some(src));
    let spec = InputSpec {
        name: "a".into(),
        open: Box::new(move || {
            Ok(Box::new(slot.lock().unwrap().take().expect("opened once")) as Box<dyn IqSource>)
        }),
        freq_correction_ppm: 0.0,
        observers: Default::default(),
        spawn_band_hz: None,
    };
    let opened = open_inputs(vec![spec], stop).unwrap();
    let mut events = Vec::new();
    listen_inputs(
        opened,
        &PipelineConfig::default(),
        SessionClock::Replay,
        SharedObservers::default(),
        |e| events.push(e.clone()),
        |_| {},
        |_input, _n| {},
        |_end| {},
    )
    .unwrap();
    events
}

/// The semantic skeleton MAN-202 Scenario 2 compares: `TrackPromoted`,
/// `CharDecoded`, `WordBoundary` and `TrackClosed`, in emission order.
pub fn skeleton(events: &[DecoderEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            DecoderEvent::TrackPromoted {
                track_id,
                sample_ts,
                ..
            } => Some(format!("P{track_id}@{sample_ts}")),
            DecoderEvent::CharDecoded {
                track_id,
                sample_ts,
                glyph,
                ..
            } => Some(format!("C{track_id}@{sample_ts}:{glyph:?}")),
            DecoderEvent::WordBoundary {
                track_id,
                sample_ts,
                ..
            } => Some(format!("W{track_id}@{sample_ts}")),
            DecoderEvent::TrackClosed { track_id, closure } => {
                Some(format!("X{track_id}:{closure:?}"))
            }
            _ => None,
        })
        .collect()
}

/// The decoded characters, in emission order, spaces dropped.
pub fn text(events: &[DecoderEvent]) -> Vec<char> {
    events
        .iter()
        .filter_map(|e| match e {
            DecoderEvent::CharDecoded { glyph, .. } => Some(glyph.text_char().unwrap_or('#')),
            _ => None,
        })
        .collect()
}

/// `(wrong, missing)` edits from `truth` to `got`: wrong = substitutions
/// plus insertions, missing = deletions, from one minimum-cost alignment.
pub fn edits(truth: &[char], got: &[char]) -> (usize, usize) {
    let (n, m) = (truth.len(), got.len());
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let sub = d[i - 1][j - 1] + usize::from(truth[i - 1] != got[j - 1]);
            d[i][j] = sub.min(d[i - 1][j] + 1).min(d[i][j - 1] + 1);
        }
    }
    let (mut i, mut j, mut wrong, mut missing) = (n, m, 0, 0);
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && d[i][j] == d[i - 1][j - 1] + usize::from(truth[i - 1] != got[j - 1]) {
            wrong += usize::from(truth[i - 1] != got[j - 1]);
            i -= 1;
            j -= 1;
        } else if i > 0 && d[i][j] == d[i - 1][j] + 1 {
            missing += 1;
            i -= 1;
        } else {
            wrong += 1;
            j -= 1;
        }
    }
    (wrong, missing)
}

/// How many `TrackClosed` events `events` holds.
pub fn closures(events: &[DecoderEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, DecoderEvent::TrackClosed { .. }))
        .count()
}

/// Write `samples` as headerless little-endian cf32 (I then Q).
pub fn write_cf32(path: &std::path::Path, samples: &[Complex32]) {
    let mut bytes = Vec::with_capacity(samples.len() * 8);
    for s in samples {
        bytes.extend_from_slice(&s.re.to_le_bytes());
        bytes.extend_from_slice(&s.im.to_le_bytes());
    }
    std::fs::write(path, bytes).unwrap();
}
