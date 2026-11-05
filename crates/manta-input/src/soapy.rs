//! SoapySDR IQ source (RTL-SDR, Airspy HF+, SDRplay). ARCHITECTURE §3.
//! Feature-gated `soapy` — the native SoapySDR C library is not required to
//! build without this feature (ROADMAP.md: "CI green on Linux + macOS (no
//! SoapySDR dependency in default features)").
//!
//! Driver stream overflows reported as a read error are counted, not
//! silent: each `ErrorCode::Overflow` records a gap event on the shared
//! `InputHealthCounters` (`health_counters`, MAN-233) and fires a
//! rate-limited operator notice (`with_overflow_notice`), so a dropping
//! host is not mistaken for a quiet band. Since MAN-202 each overflow is
//! also reported through `take_unknown_gap`, so the engine invalidates its
//! tracks' timing state instead of decoding the splice as Morse. An overflow
//! a driver reports only as the `SOAPY_SDR_END_ABRUPT` flag on a successful
//! read is not counted: soapysdr 0.5.1's `RxStream::read` keeps the read
//! flags private.

use crate::overflow::{account_overflow, UnknownGapLatch};
use crate::stall::StallEpisode;
use crate::InputHealthCounters;
use crate::IqSource;
use anyhow::Result;
use num_complex::Complex32;
use soapysdr::Direction::Rx;
use soapysdr::ErrorCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A live SoapySDR device (RTL-SDR/Airspy HF+/SDRplay/...) as an `IqSource`.
/// ARCHITECTURE §3.
pub struct SoapySdrIqSource {
    stream: soapysdr::RxStream<Complex32>,
    fs: f64,
    center_freq_hz: f64,
    /// Shared overflow-event counters (MAN-233), published through
    /// `health_counters()` so `/metrics` renders
    /// `manta_input_gaps_detected_total{source="soapy"}` from the first
    /// scrape.
    health: Arc<InputHealthCounters>,
    /// Operator notice fired by `account_overflow` when the rate-limit gate
    /// is open (MAN-233); `manta-input` itself stays log-silent, so the
    /// CLI injects the warning line here (audio's `with_silence_notice`
    /// pattern).
    overflow_notice: Option<Box<dyn Fn() + Send + Sync>>,
    /// When the overflow notice last fired; `None` until the first
    /// overflow. The rate-limit gate's state, kept where the events live so
    /// a burst inside one `read()` logs at most once.
    last_overflow_warn: Option<Instant>,
    /// MAN-202: set by every overflow, taken by `take_unknown_gap()`.
    unknown_gap: UnknownGapLatch,
    /// MAN-259: `open`'s runtime up to `stream.activate`; surfaced by
    /// `acquisition_start_offset()`.
    acquisition_start_offset: Duration,
    /// MAN-489: the retry/soft-stall loop's state (retry counters, wait
    /// episode, one-shot stall latch).
    retry: RetryState,
}

/// Read timeout, microseconds. Short enough that a caller polling a stop
/// signal between `read()` calls (matching `AudioIqSource`/`ctrlc`'s pattern
/// in `manta-cli`) stays responsive; long enough not to busy-loop on an
/// idle stream.
const TIMEOUT_US: i64 = 100_000;

/// Bound on consecutive read-timeout retries before giving up and
/// propagating an error. TIMEOUT_US * MAX_TIMEOUT_RETRIES = ~2s of
/// sustained silence tolerated before `read()` gives up -- long enough to
/// absorb a normal brief signal gap or USB scheduling jitter (the reason
/// SoapySDR's per-call timeout exists at all), short enough that a genuinely
/// dead/disconnected device is still reported in a reasonable time, and
/// bounded so this can't turn into an unbounded retry loop that would break
/// `listen()`'s Ctrl-C responsiveness (which depends on `read()` returning
/// promptly, not blocking forever).
const MAX_TIMEOUT_RETRIES: u32 = 20;

