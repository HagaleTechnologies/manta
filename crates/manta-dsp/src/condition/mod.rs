//! Optional, reversible input conditioning (MAN-203), run inside
//! [`crate::channelizer::Channelizer::process`] right after the MAN-234
//! sample guard (SPEC §1.3 step 0a).
//!
//! Each stage is [`StageMode::Off`] (the default: no conditioner exists, so
//! the channelizer is bit-identical to one built with
//! [`crate::channelizer::Channelizer::new`]), [`StageMode::Observe`]
//! (measure and annotate, never write a sample) or [`StageMode::Apply`].
//! Every stage records the samples it touched in a bounded span log
//! ([`ConditioningLog`]) and has an exact inverse given that log, so a
//! correction can be audited and undone. Raw recordings are taken upstream
//! of the channelizer, so an A/B comparison is a re-decode of the same file
//! with a different config.
//!
//! The chain runs short-impulse suppression ([`impulse`]), DC removal
//! ([`dc`]), tone measurement, then calibrated IQ correction ([`iq`]).
//! An applying impulse stage holds back
//! [`impulse::IMPULSE_LOOKAHEAD`] samples, so [`InputConditioner::condition`]
//! returns how many samples are final and the channelizer forms hops only
//! from those.

pub mod dc;
pub mod impulse;
pub mod iq;

use num_complex::{Complex32, Complex64};
use std::collections::VecDeque;

/// How one conditioning stage runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StageMode {
    /// The stage does nothing and allocates nothing (the default).
    #[default]
    Off,
    /// The stage measures and annotates, and never writes a sample.
    Observe,
    /// The stage corrects the samples and annotates what it changed.
    Apply,
}

/// Which conditioning stages run, and how. The default turns every stage
/// off.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ConditioningConfig {
    /// Short-impulse suppression: repair isolated runs of up to
    /// [`impulse::IMPULSE_MAX_RUN`] samples; longer bursts are never
    /// touched.
    pub impulse: StageMode,
    /// DC removal: a 5 Hz first-order high-pass at the centre frequency
    /// ([`dc::DC_CORNER_HZ`]).
    pub dc: StageMode,
    /// Calibrated IQ correction, or conservative mirror observation.
    pub iq: StageMode,
    /// Controlled-tone calibration for this receiver and centre frequency.
    pub iq_calibration: Option<iq::IqCalibration>,
    /// Declared calibration-tone offset, measured before correction.
    pub iq_tone_hz: Option<f64>,
}

impl ConditioningConfig {
    /// True when every stage is off, so no conditioner is built.
    pub fn is_off(&self) -> bool {
        self.impulse == StageMode::Off && self.dc == StageMode::Off && self.iq == StageMode::Off
    }
}

/// IQ correction authorization, frozen at stream construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IqStatus {
    /// No IQ work requested.
    Off,
    /// Evidence only; samples are unchanged.
    Observing,
    /// Valid calibration at a matching receiver centre.
    Correcting,
    /// Requested correction withheld, including invalid or absent calibration.
    CalibrationMismatch,
}

/// A conditioning stage, for span and counter labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Stage {
    /// Short-impulse suppression ([`impulse`]).
    Impulse,
    /// DC removal ([`dc`]).
    Dc,
    /// IQ imbalance ([`iq`]).
    Iq,
}

impl Stage {
    /// Every stage, in chain order.
    pub const ALL: [Stage; 3] = [Stage::Impulse, Stage::Dc, Stage::Iq];
}

/// What a stage did to a span of samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Mark {
    /// The impulse stage replaced these samples by interpolation; the log
    /// keeps their originals.
    Repaired,
    /// The impulse stage judged these samples impulsive and left them
    /// unchanged: an `Observe` stage, a run too long to repair, or a run in
    /// a paused block.
    Flagged,
    /// The stage changed these samples (an `Apply` DC or IQ stage).
    Corrected,
    /// The stage's assumption did not hold here, so it changed nothing:
    /// a block the impulse stage paused itself in.
    Uncertain,
}

