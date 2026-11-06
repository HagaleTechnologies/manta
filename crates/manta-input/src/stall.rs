//! MAN-489: the shared soft-stall episode state behind every live source's
//! `take_stalled` report.
//!
//! A live stream that stops delivering samples sends no hops, so nothing
//! settles the last call on a track until the engine hears the stream
//! stopped (MAN-201 D3). ka9q already reported that stop through
//! `Ok(0)` plus one consumable `IqSource::take_stalled` flag; this module
//! gives the other live sources (Kiwi, both HPSDR protocols, audio,
//! SoapySDR) the same one-shot latch so each reports inside the 5 s bound
//! while its transport may still be alive, keeps reading the same
//! connection after the report, and re-arms only on a fresh positive
//! delivery.
//!
//! The latch tracks *sample* progress only: transport keepalives, status
//! traffic and control frames never re-arm it, and a consumer's parked
//! interval is the caller's clock, not the source's (each source keeps its
//! own last-sample `Instant` and passes it in explicitly, so the helper
//! itself stays free of any acquisition-clock or missing-sample arithmetic).

use std::time::{Duration, Instant};

/// Default soft report deadline for network sources (Kiwi, both HPSDR
/// protocols, audio): decision D3's 5 s held-call bound minus the 250 ms
/// receive-poll granularity, so the report lands inside the bound with
/// margin instead of on its edge. SoapySDR keeps its own tighter 1 s
/// deadline ahead of its ~2 s retry limit.
pub(crate) const SOFT_STALL_AFTER: Duration = Duration::from_millis(4_500);

/// One outage episode of a delivering live source: armed by sample
/// delivery, fired once per outage, re-armed by the next delivery.
#[derive(Debug)]
pub(crate) struct StallEpisode {
    /// How long after the last accepted sample the report fires. A test
    /// knob at source level; production passes [`SOFT_STALL_AFTER`] (or
    /// SoapySDR's tighter deadline).
    soft_after: Duration,
    /// Samples were accepted since the last report. The report only ever
    /// fires for a stream that had delivered: a source that never delivered
    /// has nothing held to settle.
    delivering: bool,
    /// The one-shot report flag: set when the soft deadline elapses,
    /// consumed by `take_stalled`.
    pending: bool,
}

impl StallEpisode {
    pub(crate) fn new(soft_after: Duration) -> Self {
        Self {
            soft_after,
            delivering: false,
            pending: false,
        }
    }

    /// Note accepted sample progress (a positive payload; all-zero samples
    /// are delivery). Arms the episode; a source that only ever received
    /// keepalive or status traffic stays unarmed.
    pub(crate) fn note_delivery(&mut self) {
        self.delivering = true;
    }

    /// Whether the soft report is due at `now`, given the last accepted
    /// sample at `last_sample`. `hard` is the source's own hard-failure
    /// bound: when it is not longer than the soft deadline (a test with
    /// every bound shortened), the hard failure owns the outage and no
    /// soft report is made, exactly as ka9q guards its own settle bound.
    pub(crate) fn due(&self, last_sample: Instant, now: Instant, hard: Duration) -> bool {
        self.delivering
            && hard > self.soft_after
            && now.duration_since(last_sample) >= self.soft_after
    }

    /// Fire the report: latch the one-shot flag and disarm until the next
    /// delivery. Call only after `due` returned `true`.
    pub(crate) fn report(&mut self) {
        self.delivering = false;
        self.pending = true;
    }

    /// Consume the pending report flag once (`IqSource::take_stalled`).
    pub(crate) fn take(&mut self) -> bool {
        std::mem::take(&mut self.pending)
    }

