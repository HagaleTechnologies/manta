//! SoapySDR IQ source (RTL-SDR, Airspy HF+, SDRplay). ARCHITECTURE §3.
//! Feature-gated `soapy` — the native SoapySDR C library is not required to
//! build without this feature (ROADMAP.md: "CI green on Linux + macOS (no
//! SoapySDR dependency in default features)").

use crate::IqSource;
use anyhow::Result;
use num_complex::Complex32;
use soapysdr::Direction::Rx;

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

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        let n = self.stream.read(&mut [buf], TIMEOUT_US)?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_surfaces_device_not_found_as_a_clean_error() {
        // No RTL-SDR hardware is attached in CI or this dev environment --
        // Device::new() itself must fail, not panic.
        let result = SoapySdrIqSource::open("driver=rtlsdr", 96_000.0, 14_025_000.0, None);
        assert!(
            result.is_err(),
            "expected an Err with no RTL-SDR hardware attached"
        );
    }

    #[test]
    fn open_surfaces_stream_not_supported_as_a_clean_error() {
        // SoapySDR's built-in `type=null` device opens successfully with no
        // hardware and no extra module install, but does not support RX
        // streaming -- open() must still return Err cleanly (confirms error
        // propagation past device construction, through to rx_stream()).
        let result = SoapySdrIqSource::open("type=null", 96_000.0, 14_025_000.0, None);
        assert!(
            result.is_err(),
            "expected an Err from type=null (no RX streaming support)"
        );
    }
}
