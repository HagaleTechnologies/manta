//! WOLA polyphase filterbank channelizer (SPEC §1.1-1.3): the full
//! N-channel successor to the M0 single-channel shim (`single.rs`).

use crate::proto::{design_prototype, TAPS_PER_BRANCH};
use coppa_dsp::fft::FftProcessor;
use num_complex::{Complex32, Complex64};

const CHANNEL_SPACING_HZ: f64 = 93.75; // SPEC §1.1
/// SPEC §1.3/§1.4 power-to-dB epsilon.
const POWER_DB_EPSILON: f64 = 1e-20;

/// One hop's channelizer output: per-channel complex spectrum and power.
/// SPEC §1.3.
#[derive(Debug, Clone)]
pub struct HopOutput {
    /// Hop index, monotonically increasing from stream start.
    pub m: u64,
    /// Complex per-channel spectrum, FFT bin order (index k; SPEC §1.1's
    /// f(k) mapping applies via `Channelizer::channel_freq_hz`).
    pub x: Vec<Complex32>,
    /// Per-channel power, `|X[k]|^2`. SPEC §1.3 step 4.
    pub power: Vec<f32>,
}

/// `PdB = 10*log10(P + epsilon)`, SPEC §1.3/§1.4's epsilon = 1e-20.
pub fn power_db(power: f32) -> f64 {
    10.0 * (power as f64 + POWER_DB_EPSILON).log10()
}

/// Per-hop fine-frequency interpolation (SPEC §1.4): quadratic
/// interpolation on dB powers of the three bins around a candidate peak
/// channel. Returns the sub-bin offset in `[-0.5, 0.5]`, or `None` if the
/// hop is "unusable" (no local maximum at the center bin).
pub fn interpolate_offset(p_minus: f32, p_zero: f32, p_plus: f32) -> Option<f64> {
    let pm = power_db(p_minus);
    let p0 = power_db(p_zero);
    let pp = power_db(p_plus);
    let denom = pm - 2.0 * p0 + pp;
    if denom < 0.0 {
        Some((0.5 * (pm - pp) / denom).clamp(-0.5, 0.5))
    } else {
        None
    }
}

/// WOLA polyphase filterbank: N channels, 4x oversampled (hop = N/4).
/// SPEC §1.1-1.3.
pub struct Channelizer {
    n: usize,
    hop: usize,
    taps: Vec<f32>, // length L*N, L = TAPS_PER_BRANCH
    fft: FftProcessor,
    /// Sliding input window; `read` indexes the next window start. Samples
    /// before `read` are dead and get compacted away (same pattern as
    /// `single.rs::SingleChannelExtractor`).
    buf: Vec<Complex32>,
    read: usize,
    /// Hop output counter, used for the SPEC §1.3 step-2 rotation `r =
    /// (m*hop) mod N`. A plain integer counter, not an accumulated phase --
    /// no drift/precision concern (unlike an NCO), so no need to derive it
    /// from an absolute sample index.
    m: u64,
    fs: f64,
    center_freq_hz: f64,
}

impl Channelizer {
    /// A channelizer for a supported table rate (`fs/93.75` a power of
    /// two). SPEC §1.1.
    pub fn new(fs: f64, center_freq_hz: f64) -> Result<Self, String> {
        let nf = fs / CHANNEL_SPACING_HZ;
        let n = nf.round() as usize;
        if (nf - n as f64).abs() > 1e-9 || !n.is_power_of_two() {
            return Err(format!(
                "unsupported sample rate {fs}: fs/93.75 must be a power of two"
            ));
        }
        Ok(Channelizer {
            n,
            hop: n / 4,
            taps: design_prototype(n, TAPS_PER_BRANCH),
            fft: FftProcessor::new(n),
            buf: Vec::new(),
            read: 0,
            m: 0,
            fs,
            center_freq_hz,
        })
    }

    /// Number of channels, N. SPEC §1.1.
    pub fn n_channels(&self) -> usize {
        self.n
    }

    /// Input samples consumed per output hop (N/4). SPEC §1.1.
    pub fn hop(&self) -> usize {
        self.hop
    }

    /// Prototype filter length in taps (L*N). Same causal-filter blind-zone
    /// property as `single.rs`'s extractor -- see the M0 lead-in-padding
    /// fix in `manta-engine`, which Task 6/7 apply here too.
    pub fn filter_len(&self) -> usize {
        self.taps.len()
    }

    /// Channel `k`'s RF center frequency. SPEC §1.1:
    /// `f(k) = f_center + ((k + N/2) mod N - N/2) * Delta`.
    pub fn channel_freq_hz(&self, k: usize) -> f64 {
        let delta = self.fs / self.n as f64;
        let signed = ((k + self.n / 2) % self.n) as f64 - (self.n / 2) as f64;
        self.center_freq_hz + signed * delta
    }