    /// Clamp a receive or sleep wait so it can never be scheduled past the
    /// next soft deadline: a blocking receive longer than the remaining
    /// wait would return only after the deadline had already elapsed,
    /// overshooting the 5 s bound by up to a full poll interval. Used by
    /// SoapySDR's receive-timeout capping (feature `soapy`).
    #[cfg_attr(not(feature = "soapy"), allow(dead_code))]
    pub(crate) fn clamp_wait(
        &self,
        wait: Duration,
        last_sample: Instant,
        now: Instant,
    ) -> Duration {
        if !self.delivering {
            return wait;
        }
        let until_deadline = self
            .soft_after
            .checked_sub(now.duration_since(last_sample))
            .unwrap_or(Duration::ZERO);
        wait.min(until_deadline)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOFT: Duration = Duration::from_millis(4_500);
    const HARD: Duration = Duration::from_secs(10);

    /// No report before the first positive delivery, however long the
    /// stream has been silent: a source that never delivered holds nothing.
    #[test]
    fn no_report_before_the_first_delivery() {
        let opened = Instant::now();
        let episode = StallEpisode::new(SOFT);
        assert!(!episode.due(opened, opened + SOFT + SOFT, HARD));
    }

    /// The report fires exactly at the soft deadline and its flag is
    /// consumed once.
    #[test]
    fn report_is_due_at_the_soft_deadline_and_consumed_once() {
        let last = Instant::now();
        let mut episode = StallEpisode::new(SOFT);
        episode.note_delivery();
        assert!(!episode.due(last, last + SOFT - Duration::from_millis(1), HARD));
        assert!(episode.due(last, last + SOFT, HARD));
        episode.report();
        assert!(episode.take());
        assert!(!episode.take());
    }

    /// After the flag is consumed the episode waits: no re-arm, no repeat
    /// report, until new samples arrive.
    #[test]
    fn after_the_report_the_episode_waits_until_new_samples() {
        let last = Instant::now();
        let mut episode = StallEpisode::new(SOFT);
        episode.note_delivery();
        episode.report();
        assert!(!episode.due(last, last + SOFT + SOFT, HARD));
    }

    /// Delivery re-arms the episode -- all-zero samples are delivery -- so
    /// a second outage reports again.
    #[test]
    fn delivery_re_arms_for_a_second_outage() {
        let mut episode = StallEpisode::new(SOFT);
        episode.note_delivery();
        episode.report();
        // Samples resume (all-zero samples still count) at `resumed`.
        let resumed = Instant::now();
        episode.note_delivery();
        assert!(!episode.due(resumed, resumed + Duration::from_millis(100), HARD));
        assert!(episode.due(resumed, resumed + SOFT, HARD));
        episode.report();
        assert!(episode.take());
    }

    /// A soft report never fires when the source's hard bound is not
    /// longer than the soft one (tests with every bound shortened): the
    /// hard failure owns the outage.
    #[test]
    fn a_hard_bound_at_or_below_the_soft_one_owns_the_outage() {
        let last = Instant::now();
        let mut episode = StallEpisode::new(SOFT);
        episode.note_delivery();
        assert!(!episode.due(last, last + SOFT + SOFT, SOFT));
        assert!(!episode.due(last, last + SOFT + SOFT, SOFT - Duration::from_millis(1)));
    }

    /// Waiting arithmetic can never schedule a receive past the next
    /// deadline.
    #[test]
    fn clamp_wait_never_overshoots_the_deadline() {
        let last = Instant::now();
        let episode = StallEpisode::new(SOFT);
        // Unarmed: the wait passes through untouched.
        assert_eq!(
            episode.clamp_wait(Duration::from_secs(30), last, last),
            Duration::from_secs(30)
        );
        // Armed with 1 s of the soft window left: a 250 ms poll fits, a
        // 30 s wait does not.
        let mut armed = StallEpisode::new(SOFT);
        armed.note_delivery();
        let now = last + SOFT - Duration::from_secs(1);
        assert_eq!(
            armed.clamp_wait(Duration::from_millis(250), last, now),
            Duration::from_millis(250)
        );
        assert_eq!(
            armed.clamp_wait(Duration::from_secs(30), last, now),
            Duration::from_secs(1)
        );
        // Past the deadline: nothing left to wait.
        assert_eq!(
            armed.clamp_wait(Duration::from_millis(250), last, last + SOFT),
            Duration::ZERO
        );
    }

    /// Each production soft duration sits below its source's hard limit,
    /// so the report always precedes the hard failure.
    #[test]
    fn production_soft_durations_sit_below_their_hard_limits() {
        // Kiwi / HPSDR protocol 1: 40 x 250 ms.
        assert!(SOFT_STALL_AFTER < Duration::from_millis(250 * 40));
        // HPSDR protocol 2: STALL_AFTER, asserted 10 s by its own test.
        assert!(SOFT_STALL_AFTER < Duration::from_secs(10));
        // Audio: 5 s device-loss bound.
        assert!(SOFT_STALL_AFTER < Duration::from_secs(5));
        // SoapySDR: 1 s soft, ahead of its ~2 s retry limit.
        assert!(Duration::from_secs(1) < Duration::from_millis(100 * 20));
    }
}
