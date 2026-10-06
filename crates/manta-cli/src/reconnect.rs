//! MAN-73: keep a live input source alive across disconnects. A read error
//! from the wrapped source is not fatal -- it's retried forever (until
//! Ctrl-C) with `manta_server::backoff`'s shared 1s->60s policy, reusing
//! the same policy the outbound RBN uplink (`uplink.rs`) already uses.
//! Health transitions (`manta_source_health`) are reported through a
//! caller-supplied sink, and a reconnect reports the outage, in samples,
//! via `IqSource::take_discontinuity()` so `manta_engine::listen` can keep
//! spot timestamps wall-clock-true without splicing or zero-filling the
//! gap. Never wraps file replay: its errors and EOF are deterministic and
//! must reach `listen()` unchanged.
//!
//! Time and sleeping are behind the `ReconnectEnv` trait so the state
//! machine's tests run instantly, with no real sleeping.

use manta_input::IqSource;
use manta_server::backoff::{next_backoff, AttemptOutcome, INITIAL_BACKOFF};
use num_complex::Complex32;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Wall clock + interruptible sleep, injected so tests run instantly and
/// deterministically instead of depending on real time.
pub(crate) trait ReconnectEnv {
    fn now(&mut self) -> Instant;
    /// Sleep up to `d`, checking `stop` as it goes. Returns `false` as
    /// soon as `stop` is observed set (the sleep may not have run for the
    /// full duration), `true` otherwise.
    fn sleep(&mut self, d: Duration, stop: &AtomicBool) -> bool;
}

/// Real wall clock, sleeping in short slices so a `stop` set mid-backoff
/// is noticed promptly (bounds Ctrl-C latency during backoff to the slice
/// length) rather than only after the full backoff duration elapses.
pub(crate) struct RealEnv;

const REAL_ENV_SLEEP_SLICE: Duration = Duration::from_millis(100);

impl ReconnectEnv for RealEnv {
    fn now(&mut self) -> Instant {
        Instant::now()
    }

    fn sleep(&mut self, d: Duration, stop: &AtomicBool) -> bool {
        let deadline = Instant::now() + d;
        loop {
            if stop.load(Ordering::Relaxed) {
                return false;
            }
            let now = Instant::now();
            if now >= deadline {
                return true;
            }
            std::thread::sleep(REAL_ENV_SLEEP_SLICE.min(deadline - now));
        }
    }
}

pub(crate) type Opener = Box<dyn FnMut() -> anyhow::Result<Box<dyn IqSource>>>;
pub(crate) type HealthSink = Box<dyn FnMut(bool)>;

/// Wraps a live `IqSource`, reopening it with backoff whenever it errors.
/// `listen()` never observes the intermediate error: from its perspective
/// `read()` just returns real samples a little later than usual, after an
/// outage reported once via `take_discontinuity()`.
pub(crate) struct ReconnectingSource<E: ReconnectEnv = RealEnv> {
    name: &'static str,
    inner: Option<Box<dyn IqSource>>,
    opener: Opener,
    stop: Arc<AtomicBool>,
    env: E,
    on_health: HealthSink,
    reported_healthy: bool,
    fs: f64,
    center_freq_hz: f64,
    backoff: Duration,
    /// Whether the current `inner` has returned at least one real sample
    /// since it was (re)opened -- decides `Disconnected` vs
    /// `NeverConnected` the next time it errors, mirroring `uplink.rs`'s
    /// own "reached login" distinction.
    productive: bool,
    /// Whether the current `inner` came from `opener` (a reconnect) rather
    /// than being the original source passed to `new`/`with_env` -- only a
    /// reconnected source's first successful read produces a
    /// discontinuity.
    reopened: bool,
    last_ok: Instant,
    discontinuity: Option<u64>,
}

impl ReconnectingSource<RealEnv> {
    pub(crate) fn new(
        name: &'static str,
        first: Box<dyn IqSource>,
        opener: Opener,
        stop: Arc<AtomicBool>,
        initial_healthy: bool,
        on_health: HealthSink,
    ) -> Self {
        Self::with_env(
            name,
            first,
            opener,
            stop,
            initial_healthy,
            on_health,
            RealEnv,
        )
    }
}