/// Bound on consecutive `ErrorCode::Overflow` retries before giving up and
/// propagating a real `Err`. A single overflow just means this call's
/// chunk was dropped by a full ring buffer -- normal under brief USB/
/// scheduling pressure on a live SDR, and observed in practice on an
/// SDRplay RSP1B several minutes into a sustained 192 kS/s capture -- so
/// it's retried rather than torn down like `MAX_TIMEOUT_RETRIES` silence.
/// Unlike `Timeout`, `Overflow` returns immediately without waiting out
/// `TIMEOUT_US`, so retrying it unboundedly could busy-loop forever
/// without `read()` ever returning if overflows kept recurring back-to-
/// back -- the same Ctrl-C-responsiveness hazard `MAX_TIMEOUT_RETRIES`
/// already guards against for silence, so this gets its own bound rather
/// than sharing that counter (a burst of overflows shouldn't spend down
/// the tolerance-for-a-dead-source budget and vice versa). A sustained run
/// of overflows this long means the host genuinely can't keep up, which
/// should surface as a real error instead of spinning forever.
const MAX_OVERFLOW_RETRIES: u32 = 100;

/// MAN-489: soft report deadline, ahead of the ~2 s hard retry limit
/// (`TIMEOUT_US * MAX_TIMEOUT_RETRIES`) so a held call spots inside the
/// 5 s bound on SoapySDR too.
const SOFT_STALL_AFTER: Duration = Duration::from_secs(1);

/// The hard bound the soft report must precede, as elapsed time.
const HARD_STALL_AFTER: Duration =
    Duration::from_millis((MAX_TIMEOUT_RETRIES as u64 * TIMEOUT_US as u64) / 1_000);

/// MAN-489: what one read attempt did, so the retry loop below can run
/// against a scripted operation in tests without the native library. The
/// production closure maps the native `RxStream::read` result onto this.
enum ReadAttempt {
    /// `Ok(n)` -- `n == 0` is "nothing this attempt", never end of stream.
    Samples(usize),
    /// `ErrorCode::Timeout`.
    Timeout,
    /// `ErrorCode::Overflow`.
    Overflow,
    /// Any other error.
    Fatal(anyhow::Error),
}

/// MAN-489: the retry/soft-stall loop's state -- everything except the
/// read operation itself. The retry counters persist across a soft
/// report's `Ok(0)` return, so the hard ~2 s budget cannot restart
/// mid-outage; delivery resets them.
struct RetryState {
    timeout_retries: u32,
    overflow_retries: u32,
    /// When the current no-sample wait began (the receive-timeout cap and
    /// the soft clock's anchor); cleared by delivery.
    waiting_since: Option<Instant>,
    /// Time of the last delivered samples.
    last_delivery: Instant,
    /// The one-shot soft-stall episode behind `take_stalled`.
    stall: StallEpisode,
}

impl RetryState {
    fn new() -> Self {
        Self {
            timeout_retries: 0,
            overflow_retries: 0,
            waiting_since: None,
            last_delivery: Instant::now(),
            stall: StallEpisode::new(SOFT_STALL_AFTER),
        }
    }

    fn note_wait(&mut self) {
        self.waiting_since.get_or_insert_with(Instant::now);
    }

    /// The next native read timeout: the usual `TIMEOUT_US`, capped by
    /// the remaining soft deadline so a blocking read cannot overshoot
    /// the report bound (the driver's own units are microseconds).
    fn next_timeout_us(&self) -> i64 {
        let mut us = TIMEOUT_US;
        if let Some(since) = self.waiting_since {
            if self.stall.is_delivering() {
                let remaining = self.stall.clamp_wait(
                    Duration::from_micros(TIMEOUT_US as u64),
                    since,
                    Instant::now(),
                );
                us = us.min(remaining.as_micros() as i64);
            }
        }
        us.max(1)
    }

