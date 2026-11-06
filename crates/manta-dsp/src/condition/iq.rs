//! Fixed receiver IQ calibration, controlled-tone measurement and conservative
//! mirror observation. A mirror observation never supplies correction coefficients.
use num_complex::{Complex32, Complex64};

use coppa_dsp::fft::FftProcessor;
use std::f64::consts::TAU;
fn wide(x: Complex32) -> Complex64 {
    Complex64::new(x.re as f64, x.im as f64)
}
fn narrow(x: Complex64) -> Complex32 {
    Complex32::new(x.re as f32, x.im as f32)
}

/// A receiver calibration measured with a separate, continuous single tone.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IqCalibration {
    /// Q-channel gain divided by I-channel gain, in [0.5, 2].
    pub gain_ratio: f64,
    /// Q phase error in degrees, in [-45, 45].
    pub phase_deg: f64,
    /// Actual receiver centre when measured, in Hz.
    pub center_freq_hz: f64,
}
impl IqCalibration {
    /// Reject unusable calibration values at every entry point.
    pub fn validate(&self) -> Result<(), String> {
        if !self.gain_ratio.is_finite() || !(0.5..=2.0).contains(&self.gain_ratio) {
            return Err("iq_gain_ratio must be finite and in [0.5, 2.0]".into());
        }
        if !self.phase_deg.is_finite() || !(-45.0..=45.0).contains(&self.phase_deg) {
            return Err("iq_phase_deg must be finite and in [-45, 45]".into());
        }
        if !self.center_freq_hz.is_finite() || self.center_freq_hz <= 0.0 {
            return Err("iq_calibrated_center_hz must be finite and positive".into());
        }
        Ok(())
    }
    /// Conjugate leakage coefficient for `m.im = g*(s.im*cos(phi)+s.re*sin(phi))`.
    pub fn image_ratio(&self) -> Complex64 {
        let w = Complex64::from_polar(self.gain_ratio, -self.phase_deg.to_radians());
        (1.0 - w) / (1.0 + w)
    }
    /// Convert a measured image coefficient into receiver gain and phase.
    pub fn from_image_ratio(rho: Complex64, center_freq_hz: f64) -> Result<Self, String> {
        IqCorrector::new(rho)?;
        let w = (1.0 - rho) / (1.0 + rho);
        let mut c = Self {
            gain_ratio: w.norm(),
            phase_deg: -w.arg().to_degrees(),
            center_freq_hz,
        };
        // Permit only roundoff at the documented inclusive boundaries.
        if (c.gain_ratio - 0.5).abs() < 1e-14 {
            c.gain_ratio = 0.5;
        }
        if (c.gain_ratio - 2.0).abs() < 1e-14 {
            c.gain_ratio = 2.0;
        }
        if (c.phase_deg.abs() - 45.0).abs() < 1e-12 {
            c.phase_deg = c.phase_deg.signum() * 45.0;
        }
        c.validate()?;
        Ok(c)
    }
}

/// Fixed, validated conjugate cancellation. No observation changes `rho`.
#[derive(Debug, Clone)]
pub struct IqCorrector {
    rho: Complex64,
}
impl IqCorrector {
    /// Build an invertible correction from a finite, nonsingular coefficient.
    pub fn new(rho: Complex64) -> Result<Self, String> {
        if !rho.re.is_finite() || !rho.im.is_finite() || rho.norm_sqr() >= 1.0 - 1e-12 {
            return Err("IQ image coefficient must be finite with magnitude below one".into());
        }
        Ok(Self { rho })
    }
    /// `y = m - rho*conj(m)`, evaluated in f64 then rounded once to f32.
    pub fn process_sample(&self, m: Complex32) -> Complex32 {
        narrow(wide(m) - self.rho * wide(m).conj())
    }
}
/// Undo IQ correction before undoing DC and impulse repairs.
pub fn revert_iq(samples: &mut [Complex32], rho: Complex64) -> Result<(), String> {
    IqCorrector::new(rho)?;
    for s in samples {
        let y = wide(*s);
        *s = narrow((y + rho * y.conj()) / (1.0 - rho.norm_sqr()));
    }
    Ok(())
}
/// Image rejection in dB. A zero coefficient is perfectly balanced.
pub fn image_rejection_db(rho: Complex64) -> f64 {
    -10.0 * rho.norm_sqr().max(f64::MIN_POSITIVE).log10()
}