impl<E: ReconnectEnv> ReconnectingSource<E> {
    pub(crate) fn with_env(
        name: &'static str,
        first: Box<dyn IqSource>,
        opener: Opener,
        stop: Arc<AtomicBool>,
        initial_healthy: bool,
        mut on_health: HealthSink,
        mut env: E,
    ) -> Self {
        on_health(initial_healthy);
        let fs = first.sample_rate();
        let center_freq_hz = first.center_freq_hz();
        let last_ok = env.now();
        ReconnectingSource {
            name,
            inner: Some(first),
            opener,
            stop,
            env,
            on_health,
            reported_healthy: initial_healthy,
            fs,
            center_freq_hz,
            backoff: INITIAL_BACKOFF,
            productive: false,
            reopened: false,
            last_ok,
            discontinuity: None,
        }
    }

    fn report_health(&mut self, healthy: bool) {
        if healthy != self.reported_healthy {
            (self.on_health)(healthy);
            self.reported_healthy = healthy;
        }
    }
}

impl<E: ReconnectEnv> IqSource for ReconnectingSource<E> {
    fn sample_rate(&self) -> f64 {
        self.fs
    }

    fn center_freq_hz(&self) -> f64 {
        self.center_freq_hz
    }

    fn read(&mut self, buf: &mut [Complex32]) -> anyhow::Result<usize> {
        loop {
            if self.inner.is_none() {
                if !self.env.sleep(self.backoff, &self.stop) {
                    // Ctrl-C during the outage: a clean end, matching EOF.
                    return Ok(0);
                }
                match (self.opener)() {
                    Ok(src) => {
                        if src.sample_rate() != self.fs {
                            anyhow::bail!(
                                "source {} reopened with a different sample rate \
                                 ({} Hz, expected {} Hz)",
                                self.name,
                                src.sample_rate(),
                                self.fs
                            );
                        }
                        if src.center_freq_hz() != self.center_freq_hz {
                            anyhow::bail!(
                                "source {} reopened with a different center frequency \
                                 ({} Hz, expected {} Hz)",
                                self.name,
                                src.center_freq_hz(),
                                self.center_freq_hz
                            );
                        }
                        self.inner = Some(src);
                        self.productive = false;
                        self.reopened = true;
                        continue;
                    }
                    Err(e) => {
                        self.backoff = next_backoff(self.backoff, &AttemptOutcome::NeverConnected);
                        eprintln!(
                            "source {} reconnect attempt failed: {e:#}; retrying in {}s",
                            self.name,
                            self.backoff.as_secs()
                        );
                        continue;
                    }
                }
            }

            // A reopened source's outage ends where its first read begins,
            // not where it returns (PR #207 review): that read can block
            // while capturing the samples it returns (a Kiwi fills its
            // resampler first), and the engine counts those samples as it
            // processes them. The same anchor the session's own `SpotBus`
            // epoch uses, which is taken before the first read. Bound: time
            // that read spends waiting before any data arrives is excluded
            // from the gap too.
            let first_read_started = self.reopened.then(|| self.env.now());
            let inner = self.inner.as_mut().expect("checked Some above");
            match inner.read(buf) {
                Ok(0) => return Ok(0),
                Ok(n) => {
                    if !self.productive {
                        self.productive = true;
                        self.backoff = INITIAL_BACKOFF; // ladder resets on recovery
                        if let Some(resumed_at) = first_read_started {
                            let outage = resumed_at.duration_since(self.last_ok);
                            let gap = (outage.as_secs_f64() * self.fs).round() as u64;
                            self.discontinuity = Some(gap);
                            self.reopened = false;
                            eprintln!(
                                "source {} reconnected after {:.1}s",
                                self.name,
                                outage.as_secs_f64()
                            );
                        }
                    }
                    self.last_ok = self.env.now();
                    self.report_health(true);
                    return Ok(n);
                }
                Err(e) => {
                    let outcome = if self.productive {
                        AttemptOutcome::Disconnected
                    } else {
                        AttemptOutcome::NeverConnected
                    };
                    self.inner = None;
                    self.backoff = next_backoff(self.backoff, &outcome);
                    eprintln!(
                        "source {} lost: {e:#}; reconnecting in {}s",
                        self.name,
                        self.backoff.as_secs()
                    );
                    self.report_health(false);
                }
            }
        }
    }