    /// The production retry loop. `read_once` performs one native (or, in
    /// tests, scripted) read attempt with the timeout it is given.
    fn run(
        &mut self,
        read_once: &mut dyn FnMut(i64) -> ReadAttempt,
        health: &Arc<InputHealthCounters>,
        overflow_notice: &Option<Box<dyn Fn() + Send + Sync>>,
        last_overflow_warn: &mut Option<Instant>,
        unknown_gap: &mut UnknownGapLatch,
    ) -> Result<usize> {
        loop {
            match read_once(self.next_timeout_us()) {
                ReadAttempt::Samples(n) if n > 0 => {
                    self.timeout_retries = 0;
                    self.overflow_retries = 0;
                    self.waiting_since = None;
                    self.last_delivery = Instant::now();
                    self.stall.note_delivery();
                    return Ok(n);
                }
                // A native `Ok(0)` means the stream is waiting, not that
                // it ended: it must not leak as an unflagged EOF while the
                // live stream may still deliver (MAN-489).
                ReadAttempt::Samples(_) | ReadAttempt::Timeout => {
                    self.note_wait();
                    self.timeout_retries += 1;
                    if self.timeout_retries >= MAX_TIMEOUT_RETRIES {
                        anyhow::bail!(
                            "SoapySDR read timed out after {} consecutive attempts (~{}s of silence)",
                            MAX_TIMEOUT_RETRIES,
                            (MAX_TIMEOUT_RETRIES as i64 * TIMEOUT_US) / 1_000_000
                        );
                    }
                }
                ReadAttempt::Overflow => {
                    // Before the retry-bound check so the bound's own
                    // `bail!` overflow is counted too (MAN-233). The
                    // latch tells the engine the next samples are not
                    // contiguous with the last ones (MAN-202).
                    account_overflow(
                        health,
                        last_overflow_warn,
                        overflow_notice,
                        Instant::now(),
                        unknown_gap,
                    );
                    self.note_wait();
                    self.overflow_retries += 1;
                    if self.overflow_retries >= MAX_OVERFLOW_RETRIES {
                        anyhow::bail!(
                            "SoapySDR stream overflowed on {} consecutive reads -- host cannot keep up",
                            MAX_OVERFLOW_RETRIES
                        );
                    }
                }
                ReadAttempt::Fatal(e) => return Err(e),
            }
            // MAN-489: after every no-sample attempt, report a delivering
            // stream's stop once, inside the 5 s bound, before the hard
            // retry limit takes the stream down.
            if self
                .stall
                .due(self.last_delivery, Instant::now(), HARD_STALL_AFTER)
            {
                self.stall.report();
                return Ok(0);
            }
        }
    }
}

impl SoapySdrIqSource {
    /// Open `driver_args` (e.g. `"driver=rtlsdr"`), tune to `fs`/
    /// `center_freq_hz`, set `gain_db` (or enable AGC if `None` and the
    /// device supports gain mode), and activate an RX stream on channel 0.
    /// Every step's error (device not found, unsupported operation, etc.)
    /// propagates as a normal `Err`, never a panic.
    ///
    /// **Known gap (MAN-73 code review round 1, not fixed here):** unlike
    /// `KiwiIqSource::connect`'s `resolve_and_connect`, nothing here bounds
    /// `soapysdr::Device::new`/the rest of this call -- a SoapyRemote-style
    /// network-backed driver (or a USB device wedged in a bad enumeration
    /// state) can hang this call for however long the native driver takes.
    /// Since `manta-cli`'s `ReconnectingSource` now calls this repeatedly,
    /// forever, on every reconnect, such a hang also stalls that loop's
    /// backoff and health reporting for the duration. Since MAN-252 it
    /// delays reconnection only, not shutdown: the reopen runs on its own
    /// thread, which a stop request abandons. Left as-is because the `soapy` feature cannot be built or
    /// tested in this environment (no native `libsoapysdr`) and the ticket
    /// scopes Soapy as "can follow the same pattern" later, not required
    /// now -- fixing this blind, with no way to compile or test the
    /// result, risks shipping something worse than the documented gap.
    pub fn open(
        driver_args: &str,
        fs: f64,
        center_freq_hz: f64,
        gain_db: Option<f64>,
    ) -> Result<Self> {
        let entered = Instant::now();
        let device = soapysdr::Device::new(driver_args)?;
        device.set_sample_rate(Rx, 0, fs)?;
        device.set_frequency(Rx, 0, center_freq_hz, ())?;
        match gain_db {
            // Some drivers (confirmed on a real SDRplay RSP1B) default AGC
            // on and then silently ignore set_gain -- with AGC left
            // enabled, activateStream() fails outright
            // (sdrplay_api_Fail/NotSupported) rather than merely leaving
            // the requested gain unapplied. Disable AGC first whenever the
            // device supports the mode, so a manual gain actually takes
            // and the stream activates.
            Some(db) => {
                if device.has_gain_mode(Rx, 0)? {
                    device.set_gain_mode(Rx, 0, false)?;
                }
                device.set_gain(Rx, 0, db)?;
            }
            None => {
                if device.has_gain_mode(Rx, 0)? {
                    device.set_gain_mode(Rx, 0, true)?;
                }
            }
        }
        // Query back the actual negotiated values -- SDRs commonly round to
        // the nearest achievable rate/frequency; report truth, not the ask
        // (same convention as `Track::freq_hz`'s live-centroid reporting).
        let actual_fs = device.sample_rate(Rx, 0)?;
        let actual_freq = device.frequency(Rx, 0)?;
        let mut stream = device.rx_stream::<Complex32>(&[0])?;
        // MAN-259: the device buffers samples from activation on.
        let acquisition_start_offset = entered.elapsed();
        stream.activate(None)?;
        Ok(SoapySdrIqSource {
            stream,
            fs: actual_fs,
            center_freq_hz: actual_freq,
            health: Arc::new(InputHealthCounters::new()),
            overflow_notice: None,
            last_overflow_warn: None,
            unknown_gap: UnknownGapLatch::default(),
            acquisition_start_offset,
            retry: RetryState::new(),
        })
    }

