//! MAN-308: automatic step attenuation for a protocol-2 radio's ADC0. The
//! controller is pure state that `Protocol2IqSource` feeds with the instants
//! it reads datagrams at; [`AutoAttenuation`] is the state it shares with the
//! CLI, which logs each change and samples the level into `/metrics`.
//!
//! Every number here is recorded in
//! `docs/DECISIONS/2026-10-27-man308-hpsdr-auto-attenuation.md` (D3, D4).

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::time::{Duration, Instant};

/// Overload reports this close together are one overload: the status
/// datagram arrives every 50 ms and its bit covers one report (MAN-308 D3).
pub const OVERLOAD_HOLD: Duration = Duration::from_millis(500);
/// Overload this long raises the attenuator one step (MAN-308 D3).
pub const TRIP_AFTER: Duration = Duration::from_secs(2);
/// No overload this long lowers it one step (MAN-308 D3).
pub const RELEASE_AFTER: Duration = Duration::from_secs(600);
/// One step, dB (MAN-308 D4).
pub const STEP_DB: u8 = 6;
/// The most automatic attenuation adds, dB (MAN-308 D4).
pub const MAX_DB: u8 = 30;

/// The controller's three timings; tests shorten them after open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Timing {
    /// [`OVERLOAD_HOLD`] in production.
    pub(crate) hold: Duration,
    /// [`TRIP_AFTER`] in production.
    pub(crate) trip_after: Duration,
    /// [`RELEASE_AFTER`] in production.
    pub(crate) release_after: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            hold: OVERLOAD_HOLD,
            trip_after: TRIP_AFTER,
            release_after: RELEASE_AFTER,
        }
    }
}

/// Why automatic attenuation changed the step attenuator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttenuationCause {
    /// ADC overload for [`TRIP_AFTER`]: one step up.
    Overload,
    /// No ADC overload for [`RELEASE_AFTER`]: one step down.
    Quiet,
}

/// One step the controller took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AttenuationStep {
    pub(crate) from_db: u8,
    pub(crate) to_db: u8,
    pub(crate) cause: AttenuationCause,
}

/// The pure state machine (MAN-308 D3/D4). It reads no clock: callers pass
/// `now`.
#[derive(Debug)]
pub(crate) struct Controller {
    db: u8,
    pub(crate) timing: Timing,
    /// Start of the current overload run, moved to each raise.
    run_start: Option<Instant>,
    /// The last overload report.
    last_overload: Option<Instant>,
    /// The later of the last overload report, the last change and the open.
    quiet_since: Instant,
}

impl Controller {
    /// A controller at `db` (the level restored on a reopen, MAN-308 D6)
    /// whose quiet clock starts at `now`.
    pub(crate) fn new(db: u8, now: Instant, timing: Timing) -> Self {
        Self {
            db: db.min(MAX_DB),
            timing,
            run_start: None,
            last_overload: None,
            quiet_since: now,
        }
    }

    /// The step attenuation to send, dB.
    pub(crate) fn db(&self) -> u8 {
        self.db
    }

    /// One status datagram's ADC0 overload bit, read at `now`.
    pub(crate) fn observe(&mut self, overloaded: bool, now: Instant) -> Option<AttenuationStep> {
        // A clear report changes nothing: the run ends when the hold lapses.
        if !overloaded {
            return None;
        }
        let continues = self
            .last_overload
            .is_some_and(|last| now.saturating_duration_since(last) <= self.timing.hold);
        if !continues {
            self.run_start = Some(now);
        }
        self.last_overload = Some(now);
        self.quiet_since = now;
        let run_start = *self.run_start.get_or_insert(now);
        if self.db >= MAX_DB || now.saturating_duration_since(run_start) < self.timing.trip_after {
            return None;
        }
        let from_db = self.db;
        self.db = from_db.saturating_add(STEP_DB).min(MAX_DB);
        self.run_start = Some(now);
        Some(AttenuationStep {
            from_db,
            to_db: self.db,
            cause: AttenuationCause::Overload,
        })
    }

    /// Lower one step once `release_after` has passed since the later of the
    /// last overload report, the last change and the open.
    pub(crate) fn tick(&mut self, now: Instant) -> Option<AttenuationStep> {
        if self.db == 0
            || now.saturating_duration_since(self.quiet_since) < self.timing.release_after
        {
            return None;
        }
        let from_db = self.db;
        self.db = from_db.saturating_sub(STEP_DB);
        self.quiet_since = now;
        Some(AttenuationStep {
            from_db,
            to_db: self.db,
            cause: AttenuationCause::Quiet,
        })
    }
}

/// One change automatic attenuation made, as the CLI logs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttenuationChange {
    /// The radio, as `host:port`.
    pub radio: String,
    /// The level before the change, dB.
    pub from_db: u8,
    /// The level after the change, dB.
    pub to_db: u8,
    /// Why it changed.
    pub cause: AttenuationCause,
}

/// What the metrics poller publishes (MAN-308 D8).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AttenuationSnapshot {
    /// The step attenuation manta last set on ADC0, dB.
    pub db: u8,
    /// Changes made since the daemon started, raises and lowers together.
    pub changes: u64,
}