    fn take_discontinuity(&mut self) -> Option<u64> {
        self.discontinuity.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::rc::Rc;

    struct ScriptedSource {
        fs: f64,
        center_freq_hz: f64,
        script: VecDeque<anyhow::Result<Vec<Complex32>>>,
    }

    impl ScriptedSource {
        fn new(fs: f64, center_freq_hz: f64, script: Vec<anyhow::Result<Vec<Complex32>>>) -> Self {
            ScriptedSource {
                fs,
                center_freq_hz,
                script: script.into(),
            }
        }
    }

    impl IqSource for ScriptedSource {
        fn sample_rate(&self) -> f64 {
            self.fs
        }
        fn center_freq_hz(&self) -> f64 {
            self.center_freq_hz
        }
        fn read(&mut self, buf: &mut [Complex32]) -> anyhow::Result<usize> {
            match self.script.pop_front() {
                Some(Ok(samples)) => {
                    let n = samples.len().min(buf.len());
                    buf[..n].copy_from_slice(&samples[..n]);
                    Ok(n)
                }
                Some(Err(e)) => Err(e),
                None => Ok(0),
            }
        }
    }

    fn ok_chunk(n: usize) -> anyhow::Result<Vec<Complex32>> {
        Ok(vec![Complex32::new(0.1, 0.0); n])
    }

    fn err_chunk(msg: &'static str) -> anyhow::Result<Vec<Complex32>> {
        Err(anyhow::anyhow!(msg))
    }

    fn scripted(
        fs: f64,
        center: f64,
        script: Vec<anyhow::Result<Vec<Complex32>>>,
    ) -> ScriptedSource {
        ScriptedSource::new(fs, center, script)
    }

    #[derive(Clone)]
    struct FakeEnv {
        clock: Rc<Cell<Instant>>,
        sleeps: Rc<RefCell<Vec<Duration>>>,
    }

    impl FakeEnv {
        fn new() -> Self {
            FakeEnv {
                clock: Rc::new(Cell::new(Instant::now())),
                sleeps: Rc::new(RefCell::new(Vec::new())),
            }
        }

        fn advance(&self, d: Duration) {
            self.clock.set(self.clock.get() + d);
        }

        fn sleeps_secs(&self) -> Vec<u64> {
            self.sleeps.borrow().iter().map(|d| d.as_secs()).collect()
        }
    }

    impl ReconnectEnv for FakeEnv {
        fn now(&mut self) -> Instant {
            self.clock.get()
        }

        fn sleep(&mut self, d: Duration, stop: &AtomicBool) -> bool {
            self.sleeps.borrow_mut().push(d);
            if stop.load(Ordering::Relaxed) {
                return false;
            }
            self.advance(d);
            !stop.load(Ordering::Relaxed)
        }
    }

    const FS: f64 = 96_000.0;
    const CENTER: f64 = 14_000_000.0;

    fn no_stop() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    fn health_recorder() -> (HealthSink, Rc<RefCell<Vec<bool>>>) {
        let log = Rc::new(RefCell::new(Vec::new()));
        let log_for_sink = log.clone();
        let sink: HealthSink = Box::new(move |h| log_for_sink.borrow_mut().push(h));
        (sink, log)
    }

    fn opener_from(
        mut sources: VecDeque<anyhow::Result<ScriptedSource>>,
    ) -> (Opener, Rc<Cell<usize>>) {
        let calls = Rc::new(Cell::new(0));
        let calls_for_opener = calls.clone();
        let opener: Opener = Box::new(move || {
            calls_for_opener.set(calls_for_opener.get() + 1);
            match sources.pop_front() {
                Some(Ok(src)) => Ok(Box::new(src) as Box<dyn IqSource>),
                Some(Err(e)) => Err(e),
                None => panic!("opener called more times than the test scripted"),
            }
        });
        (opener, calls)
    }

    fn buf(n: usize) -> Vec<Complex32> {
        vec![Complex32::new(0.0, 0.0); n]
    }

    #[test]
    fn read_error_reopens_and_resumes_returning_samples() {
        let first = scripted(FS, CENTER, vec![ok_chunk(8), err_chunk("lost")]);
        let (opener, calls) = opener_from(VecDeque::from(vec![Ok(scripted(
            FS,
            CENTER,
            vec![ok_chunk(8)],
        ))]));
        let (health, _log) = health_recorder();
        let mut src = ReconnectingSource::with_env(
            "test",
            Box::new(first),
            opener,
            no_stop(),
            true,
            health,
            FakeEnv::new(),
        );

        let mut b = buf(8);
        assert_eq!(src.read(&mut b).unwrap(), 8, "first chunk comes through");
        let n = src.read(&mut b).unwrap();
        assert_eq!(n, 8, "the error is retried transparently, not surfaced");
        assert_eq!(calls.get(), 1, "the opener must be called exactly once");
    }

    #[test]
    fn first_wait_after_a_productive_connection_drops_is_initial_backoff() {
        let first = scripted(FS, CENTER, vec![ok_chunk(8), err_chunk("lost")]);
        let (opener, _calls) = opener_from(VecDeque::from(vec![Ok(scripted(
            FS,
            CENTER,
            vec![ok_chunk(8)],
        ))]));
        let (health, _log) = health_recorder();
        let env = FakeEnv::new();
        let env_handle = env.clone();
        let mut src = ReconnectingSource::with_env(
            "test",
            Box::new(first),
            opener,
            no_stop(),
            true,
            health,
            env,
        );

        let mut b = buf(8);
        src.read(&mut b).unwrap(); // productive
        src.read(&mut b).unwrap(); // errors, reopens, resumes
        assert_eq!(env_handle.sleeps_secs(), vec![1]);
    }

    #[test]
    fn failed_reopens_back_off_2_4_8_16_32_60_60_then_reset_after_recovery() {
        let first = scripted(FS, CENTER, vec![ok_chunk(8), err_chunk("lost")]);
        let mut script: VecDeque<anyhow::Result<ScriptedSource>> =
            (0..7).map(|_| Err(anyhow::anyhow!("refused"))).collect();
        script.push_back(Ok(scripted(
            FS,
            CENTER,
            vec![ok_chunk(8), err_chunk("lost again")],
        )));
        script.push_back(Ok(scripted(FS, CENTER, vec![ok_chunk(8)])));
        let (opener, calls) = opener_from(script);
        let (health, _log) = health_recorder();
        let env = FakeEnv::new();
        let env_handle = env.clone();
        let mut src = ReconnectingSource::with_env(
            "test",
            Box::new(first),
            opener,
            no_stop(),
            true,
            health,
            env,
        );

        let mut b = buf(8);
        src.read(&mut b).unwrap(); // productive
        let n = src.read(&mut b).unwrap(); // errors; 7 failed reopens; 8th succeeds
        assert_eq!(n, 8);
        assert_eq!(calls.get(), 8);
        assert_eq!(env_handle.sleeps_secs(), vec![1, 2, 4, 8, 16, 32, 60, 60]);

        // The recovered source drops again -- the ladder must have reset.
        let n2 = src.read(&mut b).unwrap();
        assert_eq!(n2, 8);
        assert_eq!(
            env_handle.sleeps_secs().last().copied(),
            Some(1),
            "the ladder must reset to the initial backoff after a productive connection"
        );
    }

    #[test]
    fn a_reopened_source_that_errors_before_any_data_counts_as_never_connected() {
        let first = scripted(FS, CENTER, vec![ok_chunk(8), err_chunk("lost")]);
        let (opener, _calls) = opener_from(VecDeque::from(vec![
            Ok(scripted(FS, CENTER, vec![err_chunk("dead on arrival")])),
            Ok(scripted(FS, CENTER, vec![ok_chunk(8)])),
        ]));
        let (health, _log) = health_recorder();
        let env = FakeEnv::new();
        let env_handle = env.clone();
        let mut src = ReconnectingSource::with_env(
            "test",
            Box::new(first),
            opener,
            no_stop(),
            true,
            health,
            env,
        );

        let mut b = buf(8);
        src.read(&mut b).unwrap(); // productive
                                   // Errors -> reopens -> that source errors before any data (counts
                                   // as NeverConnected, so the ladder keeps doubling rather than
                                   // resetting) -> reopens again -> real data.
        let n = src.read(&mut b).unwrap();
        assert_eq!(n, 8);
        assert_eq!(env_handle.sleeps_secs(), vec![1, 2]);
    }

    #[test]
    fn health_reports_false_on_loss_and_true_on_first_samples_only_on_change() {
        let first = scripted(
            FS,
            CENTER,
            vec![ok_chunk(8), ok_chunk(8), err_chunk("lost")],
        );
        let (opener, _calls) = opener_from(VecDeque::from(vec![Ok(scripted(
            FS,
            CENTER,
            vec![ok_chunk(8), ok_chunk(8)],
        ))]));
        let (health, log) = health_recorder();
        let mut src = ReconnectingSource::with_env(
            "test",
            Box::new(first),
            opener,
            no_stop(),
            true,
            health,
            FakeEnv::new(),
        );

        let mut b = buf(8);
        assert_eq!(
            log.borrow().clone(),
            vec![true],
            "initial_healthy reported once at construction"
        );
        src.read(&mut b).unwrap();
        src.read(&mut b).unwrap();
        assert_eq!(
            log.borrow().clone(),
            vec![true],
            "multiple successful reads must not add duplicate entries"
        );
        src.read(&mut b).unwrap(); // errors, reopens, resumes
        assert_eq!(log.borrow().clone(), vec![true, false, true]);
        src.read(&mut b).unwrap(); // another successful read
        assert_eq!(
            log.borrow().clone(),
            vec![true, false, true],
            "no duplicate true entry"
        );
    }

    #[test]
    fn multiple_failed_reopen_attempts_during_one_outage_add_no_duplicate_false() {
        let first = scripted(FS, CENTER, vec![ok_chunk(8), err_chunk("lost")]);
        let mut script: VecDeque<anyhow::Result<ScriptedSource>> =
            (0..3).map(|_| Err(anyhow::anyhow!("refused"))).collect();
        script.push_back(Ok(scripted(FS, CENTER, vec![ok_chunk(8)])));
        let (opener, _calls) = opener_from(script);
        let (health, log) = health_recorder();
        let mut src = ReconnectingSource::with_env(
            "test",
            Box::new(first),
            opener,
            no_stop(),
            true,
            health,
            FakeEnv::new(),
        );

        let mut b = buf(8);
        src.read(&mut b).unwrap();
        src.read(&mut b).unwrap();
        assert_eq!(log.borrow().clone(), vec![true, false, true]);
    }

    #[test]
    fn initial_unconfirmed_source_reports_true_on_its_first_samples() {
        let first = scripted(FS, CENTER, vec![ok_chunk(8)]);
        let (opener, calls) = opener_from(VecDeque::new());
        let (health, log) = health_recorder();
        let mut src = ReconnectingSource::with_env(
            "test",
            Box::new(first),
            opener,
            no_stop(),
            false,
            health,
            FakeEnv::new(),
        );

        assert_eq!(log.borrow().clone(), vec![false]);
        let mut b = buf(8);
        src.read(&mut b).unwrap();
        assert_eq!(log.borrow().clone(), vec![false, true]);
        assert_eq!(calls.get(), 0, "no reconnect was involved");
    }

    #[test]
    fn discontinuity_reports_outage_in_samples_once() {
        let first = scripted(FS, CENTER, vec![ok_chunk(8), err_chunk("lost")]);
        let (opener, _calls) = opener_from(VecDeque::from(vec![Ok(scripted(
            FS,
            CENTER,
            vec![ok_chunk(8)],
        ))]));
        let (health, _log) = health_recorder();
        let env = FakeEnv::new();
        let env_handle = env.clone();
        let mut src = ReconnectingSource::with_env(
            "test",
            Box::new(first),
            opener,
            no_stop(),
            true,
            health,
            env,
        );

        let mut b = buf(8);
        src.read(&mut b).unwrap(); // t0: last_ok set here
        assert_eq!(
            src.take_discontinuity(),
            None,
            "no reconnect has happened yet"
        );

        env_handle.advance(Duration::from_secs(10)); // simulated stall before the error surfaces
        let t0 = env_handle.clock.get() - Duration::from_secs(10);
        src.read(&mut b).unwrap(); // errors, sleeps 1s (env advances), reopens, resumes
        let t_now = env_handle.clock.get();
        let expected = ((t_now - t0).as_secs_f64() * FS).round() as u64;

        assert_eq!(src.take_discontinuity(), Some(expected));
        assert_eq!(src.take_discontinuity(), None, "reported only once");
    }

    /// A source whose every `read` takes `takes` of virtual time before it
    /// returns, like a reopened Kiwi filling its resampler before its first
    /// output.
    struct SlowSource {
        inner: ScriptedSource,
        env: FakeEnv,
        takes: Duration,
    }

    impl IqSource for SlowSource {
        fn sample_rate(&self) -> f64 {
            self.inner.sample_rate()
        }
        fn center_freq_hz(&self) -> f64 {
            self.inner.center_freq_hz()
        }
        fn read(&mut self, buf: &mut [Complex32]) -> anyhow::Result<usize> {
            self.env.advance(self.takes);
            self.inner.read(buf)
        }
    }

    #[test]
    fn discontinuity_excludes_time_the_first_read_spends_capturing_samples() {
        // PR #207 review: the engine counts the samples a reopened source
        // returns as it processes them, so the time its first read spent
        // capturing them must not be counted again in the gap.
        let first = scripted(FS, CENTER, vec![ok_chunk(8), err_chunk("lost")]);
        let env = FakeEnv::new();
        let env_handle = env.clone();
        let mut reopened = Some(SlowSource {
            inner: scripted(FS, CENTER, vec![ok_chunk(8)]),
            env: env.clone(),
            takes: Duration::from_millis(1500),
        });
        let opener: Opener = Box::new(move || {
            Ok(Box::new(reopened.take().expect("reopened only once")) as Box<dyn IqSource>)
        });
        let (health, _log) = health_recorder();
        let mut src = ReconnectingSource::with_env(
            "test",
            Box::new(first),
            opener,
            no_stop(),
            true,
            health,
            env,
        );

        let mut b = buf(8);
        src.read(&mut b).unwrap();
        let t0 = env_handle.clock.get();
        src.read(&mut b).unwrap(); // errors, sleeps 1s, reopens, first read takes 1.5s
        assert_eq!(
            env_handle.clock.get() - t0,
            INITIAL_BACKOFF + Duration::from_millis(1500),
            "the reopened source's first read must have taken its 1.5s"
        );

        assert_eq!(
            src.take_discontinuity(),
            Some((INITIAL_BACKOFF.as_secs_f64() * FS).round() as u64),
            "the gap ends where the first read began, not where it returned"
        );
    }

    #[test]
    fn stop_during_backoff_returns_eof_promptly() {
        let first = scripted(FS, CENTER, vec![err_chunk("dead")]);
        let (opener, calls) = opener_from(VecDeque::new());
        let (health, _log) = health_recorder();
        let stop = no_stop();
        stop.store(true, Ordering::Relaxed);
        let mut src = ReconnectingSource::with_env(
            "test",
            Box::new(first),
            opener,
            stop,
            true,
            health,
            FakeEnv::new(),
        );

        let mut b = buf(8);
        assert_eq!(src.read(&mut b).unwrap(), 0);
        assert_eq!(
            calls.get(),
            0,
            "the opener must never be called once stop is set"
        );
    }

    #[test]
    fn reopened_source_with_a_different_sample_rate_is_a_fatal_error() {
        let first = scripted(FS, CENTER, vec![ok_chunk(8), err_chunk("lost")]);
        let (opener, _calls) = opener_from(VecDeque::from(vec![Ok(scripted(
            48_000.0,
            CENTER,
            vec![ok_chunk(8)],
        ))]));
        let (health, _log) = health_recorder();
        let mut src = ReconnectingSource::with_env(
            "test",
            Box::new(first),
            opener,
            no_stop(),
            true,
            health,
            FakeEnv::new(),
        );

        let mut b = buf(8);
        src.read(&mut b).unwrap();
        let err = src.read(&mut b).unwrap_err();
        assert!(
            err.to_string().contains("sample rate"),
            "error must mention the sample rate mismatch, got: {err}"
        );
    }

    #[test]
    fn reopened_source_with_a_different_center_freq_is_a_fatal_error() {
        let first = scripted(FS, CENTER, vec![ok_chunk(8), err_chunk("lost")]);
        let (opener, _calls) = opener_from(VecDeque::from(vec![Ok(scripted(
            FS,
            CENTER + 1000.0,
            vec![ok_chunk(8)],
        ))]));
        let (health, _log) = health_recorder();
        let mut src = ReconnectingSource::with_env(
            "test",
            Box::new(first),
            opener,
            no_stop(),
            true,
            health,
            FakeEnv::new(),
        );

        let mut b = buf(8);
        src.read(&mut b).unwrap();
        let err = src.read(&mut b).unwrap_err();
        assert!(
            err.to_string().contains("center frequency"),
            "error must mention the center frequency mismatch, got: {err}"
        );
    }

    #[test]
    fn eof_from_inner_is_passed_through_not_retried() {
        let first = scripted(FS, CENTER, vec![ok_chunk(8), Ok(Vec::new())]);
        let (opener, calls) = opener_from(VecDeque::new());
        let (health, _log) = health_recorder();
        let mut src = ReconnectingSource::with_env(
            "test",
            Box::new(first),
            opener,
            no_stop(),
            true,
            health,
            FakeEnv::new(),
        );

        let mut b = buf(8);
        src.read(&mut b).unwrap();
        assert_eq!(src.read(&mut b).unwrap(), 0);
        assert_eq!(calls.get(), 0, "EOF must not trigger a reopen");
    }
}
