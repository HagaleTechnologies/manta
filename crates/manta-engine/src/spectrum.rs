//! MAN-141: the spectrum tap. The channelizer's per-channel power, averaged
//! over [`SPECTRUM_HOPS_PER_FRAME`] usable hops and handed to a
//! [`SpectrumSink`] (`manta run` sends it to N1MM+'s Spectrum Display).
//! Write-only from the decode path, stream time only, so spots are
//! unchanged.

use manta_dsp::channelizer::{power_db, HopOutput, CHANNEL_SPACING_HZ};
use std::sync::Arc;

/// Usable hops averaged into one frame: 1/3 s at 375 hops/s.
pub const SPECTRUM_HOPS_PER_FRAME: usize = 125;

/// Where the decode loop hands spectrum frames. `publish` runs on the decode
/// thread (or the input's own thread) and must never block.
pub trait SpectrumSink: Send + Sync {
    fn publish(&self, frame: SpectrumFrame);
}

/// One averaged spectrum, ascending frequency, evenly spaced. Calibrated Hz.
#[derive(Debug, Clone, PartialEq)]
pub struct SpectrumFrame {
    /// The first bin's centre.
    pub low_hz: f64,
    /// The last bin's centre.
    pub high_hz: f64,
    /// `power_db` of the mean linear power per bin, ascending frequency.
    pub power_db: Vec<f32>,
}

/// Which channels a feed sends, in ascending RF order, and its calibrated
/// edges (the first and last bin centres).
#[derive(Debug, Clone, PartialEq)]
pub struct SpectrumSpan {
    /// Channel indices (FFT bin order, as `HopOutput::power` holds them).
    pub bins: Vec<usize>,
    pub low_hz: f64,
    pub high_hz: f64,
}

/// The channels inside `rf_passband_hz` (offsets from the centre, both ends
/// inclusive) whose calibrated RF frequency is above 0 Hz, sorted by
/// frequency. The edges are SPEC §1.1's
/// `f(k) = f_center + ((k + N/2) mod N - N/2) * Delta` times
/// `calibration_factor`. `None` when the centre is not positive (no RF
/// reference), the rate is not a channelizer rate, or no channel is inside
/// the passband. Pure, so `manta-cli` can print the startup note from the
/// same numbers. The only place a spectrum bin is mapped to a frequency.
pub fn spectrum_span(
    fs: f64,
    center_freq_hz: f64,
    rf_passband_hz: (f64, f64),
    calibration_factor: f64,
) -> Option<SpectrumSpan> {
    // `is_nan` too: a NaN centre is no RF reference either.
    if center_freq_hz.is_nan()
        || center_freq_hz <= 0.0
        || !calibration_factor.is_finite()
        || !fs.is_finite()
    {
        return None;
    }
    let nf = fs / CHANNEL_SPACING_HZ;
    let n = nf.round() as usize;
    if n < 2 || (nf - n as f64).abs() > 1e-9 || !n.is_power_of_two() {
        return None;
    }
    let delta = fs / n as f64;
    let half = (n / 2) as i64;
    let (lo, hi) = rf_passband_hz;
    let mut bins = Vec::new();
    let mut edges: Option<(f64, f64)> = None;
    for signed in -half..half {
        let offset = signed as f64 * delta;
        if offset < lo || offset > hi {
            continue;
        }
        let freq = (center_freq_hz + offset) * calibration_factor;
        if freq <= 0.0 {
            continue;
        }
        bins.push((signed + n as i64) as usize % n);
        edges = Some(match edges {
            None => (freq, freq),
            Some((low, _)) => (low, freq),
        });
    }
    let (low_hz, high_hz) = edges?;
    Some(SpectrumSpan {
        bins,
        low_hz,
        high_hz,
    })
}

/// A `ListenObservers::spectrum` / `InputObservers::spectrum` sink and the
/// input's calibration factor, from which every segment builds its own
/// accumulator.
#[derive(Clone)]
pub(crate) struct SpectrumFeed {
    pub(crate) sink: Arc<dyn SpectrumSink>,
    pub(crate) calibration_factor: f64,
}

impl SpectrumFeed {
    /// A new, empty accumulator for a segment over this source; `None`
    /// when `spectrum_span` is (no RF centre), so nothing is sent.
    pub(crate) fn accumulator(
        &self,
        fs: f64,
        center_freq_hz: f64,
        rf_passband_hz: (f64, f64),
    ) -> Option<SpectrumAccumulator> {
        spectrum_span(fs, center_freq_hz, rf_passband_hz, self.calibration_factor)
            .map(|span| SpectrumAccumulator::new(self.sink.clone(), span))
    }
}

/// One segment's running mean. A new segment starts a new accumulator, so
/// a frame never spans a discontinuity; a partial frame at a segment or
/// stream end is dropped.
pub(crate) struct SpectrumAccumulator {
    sink: Arc<dyn SpectrumSink>,
    span: SpectrumSpan,
    sum: Vec<f64>,
    hops: usize,
}

