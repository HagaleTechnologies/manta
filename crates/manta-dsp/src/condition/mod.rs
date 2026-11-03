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
//! The chain runs short-impulse suppression ([`impulse`]) and then DC
//! removal ([`dc`]). The IQ-imbalance stage from the MAN-203 plan is a
//! follow-up; it has no mode here, so nothing can be configured that this
//! module ignores. An applying impulse stage holds back
//! [`impulse::IMPULSE_LOOKAHEAD`] samples, so [`InputConditioner::condition`]
//! returns how many samples are final and the channelizer forms hops only
//! from those.

pub mod dc;
pub mod impulse;

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
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConditioningConfig {
    /// Short-impulse suppression: repair isolated runs of up to
    /// [`impulse::IMPULSE_MAX_RUN`] samples; longer bursts are never
    /// touched.
    pub impulse: StageMode,
    /// DC removal: a 5 Hz first-order high-pass at the centre frequency
    /// ([`dc::DC_CORNER_HZ`]).
    pub dc: StageMode,
}

impl ConditioningConfig {
    /// True when every stage is off, so no conditioner is built.
    pub fn is_off(&self) -> bool {
        self.impulse == StageMode::Off && self.dc == StageMode::Off
    }
}

/// A conditioning stage, for span and counter labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Stage {
    /// Short-impulse suppression ([`impulse`]).
    Impulse,
    /// DC removal ([`dc`]).
    Dc,
}

impl Stage {
    /// Every stage, in chain order.
    pub const ALL: [Stage; 2] = [Stage::Impulse, Stage::Dc];
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
    /// The stage changed these samples (an `Apply` DC stage).
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
        }
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
        if cfg.is_off() {
            return None;
        }
        let impulse_apply = cfg.impulse == StageMode::Apply;
        let impulse = (cfg.impulse != StageMode::Off)
            .then(|| impulse::ImpulseSuppressor::new(fs, impulse_apply));
        let dc = (cfg.dc != StageMode::Off).then(|| dc::DcBlocker::new(fs));
        let mut log = ConditioningLog::new(cfg.clone());
        log.dc_level = dc.as_ref().map(dc::DcBlocker::level);
        Some(InputConditioner {
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
        self.released += ready as u64;
        self.held = tail.len() - ready;
        ready
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
    fn all_off_builds_no_conditioner() {
        assert!(InputConditioner::new(48_000.0, &ConditioningConfig::default()).is_none());
    }
}
