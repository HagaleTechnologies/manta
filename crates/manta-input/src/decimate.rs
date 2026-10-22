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

/// Scratch buffer size for reading from `inner`, in inner-source samples --
/// fixed and reused across calls rather than sized from the caller's
/// remaining output capacity (MAN-169 round-5 Codex finding: a packetized
/// source like HPSDR only ever fills a few hundred samples per `read()`
/// regardless of buffer size, so sizing the scratch buffer from a large
/// caller request -- e.g. `listen()`'s ~2 s startup calibration read --
/// allocated and zeroed a multi-hundred-thousand-sample `Vec` on every loop
/// iteration: wasted work, and real memory-pressure/UDP-packet-loss risk on
/// a Pi4). A file/bulk source just takes a few more `read()` calls to fill
/// the same total, which is cheap (no syscall, no DSP-comparable cost).
const RAW_CHUNK_SAMPLES: usize = 4096;

pub struct DecimatingSource {
    /// `Send`, so a source built on `manta-cli`'s reopen thread can move to
    /// the decode thread (MAN-252).
    inner: Box<dyn IqSource + Send>,
    decimator: Decimator,
    fs_out: f64,
    /// MAN-196: the decimator's <= 1 dB half-bandwidth, captured at
    /// construction (a constant multiply -- zero per-read cost) so both
    /// `rf_passband_hz` and `trusted_band_hz` derive from one number.
    /// `None` for a factor-1 wrap (zero cascade stages: a pure
    /// pass-through with no filter, hence no transition band to exclude
    /// -- every channel is trusted). The CLI never builds one
    /// (`maybe_decimate` skips the wrap when the target equals the
    /// native rate), but this wrapper's contract allows it.
    trusted_half: Option<f64>,
    /// Decimated samples produced by a prior inner read that didn't fit
    /// in the caller's buffer -- carried over so no decimated sample is
    /// ever dropped just because the caller's buffer was smaller than one
    /// inner read happened to produce.
    pending: VecDeque<Complex32>,
    /// Reused scratch buffer for `inner.read()`, sized once at construction
    /// (`RAW_CHUNK_SAMPLES * factor`) -- see `RAW_CHUNK_SAMPLES`.
    raw: Vec<Complex32>,
    /// MAN-305: the decimation factor, `fs_in / fs_out` exactly, for
    /// scaling the inner source's gap reports to the output rate.
    factor: u64,
    /// MAN-305: the inner source's gap reports, scaled to the output rate
    /// and held for the outer `take_discontinuity()` /
    /// `take_missing_samples()`. Taken after every inner read, so neither
    /// is lost behind a buffered `pending`.
    gap: GapScaler,
    missing: GapScaler,
}

/// MAN-305: one gap hook's pending report at the output rate. Each inner
/// report of `k` input samples adds `(carry + k·fs_out) / fs_in` output
/// samples -- `(carry + k) / factor` here, `fs_in` being exactly
/// `factor·fs_out` -- and carries the remainder to the next report, so a
/// run of small reports sums to the same total as one large one.
#[derive(Default)]
struct GapScaler {
    carry: u64,
    pending: Option<u64>,
}

impl GapScaler {
    fn add(&mut self, inner: Option<u64>, factor: u64) {
        if let Some(k) = inner {
            let total = self.carry + k;
            self.carry = total % factor;
            *self.pending.get_or_insert(0) += total / factor;
        }
    }
}