impl SpectrumAccumulator {
    pub(crate) fn new(sink: Arc<dyn SpectrumSink>, span: SpectrumSpan) -> Self {
        let sum = vec![0.0; span.bins.len()];
        Self {
            sink,
            span,
            sum,
            hops: 0,
        }
    }

    /// Adds every hop with `m >= pad_hops` (earlier hops are the
    /// channelizer's pre-roll); publishes and clears after
    /// [`SPECTRUM_HOPS_PER_FRAME`].
    pub(crate) fn fold(&mut self, hops: &[HopOutput], pad_hops: u64) {
        for hop in hops.iter().filter(|h| h.m >= pad_hops) {
            for (sum, &k) in self.sum.iter_mut().zip(&self.span.bins) {
                *sum += hop.power[k] as f64;
            }
            self.hops += 1;
            if self.hops == SPECTRUM_HOPS_PER_FRAME {
                let n = self.hops as f64;
                let power_db = self
                    .sum
                    .iter()
                    .map(|&s| power_db((s / n) as f32) as f32)
                    .collect();
                self.sink.publish(SpectrumFrame {
                    low_hz: self.span.low_hz,
                    high_hz: self.span.high_hz,
                    power_db,
                });
                self.sum.iter_mut().for_each(|s| *s = 0.0);
                self.hops = 0;
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use manta_dsp::channelizer::Channelizer;
    use num_complex::Complex32;
    use std::sync::Mutex;

    /// Records every published frame.
    #[derive(Default)]
    pub(crate) struct RecordingSpectrumSink {
        pub(crate) frames: Mutex<Vec<SpectrumFrame>>,
    }

    impl RecordingSpectrumSink {
        pub(crate) fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }
        pub(crate) fn take(&self) -> Vec<SpectrumFrame> {
            std::mem::take(&mut *self.frames.lock().unwrap())
        }
    }

    impl SpectrumSink for RecordingSpectrumSink {
        fn publish(&self, frame: SpectrumFrame) {
            self.frames.lock().unwrap().push(frame);
        }
    }

    const CENTRE: f64 = 14_050_000.0;

    #[test]
    fn span_covers_the_sources_passband_in_ascending_order() {
        // Values measured by the plan's probe with the real channelizer.
        let audio = spectrum_span(48_000.0, CENTRE, (300.0, 3000.0), 1.0).unwrap();
        assert_eq!(
            (audio.bins.len(), audio.low_hz, audio.high_hz),
            (29, 14_050_375.0, 14_053_000.0)
        );
        let iq48 = spectrum_span(48_000.0, CENTRE, (-24_000.0, 24_000.0), 1.0).unwrap();
        assert_eq!(
            (iq48.bins.len(), iq48.low_hz, iq48.high_hz),
            (512, 14_026_000.0, 14_073_906.25)
        );
        let half = 0.4526 * 96_000.0;
        let dec = spectrum_span(192_000.0, CENTRE, (-half, half), 1.0).unwrap();
        assert_eq!(dec.bins.len(), 927);
        // Bins are channel indices in strictly ascending RF order, as the
        // channelizer maps them.
        for (fs, span) in [(48_000.0, &audio), (48_000.0, &iq48), (192_000.0, &dec)] {
            let ch = Channelizer::new(fs, CENTRE).unwrap();
            let freqs: Vec<f64> = span.bins.iter().map(|&k| ch.channel_freq_hz(k)).collect();
            assert!(freqs.windows(2).all(|w| w[0] < w[1]));
            assert_eq!(freqs[0], span.low_hz);
            assert_eq!(*freqs.last().unwrap(), span.high_hz);
        }
    }

    #[test]
    fn span_applies_the_calibration_factor() {
        let factor = 1.0 + 10e-6;
        let span = spectrum_span(48_000.0, CENTRE, (-24_000.0, 24_000.0), factor).unwrap();
        assert_eq!(span.low_hz, 14_026_000.0 * factor);
        assert_eq!(span.high_hz, 14_073_906.25 * factor);
    }

    #[test]
    fn span_is_none_without_an_rf_centre() {
        assert_eq!(
            spectrum_span(48_000.0, 0.0, (-24_000.0, 24_000.0), 1.0),
            None
        );
        assert_eq!(
            spectrum_span(48_000.0, -1.0, (-24_000.0, 24_000.0), 1.0),
            None
        );
        // A 20 kHz centre at 48 kS/s: the bins at or below 0 Hz are dropped.
        let span = spectrum_span(48_000.0, 20_000.0, (-24_000.0, 24_000.0), 1.0).unwrap();
        assert!(span.low_hz > 0.0);
        assert_eq!(span.low_hz, 20_000.0 - 213.0 * 93.75);
        assert_eq!(span.bins.len(), 213 + 256);
        // Not a channelizer rate.
        assert_eq!(
            spectrum_span(44_100.0, CENTRE, (-22_050.0, 22_050.0), 1.0),
            None
        );
    }

    /// A deterministic small noise source (an LCG), so no dependency.
    fn noise(seed: &mut u64) -> f32 {
        *seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((*seed >> 40) as f32 / (1u64 << 24) as f32 - 0.5) * 2e-3
    }

    /// Runs the probe's input through a real channelizer as `listen.rs`
    /// does: `filter_len` zeros, then 2 s of a 0.1-amplitude tone 1500 Hz
    /// above the centre with a little noise, in 2048-sample chunks.
    fn probe_frames(tone: bool) -> (Vec<SpectrumFrame>, usize) {
        let fs = 48_000.0;
        let mut ch = Channelizer::new(fs, CENTRE).unwrap();
        let pad_hops = (ch.filter_len() as u64).div_ceil(ch.hop() as u64);
        let sink = RecordingSpectrumSink::new();
        let feed = SpectrumFeed {
            sink: sink.clone(),
            calibration_factor: 1.0,
        };
        let mut acc = feed.accumulator(fs, CENTRE, (-24_000.0, 24_000.0)).unwrap();
        let padding = vec![Complex32::new(0.0, 0.0); ch.filter_len()];
        acc.fold(&ch.process(&padding), pad_hops);
        let after_padding = sink.take().len();
        let mut seed = 7u64;
        let n = (fs * 2.0) as usize;
        let w = std::f64::consts::TAU * 1500.0 / fs;
        let samples: Vec<Complex32> = (0..n)
            .map(|i| {
                let a = if tone { 0.1 } else { 0.0 };
                let p = w * i as f64;
                Complex32::new(
                    (a * p.cos()) as f32 + noise(&mut seed),
                    (a * p.sin()) as f32 + noise(&mut seed),
                )
            })
            .collect();
        for chunk in samples.chunks(2048) {
            acc.fold(&ch.process(chunk), pad_hops);
        }
        (sink.take(), after_padding)
    }

    #[test]
    fn accumulator_emits_one_frame_per_125_hops_after_preroll() {
        let (frames, after_padding) = probe_frames(true);
        assert_eq!(after_padding, 0, "padding alone yields no frame");
        // 751 hops, 32 of them pre-roll: 719 usable, 5 whole frames.
        assert_eq!(frames.len(), 5);
        for f in &frames {
            assert_eq!(f.power_db.len(), 512);
            assert_eq!((f.low_hz, f.high_hz), (14_026_000.0, 14_073_906.25));
        }
    }

    #[test]
    fn frame_peak_is_the_tone_bin() {
        let (frames, _) = probe_frames(true);
        let span = spectrum_span(48_000.0, CENTRE, (-24_000.0, 24_000.0), 1.0).unwrap();
        let step = (span.high_hz - span.low_hz) / (span.bins.len() - 1) as f64;
        for f in &frames {
            let (i, &peak) = f
                .power_db
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .unwrap();
            let freq = f.low_hz + i as f64 * step;
            assert_eq!(freq, CENTRE + 1500.0);
            assert!((peak + 20.0).abs() < 1.0, "{peak}");
        }
    }

    #[test]
    fn a_frame_never_contains_preroll_power() {
        // Pre-roll hops (m < pad_hops) carry a huge power; the usable hops
        // carry 1.0. Every value of the one frame is 0 dB, so no pre-roll
        // hop was averaged in.
        let pad_hops = 32u64;
        let sink = RecordingSpectrumSink::new();
        let span = SpectrumSpan {
            bins: vec![0, 1, 2],
            low_hz: 14_000_000.0,
            high_hz: 14_000_187.5,
        };
        let mut acc = SpectrumAccumulator::new(sink.clone(), span);
        let hop = |m: u64, p: f32| HopOutput {
            m,
            x: vec![Complex32::new(0.0, 0.0); 4],
            power: vec![p; 4],
        };
        let pre: Vec<HopOutput> = (0..pad_hops).map(|m| hop(m, 1e6)).collect();
        acc.fold(&pre, pad_hops);
        assert!(sink.take().is_empty());
        let usable: Vec<HopOutput> = (pad_hops..pad_hops + SPECTRUM_HOPS_PER_FRAME as u64)
            .map(|m| hop(m, 1.0))
            .collect();
        acc.fold(&usable, pad_hops);
        let frames = sink.take();
        assert_eq!(frames.len(), 1);
        assert!(
            frames[0].power_db.iter().all(|&d| d.abs() < 1e-6),
            "{:?}",
            frames[0]
        );
        // And a noise input never reads the empty-channel floor.
        let (frames, _) = probe_frames(false);
        let floor = power_db(0.0) as f32;
        assert!(frames
            .iter()
            .all(|f| f.power_db.iter().all(|&d| d > floor + 1.0)));
    }
}