/// Why a controlled-tone measurement cannot supply a calibration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToneRejection {
    /// No suitable strong isolated tone, or its frequency/probes are unusable.
    NotFound,
    /// Fewer than five complete one-second segments.
    TooShort,
    /// Segment coefficients disagree, for example a keyed mirror station.
    Inconsistent,
}
/// Latest controlled-tone measurement; accepted values calibrate a later stream.
#[derive(Debug, Clone, PartialEq)]
pub enum ToneResult {
    /// A stable continuous tone, at least 60 dB SNR in a one-Hz bin.
    Accepted {
        /// Refined signed baseband offset, Hz.
        offset_hz: f64,
        /// Conservative median SNR in a one-Hz bin, dB.
        snr_1hz_db: f64,
        /// Mean measured `c_minus/conj(c_plus)`.
        rho: Complex64,
        /// Complete one-second segments measured.
        segments: u64,
        /// True when image rejection is at least 60 dB.
        balanced: bool,
    },
    /// Measurement is unusable or not yet long enough.
    Rejected(ToneRejection),
}

const SNR_BINS: usize = 6001;
/// A sample-domain continuous-tone meter. It retains only the first second,
/// then uses 18 phasor sums per segment and bounded aggregate statistics.
#[derive(Debug, Clone)]
pub struct ToneMeter {
    fs: f64,
    declared: f64,
    len: usize,
    first: Vec<Complex32>,
    offset: Option<f64>,
    pos: usize,
    sums: [Complex64; 18],
    oscillators: [Complex64; 18],
    steps: [Complex64; 18],
    segments: u64,
    rho_sum: Complex64,
    rho_min: Complex64,
    rho_max: Complex64,
    snrs: Box<[u64; SNR_BINS]>,
    result: ToneResult,
    valid: bool,
}
impl ToneMeter {
    /// `declared_hz` identifies a tone away from DC and Nyquist.
    /// Search is limited to ±300 Hz; all 16 noise probes must fit unaliased.
    pub fn new(fs: f64, declared_hz: f64) -> Self {
        let valid = fs.is_finite()
            && (2.0..=10_000_000.0).contains(&fs)
            && declared_hz.is_finite()
            && declared_hz.abs() > 301.0
            && declared_hz - 301.0 > -fs / 2.0
            && declared_hz + 651.0 < fs / 2.0;
        let len = if valid { fs.round() as usize } else { 1 };
        Self {
            fs,
            declared: declared_hz,
            len,
            first: Vec::new(),
            offset: None,
            pos: 0,
            sums: [Complex64::new(0.0, 0.0); 18],
            oscillators: [Complex64::new(1.0, 0.0); 18],
            steps: [Complex64::new(1.0, 0.0); 18],
            segments: 0,
            rho_sum: Complex64::new(0.0, 0.0),
            rho_min: Complex64::new(f64::INFINITY, f64::INFINITY),
            rho_max: Complex64::new(f64::NEG_INFINITY, f64::NEG_INFINITY),
            snrs: Box::new([0; SNR_BINS]),
            result: ToneResult::Rejected(if valid {
                ToneRejection::TooShort
            } else {
                ToneRejection::NotFound
            }),
            valid,
        }
    }
    /// Observe one final sample after DC removal and before IQ correction.
    pub fn push(&mut self, sample: Complex32) {
        if !self.valid {
            return;
        }
        if !sample.re.is_finite() || !sample.im.is_finite() {
            self.valid = false;
            self.result = ToneResult::Rejected(ToneRejection::NotFound);
            return;
        }
        if self.offset.is_none() {
            self.first.push(sample);
            if self.first.len() == self.len {
                let f = refine_frequency(&self.first, self.fs, self.declared);
                self.offset = Some(f);
                let frequencies = frequencies(f);
                for (step, freq) in self.steps.iter_mut().zip(frequencies) {
                    *step = Complex64::from_polar(1.0, -TAU * freq / self.fs);
                }
                let first = std::mem::take(&mut self.first);
                for x in first {
                    self.accumulate(x);
                }
            }
        } else {
            self.accumulate(sample);
        }
    }
    /// Constant-cost cached result, updated only at one-second boundaries.
    pub fn result(&self) -> ToneResult {
        self.result.clone()
    }
    fn accumulate(&mut self, x: Complex32) {
        let x = wide(x);
        for i in 0..18 {
            self.sums[i] += x * self.oscillators[i];
            self.oscillators[i] *= self.steps[i];
        }
        self.pos += 1;
        if self.pos == self.len {
            self.finish_segment();
        }
    }
    fn finish_segment(&mut self) {
        let f = self.offset.expect("frequency precedes streaming");
        let a = self.sums[0] / self.len as f64;
        let b = self.sums[1] / self.len as f64;
        let (plus, minus) = separate(a, b, f, self.fs, self.len);
        let rho = if plus.norm_sqr() > 1e-30 {
            minus / plus.conj()
        } else {
            Complex64::new(2.0, 0.0)
        };
        let mut noise = [0.0; 16];
        for (i, power) in noise.iter_mut().enumerate() {
            let probe = f + 50.0 + 20.0 * i as f64;
            let leakage = plus * dirichlet(f - probe, self.fs, self.len)
                + minus * dirichlet(-f - probe, self.fs, self.len);
            *power = (self.sums[i + 2] / self.len as f64 - leakage).norm_sqr();
        }
        noise.sort_by(f64::total_cmp);
        let noise_power = (noise[7] + noise[8]) * 0.5;
        let snr = 10.0 * (plus.norm_sqr().max(1e-300) / noise_power.max(1e-300)).log10();
        let bin = ((snr.clamp(-300.0, 300.0) + 300.0) * 10.0).floor() as usize;
        self.snrs[bin] += 1;
        self.segments += 1;
        self.rho_sum += rho;
        self.rho_min.re = self.rho_min.re.min(rho.re);
        self.rho_min.im = self.rho_min.im.min(rho.im);
        self.rho_max.re = self.rho_max.re.max(rho.re);
        self.rho_max.im = self.rho_max.im.max(rho.im);
        if self.segments >= 5 {
            let mut count = 0;
            let mut median = -300.0;
            for (i, n) in self.snrs.iter().enumerate() {
                count += n;
                if count > (self.segments - 1) / 2 {
                    median = i as f64 / 10.0 - 300.0;
                    break;
                }
            }
            let mean = self.rho_sum / self.segments as f64;
            // The bounding rectangle conservatively bounds every segment's
            // distance from the final mean without retaining segment history.
            let delta_re = (self.rho_min.re - mean.re)
                .abs()
                .max((self.rho_max.re - mean.re).abs());
            let delta_im = (self.rho_min.im - mean.im)
                .abs()
                .max((self.rho_max.im - mean.im).abs());
            self.result = if median < 60.0 || IqCalibration::from_image_ratio(mean, 1.0).is_err() {
                ToneResult::Rejected(ToneRejection::NotFound)
            } else if delta_re.hypot(delta_im) > 10f64.powf(-50.0 / 20.0) {
                ToneResult::Rejected(ToneRejection::Inconsistent)
            } else {
                ToneResult::Accepted {
                    offset_hz: f,
                    snr_1hz_db: median,
                    rho: mean,
                    segments: self.segments,
                    balanced: image_rejection_db(mean) >= 60.0,
                }
            };
        }
        self.pos = 0;
        self.sums.fill(Complex64::new(0.0, 0.0));
        self.oscillators.fill(Complex64::new(1.0, 0.0));
    }
}
fn frequencies(f: f64) -> [f64; 18] {
    std::array::from_fn(|i| match i {
        0 => f,
        1 => -f,
        _ => f + 50.0 + 20.0 * (i - 2) as f64,
    })
}
fn dirichlet(f: f64, fs: f64, n: usize) -> Complex64 {
    let a = TAU * f / fs;
    if a.abs() < 1e-12 {
        return Complex64::new(1.0, 0.0);
    }
    Complex64::from_polar(
        (n as f64 * a / 2.0).sin() / (n as f64 * (a / 2.0).sin()),
        a * (n - 1) as f64 / 2.0,
    )
}
fn separate(a: Complex64, b: Complex64, f: f64, fs: f64, n: usize) -> (Complex64, Complex64) {
    let d = dirichlet(-2.0 * f, fs, n);
    let denom = 1.0 - d.norm_sqr();
    ((a - d * b) / denom, (b - d.conj() * a) / denom)
}
fn fit_energy(x: &[Complex32], fs: f64, f: f64) -> f64 {
    let step = Complex64::from_polar(1.0, -TAU * f / fs);
    let mut osc = Complex64::new(1.0, 0.0);
    let mut a = Complex64::new(0.0, 0.0);
    let mut b = a;
    for sample in x {
        let s = wide(*sample);
        a += s * osc;
        b += s * osc.conj();
        osc *= step;
    }
    a /= x.len() as f64;
    b /= x.len() as f64;
    let (plus, minus) = separate(a, b, f, fs, x.len());
    (plus.conj() * a + minus.conj() * b).re
}
fn refine_frequency(x: &[Complex32], fs: f64, declared: f64) -> f64 {
    let n = x.len().next_power_of_two();
    let mut padded = vec![Complex32::new(0.0, 0.0); n];
    padded[..x.len()].copy_from_slice(x);
    let spec = FftProcessor::new(n).forward(&padded);
    let lo = ((declared - 300.0) * n as f64 / fs).ceil() as i64;
    let hi = ((declared + 300.0) * n as f64 / fs).floor() as i64;
    let index = |k: i64| k.rem_euclid(n as i64) as usize;
    let peak = (lo..=hi)
        .max_by(|&a, &b| {
            spec[index(a)]
                .norm_sqr()
                .total_cmp(&spec[index(b)].norm_sqr())
        })
        .unwrap_or(lo);
    let bin = fs / n as f64;
    let mut left = (peak as f64 * bin - bin).max(declared - 300.0);
    let mut right = (peak as f64 * bin + bin).min(declared + 300.0);
    let ratio = (5f64.sqrt() - 1.0) / 2.0;
    let mut a = right - ratio * (right - left);
    let mut b = left + ratio * (right - left);
    let mut ea = fit_energy(x, fs, a);
    let mut eb = fit_energy(x, fs, b);
    for _ in 0..50 {
        if ea > eb {
            right = b;
            b = a;
            eb = ea;
            a = right - ratio * (right - left);
            ea = fit_energy(x, fs, a);
        } else {
            left = a;
            a = b;
            ea = eb;
            b = left + ratio * (right - left);
            eb = fit_energy(x, fs, b);
        }
    }
    (left + right) / 2.0
}

