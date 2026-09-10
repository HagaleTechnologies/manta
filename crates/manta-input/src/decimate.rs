//! Wraps any `IqSource` with a `manta_dsp::decimate::Decimator`, reporting
//! the decimated rate as its own `sample_rate()` -- issue #169. Composes
//! transparently with every existing `IqSource` impl (kiwi/soapy/hpsdr/
//! audio); `manta-engine::listen` needs no changes since it only ever
//! calls `sample_rate()` once, up front.

use crate::{InputHealthCounters, IqSource};
use anyhow::{bail, Result};
use manta_dsp::decimate::Decimator;
use num_complex::Complex32;
use std::collections::VecDeque;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

pub struct DecimatingSource {
    inner: Box<dyn IqSource>,
    decimator: Decimator,
    fs_out: f64,
    factor: usize,
    /// Decimated samples produced by a prior inner read that didn't fit
    /// in the caller's buffer -- carried over so no decimated sample is
    /// ever dropped just because the caller's buffer was smaller than one
    /// inner read happened to produce.
    pending: VecDeque<Complex32>,
}

impl DecimatingSource {
    /// Wrap `inner` (reporting `inner.sample_rate()`, Hz) with a decimator
    /// targeting `target_rate_hz`. Errors (non-power-of-two factor,
    /// non-table target rate) match `Decimator::new`'s.
    pub fn new(inner: Box<dyn IqSource>, target_rate_hz: f64) -> Result<Self> {
        let fs_in = inner.sample_rate();
        let factor = (fs_in / target_rate_hz).round() as usize;
        if factor == 0 || (fs_in / factor as f64 - target_rate_hz).abs() > 1e-6 {
            bail!(
                "--capture-rate-hz {target_rate_hz} does not evenly divide the source's \
                 native rate {fs_in} by a power of two"
            );
        }
        let decimator = Decimator::new(fs_in, factor).map_err(|e| anyhow::anyhow!(e))?;
        let fs_out = decimator.fs_out();
        Ok(DecimatingSource {
            inner,
            decimator,
            fs_out,
            factor,
            pending: VecDeque::new(),
        })
    }
}

impl IqSource for DecimatingSource {
    fn sample_rate(&self) -> f64 {
        self.fs_out
    }

    fn center_freq_hz(&self) -> f64 {
        self.inner.center_freq_hz()
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        while self.pending.is_empty() {
            let mut raw = vec![Complex32::new(0.0, 0.0); buf.len().max(1) * self.factor];
            let n = self.inner.read(&mut raw)?;
            if n == 0 {
                return Ok(0);
            }
            let decimated = self.decimator.process(&raw[..n]);
            self.pending.extend(decimated);
        }
        let n = buf.len().min(self.pending.len());
        for (slot, s) in buf[..n].iter_mut().zip(self.pending.drain(..n)) {
            *slot = s;
        }
        Ok(n)
    }

    fn confirmed_live_handle(&self) -> Option<Arc<AtomicBool>> {
        self.inner.confirmed_live_handle()
    }