    /// Call `notice` at most once per `OVERFLOW_WARN_INTERVAL` while the
    /// driver reports stream overflows (MAN-233) -- the audio
    /// `with_silence_notice` shape: `manta-input` has no logging
    /// dependency, so the CLI injects the operator warning line here.
    pub fn with_overflow_notice(mut self, notice: Box<dyn Fn() + Send + Sync>) -> Self {
        self.overflow_notice = Some(notice);
        self
    }
}

impl IqSource for SoapySdrIqSource {
    fn sample_rate(&self) -> f64 {
        self.fs
    }

    fn center_freq_hz(&self) -> f64 {
        self.center_freq_hz
    }

    fn health_counters(&self) -> Option<Arc<InputHealthCounters>> {
        Some(self.health.clone())
    }

    fn acquisition_start_offset(&self) -> Duration {
        self.acquisition_start_offset
    }

    /// MAN-202: every `ErrorCode::Overflow` dropped samples of unknown
    /// count before the next successful read; reported once per read.
    fn take_unknown_gap(&mut self) -> bool {
        self.unknown_gap.take()
    }

    /// A per-call SoapySDR read timeout (`ErrorCode::Timeout`) or buffer
    /// overflow (`ErrorCode::Overflow`) is a normal, expected event on a
    /// live stream -- neither is end-of-stream nor fatal on its own -- so
    /// each is retried internally against its own bound (`MAX_TIMEOUT_
    /// RETRIES`, `MAX_OVERFLOW_RETRIES`) rather than surfaced to the
    /// caller; any other error still propagates as `Err` immediately.
    /// Since MAN-233 an overflow is no longer *silent* while it is being
    /// retried: each one is counted as a gap event on the shared
    /// `InputHealthCounters` (see `health_counters`) and, at most once per
    /// `OVERFLOW_WARN_INTERVAL`, fires the operator notice injected with
    /// `with_overflow_notice`, and (MAN-202) is latched as an unknown gap
    /// reported by `take_unknown_gap` after the read that resumes. A
    /// `Timeout` counts and latches nothing -- that is how a
    /// quiet band presents, and counting it would recreate the
    /// dropped-input/quiet-band confusion MAN-233 removes. Only the error
    /// form is seen: SoapySDR also defines `SOAPY_SDR_END_ABRUPT`, the flag
    /// form of an overflow returned with the final samples of a successful
    /// read, but soapysdr 0.5.1 (the pinned version; the newest release on
    /// 2026-10-15) stores `read`'s flags in a private field with no
    /// accessor, so such an overflow is neither counted nor warned.
    ///
    /// MAN-489: a delivering stream that stops -- timeouts, or a native
    /// `Ok(0)` -- reports `Ok(0)` with `take_stalled` pending once, at the
    /// 1 s soft deadline, before the ~2 s hard retry limit ends the
    /// stream; the same stream keeps reading, and the retry budgets
    /// survive the soft return unchanged.
    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let stream = &mut self.stream;
        let mut last_overflow_warn = self.last_overflow_warn;
        let outcome = self.retry.run(
            &mut |timeout_us| match stream.read(&mut [buf], timeout_us) {
                Ok(n) => ReadAttempt::Samples(n),
                Err(e) if e.code == ErrorCode::Timeout => ReadAttempt::Timeout,
                Err(e) if e.code == ErrorCode::Overflow => ReadAttempt::Overflow,
                Err(e) => ReadAttempt::Fatal(e.into()),
            },
            &self.health,
            &self.overflow_notice,
            &mut last_overflow_warn,
            &mut self.unknown_gap,
        );
        self.last_overflow_warn = last_overflow_warn;
        outcome
    }

    /// MAN-489: consumed once per sample outage; see the private
    /// `StallEpisode` episode state.
    fn take_stalled(&mut self) -> bool {
        self.retry.stall.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// SoapySDR lazily loads and initializes every installed plugin module
    /// on the *first* `Device::new()` call in the process. That one-time
    /// init isn't verified thread-safe, and `cargo test`'s default
    /// thread-per-test concurrency lets these tests race each other into
    /// it -- observed live in CI as a `Hash collision!!! Fatal error!!`
    /// abort (a corrupted-registry symptom, not manta's own code), see
    /// docs/DECISIONS/2026-07-25-m2-soapysdr-input-pins.md. Serializing the
    /// tests that call `Device::new()` sidesteps the race regardless of
    /// which modules happen to be installed.
    static SOAPY_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn lock_soapy() -> std::sync::MutexGuard<'static, ()> {
        SOAPY_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn open_surfaces_device_not_found_as_a_clean_error() {
        let _guard = lock_soapy();
        // No RTL-SDR hardware is attached in CI or this dev environment --
        // Device::new() itself must fail, not panic.
        let result = SoapySdrIqSource::open("driver=rtlsdr", 96_000.0, 14_025_000.0, None);
        assert!(
            result.is_err(),
            "expected an Err with no RTL-SDR hardware attached"
        );
    }

    #[test]
    fn open_succeeds_against_the_null_device() {
        let _guard = lock_soapy();
        // SoapySDR's built-in `type=null` device opens with no hardware and
        // no extra module install -- and, when driven through open()'s full
        // sequence (gain-mode check + query-back reads before rx_stream()),
        // it succeeds all the way through activate(). Real, hardware-free
        // coverage of the entire open()/tune/stream/activate happy path.
        let result = SoapySdrIqSource::open("type=null", 96_000.0, 14_025_000.0, None);
        assert!(
            result.is_ok(),
            "expected Ok opening type=null, got {:?}",
            result.err()
        );
    }

    #[test]
    fn read_surfaces_not_supported_as_a_clean_error_on_the_null_device() {
        let _guard = lock_soapy();
        // type=null's stream opens/activates but does not actually support
        // reading samples -- read() fails with a real NotSupported error.
        // This is real, hardware-free coverage of IqSource::read()'s error
        // path, previously believed untestable without real hardware.
        let mut src = SoapySdrIqSource::open("type=null", 96_000.0, 14_025_000.0, None).unwrap();
        let mut buf = vec![Complex32::new(0.0, 0.0); 1024];
        let result = src.read(&mut buf);
        assert!(
            result.is_err(),
            "expected Err reading from type=null's stream (NotSupported)"
        );
    }

    /// MAN-233 Scenario 1 at source level, on real SoapySDR code,
    /// hardware-free via the null device: the health-counters handle
    /// exists and is the *same shared* `Arc` on every call, so the CLI's
    /// poller can keep reading it after the source moves into `listen()`
    /// (ka9q's `health_counters_and_liveness_handles_are_shared_arcs`
    /// pattern).
    #[test]
    fn soapy_health_counters_is_a_shared_stable_arc_on_the_null_device() {
        let _guard = lock_soapy();
        let src = SoapySdrIqSource::open("type=null", 96_000.0, 14_025_000.0, None).unwrap();
        let first = src
            .health_counters()
            .expect("soapy reports health counters");
        assert!(Arc::ptr_eq(&first, &src.health_counters().unwrap()));
        assert_eq!(first.gaps_detected(), 0);
        assert_eq!(first.dropped_packets(), 0);
        assert_eq!(first.malformed_packets(), 0);
    }

    /// How long one scripted read attempt takes, so a soft deadline can
    /// elapse inside the production loop without the native library (the
    /// plan's "scripted timed reads").
    const SCRIPT_POLL: Duration = Duration::from_millis(20);

    type RetryParts = (
        Arc<InputHealthCounters>,
        Option<Box<dyn Fn() + Send + Sync>>,
        Option<Instant>,
        UnknownGapLatch,
    );

    fn retry_parts() -> RetryParts {
        (
            Arc::new(InputHealthCounters::new()),
            None,
            None,
            UnknownGapLatch::default(),
        )
    }

    /// MAN-489: a delivering stream whose reads time out reports `Ok(0)`
    /// with the stall flag pending once, before the hard retry limit, and
    /// the timeout budget survives the soft return.
    #[test]
    fn timeouts_report_a_stall_once_then_still_hit_the_hard_limit() {
        let mut state = RetryState::new();
        state.stall = StallEpisode::new(Duration::from_millis(3 * SCRIPT_POLL.as_millis() as u64));
        let (health, notice, mut last_warn, mut unknown) = retry_parts();
        // Delivery first: a stream that never delivered reports nothing.
        let n = state
            .run(
                &mut |_| ReadAttempt::Samples(512),
                &health,
                &notice,
                &mut last_warn,
                &mut unknown,
            )
            .unwrap();
        assert_eq!(n, 512);
        // Then the stream stops: timed-out attempts until the report.
        let started = Instant::now();
        let got = state
            .run(
                &mut |_| {
                    std::thread::sleep(SCRIPT_POLL);
                    ReadAttempt::Timeout
                },
                &health,
                &notice,
                &mut last_warn,
                &mut unknown,
            )
            .unwrap();
        assert_eq!(got, 0);
        assert!(state.stall.take());
        assert!(!state.stall.take());
        assert!(
            state.timeout_retries < MAX_TIMEOUT_RETRIES,
            "the report must precede the hard retry limit, got {}",
            state.timeout_retries
        );
        assert!(started.elapsed() < HARD_STALL_AFTER);
        // The same outage continues: the preserved budget still errors at
        // the limit instead of restarting after the soft return.
        let err = state
            .run(
                &mut |_| {
                    std::thread::sleep(SCRIPT_POLL);
                    ReadAttempt::Timeout
                },
                &health,
                &notice,
                &mut last_warn,
                &mut unknown,
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("timed out after"), "{err}");
    }

    /// MAN-489: delivery re-arms the episode, so a second outage reports
    /// again; a native `Ok(0)` is a waiting stream, not an unflagged EOF.
    #[test]
    fn delivery_re_arms_and_a_native_zero_cannot_leak_as_eof() {
        let mut state = RetryState::new();
        state.stall = StallEpisode::new(Duration::from_millis(3 * SCRIPT_POLL.as_millis() as u64));
        let (health, notice, mut last_warn, mut unknown) = retry_parts();
        assert_eq!(
            state
                .run(
                    &mut |_| ReadAttempt::Samples(256),
                    &health,
                    &notice,
                    &mut last_warn,
                    &mut unknown,
                )
                .unwrap(),
            256
        );
        // Native Ok(0) over and over: the stream is waiting, so the loop
        // reports the stall rather than returning an unflagged zero.
        assert_eq!(
            state
                .run(
                    &mut |_| {
                        std::thread::sleep(SCRIPT_POLL);
                        ReadAttempt::Samples(0)
                    },
                    &health,
                    &notice,
                    &mut last_warn,
                    &mut unknown,
                )
                .unwrap(),
            0
        );
        assert!(state.stall.take());
        // Resume on the same stream, then stop again: a second report.
        assert_eq!(
            state
                .run(
                    &mut |_| ReadAttempt::Samples(128),
                    &health,
                    &notice,
                    &mut last_warn,
                    &mut unknown,
                )
                .unwrap(),
            128
        );
        assert_eq!(
            state
                .run(
                    &mut |_| {
                        std::thread::sleep(SCRIPT_POLL);
                        ReadAttempt::Timeout
                    },
                    &health,
                    &notice,
                    &mut last_warn,
                    &mut unknown,
                )
                .unwrap(),
            0
        );
        assert!(state.stall.take());
    }

    #[test]
    fn production_duration_soapy_stall_reports_within_five_seconds() {
        let mut state = RetryState::new();
        let (health, notice, mut warn, mut gap) = retry_parts();
        state
            .run(
                &mut |_| ReadAttempt::Samples(64),
                &health,
                &notice,
                &mut warn,
                &mut gap,
            )
            .unwrap();
        let started = Instant::now();
        assert_eq!(
            state
                .run(
                    &mut |us| {
                        std::thread::sleep(Duration::from_micros(us as u64));
                        ReadAttempt::Timeout
                    },
                    &health,
                    &notice,
                    &mut warn,
                    &mut gap
                )
                .unwrap(),
            0
        );
        assert!(
            started.elapsed() >= SOFT_STALL_AFTER && started.elapsed() < Duration::from_secs(5)
        );
        assert!(state.stall.take());
    }

    #[test]
    fn permanent_native_zero_reads_have_a_hard_exit() {
        for delivered in [false, true] {
            let mut state = RetryState::new();
            state.stall = StallEpisode::new(Duration::from_millis(1));
            let (health, notice, mut warn, mut gap) = retry_parts();
            if delivered {
                state
                    .run(
                        &mut |_| ReadAttempt::Samples(64),
                        &health,
                        &notice,
                        &mut warn,
                        &mut gap,
                    )
                    .unwrap();
            }
            let mut attempts = 0;
            loop {
                let result = state.run(
                    &mut |_| {
                        attempts += 1;
                        std::thread::sleep(Duration::from_millis(2));
                        if attempts > MAX_TIMEOUT_RETRIES {
                            ReadAttempt::Fatal(anyhow::anyhow!("watchdog"))
                        } else {
                            ReadAttempt::Samples(0)
                        }
                    },
                    &health,
                    &notice,
                    &mut warn,
                    &mut gap,
                );
                match result {
                    Ok(0) => assert!(state.stall.take()),
                    Err(e) => {
                        assert!(e.to_string().contains("timed out"), "{e}");
                        break;
                    }
                    other => panic!("{other:?}"),
                }
            }
        }
    }

    /// MAN-489/pinned MAN-202/233 behaviour through the same loop: an
    /// overflow is counted and latched as an unknown gap; a fatal error
    /// propagates.
    #[test]
    fn overflow_latches_an_unknown_gap_and_fatal_errors_propagate() {
        let mut state = RetryState::new();
        let (health, notice, mut last_warn, mut unknown) = retry_parts();
        let mut ops = std::collections::VecDeque::from([
            ReadAttempt::Samples(64),
            ReadAttempt::Overflow,
            ReadAttempt::Samples(64),
        ]);
        assert_eq!(
            state
                .run(
                    &mut |_| ops.pop_front().expect("initial samples"),
                    &health,
                    &notice,
                    &mut last_warn,
                    &mut unknown
                )
                .unwrap(),
            64
        );
        let n = state
            .run(
                &mut |_| ops.pop_front().expect("scripted attempt"),
                &health,
                &notice,
                &mut last_warn,
                &mut unknown,
            )
            .unwrap();
        assert_eq!(n, 64);
        assert_eq!(health.gaps_detected(), 1);
        assert!(unknown.take());

        let mut state = RetryState::new();
        let (health, notice, mut last_warn, mut unknown) = retry_parts();
        let err = state
            .run(
                &mut |_| ReadAttempt::Fatal(anyhow::anyhow!("driver died")),
                &health,
                &notice,
                &mut last_warn,
                &mut unknown,
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("driver died"), "{err}");
    }
}