/// The CLI's log line for one change.
pub type AttenuationNotice = Box<dyn Fn(&AttenuationChange) + Send + Sync>;

/// MAN-308: automatic attenuation's state, shared by every (re)open of one
/// input and by the CLI. `manta-input` has no logging dependency, so the CLI
/// injects the log line as `notice` (the `with_overflow_notice` pattern).
pub struct AutoAttenuation {
    db: AtomicU8,
    changes: AtomicU64,
    engaged: AtomicBool,
    notice: AttenuationNotice,
}

impl AutoAttenuation {
    /// A handle at 0 dB that no open has engaged yet; `notice` runs once per
    /// change, on the source's read thread.
    pub fn new(notice: AttenuationNotice) -> Self {
        Self {
            db: AtomicU8::new(0),
            changes: AtomicU64::new(0),
            engaged: AtomicBool::new(false),
            notice,
        }
    }

    /// The step attenuation manta last set on ADC0, dB.
    pub fn db(&self) -> u8 {
        self.db.load(Ordering::Relaxed)
    }

    /// Changes made since this handle was built.
    pub fn changes(&self) -> u64 {
        self.changes.load(Ordering::Relaxed)
    }

    /// `None` until a protocol-2 open has adopted this state (sticky after
    /// that), so an input that only ever found a protocol-1 radio publishes
    /// no series.
    pub fn snapshot(&self) -> Option<AttenuationSnapshot> {
        self.engaged
            .load(Ordering::Relaxed)
            .then(|| AttenuationSnapshot {
                db: self.db(),
                changes: self.changes(),
            })
    }

    /// A protocol-2 open: mark engaged and return the level to restore
    /// (MAN-308 D6).
    pub(crate) fn engage(&self) -> u8 {
        self.engaged.store(true, Ordering::Relaxed);
        self.db()
    }

    /// An open that landed on protocol 1: manta sets nothing on that radio,
    /// so the level reads 0. No notice; engagement unchanged.
    pub fn clear(&self) {
        self.db.store(0, Ordering::Relaxed);
    }

    /// Record one change and tell the operator.
    pub(crate) fn record(&self, change: &AttenuationChange) {
        self.db.store(change.to_db, Ordering::Relaxed);
        self.changes.fetch_add(1, Ordering::Relaxed);
        (self.notice)(change);
    }
}