impl Mark {
    /// Every mark.
    pub const ALL: [Mark; 4] = [
        Mark::Repaired,
        Mark::Flagged,
        Mark::Corrected,
        Mark::Uncertain,
    ];
}

/// A run of consecutive input samples one stage marked the same way.
/// `start_sample` counts channelizer-input samples from construction,
/// including any lead-in padding the caller fed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkedSpan {
    /// The stage that marked the span.
    pub stage: Stage,
    /// What it did.
    pub mark: Mark,
    /// The first marked sample's channelizer-input index.
    pub start_sample: u64,
    /// The number of samples in the span.
    pub len: u64,
}

/// Span-log capacity per (stage, mark): once one is full, its oldest span is
/// dropped and counted in [`ConditioningLog::spans_dropped`]. Counters never
/// drop.
pub const SPAN_LOG_CAPACITY: usize = 256;

/// What the conditioner did since construction: a bounded span log, exact
/// per-(stage, mark) sample counts, and the stages' latest measurements.
#[derive(Debug, Clone, PartialEq)]
pub struct ConditioningLog {
    config: ConditioningConfig,
    /// One span ring per (stage, mark), in [`Stage::ALL`] then
    /// [`Mark::ALL`] order, so each ring is in stream order however the
    /// input is split.
    spans: [[VecDeque<MarkedSpan>; Mark::ALL.len()]; Stage::ALL.len()],
    spans_dropped: u64,
    counts: [[u64; Mark::ALL.len()]; Stage::ALL.len()],
    samples_seen: u64,
    dc_level: Option<Complex64>,
    impulse_long_runs: u64,
    repairs: VecDeque<impulse::Repair>,
    repairs_dropped: u64,
    iq_status: IqStatus,
    image_result: Option<iq::ImageResult>,
    tone_result: Option<iq::ToneResult>,
}

impl ConditioningLog {
    fn new(config: ConditioningConfig) -> Self {
        ConditioningLog {
            config,
            spans: std::array::from_fn(|_| std::array::from_fn(|_| VecDeque::new())),
            spans_dropped: 0,
            counts: [[0; Mark::ALL.len()]; Stage::ALL.len()],
            samples_seen: 0,
            dc_level: None,
            impulse_long_runs: 0,
            repairs: VecDeque::new(),
            repairs_dropped: 0,
            iq_status: IqStatus::Off,
            image_result: None,
            tone_result: None,
        }
    }

    /// Whether calibration authorized correction for this stream.
    pub fn iq_status(&self) -> IqStatus {
        self.iq_status
    }
    /// Latest completed five-second image window.
    pub fn image_result(&self) -> Option<&iq::ImageResult> {
        self.image_result.as_ref()
    }
    /// Latest controlled-tone measurement, before correction.
    pub fn tone_result(&self) -> Option<&iq::ToneResult> {
        self.tone_result.as_ref()
    }

    /// The config the conditioner runs.
    pub fn config(&self) -> &ConditioningConfig {
        &self.config
    }

    /// The retained spans: stage by stage in chain order, then mark by
    /// mark in [`Mark::ALL`] order, each (stage, mark) oldest first. A span
    /// that starts where the newest one with the same stage and mark ends
    /// extends it instead of adding a span.
    pub fn spans(&self) -> impl Iterator<Item = &MarkedSpan> {
        self.spans.iter().flatten().flatten()
    }

    /// Spans dropped from the front of the log because it was full.
    pub fn spans_dropped(&self) -> u64 {
        self.spans_dropped
    }

    /// Samples `stage` marked `mark` since construction. Exact, never
    /// bounded by the span log.
    pub fn count(&self, stage: Stage, mark: Mark) -> u64 {
        self.counts[stage as usize][mark as usize]
    }

    /// Channelizer-input samples the conditioner has seen.
    pub fn samples_seen(&self) -> u64 {
        self.samples_seen
    }