/// Evidence from one complete five-second mirror window. Powers are finite
/// accumulated channel powers, not dB. Coherence never authorizes correction.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageResult {
    /// First newly covered channelizer-input sample.
    pub start_sample: u64,
    /// Exclusive end of newly covered input, without FFT overlap duplication.
    pub end_sample: u64,
    /// Strong primary and detectable weaker mirror over the median floor.
    pub suspected: bool,
    /// Normalized mirror correlation, clamped into `0..=1`.
    pub coherence: f64,
    /// Median channel power.
    pub floor_power: f64,
    /// Strongest usable channel's power.
    pub strongest_power: f64,
    /// Corresponding conjugate-mirror power.
    pub mirror_power: f64,
}
/// Hop-domain mirror evidence in disjoint five-second input intervals.
#[derive(Debug, Clone)]
pub struct ImageMonitor {
    n: usize,
    hop: u64,
    window_samples: u64,
    start: Option<u64>,
    end: u64,
    power: Vec<f64>,
    cross: Vec<Complex64>,
}
impl ImageMonitor {
    /// Geometry of the actual channelizer. Every hop is observed once.
    pub fn new(fs: f64, n: usize, hop: usize) -> Self {
        Self {
            n,
            hop: hop as u64,
            window_samples: (fs * 5.0).ceil().max(1.0) as u64,
            start: None,
            end: 0,
            power: vec![0.0; n],
            cross: vec![Complex64::new(0.0, 0.0); n],
        }
    }
    /// Observe a hop at its absolute input position. Partial windows wait.
    /// Each hop contributes only its newly covered interval to span coverage.
    pub fn observe(&mut self, x: &[Complex32], start_sample: u64) -> Option<ImageResult> {
        if x.len() != self.n || self.n < 8 {
            return None;
        }
        let first = *self.start.get_or_insert(start_sample);
        self.end = self.end.max(start_sample + self.hop);
        for (k, &sample) in x.iter().enumerate() {
            let value = wide(sample);
            self.power[k] += value.norm_sqr();
            self.cross[k] += value * wide(x[(self.n - k) % self.n]);
        }
        if self.end - first < self.window_samples {
            return None;
        }
        let eligible = |k: usize| k > 2 && k + 2 < self.n && k.abs_diff(self.n / 2) > 2;
        let strongest = (0..self.n)
            .filter(|&k| eligible(k))
            .max_by(|&a, &b| self.power[a].total_cmp(&self.power[b]));
        let mut sorted = self.power.clone();
        sorted.sort_by(f64::total_cmp);
        let floor = (sorted[(self.n - 1) / 2] + sorted[self.n / 2]) * 0.5;
        let (primary, mirror, coherence) = if let Some(k) = strongest {
            let m = (self.n - k) % self.n;
            let p = self.power[k];
            let q = self.power[m];
            let denominator = p * q;
            let coherence = if denominator > 0.0 {
                (self.cross[k].norm_sqr() / denominator).clamp(0.0, 1.0)
            } else {
                0.0
            };
            (p, q, coherence)
        } else {
            (0.0, 0.0, 0.0)
        };
        let result = ImageResult {
            start_sample: first,
            end_sample: self.end,
            suspected: primary > 0.0
                && mirror > 0.0
                && primary >= 1000.0 * floor
                && mirror >= 10f64.powf(0.6) * floor
                && mirror <= primary,
            coherence,
            floor_power: floor,
            strongest_power: primary,
            mirror_power: mirror,
        };
        self.start = None;
        self.power.fill(0.0);
        self.cross.fill(Complex64::new(0.0, 0.0));
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    fn calibration() -> IqCalibration {
        IqCalibration {
            gain_ratio: 1.05,
            phase_deg: 2.0,
            center_freq_hz: 14e6,
        }
    }
    fn impaired(s: Complex64, c: IqCalibration) -> Complex32 {
        let phi = c.phase_deg.to_radians();
        Complex32::new(
            s.re as f32,
            (c.gain_ratio * (s.im * phi.cos() + s.re * phi.sin())) as f32,
        )
    }
    fn meter(seconds: usize, c: IqCalibration, contaminate: bool, amplitude: f64) -> ToneResult {
        let fs = 4096.0;
        let mut meter = ToneMeter::new(fs, 700.0);
        for i in 0..seconds * fs as usize {
            let t = i as f64 / fs;
            let mut s = Complex64::from_polar(amplitude, TAU * 713.7 * t);
            if contaminate && i >= 2 * fs as usize && i < 3 * fs as usize {
                s += Complex64::from_polar(0.1, -TAU * 713.7 * t);
            }
            // Deterministic broadband noise, well below the 60 dB / Hz threshold.
            let hash = (i as u64)
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let noise = Complex64::new(
                ((hash >> 32) as f64 / u32::MAX as f64 - 0.5) * 1e-5,
                ((hash as u32) as f64 / u32::MAX as f64 - 0.5) * 1e-5,
            );
            meter.push(impaired(s + noise, c));
        }
        meter.result()
    }
    #[test]
    fn calibration_round_trip_and_validation() {
        for g in [0.5, 1.0, 1.05, 2.0] {
            for phi in [-45.0, 0.0, 2.0, 45.0] {
                let c = IqCalibration {
                    gain_ratio: g,
                    phase_deg: phi,
                    center_freq_hz: 14e6,
                };
                c.validate().unwrap();
                let r = IqCalibration::from_image_ratio(c.image_ratio(), 14e6).unwrap();
                assert!((r.gain_ratio - g).abs() < 1e-12);
                assert!((r.phase_deg - phi).abs() < 1e-12);
            }
        }
        for c in [
            IqCalibration {
                gain_ratio: f64::NAN,
                ..calibration()
            },
            IqCalibration {
                phase_deg: 46.0,
                ..calibration()
            },
            IqCalibration {
                center_freq_hz: 0.0,
                ..calibration()
            },
        ] {
            assert!(c.validate().is_err());
        }
        for rho in [Complex64::new(1.0, 0.0), Complex64::new(f64::NAN, 0.0)] {
            assert!(IqCorrector::new(rho).is_err());
        }
    }
    #[test]
    fn corrector_suppresses_image_and_reverts() {
        let c = calibration();
        let corrector = IqCorrector::new(c.image_ratio()).unwrap();
        let mut original = Vec::new();
        let mut corrected = Vec::new();
        let mut plus = Complex64::new(0.0, 0.0);
        let mut minus = plus;
        for i in 0..4096 {
            let phase = TAU * 711.0 * i as f64 / 4096.0;
            let x = impaired(Complex64::from_polar(0.5, phase), c);
            let y = corrector.process_sample(x);
            plus += wide(y) * Complex64::from_polar(1.0, -phase);
            minus += wide(y) * Complex64::from_polar(1.0, phase);
            original.push(x);
            corrected.push(y);
        }
        assert!(20.0 * (plus.norm() / minus.norm()).log10() > 80.0);
        revert_iq(&mut corrected, c.image_ratio()).unwrap();
        for (a, b) in original.iter().zip(corrected) {
            assert!((*a - b).norm() <= 1e-6 * a.norm() + 1e-8);
        }
    }
    #[test]
    fn tone_meter_refines_off_bin_tone_and_measures_rho() {
        let ToneResult::Accepted {
            offset_hz,
            rho,
            snr_1hz_db,
            segments,
            ..
        } = meter(5, calibration(), false, 0.5)
        else {
            panic!("tone rejected")
        };
        assert!((offset_hz - 713.7).abs() < 1e-4, "{offset_hz}");
        assert!((rho - calibration().image_ratio()).norm() < 1e-5, "{rho:?}");
        assert!(snr_1hz_db >= 60.0);
        assert_eq!(segments, 5);
    }
    #[test]
    fn tone_meter_balanced_short_missing_and_contaminated() {
        let balanced = IqCalibration {
            gain_ratio: 1.0,
            phase_deg: 0.0,
            ..calibration()
        };
        let ToneResult::Accepted {
            rho,
            balanced: is_balanced,
            ..
        } = meter(5, balanced, false, 0.5)
        else {
            panic!("balanced rejected")
        };
        assert!(image_rejection_db(rho) >= 60.0);
        assert!(is_balanced);
        assert_eq!(
            meter(4, calibration(), false, 0.5),
            ToneResult::Rejected(ToneRejection::TooShort)
        );
        assert_eq!(
            meter(5, calibration(), false, 0.0),
            ToneResult::Rejected(ToneRejection::NotFound)
        );
        assert_eq!(
            meter(5, calibration(), false, 1e-7),
            ToneResult::Rejected(ToneRejection::NotFound)
        );
        assert_eq!(
            meter(5, calibration(), true, 0.5),
            ToneResult::Rejected(ToneRejection::Inconsistent)
        );
    }
    #[test]
    fn tone_meter_measures_a_negative_offset() {
        let mut meter = ToneMeter::new(4096.0, -700.0);
        for i in 0..4096 * 5 {
            meter.push(impaired(
                Complex64::from_polar(0.5, -TAU * 713.7 * i as f64 / 4096.0),
                calibration(),
            ));
        }
        let ToneResult::Accepted { offset_hz, rho, .. } = meter.result() else {
            panic!("negative offset rejected")
        };
        assert!((offset_hz + 713.7).abs() < 1e-4);
        assert!((rho - calibration().image_ratio()).norm() < 1e-5);
    }
    #[test]
    fn tone_meter_refuses_dc_nyquist_and_aliased_probes() {
        for declared in [0.0, 100.0, 1700.0, 2048.0, f64::NAN] {
            let mut meter = ToneMeter::new(4096.0, declared);
            for _ in 0..4096 * 5 {
                meter.push(Complex32::new(1.0, 0.0));
            }
            assert_eq!(
                meter.result(),
                ToneResult::Rejected(ToneRejection::NotFound)
            );
            assert!(meter.first.is_empty());
        }
    }
    #[test]
    fn tone_meter_releases_first_capture_and_bounds_later_storage() {
        let mut meter = ToneMeter::new(4096.0, 700.0);
        for i in 0..4096 * 12 {
            meter.push(Complex32::from_polar(
                0.5,
                (TAU * 713.7 * i as f64 / 4096.0) as f32,
            ));
            assert!(meter.first.len() <= 4096);
            if i >= 4096 {
                assert_eq!(meter.first.capacity(), 0);
            }
        }
        assert_eq!(meter.segments, 12);
    }
    #[test]
    fn monitor_waits_for_full_window_and_handles_silence() {
        let mut m = ImageMonitor::new(128.0, 16, 4);
        let x = vec![Complex32::new(0.0, 0.0); 16];
        for i in 0..159 {
            assert!(m.observe(&x, i * 4).is_none());
        }
        let r = m.observe(&x, 636).unwrap();
        assert!(!r.suspected);
        assert_eq!(r.coherence, 0.0);
        assert_eq!((r.start_sample, r.end_sample), (0, 640));
    }
    #[test]
    fn monitor_reports_mirrored_stations_without_using_coherence_as_a_gate() {
        let mut m = ImageMonitor::new(128.0, 16, 4);
        let mut last = None;
        for i in 0..160 {
            let mut x = vec![Complex32::new(0.01, 0.0); 16];
            x[4] = Complex32::new(1.0, 0.0);
            x[12] = Complex32::from_polar(0.1, (i as f32) * std::f32::consts::PI / 2.0);
            last = m.observe(&x, i * 4);
        }
        let r = last.unwrap();
        assert!(r.suspected);
        assert!(r.coherence < 0.001);
    }
}
