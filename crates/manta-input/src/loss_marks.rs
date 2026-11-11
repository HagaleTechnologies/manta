//! MAN-529: losses a buffering source spliced over, placed in its own output
//! stream so each is reported after the `read()` that starts right after it.
//!
//! The [`crate::IqSource`] contract says a gap report describes the samples
//! lost *immediately before* the samples returned by the most recent `read()`
//! (`take_missing_samples`, `take_discontinuity`). A source that buffers ahead
//! of its reader (KiwiSDR's resampler backlog, ka9q-radio's packet queue) can
//! detect a loss while samples from before the loss are still queued, so it
//! records a mark (output position, samples lost) and ends each `read()` at
//! the next mark. The report becomes pending only when a read returns the
//! first post-loss sample.
//!
//! Classification follows MAN-305 D10: a loss under `sample_rate / 2` output
//! samples (0.5 s) is spliced and reported through `take_missing_samples`; a
//! loss of 0.5 s or more goes through `take_discontinuity` with the same
//! count, so the engine closes its tracks and starts a new segment. Nothing is
//! zero-filled. Marks at one position classify on their total.

use std::collections::VecDeque;

/// Marks of samples lost before output positions of a buffering source.
#[derive(Debug)]
pub(crate) struct LossMarks {
    /// (output position, samples lost immediately before it); positions ascend.
    marks: VecDeque<(u64, u64)>,
    /// Output samples returned by `read()` so far.
    delivered: u64,
    /// `sample_rate / 2` samples: MAN-305 D10's splice limit.
    splice_below: u64,
    /// Pending spliced loss for `take_missing_samples`.
    missing: Option<u64>,
    /// Pending long loss for `take_discontinuity`.
    discontinuity: Option<u64>,
}

impl LossMarks {
    /// A queue for a stream at `sample_rate_hz` output samples per second.
    pub(crate) fn new(sample_rate_hz: f64) -> Self {
        let half = (sample_rate_hz / 2.0).round();
        let splice_below = if half.is_finite() && half >= 1.0 {
            half as u64
        } else {
            1
        };
        Self {
            marks: VecDeque::new(),
            delivered: 0,
            splice_below,
            missing: None,
            discontinuity: None,
        }
    }

    /// Record `lost` samples missing immediately before output position `at`.
    /// A zero-sample mark is ignored. A mark behind the samples already
    /// delivered cannot be placed before them any more, so it is clamped to
    /// the delivered position (the next read carries it).
    pub(crate) fn mark(&mut self, at: u64, lost: u64) {
        if lost == 0 {
            return;
        }
        debug_assert!(
            at >= self.delivered,
            "loss mark at {at} is behind the {} samples already delivered",
            self.delivered
        );
        let at = at.max(self.delivered);
        // Positions arrive in ascending order from a driver; keep the queue
        // sorted regardless, merging marks at one position.
        match self.marks.back_mut() {
            Some((pos, sum)) if *pos == at => {
                *sum = sum.saturating_add(lost);
            }
            Some((pos, _)) if *pos > at => {
                let idx = self.marks.partition_point(|(p, _)| *p < at);
                match self.marks.get_mut(idx) {
                    Some((p, sum)) if *p == at => *sum = sum.saturating_add(lost),
                    _ => self.marks.insert(idx, (at, lost)),
                }
            }
            _ => self.marks.push_back((at, lost)),
        }
    }

    /// Call only when the read will return at least one sample: moves every
    /// mark at or before the delivered position into the pending report
    /// (split at the 0.5 s splice limit), then returns `want` capped at the
    /// next mark (at least 1 when `want` is at least 1).
    pub(crate) fn begin_read(&mut self, want: usize) -> usize {
        let mut due: u64 = 0;
        while let Some(&(pos, lost)) = self.marks.front() {
            if pos > self.delivered {
                break;
            }
            due = due.saturating_add(lost);
            self.marks.pop_front();
        }
        if due > 0 {
            let slot = if due < self.splice_below {
                &mut self.missing
            } else {
                &mut self.discontinuity
            };
            *slot = Some(slot.unwrap_or(0).saturating_add(due));
        }
        match self.marks.front() {
            Some(&(pos, _)) => {
                let room = pos - self.delivered;
                let room = usize::try_from(room).unwrap_or(usize::MAX);
                want.min(room)
            }
            None => want,
        }
    }

    /// Record that the read returned `n` samples.
    pub(crate) fn delivered(&mut self, n: usize) {
        self.delivered = self.delivered.saturating_add(n as u64);
    }