    /// The DC stage's current estimate of the input's DC offset (full scale
    /// 1.0), or `None` when the DC stage is off.
    pub fn dc_level(&self) -> Option<Complex64> {
        self.dc_level
    }

    /// Flagged runs longer than [`impulse::IMPULSE_MAX_RUN`] samples, which
    /// the impulse stage never touches.
    pub fn impulse_long_runs(&self) -> u64 {
        self.impulse_long_runs
    }

    /// The repaired runs whose originals the log still holds, oldest first
    /// ([`impulse::revert_repairs`] restores them).
    pub fn repairs(&self) -> impl Iterator<Item = &impulse::Repair> {
        self.repairs.iter()
    }

    /// Repaired runs dropped from the front of the repair log because it
    /// was full; their originals are lost.
    pub fn repairs_dropped(&self) -> u64 {
        self.repairs_dropped
    }

    fn record_repair(&mut self, r: impulse::Repair) {
        if self.repairs.len() == impulse::REPAIR_LOG_CAPACITY {
            self.repairs.pop_front();
            self.repairs_dropped += 1;
        }
        self.repairs.push_back(r);
    }

    /// Records `len` samples from `start_sample` as `mark` by `stage`,
    /// extending the newest span with the same stage and mark when it ends
    /// at `start_sample`.
    fn mark(&mut self, stage: Stage, mark: Mark, start_sample: u64, len: u64) {
        if len == 0 {
            return;
        }
        self.counts[stage as usize][mark as usize] += len;
        let ring = &mut self.spans[stage as usize][mark as usize];
        if let Some(last) = ring.back_mut() {
            if last.start_sample + last.len == start_sample {
                last.len += len;
                return;
            }
        }
        if ring.len() == SPAN_LOG_CAPACITY {
            ring.pop_front();
            self.spans_dropped += 1;
        }
        ring.push_back(MarkedSpan {
            stage,
            mark,
            start_sample,
            len,
        });
    }
}

/// The conditioning chain one channelizer runs. Build it with
/// [`InputConditioner::new`], which returns `None` when every stage is off.
#[derive(Debug, Clone)]
pub struct InputConditioner {
    impulse: Option<impulse::ImpulseSuppressor>,
    impulse_apply: bool,
    dc: Option<dc::DcBlocker>,
    dc_apply: bool,
    iq: Option<iq::IqCorrector>,
    tone: Option<iq::ToneMeter>,
    image: Option<iq::ImageMonitor>,
    /// Samples an earlier [`InputConditioner::condition`] call held back
    /// (not yet final).
    held: usize,
    /// Samples released as final since construction.
    released: u64,
    log: ConditioningLog,
}

impl InputConditioner {
    /// The chain for `cfg` at sample rate `fs`, or `None` when every stage
    /// is off (so an all-off channelizer does no per-sample work).
    pub fn new(fs: f64, cfg: &ConditioningConfig) -> Option<Self> {
        Self::with_geometry(fs, f64::NAN, (fs / 93.75).round() as usize, cfg)
    }

    /// Construct with the actual receiver centre and channelizer geometry.
    /// Unknown centres never authorize correction.
    pub fn with_geometry(fs: f64, center: f64, n: usize, cfg: &ConditioningConfig) -> Option<Self> {
        if cfg.is_off() {
            return None;
        }
        let impulse_apply = cfg.impulse == StageMode::Apply;
        let impulse = (cfg.impulse != StageMode::Off)
            .then(|| impulse::ImpulseSuppressor::new(fs, impulse_apply));
        let dc = (cfg.dc != StageMode::Off).then(|| dc::DcBlocker::new(fs));
        let mut log = ConditioningLog::new(cfg.clone());
        log.dc_level = dc.as_ref().map(dc::DcBlocker::level);
        let iq = if cfg.iq == StageMode::Apply {
            cfg.iq_calibration
                .as_ref()
                .filter(|c| {
                    c.validate().is_ok()
                        && center.is_finite()
                        && center > 0.0
                        && (c.center_freq_hz - center).abs() <= 1.0
                })
                .and_then(|c| iq::IqCorrector::new(c.image_ratio()).ok())
        } else {
            None
        };
        log.iq_status = match cfg.iq {
            StageMode::Off => IqStatus::Off,
            StageMode::Observe => IqStatus::Observing,
            StageMode::Apply if iq.is_some() => IqStatus::Correcting,
            StageMode::Apply => IqStatus::CalibrationMismatch,
        };
        Some(InputConditioner {
            iq,
            tone: if cfg.iq != StageMode::Off {
                cfg.iq_tone_hz.map(|f| iq::ToneMeter::new(fs, f))
            } else {
                None
            },
            image: (cfg.iq != StageMode::Off).then(|| iq::ImageMonitor::new(fs, n, n / 4)),
            impulse,
            impulse_apply,
            dc,
            dc_apply: cfg.dc == StageMode::Apply,
            held: 0,
            released: 0,
            log,
        })
    }

