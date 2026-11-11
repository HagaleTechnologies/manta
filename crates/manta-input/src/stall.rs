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
//!
//! MAN-533 adds the hard counterpart every live source shares: a stream
//! that keeps reading -- frames arriving, a socket timing out, a device
//! returning nothing -- without one usable sample for
//! [`NO_USABLE_SAMPLES_AFTER`] gives its connection up with the typed
//! [`NoUsableSamples`] error, which the reconnect layer recognises
//! ([`is_no_usable_samples`]) and reopens. The window is measured as active
//! read time ([`UsableSampleBudget`]), so a consumer parked away from
//! `read()` spends none of it.

use std::time::{Duration, Instant};

/// MAN-533: how long a live source may keep reading -- frames arriving, a
/// socket timing out, a device returning nothing -- without one usable
/// sample before it gives the connection up with [`NoUsableSamples`].
/// Longer than every soft report and silence bound, so a stream that
/// paused reports through MAN-489's soft stall first.
pub const NO_USABLE_SAMPLES_AFTER: Duration = Duration::from_secs(30);

/// MAN-533: a live source's reads kept running for its bound without a
/// usable sample. `Display` is the source's own message, unchanged; the
/// type is what the reconnect layer recognises ([`is_no_usable_samples`]).
#[derive(Debug)]
pub struct NoUsableSamples {
    detail: String,
}

impl NoUsableSamples {
    /// Wrap the source's own message; it is shown verbatim.
    pub fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for NoUsableSamples {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for NoUsableSamples {}

/// Whether `err`, or any cause under it, is [`NoUsableSamples`].
pub fn is_no_usable_samples(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| cause.is::<NoUsableSamples>())
}

/// MAN-533: active read time spent with no usable sample since the last
/// one (D1). Only time inside a receive loop is spent, so a consumer parked
/// elsewhere spends nothing; a delivery refills it. Independent of the
/// MAN-489 [`StallEpisode`] latch: a never-delivering stream spends it from
/// open.
#[derive(Debug)]
pub(crate) struct UsableSampleBudget {
    limit: Duration,
    spent: Duration,
}

impl UsableSampleBudget {
    pub(crate) fn new(limit: Duration) -> Self {
        Self {
            limit,
            spent: Duration::ZERO,
        }
    }

    /// Usable samples reached the source's output: refill the budget.
    pub(crate) fn note_delivery(&mut self) {
        self.spent = Duration::ZERO;
    }

    /// Spend `d` of active read time with no usable sample. Returns `true`
    /// once the total spent since the last delivery reaches the limit.
    pub(crate) fn spend(&mut self, d: Duration) -> bool {
        self.spent = self.spent.saturating_add(d);
        self.spent >= self.limit
    }

    /// The window this budget enforces, for the source's error message.
    pub(crate) fn limit(&self) -> Duration {
        self.limit
    }
}

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

    #[test]
    fn usable_sample_budget_is_not_spent_before_its_limit() {
        let mut b = UsableSampleBudget::new(Duration::from_secs(30));
        assert!(!b.spend(Duration::from_secs(29)));
        assert!(!b.spend(Duration::from_millis(999)));
    }

    #[test]
    fn usable_sample_budget_is_spent_at_its_limit() {
        let mut b = UsableSampleBudget::new(Duration::from_secs(30));
        assert!(!b.spend(Duration::from_secs(20)));
        assert!(b.spend(Duration::from_secs(10)));
    }

    #[test]
    fn a_delivery_refills_the_usable_sample_budget() {
        let mut b = UsableSampleBudget::new(Duration::from_secs(30));
        assert!(!b.spend(Duration::from_secs(29)));
        b.note_delivery();
        assert!(!b.spend(Duration::from_secs(29)));
    }

    /// D1: the budget is independent of the MAN-489 latch -- a soft report
    /// neither refills nor pauses it, and a never-delivering stream spends it
    /// from open.
    #[test]
    fn the_usable_sample_budget_ignores_the_soft_stall_latch() {
        let mut b = UsableSampleBudget::new(Duration::from_secs(30));
        let mut episode = StallEpisode::new(SOFT);
        assert!(!b.spend(Duration::from_secs(10)));
        episode.note_delivery();
        episode.report();
        assert!(episode.take());
        assert!(b.spend(Duration::from_secs(20)));
    }

    #[test]
    fn no_usable_samples_keeps_the_source_message_and_is_recognised_under_context() {
        let err: anyhow::Error =
            NoUsableSamples::new("KiwiSDR delivered no usable samples for 30 s").into();
        assert_eq!(
            err.to_string(),
            "KiwiSDR delivered no usable samples for 30 s"
        );
        assert!(is_no_usable_samples(&err));
        assert!(is_no_usable_samples(&err.context("decimating source")));
        assert!(!is_no_usable_samples(&anyhow::anyhow!(
            "KiwiSDR closed the connection"
        )));
    }

    #[test]
    fn the_no_usable_samples_window_exceeds_every_other_bound() {
        assert_eq!(NO_USABLE_SAMPLES_AFTER, Duration::from_secs(30));
        assert!(NO_USABLE_SAMPLES_AFTER > SOFT_STALL_AFTER);
        assert!(NO_USABLE_SAMPLES_AFTER > Duration::from_millis(250 * 40)); // Kiwi / HPSDR p1 silence
        assert!(NO_USABLE_SAMPLES_AFTER > Duration::from_secs(20)); // kiwi_stall_acceptance's pause gate
    }
}
