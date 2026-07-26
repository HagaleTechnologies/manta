//! SoapySDR IQ source (RTL-SDR, Airspy HF+, SDRplay). ARCHITECTURE §3.
//! Feature-gated `soapy` — the native SoapySDR C library is not required to
//! build without this feature (ROADMAP.md: "CI green on Linux + macOS (no
//! SoapySDR dependency in default features)").

use crate::IqSource;
use anyhow::Result;
use num_complex::Complex32;
use soapysdr::Direction::Rx;
use soapysdr::ErrorCode;

/// A live SoapySDR device (RTL-SDR/Airspy HF+/SDRplay/...) as an `IqSource`.
/// ARCHITECTURE §3.
pub struct SoapySdrIqSource {
    stream: soapysdr::RxStream<Complex32>,
    fs: f64,
    center_freq_hz: f64,
}

/// Read timeout, microseconds. Short enough that a caller polling a stop
/// signal between `read()` calls (matching `AudioIqSource`/`ctrlc`'s pattern
/// in `skimmer-cli`) stays responsive; long enough not to busy-loop on an
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

impl SoapySdrIqSource {
    /// Open `driver_args` (e.g. `"driver=rtlsdr"`), tune to `fs`/
    /// `center_freq_hz`, set `gain_db` (or enable AGC if `None` and the
    /// device supports gain mode), and activate an RX stream on channel 0.
    /// Every step's error (device not found, unsupported operation, etc.)
    /// propagates as a normal `Err`, never a panic.
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
            Some(db) => device.set_gain(Rx, 0, db)?,
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
        })
    }
}

impl IqSource for SoapySdrIqSource {
    fn sample_rate(&self) -> f64 {
        self.fs
    }

    fn center_freq_hz(&self) -> f64 {
        self.center_freq_hz
    }

    /// A per-call SoapySDR read timeout (`ErrorCode::Timeout`) is a normal,
    /// expected event on a live stream -- not end-of-stream and not fatal --
    /// so it is retried internally (bounded by `MAX_TIMEOUT_RETRIES`) rather
    /// than surfaced to the caller; any other error still propagates as `Err`
    /// immediately.
    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        for _ in 0..MAX_TIMEOUT_RETRIES {
            match self.stream.read(&mut [buf], TIMEOUT_US) {
                Ok(n) => return Ok(n),
                Err(e) if e.code == ErrorCode::Timeout => continue,
                Err(e) => return Err(e.into()),
            }
        }
        anyhow::bail!(
            "SoapySDR read timed out after {} consecutive attempts (~{}s of silence)",
            MAX_TIMEOUT_RETRIES,
            (MAX_TIMEOUT_RETRIES as i64 * TIMEOUT_US) / 1_000_000
        )
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
    /// abort (a corrupted-registry symptom, not skimmer's own code), see
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
}