    /// The most samples the chain ever holds back: the applying impulse
    /// stage's lookahead, otherwise 0.
    pub fn lookahead(&self) -> usize {
        if self.impulse.is_some() && self.impulse_apply {
            impulse::IMPULSE_LOOKAHEAD
        } else {
            0
        }
    }

    /// Conditions `tail` in place, in stream order, and returns how many of
    /// its leading samples are now final. `tail` is every sample not yet
    /// final: the ones the previous call held back (`len - ready` of it),
    /// then the newly arrived ones. The stages are sample-by-sample state
    /// machines and an applying impulse stage releases each sample a fixed
    /// [`impulse::IMPULSE_LOOKAHEAD`] samples late, so the result does not
    /// depend on how the stream is split into calls.
    pub fn condition(&mut self, tail: &mut [Complex32]) -> usize {
        let held = self.held;
        debug_assert!(held <= tail.len());
        self.log.samples_seen += (tail.len() - held) as u64;
        let mut ready = tail.len();
        if let Some(imp) = self.impulse.as_mut() {
            let mut out = 0;
            for k in held..tail.len() {
                let released = imp.push(tail[k], &mut self.log);
                if self.impulse_apply {
                    if let Some(y) = released {
                        tail[out] = y;
                        out += 1;
                    }
                }
            }
            if self.impulse_apply {
                ready = out;
            }
        }
        // `tail[..ready]` is exactly the samples released by this call: a
        // stage that holds nothing back leaves `held` at 0.
        let start = self.released;
        if let Some(dc) = self.dc.as_mut() {
            for s in tail[..ready].iter_mut() {
                let y = dc.process_sample(*s);
                if self.dc_apply {
                    *s = y;
                }
            }
            self.log.dc_level = Some(dc.level());
            if self.dc_apply {
                self.log
                    .mark(Stage::Dc, Mark::Corrected, start, ready as u64);
            }
        }
        for sample in &mut tail[..ready] {
            if let Some(tone) = self.tone.as_mut() {
                tone.push(*sample);
            }
            if let Some(iq) = self.iq.as_ref() {
                *sample = iq.process_sample(*sample);
            }
        }
        self.log.tone_result = self.tone.as_ref().map(iq::ToneMeter::result);
        if self.iq.is_some() {
            self.log
                .mark(Stage::Iq, Mark::Corrected, start, ready as u64);
        } else if self.log.iq_status == IqStatus::CalibrationMismatch {
            self.log
                .mark(Stage::Iq, Mark::Uncertain, start, ready as u64);
        }
        self.released += ready as u64;
        self.held = tail.len() - ready;
        ready
    }

    /// Observe each completed channelizer hop once. Windows cover disjoint
    /// input intervals, independent of overlapping FFT support.
    pub fn observe_hop(&mut self, x: &[Complex32], start_sample: u64) {
        if let Some(result) = self.image.as_mut().and_then(|m| m.observe(x, start_sample)) {
            if result.suspected
                && self.iq.is_none()
                && self.log.iq_status != IqStatus::CalibrationMismatch
            {
                self.log.mark(
                    Stage::Iq,
                    Mark::Uncertain,
                    result.start_sample,
                    result.end_sample - result.start_sample,
                );
            }
            self.log.image_result = Some(result);
        }
    }