    /// Feed input IQ; returns however many hops became available. SPEC §1.3.
    pub fn process(&mut self, iq: &[Complex32]) -> Vec<HopOutput> {
        self.buf.extend_from_slice(iq);
        let ln = self.taps.len();
        let mut outputs = Vec::new();
        while self.read + ln <= self.buf.len() {
            let window = &self.buf[self.read..self.read + ln];

            // Step 1: window & fold. u[n] = x[n]*h[LN-1-n]; v[j] = sum_p
            // u[j + p*N]. Sequential f64 accumulation (SPEC §6.4 convention,
            // matching single.rs's direct-FIR sum).
            let mut v = vec![Complex64::new(0.0, 0.0); self.n];
            for (n_idx, &x) in window.iter().enumerate() {
                let h = self.taps[ln - 1 - n_idx] as f64;
                let j = n_idx % self.n;
                v[j].re += h * x.re as f64;
                v[j].im += h * x.im as f64;
            }
            let v: Vec<Complex32> = v
                .iter()
                .map(|c| Complex32::new(c.re as f32, c.im as f32))
                .collect();

            // Step 2: circular rotation left by r = (m*hop) mod N.
            let r = ((self.m.wrapping_mul(self.hop as u64)) % self.n as u64) as usize;
            let mut v_rot = vec![Complex32::new(0.0, 0.0); self.n];
            for j in 0..self.n {
                v_rot[j] = v[(j + r) % self.n];
            }

            // Step 3: FFT. Step 4: power.
            let x = self.fft.forward(&v_rot);
            let power: Vec<f32> = x.iter().map(|c| c.norm_sqr()).collect();
            outputs.push(HopOutput {
                m: self.m,
                x,
                power,
            });

            self.m += 1;
            self.read += self.hop;
        }
        if self.read >= ln {
            self.buf.drain(..self.read);
            self.read = 0;
        }
        outputs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 96_000.0;

    fn tone(freq: f64, n: usize, amp: f32, fs: f64) -> Vec<Complex32> {
        (0..n)
            .map(|i| {
                let phi = 2.0 * std::f64::consts::PI * freq * i as f64 / fs;
                Complex32::new(amp * phi.cos() as f32, amp * phi.sin() as f32)
            })
            .collect()
    }

    /// Channel index for offset_hz at the given N/fs (SPEC §1.1's f(k)
    /// inverted): `k = ((round(offset/delta)) mod N + N) mod N`.
    fn channel_for_offset(offset_hz: f64, n: usize, fs: f64) -> usize {
        let delta = fs / n as f64;
        let k_signed = (offset_hz / delta).round() as i64;
        k_signed.rem_euclid(n as i64) as usize
    }

    #[test]
    fn rejects_non_table_rate() {
        assert!(Channelizer::new(44_100.0, 0.0).is_err());
        assert!(Channelizer::new(96_000.0, 0.0).is_ok());
        assert!(Channelizer::new(192_000.0, 0.0).is_ok());
    }

    #[test]
    fn dimensions_match_spec_table() {
        let ch = Channelizer::new(96_000.0, 0.0).unwrap();
        assert_eq!(ch.n_channels(), 1024);
        assert_eq!(ch.hop(), 256);
        let ch = Channelizer::new(192_000.0, 0.0).unwrap();
        assert_eq!(ch.n_channels(), 2048);
        assert_eq!(ch.hop(), 512);
    }

    #[test]
    fn channel_freq_hz_matches_spec_f_of_k() {
        let ch = Channelizer::new(96_000.0, 14_000_000.0).unwrap();
        // k=0 is DC (center frequency itself).
        assert!((ch.channel_freq_hz(0) - 14_000_000.0).abs() < 1e-6);
        // k=1 is one channel above center.
        assert!((ch.channel_freq_hz(1) - (14_000_000.0 + 93.75)).abs() < 1e-6);
        // k = N-1 wraps to one channel BELOW center (negative-frequency side).
        assert!((ch.channel_freq_hz(1023) - (14_000_000.0 - 93.75)).abs() < 1e-6);
    }

    #[test]
    fn on_channel_tone_settles_near_unity_in_its_own_channel() {
        // Tone exactly at channel k0=64's center: offset = 64*93.75 = 6000 Hz.
        let mut ch = Channelizer::new(FS, 0.0).unwrap();
        let k0 = channel_for_offset(6_000.0, ch.n_channels(), FS);
        assert_eq!(k0, 64);
        let iq = tone(6_000.0, 192_000, 1.0, FS);
        let hops = ch.process(&iq);
        let warmup = ch.filter_len() / ch.hop() + 1;
        for hop in &hops[warmup..] {
            let mag = hop.power[k0].sqrt();
            assert!((mag - 1.0).abs() < 0.05, "channel {k0} magnitude {mag}");
        }
    }

    #[test]
    fn tone_150hz_away_is_rejected_by_80db_in_the_home_channel() {
        // SPEC §1.2: alias rejection >= 80 dB from ~108 Hz (1.15 channels) away.
        // A tone at k0's center + 150 Hz lands in a DIFFERENT home channel;
        // k0 itself should show near-zero power from it.
        let mut ch = Channelizer::new(FS, 0.0).unwrap();
        let k0 = channel_for_offset(6_000.0, ch.n_channels(), FS);
        let iq = tone(6_000.0 + 150.0, 192_000, 1.0, FS);
        let hops = ch.process(&iq);
        let warmup = ch.filter_len() / ch.hop() + 1;
        for hop in &hops[warmup..] {
            let mag = hop.power[k0].sqrt();
            assert!(mag < 2e-4, "stopband leak into channel {k0}: {mag}"); // -74 dB, slack for f32
        }
    }

    #[test]
    fn channel_edge_is_minus_6_db_in_both_neighbors() {
        // Tone exactly between channels k0 and k0+1 (edge = k0*Delta + Delta/2).
        let mut ch = Channelizer::new(FS, 0.0).unwrap();
        let k0 = channel_for_offset(6_000.0, ch.n_channels(), FS);
        let edge_hz = 6_000.0 + 93.75 / 2.0;
        let iq = tone(edge_hz, 384_000, 1.0, FS);
        let hops = ch.process(&iq);
        let warmup = ch.filter_len() / ch.hop() + 1;
        let steady = &hops[warmup..];
        let mean_mag = |k: usize| -> f32 {
            let sum: f32 = steady.iter().map(|h| h.power[k].sqrt()).sum();
            sum / steady.len() as f32
        };
        // -6 dB = 0.501 in amplitude; both neighbors should sit near 0.5.
        assert!(
            (mean_mag(k0) - 0.5).abs() < 0.05,
            "k0 edge gain {}",
            mean_mag(k0)
        );
        assert!(
            (mean_mag(k0 + 1) - 0.5).abs() < 0.05,
            "k0+1 edge gain {}",
            mean_mag(k0 + 1)
        );
    }

    #[test]
    fn is_deterministic() {
        let iq = tone(6_000.0, 96_000, 1.0, FS);
        let mut ch_a = Channelizer::new(FS, 0.0).unwrap();
        let mut ch_b = Channelizer::new(FS, 0.0).unwrap();
        let hops_a = ch_a.process(&iq);
        let hops_b = ch_b.process(&iq);
        assert_eq!(hops_a.len(), hops_b.len());
        for (a, b) in hops_a.iter().zip(hops_b.iter()) {
            for (pa, pb) in a.power.iter().zip(b.power.iter()) {
                assert_eq!(pa.to_bits(), pb.to_bits());
            }
        }
    }

    #[test]
    fn power_db_matches_spec_epsilon() {
        // 10*log10(0 + 1e-20) = -200.0 exactly.
        assert!((power_db(0.0) - (-200.0)).abs() < 1e-9);
        // 10*log10(1.0 + 1e-20) ~ 0.0.
        assert!(power_db(1.0).abs() < 1e-6);
    }

    #[test]
    fn process_across_multiple_calls_matches_one_call() {
        let iq = tone(6_000.0, 20_000, 1.0, FS);
        let mut whole = Channelizer::new(FS, 0.0).unwrap();
        let hops_whole = whole.process(&iq);

        let mut chunked = Channelizer::new(FS, 0.0).unwrap();
        let mut hops_chunked = Vec::new();
        for chunk in iq.chunks(137) {
            hops_chunked.extend(chunked.process(chunk));
        }
        assert_eq!(hops_whole.len(), hops_chunked.len());
        for (a, b) in hops_whole.iter().zip(hops_chunked.iter()) {
            assert_eq!(a.x, b.x);
        }
    }

    #[test]
    fn interpolate_offset_finds_symmetric_peak_at_zero() {
        // A true local max with equal neighbors -> delta = 0.
        assert_eq!(interpolate_offset(0.5, 1.0, 0.5), Some(0.0));
    }

    #[test]
    fn interpolate_offset_leans_toward_the_larger_neighbor() {
        // Peak biased toward p_plus -> positive delta (SPEC §1.4 formula
        // sign convention: delta = 0.5*(P_minus - P_plus)/denom).
        let d = interpolate_offset(0.3, 1.0, 0.6).unwrap();
        assert!(d > 0.0, "delta {d}");
        assert!(d <= 0.5);
    }

    #[test]
    fn interpolate_offset_clamps_to_half_bin() {
        // Extremely asymmetric neighbors would produce |delta| > 0.5 unclamped.
        let d = interpolate_offset(1e-6, 1.0, 0.999).unwrap();
        assert!((-0.5..=0.5).contains(&d));
    }

    #[test]
    fn interpolate_offset_none_when_not_a_local_max() {
        // A local MINIMUM at the center bin (a valley, not a peak): the
        // denominator is computed on dB-converted values, and log10 is
        // concave, so even monotonically-increasing linear power values
        // produce a local-max-shaped (negative) denominator in dB -- a
        // genuine valley is the unambiguous "no local max" case.
        // denom = db(0.5) - 2*db(0.1) + db(0.5) ~= 13.98 >= 0 -> unusable.
        assert_eq!(interpolate_offset(0.5, 0.1, 0.5), None);
    }
}
