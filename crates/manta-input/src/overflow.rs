//! SoapySDR driver-overflow accounting (MAN-233, MAN-202), outside the
//! feature-gated `soapy` module so its tests run in the default build: the
//! `soapy` feature needs the native SoapySDR library, which neither the
//! implement nor the validate container has. Only `soapy.rs` calls it.
#![cfg_attr(not(feature = "soapy"), allow(dead_code))]

use crate::InputHealthCounters;
use std::time::{Duration, Instant};

/// How often the operator overflow notice may fire (MAN-233). Fixed, not
/// configurable: it matches the MAN-122 status-line default interval, and
/// the counter carries the fine-grained data -- the warning only has to say
/// "this is happening", not how often.
pub(crate) const OVERFLOW_WARN_INTERVAL: Duration = Duration::from_secs(60);

/// MAN-202: an unknown-length loss waiting to be reported through
/// `IqSource::take_unknown_gap`. Any number of overflows before the next
/// `take` latch one report: the engine's response (invalidate every track's
/// continuity-dependent state) is the same for one dropped chunk or many,
/// and an overflow carries no count to sum.
#[derive(Debug, Default)]
pub(crate) struct UnknownGapLatch(bool);

impl UnknownGapLatch {
    /// Record that the stream lost samples of unknown count.
    pub(crate) fn mark(&mut self) {
        self.0 = true;
    }

    /// `true` once after one or more `mark`s, then `false`.
    pub(crate) fn take(&mut self) -> bool {
        std::mem::take(&mut self.0)
    }
}

/// Account one driver ring-buffer overflow (the field reality
/// `MAX_OVERFLOW_RETRIES`' doc in `soapy.rs` records): exactly one gap
/// *event* -- an `ErrorCode::Overflow` carries no lost-sample count, so
/// nothing is ever `record_dropped` and `dropped_packets` stays 0 -- the
/// operator notice only when the rate-limit gate is open, and (MAN-202) one
/// unknown-length gap latched for `IqSource::take_unknown_gap`, so the
/// engine does not decode the splice as Morse timing. Free-function shape
/// after kiwi's `account_snd_frame` so the count/rate-limit semantics are
/// unit-testable with crafted `Instant`s and no device (MAN-233; the
/// `soapy` feature cannot compile in the implement container).
pub(crate) fn account_overflow(
    counters: &InputHealthCounters,
    last_warn: &mut Option<Instant>,
    notice: &Option<Box<dyn Fn() + Send + Sync>>,
    now: Instant,
    latch: &mut UnknownGapLatch,
) {
    counters.record_gap();
    latch.mark();
    let gate_open = match *last_warn {
        Some(last) => now.duration_since(last) >= OVERFLOW_WARN_INTERVAL,
        None => true,
    };
    if gate_open {
        *last_warn = Some(now);
        if let Some(notice) = notice {
            notice();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MAN-233 decision 1/4: one overflow is exactly one gap *event* -- an
    /// `ErrorCode::Overflow` carries no lost-sample count, so nothing is
    /// ever `record_dropped` -- and only `Overflow` counts: a `Timeout` is
    /// how a quiet band presents and must not move any counter.
    #[test]
    fn account_overflow_counts_one_gap_event_per_overflow_and_nothing_else() {
        let counters = InputHealthCounters::new();
        let mut last_warn = None;
        let mut latch = UnknownGapLatch::default();
        let now = Instant::now();
        for _ in 0..3 {
            account_overflow(&counters, &mut last_warn, &None, now, &mut latch);
        }
        assert_eq!(counters.gaps_detected(), 3);
        assert_eq!(counters.dropped_packets(), 0);
        assert_eq!(counters.malformed_packets(), 0);
    }

    /// MAN-233 decision 2: the notice fires on the first overflow, then at
    /// most once per OVERFLOW_WARN_INTERVAL. Deterministic `Instant`
    /// arithmetic, no sleeping -- a burst of N overflows inside one
    /// `read()` yields at most one line.
    #[test]
    fn account_overflow_notice_is_rate_limited_to_the_interval() {
        let counters = InputHealthCounters::new();
        let mut last_warn = None;
        let mut latch = UnknownGapLatch::default();
        // The notice box is `dyn Fn() + Send + Sync + 'static`, so the
        // closure must own its counter: share it through an `Arc`.
        let fired = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fired_in_notice = std::sync::Arc::clone(&fired);
        let notice: Option<Box<dyn Fn() + Send + Sync>> = Some(Box::new(move || {
            fired_in_notice.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));

        let t0 = Instant::now();
        account_overflow(&counters, &mut last_warn, &notice, t0, &mut latch);
        assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(last_warn, Some(t0));

        // A burst within the interval: counted, but silent.
        account_overflow(
            &counters,
            &mut last_warn,
            &notice,
            t0 + Duration::from_secs(30),
            &mut latch,
        );
        account_overflow(
            &counters,
            &mut last_warn,
            &notice,
            t0 + Duration::from_secs(59),
            &mut latch,
        );
        assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(last_warn, Some(t0), "gate closed: last_warn must not move");

        // At exactly the interval the gate opens again.
        let t1 = t0 + OVERFLOW_WARN_INTERVAL;
        account_overflow(&counters, &mut last_warn, &notice, t1, &mut latch);
        assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(last_warn, Some(t1));

        // Every event still counted, notice or not.
        assert_eq!(counters.gaps_detected(), 4);
    }

    /// MAN-202: one or many overflows before a `take` latch exactly one
    /// unknown-length gap, reported once.
    #[test]
    fn account_overflow_latches_one_unknown_gap() {
        let counters = InputHealthCounters::new();
        let mut last_warn = None;
        let mut latch = UnknownGapLatch::default();
        assert!(!latch.take(), "nothing latched before an overflow");
        let now = Instant::now();
        account_overflow(&counters, &mut last_warn, &None, now, &mut latch);
        assert!(latch.take());
        assert!(!latch.take(), "reported once");
        for _ in 0..5 {
            account_overflow(&counters, &mut last_warn, &None, now, &mut latch);
        }
        assert!(latch.take(), "a burst latches one report");
        assert!(!latch.take());
    }

    /// MAN-233 Decision 3 ("a timeout counts nothing") holds for the latch
    /// too: the `Timeout` arm of `SoapySdrIqSource::read` never calls
    /// `account_overflow`, so a quiet band latches no unknown gap.
    #[test]
    fn a_timeout_latches_nothing() {
        let counters = InputHealthCounters::new();
        let mut latch = UnknownGapLatch::default();
        // The timeout path touches neither the counters nor the latch.
        assert!(!latch.take());
        assert_eq!(counters.gaps_detected(), 0);
        let mut last_warn = None;
        account_overflow(&counters, &mut last_warn, &None, Instant::now(), &mut latch);
        assert!(latch.take());
        // A later run of timeouts leaves the drained latch empty.
        assert!(!latch.take());
        assert_eq!(counters.gaps_detected(), 1);
    }
}
