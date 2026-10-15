//! SoapySDR IQ source (RTL-SDR, Airspy HF+, SDRplay). ARCHITECTURE §3.
//! Feature-gated `soapy` — the native SoapySDR C library is not required to
//! build without this feature (ROADMAP.md: "CI green on Linux + macOS (no
//! SoapySDR dependency in default features)").
//!
//! Driver stream overflows are counted, not silent: each one records a gap
//! event on the shared `InputHealthCounters` (`health_counters`, MAN-233)
//! and fires a rate-limited operator notice (`with_overflow_notice`), so a
//! dropping host is never mistaken for a quiet band.

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

/// How often the operator overflow notice may fire (MAN-233). Fixed, not
/// configurable: it matches the MAN-122 status-line default interval, and
/// the counter carries the fine-grained data -- the warning only has to say
/// "this is happening", not how often.
const OVERFLOW_WARN_INTERVAL: Duration = Duration::from_secs(60);

/// Account one driver ring-buffer overflow (the field reality
/// `MAX_OVERFLOW_RETRIES`' doc records): exactly one gap *event* -- an
/// `ErrorCode::Overflow` carries no lost-sample count, so nothing is ever
/// `record_dropped` and `dropped_packets` stays 0 -- and the operator
/// notice only when the rate-limit gate is open. Free-function shape after
/// kiwi's `account_snd_frame` so the count/rate-limit semantics are
/// unit-testable with crafted `Instant`s and no device (MAN-233; the
/// `soapy` feature cannot compile in the implement container).
fn account_overflow(
    counters: &InputHealthCounters,
    last_warn: &mut Option<Instant>,
    notice: &Option<Box<dyn Fn() + Send + Sync>>,
    now: Instant,
) {
    counters.record_gap();
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
    /// forever, on every reconnect, such a hang would also defeat that
    /// loop's backoff/health-reporting/Ctrl-C responsiveness for the
    /// duration. Left as-is because the `soapy` feature cannot be built or
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
        stream.activate(None)?;
        Ok(SoapySdrIqSource {
            stream,
            fs: actual_fs,
            center_freq_hz: actual_freq,
            health: Arc::new(InputHealthCounters::new()),
            overflow_notice: None,
            last_overflow_warn: None,
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
    /// `with_overflow_notice`. A `Timeout` counts nothing -- that is how a
    /// quiet band presents, and counting it would recreate the
    /// dropped-input/quiet-band confusion MAN-233 removes.
    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        let mut timeout_retries = 0;
        let mut overflow_retries = 0;
        loop {
            match self.stream.read(&mut [buf], TIMEOUT_US) {
                Ok(n) => return Ok(n),
                Err(e) if e.code == ErrorCode::Timeout => {
                    timeout_retries += 1;
                    if timeout_retries >= MAX_TIMEOUT_RETRIES {
                        anyhow::bail!(
                            "SoapySDR read timed out after {} consecutive attempts (~{}s of silence)",
                            MAX_TIMEOUT_RETRIES,
                            (MAX_TIMEOUT_RETRIES as i64 * TIMEOUT_US) / 1_000_000
                        );
                    }
                }
                Err(e) if e.code == ErrorCode::Overflow => {
                    // Before the retry-bound check so the bound's own
                    // `bail!` overflow is counted too (MAN-233).
                    account_overflow(
                        &self.health,
                        &mut self.last_overflow_warn,
                        &self.overflow_notice,
                        Instant::now(),
                    );
                    overflow_retries += 1;
                    if overflow_retries >= MAX_OVERFLOW_RETRIES {
                        anyhow::bail!(
                            "SoapySDR stream overflowed on {} consecutive reads -- host cannot keep up",
                            MAX_OVERFLOW_RETRIES
                        );
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
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

    /// MAN-233 decision 1/4: one overflow is exactly one gap *event* -- an
    /// `ErrorCode::Overflow` carries no lost-sample count, so nothing is
    /// ever `record_dropped` -- and only `Overflow` counts: a `Timeout` is
    /// how a quiet band presents and must not move any counter.
    #[test]
    fn account_overflow_counts_one_gap_event_per_overflow_and_nothing_else() {
        let counters = InputHealthCounters::new();
        let mut last_warn = None;
        let now = Instant::now();
        for _ in 0..3 {
            account_overflow(&counters, &mut last_warn, &None, now);
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
        // `Box<dyn Fn() + Send + Sync>` defaults to `'static`, so the
        // closure owns an `Arc` clone of the count rather than borrowing a
        // local (E0597).
        let fired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fired_in_notice = Arc::clone(&fired);
        let notice: Option<Box<dyn Fn() + Send + Sync>> = Some(Box::new(move || {
            fired_in_notice.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));

        let t0 = Instant::now();
        account_overflow(&counters, &mut last_warn, &notice, t0);
        assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(last_warn, Some(t0));

        // A burst within the interval: counted, but silent.
        account_overflow(
            &counters,
            &mut last_warn,
            &notice,
            t0 + Duration::from_secs(30),
        );
        account_overflow(
            &counters,
            &mut last_warn,
            &notice,
            t0 + Duration::from_secs(59),
        );
        assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(last_warn, Some(t0), "gate closed: last_warn must not move");

        // At exactly the interval the gate opens again.
        let t1 = t0 + OVERFLOW_WARN_INTERVAL;
        account_overflow(&counters, &mut last_warn, &notice, t1);
        assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(last_warn, Some(t1));

        // Every event still counted, notice or not.
        assert_eq!(counters.gaps_detected(), 4);
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
}