    /// The pending spliced loss, once.
    pub(crate) fn take_missing(&mut self) -> Option<u64> {
        self.missing.take()
    }

    /// The pending loss of 0.5 s or more, once.
    pub(crate) fn take_discontinuity(&mut self) -> Option<u64> {
        self.discontinuity.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_stops_at_the_mark_and_the_next_read_carries_the_report() {
        let mut m = LossMarks::new(96_000.0);
        m.mark(1_000, 360);
        assert_eq!(m.begin_read(4_096), 1_000); // capped at the mark
        m.delivered(1_000);
        assert_eq!((m.take_missing(), m.take_discontinuity()), (None, None));
        assert_eq!(m.begin_read(4_096), 4_096); // promotes, no next mark
        m.delivered(4_096);
        assert_eq!(m.take_missing(), Some(360));
        assert_eq!(m.take_missing(), None); // reported once
        assert_eq!(m.take_discontinuity(), None);
    }

    #[test]
    fn half_a_second_or_more_is_a_discontinuity() {
        let mut m = LossMarks::new(96_000.0);
        m.mark(0, 47_999);
        assert_eq!(m.begin_read(10), 10);
        m.delivered(10);
        assert_eq!(m.take_missing(), Some(47_999));
        assert_eq!(m.take_discontinuity(), None);

        let mut m = LossMarks::new(96_000.0);
        m.mark(0, 48_000);
        assert_eq!(m.begin_read(10), 10);
        m.delivered(10);
        assert_eq!(m.take_missing(), None);
        assert_eq!(m.take_discontinuity(), Some(48_000));
    }

    #[test]
    fn marks_at_one_position_merge_and_classify_on_the_total() {
        let mut m = LossMarks::new(96_000.0);
        m.mark(500, 30_000);
        m.mark(500, 30_000);
        assert_eq!(m.begin_read(1_000), 500);
        m.delivered(500);
        assert_eq!(m.begin_read(1_000), 1_000);
        m.delivered(1_000);
        assert_eq!(m.take_missing(), None);
        assert_eq!(m.take_discontinuity(), Some(60_000));
    }

    #[test]
    fn a_mark_at_the_delivered_position_is_promoted_by_the_next_read() {
        let mut m = LossMarks::new(96_000.0);
        assert_eq!(m.begin_read(100), 100);
        m.delivered(100);
        m.mark(100, 4_096);
        assert_eq!(m.take_missing(), None); // not before a read returns
        assert_eq!(m.begin_read(50), 50);
        m.delivered(50);
        assert_eq!(m.take_missing(), Some(4_096));
    }

    #[test]
    fn a_read_that_returns_nothing_promotes_nothing() {
        let mut m = LossMarks::new(96_000.0);
        m.mark(0, 360);
        // No begin_read: the driver's read returned Ok(0) (a stall).
        assert_eq!((m.take_missing(), m.take_discontinuity()), (None, None));
        assert_eq!(m.begin_read(8), 8);
        m.delivered(8);
        assert_eq!(m.take_missing(), Some(360));
    }

    #[test]
    fn a_zero_sample_mark_is_ignored() {
        let mut m = LossMarks::new(96_000.0);
        m.mark(10, 0);
        assert_eq!(m.begin_read(100), 100); // not capped at 10
        m.delivered(100);
        assert_eq!((m.take_missing(), m.take_discontinuity()), (None, None));
    }

    #[test]
    #[cfg(not(debug_assertions))]
    fn a_mark_behind_delivered_is_clamped_to_delivered() {
        let mut m = LossMarks::new(96_000.0);
        assert_eq!(m.begin_read(100), 100);
        m.delivered(100);
        m.mark(40, 360); // release builds clamp; debug builds assert
        assert_eq!(m.begin_read(10), 10);
        m.delivered(10);
        assert_eq!(m.take_missing(), Some(360));
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "behind")]
    fn a_mark_behind_delivered_is_clamped_to_delivered() {
        let mut m = LossMarks::new(96_000.0);
        assert_eq!(m.begin_read(100), 100);
        m.delivered(100);
        m.mark(40, 360);
    }

    #[test]
    fn separate_marks_are_reported_one_read_each() {
        let mut m = LossMarks::new(96_000.0);
        m.mark(100, 10);
        m.mark(300, 20);
        assert_eq!(m.begin_read(1_000), 100);
        m.delivered(100);
        assert_eq!(m.begin_read(1_000), 200);
        m.delivered(200);
        assert_eq!(m.take_missing(), Some(10));
        assert_eq!(m.begin_read(1_000), 1_000);
        m.delivered(1_000);
        assert_eq!(m.take_missing(), Some(20));
    }
}
