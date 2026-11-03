//! Short-impulse suppression (MAN-203 D6): a trimmed-mean power gate that
//! repairs isolated runs of up to [`IMPULSE_MAX_RUN`] samples and never
//! touches a longer burst.
//!
//! A sample is flagged when its power exceeds [`IMPULSE_RATIO`]² times the
//! reference power of the [`IMPULSE_WINDOW`] samples centred on it (fewer at
//! the stream's start): their mean once the [`IMPULSE_TRIM`] strongest are
//! left out, so up to two repairable runs in one window do not raise it. A
//! zero reference never flags, so lead-in padding is left alone. (A median
//! reference flagged clean 45 dB scenes: two strong stations whose beat is
//! near `fs/2` leave every other sample near zero, so the median sits in
//! the nulls.) A flagged run of at most [`IMPULSE_MAX_RUN`] samples is
//! repaired by linear interpolation between its unflagged neighbours; a
//! longer run is counted in [`ConditioningLog::impulse_long_runs`], marked
//! [`Mark::Flagged`] and never written. When more than
//! [`IMPULSE_PAUSE_FRACTION`] of a one-second block (`round(fs)` samples)
//! is flagged, the next block repairs nothing and is marked
//! [`Mark::Uncertain`]: dense flags mean the gate's "isolated impulse"
//! assumption has failed.
//!
//! Deciding a sample needs the [`IMPULSE_LOOKAHEAD`] samples after it, so
//! an applying suppressor releases each sample exactly that many samples
//! late; the channelizer forms hops only from released samples, so sample
//! indices do not move. The last [`IMPULSE_HALF_WINDOW`] samples of a
//! stream (or of a decode segment, which builds a new channelizer) are
//! never judged, and a run still open there is neither repaired nor
//! logged. The original value of every repaired sample is kept
//! in a bounded ring, and [`revert_repairs`] restores it bit for bit.

use super::{ConditioningLog, Mark, Stage};
use num_complex::Complex32;
use std::collections::VecDeque;

/// The reference window, samples, centred on the sample being judged.
pub const IMPULSE_WINDOW: usize = 33;

/// Samples on each side of the judged one in [`IMPULSE_WINDOW`].
pub const IMPULSE_HALF_WINDOW: usize = IMPULSE_WINDOW / 2;

/// A sample is flagged when its power exceeds `IMPULSE_RATIO²` times the
/// window's reference power.
pub const IMPULSE_RATIO: f32 = 12.0;

/// The window's strongest powers left out of the reference mean: room for
/// two repairable runs.
pub const IMPULSE_TRIM: usize = 2 * IMPULSE_MAX_RUN;

/// The longest flagged run the suppressor repairs. Longer runs are never
/// touched.
pub const IMPULSE_MAX_RUN: usize = 4;

/// When more than this fraction of a one-second block is flagged, the next
/// block repairs nothing and is marked uncertain.
pub const IMPULSE_PAUSE_FRACTION: f64 = 1e-3;

/// How many samples an applying suppressor holds back:
/// the half window, a full repairable run, and the run's right neighbour.
pub const IMPULSE_LOOKAHEAD: usize = IMPULSE_HALF_WINDOW + IMPULSE_MAX_RUN + 1;

/// Repaired runs whose originals the log keeps; older ones are dropped and
/// counted in [`ConditioningLog::repairs_dropped`].
pub const REPAIR_LOG_CAPACITY: usize = 4096;

/// One repaired run: its first sample's channelizer-input index and the
/// original values of its `len` samples.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Repair {
    /// The first repaired sample's channelizer-input index.
    pub start_sample: u64,
    /// The number of repaired samples, 1 to [`IMPULSE_MAX_RUN`].
    pub len: u8,
    /// The original values; entries past `len` are zero.
    pub originals: [Complex32; IMPULSE_MAX_RUN],
}

/// An open flagged run.
#[derive(Debug, Clone, Copy)]
struct Run {
    start: u64,
    len: u64,
    /// A sample of the run lies in a paused block, so it is never
    /// repaired.
    paused: bool,
}

/// The suppressor's state.
#[derive(Debug, Clone)]
pub struct ImpulseSuppressor {
    apply: bool,
    block_len: u64,
    /// Received samples not yet released, oldest first; `pending[0]` has
    /// index `next_out`.
    pending: VecDeque<Complex32>,
    next_out: u64,
    received: u64,
    /// The newest `IMPULSE_WINDOW` (or fewer) raw powers, oldest first, and
    /// the same values sorted.
    window: VecDeque<f32>,
    sorted: Vec<f32>,
    run: Option<Run>,
    /// The newest unflagged judged sample (never written by this stage).
    left: Option<Complex32>,
    block: u64,
    block_flagged: u64,
    paused: bool,
}