impl DecimatingSource {
    /// Wrap `inner` (reporting `inner.sample_rate()`, Hz) with a decimator
    /// targeting `target_rate_hz`. This wrapper's own pre-check raises a
    /// distinct error message when `target_rate_hz` doesn't evenly divide
    /// `inner.sample_rate()` by a power of two (a factor that isn't even an
    /// integer never reaches `Decimator::new` at all); a target rate that
    /// *does* divide evenly but still fails the channelizer's table
    /// constraint (or the minimum-channel-count floor) surfaces
    /// `Decimator::new`'s own error unchanged.
    pub fn new(inner: Box<dyn IqSource + Send>, target_rate_hz: f64) -> Result<Self> {
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
        let trusted_half = (factor >= 2).then(|| decimator.trusted_half_bandwidth_hz());
        Ok(DecimatingSource {
            inner,
            decimator,
            fs_out,
            trusted_half,
            pending: VecDeque::new(),
            raw: vec![Complex32::new(0.0, 0.0); RAW_CHUNK_SAMPLES * factor],
            factor: factor as u64,
            gap: GapScaler::default(),
            missing: GapScaler::default(),
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

    /// MAN-86: the inner source's delivered passband, clipped to the
    /// band the decimator actually delivers usefully. MAN-196 narrows
    /// that clip from the raw output Nyquist (`+/- fs_out / 2`) to the
    /// decimator's **trusted band** (`+/- trusted_half`, the <= 1 dB
    /// crossover of the halfband design): everything beyond it is
    /// transition band, 6-39 dB down by the Nyquist edge, so advertising
    /// it as usable passband promises spectrum the filter has already
    /// attenuated. A factor-1 wrap (no stages, `trusted_half == None`)
    /// keeps the plain Nyquist clip -- a pass-through filters nothing.
    /// Without this override the trait default would report the full
    /// output Nyquist span, re-introducing the "advertise the processing
    /// rate" SETT bug for a decimated rig-audio or KiwiSDR source (see
    /// `IqSource::rf_passband_hz`). The inner clip stays as a defensive
    /// intersection, subsumed by the trusted clip (the trusted
    /// half-bandwidth is always below `fs_out / 2`).
    fn rf_passband_hz(&self) -> (f64, f64) {
        let half = self.fs_out / 2.0;
        let (lo, hi) = self.inner.rf_passband_hz();
        match self.trusted_half {
            Some(t) => (lo.max(-half).max(-t), hi.min(half).min(t)),
            None => (lo.max(-half), hi.min(half)),
        }
    }

    /// MAN-196: the decimator's <= 1 dB trusted band, `(lo, hi)` Hz
    /// relative to the centre. Consumers use it to veto track spawns
    /// beyond the boundary (the transition band's frequency-estimate
    /// corruption, duplicate tracks and foldover promotions all measured
    /// beyond it). Reported as `rf_passband_hz` itself -- the trusted
    /// band intersected with the inner source's passband -- so a
    /// decimation wrapping an already narrower source (one-sided rig
    /// audio, a KiwiSDR filter) vetoes exactly what it advertises, and
    /// advertisement and veto can never disagree (round-2 review on PR
    /// #300). `None` for a factor-1 pass-through wrap (no filter, no
    /// transition band).
    fn trusted_band_hz(&self) -> Option<(f64, f64)> {
        self.trusted_half.map(|_| self.rf_passband_hz())
    }

    /// MAN-273: forwarded -- the inner source's DC rim stays at DC after
    /// decimation (the near-DC Hilbert leak survives resampling), and the
    /// guard's being non-zero also keeps `listen`'s mirror-image veto on
    /// (a decimated analytic signal is still analytic). The Nyquist rim
    /// needs no handling here: `TrackManager::set_guard_hz` yields it to
    /// this source's trusted band, because the leak it guards sat at the
    /// input rate's Nyquist, which the decimator's stopband removed.
    fn analytic_guard_hz(&self) -> f64 {
        self.inner.analytic_guard_hz()
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        while self.pending.is_empty() {
            let n = self.inner.read(&mut self.raw)?;
            // MAN-305: the inner reports describe the samples this inner
            // read returned, which `pending` (empty here) will start with,
            // so they reach the caller with the first outer read that
            // returns them. Taken on an EOF too: an end of stream may
            // leave an outage pending (MAN-258).
            let gap = self.inner.take_discontinuity();
            self.gap.add(gap, self.factor);
            self.missing
                .add(self.inner.take_missing_samples(), self.factor);
            if gap.is_some() {
                // An outage is not spliced: no pre-gap sample may leak
                // into post-gap output through the filter history.
                self.decimator.reset();
            }
            if n == 0 {
                return Ok(0);
            }
            let decimated = self.decimator.process(&self.raw[..n]);
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

    /// MAN-305: the inner source's outage, scaled to the output rate.
    fn take_discontinuity(&mut self) -> Option<u64> {
        self.gap.pending.take()
    }

    /// MAN-305: the inner source's spliced loss, scaled to the output rate.
    fn take_missing_samples(&mut self) -> Option<u64> {
        self.missing.pending.take()
    }

    fn health_counters(&self) -> Option<Arc<InputHealthCounters>> {
        self.inner.health_counters()
    }

    /// MAN-259: the inner driver's acquisition start; decimation adds no
    /// open-time work of its own.
    fn acquisition_start_offset(&self) -> std::time::Duration {
        self.inner.acquisition_start_offset()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct GapChunks {
        chunks: std::collections::VecDeque<(Vec<Complex32>, Option<u64>)>,
        gap: Option<u64>,
    }
    impl IqSource for GapChunks {
        fn sample_rate(&self) -> f64 {
            192000.0
        }
        fn center_freq_hz(&self) -> f64 {
            7e6
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            let Some((samples, gap)) = self.chunks.pop_front() else {
                return Ok(0);
            };
            buf[..samples.len()].copy_from_slice(&samples);
            self.gap = gap;
            Ok(samples.len())
        }
        fn take_discontinuity(&mut self) -> Option<u64> {
            self.gap.take()
        }
    }

    #[test]
    fn gaps_reset_filter_and_are_latched_to_post_gap_samples() {
        let before = vec![Complex32::new(1.0, 0.0); 128];
        let after = vec![Complex32::new(-0.5, 0.0); 128];
        let chunks = GapChunks {
            chunks: [(before, None), (after.clone(), Some(7))].into(),
            gap: None,
        };
        let mut src = DecimatingSource::new(Box::new(chunks), 48000.0).unwrap();
        let mut buf = [Complex32::default(); 256];
        assert_eq!(src.read(&mut buf).unwrap(), 32);
        assert_eq!(src.take_discontinuity(), None);
        let n = src.read(&mut buf).unwrap();
        assert_eq!(src.take_discontinuity(), Some(1));
        assert_eq!(src.take_discontinuity(), None);
        let mut fresh = Decimator::new(192000.0, 4).unwrap();
        assert_eq!(&buf[..n], fresh.process(&after));
    }

    #[test]
    fn decimated_gaps_carry_remainders_and_survive_eof() {
        let chunk = vec![Complex32::default(); 16];
        let chunks = GapChunks {
            chunks: [
                (chunk.clone(), Some(3)),
                (chunk, Some(3)),
                (vec![], Some(2)),
            ]
            .into(),
            gap: None,
        };
        let mut src = DecimatingSource::new(Box::new(chunks), 48000.0).unwrap();
        let mut buf = [Complex32::default(); 256];
        assert_eq!(src.read(&mut []).unwrap(), 0);
        assert_eq!(src.take_discontinuity(), None);
        assert_eq!(src.read(&mut buf).unwrap(), 4);
        assert_eq!(src.take_discontinuity(), Some(0));
        assert_eq!(src.read(&mut buf).unwrap(), 4);
        assert_eq!(src.take_discontinuity(), Some(1));
        assert_eq!(src.read(&mut buf).unwrap(), 0);
        assert_eq!(src.take_discontinuity(), Some(1));
    }

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

    /// A packetized `IqSource` test double mimicking HPSDR: each `read()`
    /// call fills at most `packet_len` samples regardless of the caller's
    /// buffer size, proving `DecimatingSource` no longer sizes its scratch
    /// buffer from the caller's remaining output capacity (MAN-169
    /// round-5 Codex finding).
    struct PacketizedSource {
        samples: Vec<Complex32>,
        cursor: usize,
        fs: f64,
        packet_len: usize,
    }

    impl IqSource for PacketizedSource {
        fn sample_rate(&self) -> f64 {
            self.fs
        }
        fn center_freq_hz(&self) -> f64 {
            0.0
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            let n = buf
                .len()
                .min(self.packet_len)
                .min(self.samples.len() - self.cursor);
            buf[..n].copy_from_slice(&self.samples[self.cursor..self.cursor + n]);
            self.cursor += n;
            Ok(n)
        }
    }

    #[test]
    fn read_stays_correct_against_a_packetized_source_with_a_large_caller_buffer() {
        // MAN-169 round-5 Codex finding: sizing the scratch buffer from the
        // caller's remaining output capacity (buf.len() * factor) meant a
        // large caller request (e.g. listen()'s ~2 s startup calibration
        // read) allocated and zeroed a multi-hundred-thousand-sample Vec on
        // every loop iteration, even though a packetized source like HPSDR
        // only ever fills a few hundred samples per read() regardless of
        // buffer size. Prove correctness still holds against a source that
        // caps every read() at a small fixed packet size (126, HPSDR's real
        // per-packet count), using a caller buffer far larger than that
        // packet size -- the exact shape a large calibration read produces.
        let fs_in = 192_000.0;
        let n_in = 40_000;
        let inner: Box<dyn IqSource + Send> = Box::new(PacketizedSource {
            samples: tone(2_000.0, n_in, fs_in),
            cursor: 0,
            fs: fs_in,
            packet_len: 126,
        });
        let mut src = DecimatingSource::new(inner, 48_000.0).unwrap(); // factor 4
        let mut buf = vec![Complex32::new(0.0, 0.0); 20_000];
        let mut total = 0usize;
        loop {
            let n = src.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            total += n;
        }
        let expected = n_in / 4;
        assert!(
            (total as i64 - expected as i64).unsigned_abs() < 200,
            "total {total}, expected ~{expected}"
        );
    }

    #[test]
    fn sample_rate_reports_the_decimated_value() {
        let inner: Box<dyn IqSource + Send> = Box::new(InMemorySource {
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
        let inner: Box<dyn IqSource + Send> = Box::new(InMemorySource {
            samples: tone(1_000.0, 1_000, 192_000.0),
            cursor: 0,
            fs: 192_000.0,
            center_freq_hz: 14_035_000.0,
            live: Some(live.clone()),
            counters: None,
        });
        let src = DecimatingSource::new(inner, 48_000.0).unwrap();
        assert_eq!(src.center_freq_hz(), 14_035_000.0);
        assert!(src
            .confirmed_live_handle()
            .unwrap()
            .load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn read_yields_roughly_input_len_over_factor_samples_then_eof() {
        let fs_in = 192_000.0;
        let n_in = 40_000;
        let inner: Box<dyn IqSource + Send> = Box::new(InMemorySource {
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
    fn read_output_is_bit_identical_to_processing_the_whole_input_at_once() {
        // Only asserting a sample COUNT (as
        // read_yields_roughly_input_len_over_factor_samples_then_eof does
        // above) doesn't prove the `pending`-buffer draining logic in
        // read() never drops, duplicates, or reorders a sample. Use a
        // caller buffer size (137) that does NOT evenly divide the
        // decimation factor (4), so buffer boundaries never align with
        // factor boundaries, then compare against calling
        // `Decimator::process` directly on the whole input.
        let fs_in = 192_000.0;
        let n_in = 40_000;
        let samples = tone(2_000.0, n_in, fs_in);

        let inner: Box<dyn IqSource + Send> = Box::new(InMemorySource {
            samples: samples.clone(),
            cursor: 0,
            fs: fs_in,
            center_freq_hz: 0.0,
            live: None,
            counters: None,
        });
        let mut src = DecimatingSource::new(inner, 48_000.0).unwrap(); // factor 4
        let mut via_read: Vec<Complex32> = Vec::new();
        let mut buf = vec![Complex32::new(0.0, 0.0); 137];
        loop {
            let n = src.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            via_read.extend_from_slice(&buf[..n]);
        }

        let mut direct = manta_dsp::decimate::Decimator::new(fs_in, 4).unwrap();
        let via_direct = direct.process(&samples);

        assert_eq!(via_read.len(), via_direct.len());
        for (a, b) in via_read.iter().zip(via_direct.iter()) {
            assert_eq!(a.re.to_bits(), b.re.to_bits());
            assert_eq!(a.im.to_bits(), b.im.to_bits());
        }
    }

    #[test]
    fn rejects_non_power_of_two_factor() {
        let inner: Box<dyn IqSource + Send> = Box::new(InMemorySource {
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
        let inner: Box<dyn IqSource + Send> = Box::new(InMemorySource {
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
        let inner: Box<dyn IqSource + Send> = Box::new(InMemorySource {
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
        assert_eq!(forwarded_counters.dropped_packets(), 5);
    }

    /// An `InMemorySource` that reports a reconnect-side signal: an
    /// acquisition-start offset (MAN-259) and a one-shot discontinuity
    /// (MAN-73), as a reopened live driver does.
    struct ReopenedSource {
        inner: InMemorySource,
        acquisition_start_offset: std::time::Duration,
        gap: Option<u64>,
    }

    impl IqSource for ReopenedSource {
        fn sample_rate(&self) -> f64 {
            self.inner.sample_rate()
        }
        fn center_freq_hz(&self) -> f64 {
            self.inner.center_freq_hz()
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            self.inner.read(buf)
        }
        fn take_discontinuity(&mut self) -> Option<u64> {
            self.gap.take()
        }
        fn acquisition_start_offset(&self) -> std::time::Duration {
            self.acquisition_start_offset
        }
    }

    fn reopened(
        acquisition_start_offset: std::time::Duration,
        gap: Option<u64>,
    ) -> DecimatingSource {
        let inner: Box<dyn IqSource + Send> = Box::new(ReopenedSource {
            inner: InMemorySource {
                samples: tone(1_000.0, 1_000, 192_000.0),
                cursor: 0,
                fs: 192_000.0,
                center_freq_hz: 14_000_000.0,
                live: None,
                counters: None,
            },
            acquisition_start_offset,
            gap,
        });
        DecimatingSource::new(inner, 48_000.0).unwrap()
    }

    /// MAN-259: without forwarding, the trait's `ZERO` default would put
    /// every `--capture-rate-hz` reconnect's outage end at the start of
    /// the open call, dropping the driver's pre-start handshake from it.
    #[test]
    fn acquisition_start_offset_forwards_to_the_inner_source() {
        let src = reopened(std::time::Duration::from_millis(250), None);
        assert_eq!(
            src.acquisition_start_offset(),
            std::time::Duration::from_millis(250)
        );
    }

    /// MAN-259: the same forwarding duty for MAN-73's discontinuity, which
    /// this wrapper used to swallow through the trait's `None` default. One
    /// second at the inner 192 kS/s is one second at the decimated 48 kS/s.
    /// The report is taken during `read()` (MAN-305), so it reaches the
    /// caller with the first samples that follow the gap.
    #[test]
    fn take_discontinuity_forwards_to_the_inner_source() {
        let mut src = reopened(std::time::Duration::ZERO, Some(192_000));
        let mut buf = vec![Complex32::new(0.0, 0.0); 1_024];
        assert!(src.read(&mut buf).unwrap() > 0);
        assert_eq!(src.take_discontinuity(), Some(48_000));
        assert_eq!(src.take_discontinuity(), None, "reported only once");
    }

    /// MAN-86: a decimated source must report the inner source's delivered
    /// passband (so SETT never advertises the processing rate), clipped to
    /// the band the decimator delivers usefully -- MAN-196: the trusted
    /// band, not the raw output Nyquist.
    #[test]
    fn rf_passband_forwards_the_inner_passband_clipped_to_the_output_nyquist() {
        struct NarrowSource;
        impl IqSource for NarrowSource {
            fn sample_rate(&self) -> f64 {
                192_000.0
            }
            fn center_freq_hz(&self) -> f64 {
                14_000_000.0
            }
            fn rf_passband_hz(&self) -> (f64, f64) {
                (300.0, 3_000.0)
            }
            fn read(&mut self, _buf: &mut [Complex32]) -> Result<usize> {
                Ok(0)
            }
        }
        let narrow = DecimatingSource::new(Box::new(NarrowSource), 48_000.0).unwrap();
        assert_eq!(narrow.rf_passband_hz(), (300.0, 3_000.0));

        // Inner default = full 192 kS/s Nyquist span; output is 48 kS/s.
        // MAN-196: the clip is the decimator's trusted band
        // (TRUSTED_HALF_BAND_FRACTION * 48_000 = 21_724.8 Hz), no longer
        // the 24_000 Hz output Nyquist.
        let wide: Box<dyn IqSource + Send> = Box::new(InMemorySource {
            samples: Vec::new(),
            cursor: 0,
            fs: 192_000.0,
            center_freq_hz: 14_000_000.0,
            live: None,
            counters: None,
        });
        let wide = DecimatingSource::new(wide, 48_000.0).unwrap();
        assert_eq!(wide.rf_passband_hz(), (-21_724.8, 21_724.8));
    }

    /// MAN-196: the reported usable passband is the trusted band, not the
    /// decimated Nyquist -- the transition band beyond the <= 1 dB
    /// crossover is spectrum the filter has already attenuated.
    #[test]
    fn rf_passband_is_clipped_to_the_trusted_band_not_the_nyquist() {
        let expected = manta_dsp::decimate::TRUSTED_HALF_BAND_FRACTION * 48_000.0;
        assert!((expected - 21_724.8).abs() < 1e-9);
        let wide: Box<dyn IqSource + Send> = Box::new(InMemorySource {
            samples: Vec::new(),
            cursor: 0,
            fs: 192_000.0,
            center_freq_hz: 14_000_000.0,
            live: None,
            counters: None,
        });
        let wide = DecimatingSource::new(wide, 48_000.0).unwrap();
        let (lo, hi) = wide.rf_passband_hz();
        assert!((lo - -expected).abs() < 1e-9, "lo {lo}");
        assert!((hi - expected).abs() < 1e-9, "hi {hi}");
        // NOT the Nyquist: the boundary must sit strictly inside 24 kHz.
        assert!(lo > -24_000.0 && hi < 24_000.0);
    }

    /// MAN-196: `trusted_band_hz` reports the decimator boundary for a
    /// decimated source and `None` (the trait default) without one.
    #[test]
    fn trusted_band_hz_reports_the_decimator_boundary() {
        let expected = manta_dsp::decimate::TRUSTED_HALF_BAND_FRACTION * 48_000.0;
        let decimated = DecimatingSource::new(
            Box::new(InMemorySource {
                samples: Vec::new(),
                cursor: 0,
                fs: 192_000.0,
                center_freq_hz: 14_000_000.0,
                live: None,
                counters: None,
            }),
            48_000.0,
        )
        .unwrap();
        assert_eq!(decimated.trusted_band_hz(), Some((-expected, expected)));

        // A bare source with no decimator: the trait default, no boundary.
        let bare = InMemorySource {
            samples: Vec::new(),
            cursor: 0,
            fs: 48_000.0,
            center_freq_hz: 14_000_000.0,
            live: None,
            counters: None,
        };
        assert_eq!(bare.trusted_band_hz(), None);
    }

    /// MAN-196: a factor-1 wrap (target == native rate, zero cascade
    /// stages) is a pure pass-through: no filter, no transition band, so
    /// it must neither report a trusted band nor narrow the advertised
    /// passband. The CLI never builds one (`maybe_decimate` skips the
    /// wrap for an equal rate); this pins the wrapper's own honesty.
    #[test]
    fn a_factor_one_pass_through_has_no_trusted_band() {
        let src = DecimatingSource::new(
            Box::new(InMemorySource {
                samples: Vec::new(),
                cursor: 0,
                fs: 48_000.0,
                center_freq_hz: 14_000_000.0,
                live: None,
                counters: None,
            }),
            48_000.0,
        )
        .unwrap();
        assert_eq!(src.sample_rate(), 48_000.0);
        assert_eq!(src.trusted_band_hz(), None);
        assert_eq!(src.rf_passband_hz(), (-24_000.0, 24_000.0));
    }

    /// An `InMemorySource` reporting a narrower RF passband than its
    /// Nyquist span, as a one-sided rig-audio source does.
    struct NarrowSource {
        inner: InMemorySource,
        passband: (f64, f64),
    }

    impl IqSource for NarrowSource {
        fn sample_rate(&self) -> f64 {
            self.inner.sample_rate()
        }
        fn center_freq_hz(&self) -> f64 {
            self.inner.center_freq_hz()
        }
        fn rf_passband_hz(&self) -> (f64, f64) {
            self.passband
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            self.inner.read(buf)
        }
    }

    /// MAN-196 (round-2 review on PR #300): when the wrapped source is
    /// narrower than the decimator's trusted band, the spawn veto is the
    /// same intersected band `rf_passband_hz` (and the startup notice)
    /// advertises -- advertisement and enforcement never disagree.
    #[test]
    fn trusted_band_hz_matches_the_advertised_passband_for_a_narrow_source() {
        let src = DecimatingSource::new(
            Box::new(NarrowSource {
                inner: InMemorySource {
                    samples: Vec::new(),
                    cursor: 0,
                    fs: 48_000.0,
                    center_freq_hz: 14_000_000.0,
                    live: None,
                    counters: None,
                },
                passband: (300.0, 3_000.0),
            }),
            24_000.0,
        )
        .unwrap();
        assert_eq!(src.rf_passband_hz(), (300.0, 3_000.0));
        assert_eq!(src.trusted_band_hz(), Some((300.0, 3_000.0)));
    }
    // ---- MAN-305: both gap hooks forwarded, scaled to the output rate ----

    /// Serves `samples` `chunk` at a time; on inner read number `i`
    /// (0-based) leaves pending whatever `gaps`/`missing` hold for `i`.
    struct GapReportingSource {
        samples: Vec<Complex32>,
        cursor: usize,
        chunk: usize,
        reads: usize,
        gaps: Vec<(usize, u64)>,
        missing: Vec<(usize, u64)>,
        pending_gap: Option<u64>,
        pending_missing: Option<u64>,
    }

    impl GapReportingSource {
        fn new(samples: Vec<Complex32>, chunk: usize) -> Self {
            GapReportingSource {
                samples,
                cursor: 0,
                chunk,
                reads: 0,
                gaps: Vec::new(),
                missing: Vec::new(),
                pending_gap: None,
                pending_missing: None,
            }
        }
    }

    impl IqSource for GapReportingSource {
        fn sample_rate(&self) -> f64 {
            192_000.0
        }
        fn center_freq_hz(&self) -> f64 {
            0.0
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            let i = self.reads;
            self.reads += 1;
            for &(_, k) in self.gaps.iter().filter(|(at, _)| *at == i) {
                *self.pending_gap.get_or_insert(0) += k;
            }
            for &(_, k) in self.missing.iter().filter(|(at, _)| *at == i) {
                *self.pending_missing.get_or_insert(0) += k;
            }
            let n = buf
                .len()
                .min(self.chunk)
                .min(self.samples.len() - self.cursor);
            buf[..n].copy_from_slice(&self.samples[self.cursor..self.cursor + n]);
            self.cursor += n;
            Ok(n)
        }
        fn take_discontinuity(&mut self) -> Option<u64> {
            self.pending_gap.take()
        }
        fn take_missing_samples(&mut self) -> Option<u64> {
            self.pending_missing.take()
        }
    }

    /// `(take_discontinuity(), take_missing_samples())` after one read.
    type GapReport = (Option<u64>, Option<u64>);

    /// Reads `src` to EOF with a `buf_len` buffer: every decimated sample,
    /// and `(take_discontinuity(), take_missing_samples())` after each read
    /// that returned samples.
    fn drain_with_reports(
        src: &mut DecimatingSource,
        buf_len: usize,
    ) -> (Vec<Complex32>, Vec<GapReport>) {
        let mut out = Vec::new();
        let mut reports = Vec::new();
        let mut buf = vec![Complex32::new(0.0, 0.0); buf_len];
        loop {
            let n = src.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
            reports.push((src.take_discontinuity(), src.take_missing_samples()));
        }
        (out, reports)
    }

    fn assert_bit_identical(a: &[Complex32], b: &[Complex32]) {
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b) {
            assert_eq!(x.re.to_bits(), y.re.to_bits());
            assert_eq!(x.im.to_bits(), y.im.to_bits());
        }
    }

    /// MAN-305: an inner outage of 11,900 samples at 192 kHz reaches the
    /// caller as 5,950 at 96 kHz, once, with the read that returns the
    /// post-gap samples; and the filter history is reset there, so the
    /// post-gap output is exactly a fresh decimator's.
    #[test]
    fn decimating_source_forwards_take_discontinuity_scaled() {
        let samples = tone(2_000.0, 4_000, 192_000.0);
        let mut inner = GapReportingSource::new(samples.clone(), 1_000);
        inner.gaps.push((2, 11_900));
        let mut src = DecimatingSource::new(Box::new(inner), 96_000.0).unwrap(); // factor 2
        let (out, reports) = drain_with_reports(&mut src, 500);
        assert_eq!(
            reports,
            vec![
                (None, None),
                (None, None),
                (Some(5_950), None),
                (None, None)
            ]
        );
        assert_eq!(src.take_discontinuity(), None);

        let fresh = manta_dsp::decimate::Decimator::new(192_000.0, 2)
            .unwrap()
            .process(&samples[2_000..]);
        assert_bit_identical(&out[1_000..], &fresh);
    }

    /// MAN-305: a spliced inner loss is forwarded the same way, scaled and
    /// once, but it is a splice: the filter history is kept, so the output
    /// is the uninterrupted decimation of every sample served.
    #[test]
    fn decimating_source_forwards_take_missing_samples_scaled() {
        let samples = tone(2_000.0, 4_000, 192_000.0);
        let mut inner = GapReportingSource::new(samples.clone(), 1_000);
        inner.missing.push((2, 11_900));
        let mut src = DecimatingSource::new(Box::new(inner), 96_000.0).unwrap();
        let (out, reports) = drain_with_reports(&mut src, 500);
        assert_eq!(
            reports,
            vec![
                (None, None),
                (None, None),
                (None, Some(5_950)),
                (None, None)
            ]
        );
        assert_eq!(src.take_missing_samples(), None);

        let whole = manta_dsp::decimate::Decimator::new(192_000.0, 2)
            .unwrap()
            .process(&samples);
        assert_bit_identical(&out, &whole);
    }

    /// MAN-305: 238-sample losses (one protocol-2 datagram each) at factor
    /// 4 are not whole output samples; the remainder is carried, so four of
    /// them report 59, 60, 59, 60 -- exactly the 238 output samples the
    /// 952 lost input samples are.
    #[test]
    fn decimating_source_carries_the_fractional_remainder() {
        let samples = tone(2_000.0, 4_000, 192_000.0);
        let mut inner = GapReportingSource::new(samples, 1_000);
        inner.missing.extend((0..4).map(|i| (i, 238)));
        inner.gaps.extend((0..4).map(|i| (i, 238)));
        let mut src = DecimatingSource::new(Box::new(inner), 48_000.0).unwrap(); // factor 4
        let (_, reports) = drain_with_reports(&mut src, 250);
        let scaled = [59, 60, 59, 60].map(Some);
        assert_eq!(
            reports,
            scaled.iter().map(|&k| (k, k)).collect::<Vec<_>>(),
            "each hook carries its own remainder"
        );
    }
}