    fn health_counters(&self) -> Option<Arc<InputHealthCounters>> {
        self.inner.health_counters()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal in-memory IqSource test double, mirroring
    /// `manta-engine::listen`'s own `FixedFreqSource` test helper.
    struct InMemorySource {
        samples: Vec<Complex32>,
        cursor: usize,
        fs: f64,
        center_freq_hz: f64,
        live: Option<Arc<AtomicBool>>,
        counters: Option<Arc<InputHealthCounters>>,
    }

    impl IqSource for InMemorySource {
        fn sample_rate(&self) -> f64 {
            self.fs
        }
        fn center_freq_hz(&self) -> f64 {
            self.center_freq_hz
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            let n = buf.len().min(self.samples.len() - self.cursor);
            buf[..n].copy_from_slice(&self.samples[self.cursor..self.cursor + n]);
            self.cursor += n;
            Ok(n)
        }
        fn confirmed_live_handle(&self) -> Option<Arc<AtomicBool>> {
            self.live.clone()
        }
        fn health_counters(&self) -> Option<Arc<InputHealthCounters>> {
            self.counters.clone()
        }
    }

    fn tone(freq: f64, n: usize, fs: f64) -> Vec<Complex32> {
        (0..n)
            .map(|i| {
                let phi = 2.0 * std::f64::consts::PI * freq * i as f64 / fs;
                Complex32::new(phi.cos() as f32, phi.sin() as f32)
            })
            .collect()
    }

    #[test]
    fn sample_rate_reports_the_decimated_value() {
        let inner: Box<dyn IqSource> = Box::new(InMemorySource {
            samples: tone(1_000.0, 1_000, 192_000.0),
            cursor: 0,
            fs: 192_000.0,
            center_freq_hz: 14_000_000.0,
            live: None,
            counters: None,
        });
        let src = DecimatingSource::new(inner, 48_000.0).unwrap();
        assert_eq!(src.sample_rate(), 48_000.0);
    }

    #[test]
    fn center_freq_and_liveness_forward_to_the_inner_source() {
        let live = Arc::new(AtomicBool::new(true));
        let inner: Box<dyn IqSource> = Box::new(InMemorySource {
            samples: tone(1_000.0, 1_000, 192_000.0),
            cursor: 0,
            fs: 192_000.0,
            center_freq_hz: 14_035_000.0,
            live: Some(live.clone()),
            counters: None,
        });
        let src = DecimatingSource::new(inner, 48_000.0).unwrap();
        assert_eq!(src.center_freq_hz(), 14_035_000.0);
        assert!(src.confirmed_live_handle().unwrap().load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn read_yields_roughly_input_len_over_factor_samples_then_eof() {
        let fs_in = 192_000.0;
        let n_in = 40_000;
        let inner: Box<dyn IqSource> = Box::new(InMemorySource {
            samples: tone(2_000.0, n_in, fs_in),
            cursor: 0,
            fs: fs_in,
            center_freq_hz: 0.0,
            live: None,
            counters: None,
        });
        let mut src = DecimatingSource::new(inner, 48_000.0).unwrap(); // factor 4
        let mut total = 0usize;
        let mut buf = vec![Complex32::new(0.0, 0.0); 500];
        loop {
            let n = src.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            total += n;
        }
        // factor 4: ~n_in/4 output samples, within a cascade's worth of
        // rounding/warm-up slack either side.
        let expected = n_in / 4;
        assert!(
            (total as i64 - expected as i64).unsigned_abs() < 200,
            "total {total}, expected ~{expected}"
        );
    }

    #[test]
    fn rejects_non_power_of_two_factor() {
        let inner: Box<dyn IqSource> = Box::new(InMemorySource {
            samples: vec![],
            cursor: 0,
            fs: 192_000.0,
            center_freq_hz: 0.0,
            live: None,
            counters: None,
        });
        assert!(DecimatingSource::new(inner, 70_000.0).is_err());
    }

    #[test]
    fn rejects_target_rate_failing_channelizer_table_constraint() {
        let inner: Box<dyn IqSource> = Box::new(InMemorySource {
            samples: vec![],
            cursor: 0,
            fs: 100_000.0,
            center_freq_hz: 0.0,
            live: None,
            counters: None,
        });
        assert!(DecimatingSource::new(inner, 50_000.0).is_err());
    }

    #[test]
    fn health_counters_forward_to_the_inner_source() {
        let counters = Arc::new(InputHealthCounters::new());
        let inner: Box<dyn IqSource> = Box::new(InMemorySource {
            samples: tone(1_000.0, 1_000, 192_000.0),
            cursor: 0,
            fs: 192_000.0,
            center_freq_hz: 14_000_000.0,
            live: None,
            counters: Some(counters.clone()),
        });
        let src = DecimatingSource::new(inner, 48_000.0).unwrap();
        let forwarded_counters = src.health_counters().unwrap();
        assert_eq!(
            forwarded_counters.dropped_packets(),
            counters.dropped_packets()
        );
        // Record a packet drop in the original counters
        counters.record_dropped(5);
        // Verify the forwarded reference sees the same update
        assert_eq!(
            forwarded_counters.dropped_packets(),
            5
        );
    }
}