impl ImpulseSuppressor {
    /// A suppressor at sample rate `fs`; `apply` repairs, otherwise it only
    /// flags.
    pub fn new(fs: f64, apply: bool) -> Self {
        ImpulseSuppressor {
            apply,
            block_len: (fs.round() as u64).max(1),
            pending: VecDeque::with_capacity(IMPULSE_LOOKAHEAD + 1),
            next_out: 0,
            received: 0,
            window: VecDeque::with_capacity(IMPULSE_WINDOW + 1),
            sorted: Vec::with_capacity(IMPULSE_WINDOW + 1),
            run: None,
            left: None,
            block: 0,
            block_flagged: 0,
            paused: false,
        }
    }

    /// Takes the next raw sample and returns the sample released
    /// [`IMPULSE_LOOKAHEAD`] samples earlier (repaired when the suppressor
    /// applies), or `None` while the first samples are still held back.
    pub fn push(&mut self, x: Complex32, log: &mut ConditioningLog) -> Option<Complex32> {
        let p = x.norm_sqr();
        self.pending.push_back(x);
        self.window.push_back(p);
        let at = self.sorted.partition_point(|&q| q < p);
        self.sorted.insert(at, p);
        if self.window.len() > IMPULSE_WINDOW {
            let old = self.window.pop_front().expect("window is non-empty");
            let at = self.sorted.partition_point(|&q| q < old);
            self.sorted.remove(at);
        }
        self.received += 1;
        if self.received > IMPULSE_HALF_WINDOW as u64 {
            self.judge(self.received - 1 - IMPULSE_HALF_WINDOW as u64, log);
        }
        if self.received > IMPULSE_LOOKAHEAD as u64 {
            self.next_out += 1;
            self.pending.pop_front()
        } else {
            None
        }
    }

    /// Judges sample `j`, whose window ends at the newest sample.
    fn judge(&mut self, j: u64, log: &mut ConditioningLog) {
        let block = j / self.block_len;
        if block != self.block {
            self.paused =
                self.block_flagged as f64 > IMPULSE_PAUSE_FRACTION * self.block_len as f64;
            self.block = block;
            self.block_flagged = 0;
        }
        if self.paused {
            log.mark(Stage::Impulse, Mark::Uncertain, j, 1);
        }
        let first = self.received - self.window.len() as u64;
        let p = self.window[(j - first) as usize];
        // The window holds at least `IMPULSE_HALF_WINDOW + 1 > IMPULSE_TRIM`
        // powers here.
        let kept = &self.sorted[..self.sorted.len() - IMPULSE_TRIM];
        let reference = kept.iter().map(|&q| q as f64).sum::<f64>() / kept.len() as f64;
        let flagged =
            reference > 0.0 && p as f64 > (IMPULSE_RATIO * IMPULSE_RATIO) as f64 * reference;
        let x = self.pending[(j - self.next_out) as usize];
        if flagged {
            self.block_flagged += 1;
            match self.run.as_mut() {
                Some(run) => {
                    run.len += 1;
                    // A run that reaches into a paused block is not repaired.
                    run.paused |= self.paused;
                }
                None => {
                    self.run = Some(Run {
                        start: j,
                        len: 1,
                        paused: self.paused,
                    })
                }
            }
        } else {
            if let Some(run) = self.run.take() {
                self.close(run, x, log);
            }
            self.left = Some(x);
        }
    }

    /// Ends `run` at its unflagged right neighbour `right`: repairs it, or
    /// marks it flagged when it is too long, paused or only observed.
    fn close(&mut self, run: Run, right: Complex32, log: &mut ConditioningLog) {
        if run.len > IMPULSE_MAX_RUN as u64 {
            log.impulse_long_runs += 1;
            log.mark(Stage::Impulse, Mark::Flagged, run.start, run.len);
            return;
        }
        if !self.apply || run.paused {
            log.mark(Stage::Impulse, Mark::Flagged, run.start, run.len);
            return;
        }
        let left = self.left.unwrap_or(right);
        let base = (run.start - self.next_out) as usize;
        let mut originals = [Complex32::new(0.0, 0.0); IMPULSE_MAX_RUN];
        let steps = (run.len + 1) as f32;
        for (t, original) in originals.iter_mut().take(run.len as usize).enumerate() {
            let s = &mut self.pending[base + t];
            *original = *s;
            let frac = (t + 1) as f32 / steps;
            *s = left + (right - left) * frac;
        }
        log.record_repair(Repair {
            start_sample: run.start,
            len: run.len as u8,
            originals,
        });
        log.mark(Stage::Impulse, Mark::Repaired, run.start, run.len);
    }
}