    /// What the chain did so far.
    pub fn log(&self) -> &ConditioningLog {
        &self.log
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_coalesce_and_the_ring_drops_oldest() {
        let mut log = ConditioningLog::new(ConditioningConfig::default());
        // 300 non-adjacent spans: each stands alone.
        for i in 0..300u64 {
            log.mark(Stage::Dc, Mark::Corrected, i * 10, 3);
        }
        assert_eq!(log.spans().count(), SPAN_LOG_CAPACITY);
        assert_eq!(log.spans_dropped(), 44);
        assert_eq!(log.count(Stage::Dc, Mark::Corrected), 900);
        assert_eq!(log.spans().next().unwrap().start_sample, 440);
        // An adjacent span extends the newest one instead of adding.
        log.mark(Stage::Dc, Mark::Corrected, 2993, 7);
        assert_eq!(log.spans().count(), SPAN_LOG_CAPACITY);
        assert_eq!(log.spans().last().unwrap().len, 10);
        // A zero-length mark is a no-op.
        log.mark(Stage::Dc, Mark::Corrected, 5000, 0);
        assert_eq!(log.spans().last().unwrap().start_sample, 2990);
    }

    #[test]
    fn apply_dc_logs_one_coalesced_corrected_span() {
        let cfg = ConditioningConfig {
            dc: StageMode::Apply,
            ..ConditioningConfig::default()
        };
        let mut c = InputConditioner::new(48_000.0, &cfg).unwrap();
        let mut x = vec![Complex32::new(0.1, -0.05); 1000];
        for chunk in x.chunks_mut(97) {
            assert_eq!(c.condition(chunk), chunk.len());
        }
        let spans: Vec<_> = c.log().spans().copied().collect();
        assert_eq!(
            spans,
            vec![MarkedSpan {
                stage: Stage::Dc,
                mark: Mark::Corrected,
                start_sample: 0,
                len: 1000,
            }]
        );
        assert_eq!(c.log().count(Stage::Dc, Mark::Corrected), 1000);
        assert_eq!(c.log().samples_seen(), 1000);
    }

    #[test]
    fn observe_measures_without_writing_or_marking() {
        let cfg = ConditioningConfig {
            dc: StageMode::Observe,
            ..ConditioningConfig::default()
        };
        let mut c = InputConditioner::new(48_000.0, &cfg).unwrap();
        let orig = vec![Complex32::new(0.01, 0.007); 48_000];
        let mut x = orig.clone();
        assert_eq!(c.condition(&mut x), x.len());
        assert_eq!(x, orig);
        assert_eq!(c.log().spans().count(), 0);
        assert_eq!(c.log().count(Stage::Dc, Mark::Corrected), 0);
        let level = c.log().dc_level().unwrap();
        assert!((level.re - 0.01).abs() < 1e-6 && (level.im - 0.007).abs() < 1e-6);
    }

    #[test]
    fn iq_authorization_requires_valid_calibration_and_known_matching_center() {
        let calibration = iq::IqCalibration {
            gain_ratio: 1.05,
            phase_deg: 2.0,
            center_freq_hz: 14e6,
        };
        let cfg = ConditioningConfig {
            iq: StageMode::Apply,
            iq_calibration: Some(calibration),
            ..Default::default()
        };
        assert_eq!(
            InputConditioner::new(48000.0, &cfg)
                .unwrap()
                .log()
                .iq_status(),
            IqStatus::CalibrationMismatch
        );
        for (center, expected) in [
            (14e6, IqStatus::Correcting),
            (14e6 + 1.0, IqStatus::Correcting),
            (14e6 + 1.01, IqStatus::CalibrationMismatch),
            (f64::NAN, IqStatus::CalibrationMismatch),
        ] {
            assert_eq!(
                InputConditioner::with_geometry(48000.0, center, 512, &cfg)
                    .unwrap()
                    .log()
                    .iq_status(),
                expected
            );
        }
        let near_zero = ConditioningConfig {
            iq_calibration: Some(iq::IqCalibration {
                center_freq_hz: 0.5,
                ..calibration
            }),
            ..cfg.clone()
        };
        for unknown in [0.0, -0.1] {
            let mut c = InputConditioner::with_geometry(48000.0, unknown, 512, &near_zero).unwrap();
            assert_eq!(c.log().iq_status(), IqStatus::CalibrationMismatch);
            let original = vec![Complex32::new(0.1, 0.2); 100];
            let mut samples = original.clone();
            c.condition(&mut samples);
            assert_eq!(samples, original);
        }
        let invalid = ConditioningConfig {
            iq_calibration: Some(iq::IqCalibration {
                gain_ratio: f64::NAN,
                ..calibration
            }),
            ..cfg
        };
        assert_eq!(
            InputConditioner::with_geometry(48000.0, 14e6, 512, &invalid)
                .unwrap()
                .log()
                .iq_status(),
            IqStatus::CalibrationMismatch
        );
    }

    #[test]
    fn combined_inverse_reverses_iq_then_dc_then_impulse() {
        let calibration = iq::IqCalibration {
            gain_ratio: 1.05,
            phase_deg: 2.0,
            center_freq_hz: 14e6,
        };
        let cfg = ConditioningConfig {
            impulse: StageMode::Apply,
            dc: StageMode::Apply,
            iq: StageMode::Apply,
            iq_calibration: Some(calibration),
            ..Default::default()
        };
        let original: Vec<_> = (0..30000)
            .map(|i| {
                if i == 4000 {
                    Complex32::new(100.0, 50.0)
                } else {
                    Complex32::from_polar(0.1, i as f32 * 0.1) + Complex32::new(0.01, -0.02)
                }
            })
            .collect();
        let mut corrected = original.clone();
        let mut c = InputConditioner::with_geometry(48000.0, 14e6, 512, &cfg).unwrap();
        let ready = c.condition(&mut corrected);
        corrected.truncate(ready);
        iq::revert_iq(&mut corrected, calibration.image_ratio()).unwrap();
        let mut restored = dc::revert_dc(&corrected, 48000.0);
        impulse::revert_repairs(&mut restored, 0, c.log());
        assert_eq!(c.log().count(Stage::Impulse, Mark::Repaired), 1);
        for (a, b) in original.iter().zip(restored) {
            assert!((*a - b).norm() <= 1e-6 * a.norm() + 1e-7, "{a} vs {b}");
        }
    }

    #[test]
    fn tone_log_measures_samples_before_iq_correction() {
        let cal = iq::IqCalibration {
            gain_ratio: 1.05,
            phase_deg: 2.0,
            center_freq_hz: 14e6,
        };
        let cfg = ConditioningConfig {
            iq: StageMode::Apply,
            iq_calibration: Some(cal),
            iq_tone_hz: Some(700.0),
            ..Default::default()
        };
        let mut samples: Vec<_> = (0..4096 * 5)
            .map(|i| {
                let s =
                    Complex64::from_polar(0.5, std::f64::consts::TAU * 713.7 * i as f64 / 4096.0);
                let phi = cal.phase_deg.to_radians();
                Complex32::new(
                    s.re as f32,
                    (cal.gain_ratio * (s.im * phi.cos() + s.re * phi.sin())) as f32,
                )
            })
            .collect();
        let mut c = InputConditioner::with_geometry(4096.0, 14e6, 16, &cfg).unwrap();
        c.condition(&mut samples);
        let Some(iq::ToneResult::Accepted { rho, .. }) = c.log().tone_result() else {
            panic!("tone not accepted")
        };
        assert!((*rho - cal.image_ratio()).norm() < 1e-5);
    }

    #[test]
    fn all_off_builds_no_conditioner() {
        assert!(InputConditioner::new(48_000.0, &ConditioningConfig::default()).is_none());
    }
}