impl fmt::Debug for AutoAttenuation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AutoAttenuation")
            .field("db", &self.db())
            .field("changes", &self.changes())
            .field("engaged", &self.engaged.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    const EVERY: Duration = Duration::from_millis(50);

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// Reports every `every` over `[from, to)` after `t0`; the steps they
    /// gave, at their offsets.
    fn reports_every(
        c: &mut Controller,
        t0: Instant,
        every: Duration,
        from: Duration,
        to: Duration,
        overloaded: bool,
    ) -> Vec<(Duration, AttenuationStep)> {
        let mut out = Vec::new();
        let mut at = from;
        while at < to {
            if let Some(step) = c.observe(overloaded, t0 + at) {
                out.push((at, step));
            }
            at += every;
        }
        out
    }

    /// Reports every 50 ms (the radio's receive cadence).
    fn reports(
        c: &mut Controller,
        t0: Instant,
        from: Duration,
        to: Duration,
        overloaded: bool,
    ) -> Vec<(Duration, AttenuationStep)> {
        reports_every(c, t0, EVERY, from, to, overloaded)
    }

    fn raise(from_db: u8, to_db: u8) -> AttenuationStep {
        AttenuationStep {
            from_db,
            to_db,
            cause: AttenuationCause::Overload,
        }
    }

    fn lower(from_db: u8, to_db: u8) -> AttenuationStep {
        AttenuationStep {
            from_db,
            to_db,
            cause: AttenuationCause::Quiet,
        }
    }

    fn fresh(t0: Instant) -> Controller {
        Controller::new(0, t0, Timing::default())
    }

    #[test]
    fn two_seconds_of_overload_raises_6_db() {
        let t0 = Instant::now();
        let mut c = fresh(t0);
        assert!(reports(&mut c, t0, ms(0), ms(1950), true).is_empty());
        assert_eq!(c.observe(true, t0 + ms(1950)), None);
        assert_eq!(c.observe(true, t0 + ms(2000)), Some(raise(0, 6)));
        assert_eq!(c.db(), 6);
    }

    #[test]
    fn four_seconds_of_overload_add_12_db() {
        let t0 = Instant::now();
        let mut c = fresh(t0);
        let steps = reports(&mut c, t0, ms(0), ms(4050), true);
        assert_eq!(
            steps,
            vec![(ms(2000), raise(0, 6)), (ms(4000), raise(6, 12))]
        );
        assert_eq!(c.db(), 12);
    }

    #[test]
    fn a_gap_longer_than_the_hold_restarts_the_run() {
        let t0 = Instant::now();
        let mut c = fresh(t0);
        assert!(reports(&mut c, t0, ms(0), ms(1050), true).is_empty());
        assert!(reports(&mut c, t0, ms(1050), ms(1600), false).is_empty());
        let steps = reports(&mut c, t0, ms(1600), ms(3650), true);
        assert_eq!(steps, vec![(ms(3600), raise(0, 6))]);
    }

    #[test]
    fn reports_within_the_hold_are_one_overload() {
        let t0 = Instant::now();
        let mut c = fresh(t0);
        let steps = reports_every(&mut c, t0, ms(400), ms(0), ms(2050), true);
        assert_eq!(steps, vec![(ms(2000), raise(0, 6))]);
    }

    #[test]
    fn ten_quiet_minutes_lower_12_db_to_6() {
        let t0 = Instant::now();
        let mut c = fresh(t0);
        assert_eq!(reports(&mut c, t0, ms(0), ms(4050), true).len(), 2);
        let storm_end = t0 + secs(4);
        assert_eq!(c.tick(storm_end + ms(599_999)), None);
        assert_eq!(c.tick(storm_end + secs(600)), Some(lower(12, 6)));
        assert_eq!(c.tick(storm_end + secs(1199)), None);
        assert_eq!(c.tick(storm_end + secs(1200)), Some(lower(6, 0)));
        assert_eq!(c.db(), 0);
        assert_eq!(c.tick(storm_end + secs(1800)), None);
        assert_eq!(c.tick(storm_end + secs(100_000)), None);
    }

    #[test]
    fn one_overload_report_restarts_the_quiet_clock() {
        let t0 = Instant::now();
        let mut c = Controller::new(6, t0, Timing::default());
        assert_eq!(c.observe(true, t0 + secs(300)), None);
        assert_eq!(c.tick(t0 + secs(600)), None);
        assert_eq!(c.tick(t0 + ms(899_999)), None);
        assert_eq!(c.tick(t0 + secs(900)), Some(lower(6, 0)));
    }

    #[test]
    fn clear_reports_and_adc1_overload_never_raise() {
        let t0 = Instant::now();
        let mut c = fresh(t0);
        assert!(reports(&mut c, t0, ms(0), secs(10), false).is_empty());
        let adc1_only: u8 = 0x02;
        let mut at = secs(10);
        while at < secs(20) {
            assert_eq!(c.observe(adc1_only & 0x01 != 0, t0 + at), None);
            at += EVERY;
        }
        assert_eq!(c.db(), 0);
    }

    #[test]
    fn holds_at_30_db() {
        let t0 = Instant::now();
        let mut c = fresh(t0);
        let mut levels = Vec::new();
        let mut at = ms(0);
        while at < secs(20) {
            if let Some(step) = c.observe(true, t0 + at) {
                levels.push(step.to_db);
            }
            assert!(c.db() <= MAX_DB);
            at += EVERY;
        }
        assert_eq!(levels, vec![6, 12, 18, 24, 30]);
        assert_eq!(c.db(), 30);
    }

    #[test]
    fn a_seeded_level_starts_its_quiet_clock_at_construction() {
        let t0 = Instant::now();
        let mut c = Controller::new(12, t0, Timing::default());
        assert_eq!(c.db(), 12);
        assert_eq!(c.tick(t0 + ms(599_999)), None);
        assert_eq!(c.tick(t0 + secs(600)), Some(lower(12, 6)));
    }

    fn recording() -> (AutoAttenuation, Arc<Mutex<Vec<AttenuationChange>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        let handle = AutoAttenuation::new(Box::new(move |c| s.lock().unwrap().push(c.clone())));
        (handle, seen)
    }

    fn change(from_db: u8, to_db: u8, cause: AttenuationCause) -> AttenuationChange {
        AttenuationChange {
            radio: "192.0.2.10:1024".to_string(),
            from_db,
            to_db,
            cause,
        }
    }

    #[test]
    fn record_updates_level_and_count_and_notifies_once() {
        let (handle, seen) = recording();
        let up = change(0, 6, AttenuationCause::Overload);
        let down = change(6, 0, AttenuationCause::Quiet);
        handle.record(&up);
        handle.record(&down);
        assert_eq!(handle.db(), 0);
        assert_eq!(handle.changes(), 2);
        assert_eq!(*seen.lock().unwrap(), vec![up, down]);
    }

    #[test]
    fn snapshot_is_none_until_a_protocol_2_open_engages() {
        let (handle, seen) = recording();
        assert_eq!(handle.snapshot(), None);
        handle.clear();
        assert_eq!(handle.snapshot(), None, "clear does not engage");
        assert_eq!(handle.engage(), 0);
        handle.record(&change(0, 6, AttenuationCause::Overload));
        assert_eq!(
            handle.snapshot(),
            Some(AttenuationSnapshot { db: 6, changes: 1 })
        );
        handle.clear();
        assert_eq!(
            handle.snapshot(),
            Some(AttenuationSnapshot { db: 0, changes: 1 }),
            "engagement is sticky; clear only zeroes the level"
        );
        assert_eq!(seen.lock().unwrap().len(), 1, "clear does not notify");
    }
}