/// Restores the original value of every repaired sample the log still
/// holds, in `y`, whose first element is channelizer-input sample
/// `first_sample`. Repairs outside `y` are skipped. Bit-exact: the log keeps
/// the originals themselves.
pub fn revert_repairs(y: &mut [Complex32], first_sample: u64, log: &ConditioningLog) {
    for r in log.repairs() {
        for t in 0..r.len as u64 {
            let idx = r.start_sample + t;
            if idx >= first_sample && ((idx - first_sample) as usize) < y.len() {
                y[(idx - first_sample) as usize] = r.originals[t as usize];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::condition::{ConditioningConfig, InputConditioner, StageMode};

    const FS: f64 = 48_000.0;

    /// Deterministic noise at about `amp` rms per component.
    fn noise(n: usize, amp: f32, seed: u64) -> Vec<Complex32> {
        let mut s = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let mut next = move || {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((s >> 11) as f64 / (1u64 << 53) as f64) as f32 * 2.0 - 1.0
        };
        (0..n)
            .map(|_| Complex32::new(amp * 1.73 * next(), amp * 1.73 * next()))
            .collect()
    }

    fn cfg(mode: StageMode) -> ConditioningConfig {
        ConditioningConfig {
            impulse: mode,
            ..ConditioningConfig::default()
        }
    }

    /// Runs `x` through an impulse-only conditioner and returns the
    /// released samples and the conditioner.
    fn run(x: &[Complex32], mode: StageMode) -> (Vec<Complex32>, InputConditioner) {
        let mut c = InputConditioner::new(FS, &cfg(mode)).unwrap();
        let mut buf = x.to_vec();
        let ready = c.condition(&mut buf);
        buf.truncate(ready);
        (buf, c)
    }

    #[test]
    fn a_single_huge_sample_is_repaired_by_interpolation() {
        let mut x = noise(4_000, 0.05, 1);
        x[2_000] = Complex32::new(1e8, 0.0);
        let (y, c) = run(&x, StageMode::Apply);
        let spans: Vec<_> = c
            .log()
            .spans()
            .filter(|s| s.stage == Stage::Impulse)
            .copied()
            .collect();
        assert_eq!(spans.len(), 1, "{spans:?}");
        assert_eq!(
            (spans[0].mark, spans[0].start_sample, spans[0].len),
            (Mark::Repaired, 2_000, 1)
        );
        let mid = (x[1_999] + x[2_001]) * 0.5;
        assert!((y[2_000] - mid).norm() < 1e-6, "{} vs {mid}", y[2_000]);
        for (i, (a, b)) in x.iter().zip(&y).enumerate() {
            if i != 2_000 {
                assert_eq!(a, b, "sample {i} changed");
            }
        }
        assert_eq!(c.log().count(Stage::Impulse, Mark::Repaired), 1);
    }

    #[test]
    fn runs_longer_than_four_samples_are_never_touched() {
        let mut x = noise(10_000, 0.05, 2);
        // A 5- and an 8-sample burst are flagged as long runs; a 200-sample
        // burst is not flagged at all, because more of it than
        // `IMPULSE_TRIM` stays in the reference.
        for (start, len) in [(2_000, 5), (4_000, 8), (6_000, 200)] {
            for s in &mut x[start..start + len] {
                *s = Complex32::new(50.0, 0.0);
            }
        }
        let (y, c) = run(&x, StageMode::Apply);
        assert_eq!(c.log().impulse_long_runs(), 2);
        assert_eq!(c.log().count(Stage::Impulse, Mark::Flagged), 13);
        assert_eq!(c.log().count(Stage::Impulse, Mark::Repaired), 0);
        assert_eq!(&y[..], &x[..y.len()]);
    }

    #[test]
    fn a_keyed_carrier_step_is_never_flagged() {
        // A carrier 30 dB over the noise, keyed on and off 20 times.
        let mut x = noise(40 * 960, 0.05, 3);
        let amp = 0.05 * 10f32.powf(1.5) * 1.41;
        for cycle in 0..20 {
            let on = cycle * 2 * 960;
            for (k, s) in x[on..on + 960].iter_mut().enumerate() {
                let phi = 0.37 * k as f32;
                *s += Complex32::new(amp * phi.cos(), amp * phi.sin());
            }
        }
        let (y, c) = run(&x, StageMode::Apply);
        assert_eq!(c.log().count(Stage::Impulse, Mark::Flagged), 0);
        assert_eq!(c.log().count(Stage::Impulse, Mark::Repaired), 0);
        assert_eq!(&y[..], &x[..y.len()]);
    }

    #[test]
    fn repairs_revert_bit_exactly() {
        let mut x = noise(20_000, 0.05, 4);
        for (i, v) in [(1_000, 1e8), (5_000, -3e3), (5_002, 4e3), (12_345, 1e9)] {
            x[i] = Complex32::new(v, v * 0.5);
        }
        let (mut y, c) = run(&x, StageMode::Apply);
        assert!(c.log().count(Stage::Impulse, Mark::Repaired) >= 4);
        revert_repairs(&mut y, 0, c.log());
        for (a, b) in x.iter().zip(&y) {
            assert_eq!(a.re.to_bits(), b.re.to_bits());
            assert_eq!(a.im.to_bits(), b.im.to_bits());
        }
    }

    #[test]
    fn dense_flags_pause_the_next_block_and_mark_it_uncertain() {
        let block = FS as usize;
        let mut x = noise(3 * block + 100, 0.05, 5);
        // 200 isolated impulses in second 1, 10 in second 2.
        for k in 0..200 {
            x[block + 100 + k * 200] = Complex32::new(1e3, 0.0);
        }
        for k in 0..10 {
            x[2 * block + 500 + k * 1_000] = Complex32::new(1e3, 0.0);
        }
        let (y, c) = run(&x, StageMode::Apply);
        let log = c.log();
        // Second 1's impulses are repaired; second 2's are only flagged.
        assert_eq!(log.count(Stage::Impulse, Mark::Repaired), 200);
        assert_eq!(log.count(Stage::Impulse, Mark::Flagged), 10);
        for k in 0..10 {
            let i = 2 * block + 500 + k * 1_000;
            assert_eq!(y[i], x[i], "paused block repaired sample {i}");
        }
        let uncertain: Vec<_> = log
            .spans()
            .filter(|s| s.mark == Mark::Uncertain)
            .copied()
            .collect();
        assert_eq!(uncertain.len(), 1, "{uncertain:?}");
        assert_eq!(uncertain[0].start_sample, 2 * block as u64);
        assert_eq!(uncertain[0].len, block as u64);
    }

    #[test]
    fn a_run_reaching_into_a_paused_block_is_not_repaired() {
        let block = FS as usize;
        let mut x = noise(2 * block + 100, 0.05, 8);
        // 200 isolated impulses pause second 1.
        for k in 0..200 {
            x[100 + k * 200] = Complex32::new(1e3, 0.0);
        }
        // A 2-sample run from second 0's last sample into second 1.
        x[block - 1] = Complex32::new(1e3, 0.0);
        x[block] = Complex32::new(1e3, 0.0);
        let (y, c) = run(&x, StageMode::Apply);
        assert_eq!(c.log().count(Stage::Impulse, Mark::Repaired), 200);
        assert_eq!(c.log().count(Stage::Impulse, Mark::Flagged), 2);
        assert_eq!((y[block - 1], y[block]), (x[block - 1], x[block]));
    }

    #[test]
    fn observe_flags_without_writing() {
        let mut x = noise(4_000, 0.05, 6);
        x[1_500] = Complex32::new(1e8, 0.0);
        let (y, c) = run(&x, StageMode::Observe);
        assert_eq!(y, x, "observe released every sample unchanged");
        assert_eq!(c.log().count(Stage::Impulse, Mark::Flagged), 1);
        assert_eq!(c.log().count(Stage::Impulse, Mark::Repaired), 0);
        assert_eq!(c.log().repairs().count(), 0);
    }

    #[test]
    fn apply_watermark_trails_the_buffer_by_the_lookahead() {
        let x = noise(1_000, 0.05, 7);
        let (y, c) = run(&x, StageMode::Apply);
        assert_eq!(y.len(), x.len() - IMPULSE_LOOKAHEAD);
        assert_eq!(IMPULSE_LOOKAHEAD, 21);
        assert_eq!(c.log().samples_seen(), x.len() as u64);
        // Fewer samples than the lookahead release nothing.
        let (y, _) = run(&x[..IMPULSE_LOOKAHEAD], StageMode::Apply);
        assert!(y.is_empty());
    }
}
