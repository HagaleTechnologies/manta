//! WOLA polyphase filterbank channelizer (SPEC §1.1-1.3): the full
//! N-channel successor to the M0 single-channel shim (`single.rs`).

use crate::proto::{design_prototype, TAPS_PER_BRANCH};
use crate::sample_guard;
use coppa_dsp::fft::FftProcessor;
use num_complex::{Complex32, Complex64};

pub const CHANNEL_SPACING_HZ: f64 = 93.75; // SPEC §1.1
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

/// SPEC §1.3 step 2's circular rotation (`r = (m*hop) mod N`, `hop = N/4`)
/// leaves a residual checkerboard sign artifact on `HopOutput::x[k]`: odd
/// channels at odd hops come out negated relative to what a phase-
/// continuous baseband signal would read, `(-1)^(m*k)`. Magnitude-only
/// consumers (`power = |X[k]|^2`, everything before MAN-168) are
/// invariant to this and never needed to correct it. A consumer working
/// with the complex spectrum directly (MAN-168's narrowband refiner,
/// which derotates and lowpass-filters `x[k]` and so needs a genuinely
/// phase-continuous input) must multiply `x[k]` by this factor first.
///
/// Found by Codex review on PR #161's MAN-168 wiring (a probe using the
/// real channelizer + refiner measured ~39 dB of spurious attenuation on
/// one specific odd channel). The exact form here was verified
/// empirically, not hand-derived from the rotation step's DFT shift
/// theorem (an initial `exp(-j*2*pi*k*r/N)` derivation, mathematically
/// plausible for an abstract circular shift, measured identical -- and
/// still wrong -- results for both candidate signs; the real artifact
/// turned out to be this much simpler period-2 real sign flip). Confirmed
/// against both 96 kHz/1024-channel and 192 kHz/2048-channel
/// configurations, multiple even/odd channel pairs, and both a centered
/// and a fractionally-offset tone: after this correction, an odd
/// channel's refined output exactly matches an even channel's.
pub fn odd_channel_sign_correction(hop_m: u64, k: usize) -> f32 {
    if hop_m % 2 == 1 && k % 2 == 1 {
        -1.0
    } else {
        1.0
    }
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

/// hop = N/4 (SPEC §1.1): a 2π phase advance per hop spans 4 channels on
/// every channel table.
pub const CHANNELS_PER_PHASE_CYCLE: f64 = 4.0;

/// Lag-1 phase product of channel `k` between consecutive hops (SPEC §1.4,
/// MAN-114), rotated into the absolute frame so products from different
/// channels of one track can be averaged: its argument is
/// `2π·p/4 (mod 2π)` for a tone at channel position `p`. Both hops get
/// `odd_channel_sign_correction`. `None` unless `cur.m == prev.m + 1`, both
/// spectra are `n` long (power-only hops carry an empty `x`) and `k < n`.
///
/// The rotation `e^{jπ(k mod 4)/2}` undoes channel `k`'s downconversion:
/// the corrected output of channel `k` advances `2π·(p − k)/4` per hop.
/// It holds for upper-half (negative-frequency) channels too, because N
/// is a multiple of 4.
pub fn phase_lag_product(prev: &HopOutput, cur: &HopOutput, k: usize) -> Option<Complex64> {
    let n = cur.power.len();
    if prev.m.checked_add(1) != Some(cur.m) || cur.x.len() != n || prev.x.len() != n || k >= n {
        return None;
    }
    let a = cur.x[k] * odd_channel_sign_correction(cur.m, k);
    let b = prev.x[k] * odd_channel_sign_correction(prev.m, k);
    let z = a * b.conj();
    let z = Complex64::new(z.re as f64, z.im as f64);
    Some(z * Complex64::from_polar(1.0, std::f64::consts::FRAC_PI_2 * (k % 4) as f64))
}

/// Channel position of an absolute-frame phase sum (`phase_lag_product`
/// accumulated): `arg(z)/2π·4`, unwrapped to the value nearest `near`
/// (a track's `center`). `None` for a zero sum. SPEC §1.4.
pub fn phase_position(z: Complex64, near: f64) -> Option<f64> {
    if z.re == 0.0 && z.im == 0.0 {
        return None;
    }
    let p = z.arg() / std::f64::consts::TAU * CHANNELS_PER_PHASE_CYCLE;
    Some(p + CHANNELS_PER_PHASE_CYCLE * ((near - p) / CHANNELS_PER_PHASE_CYCLE).round())
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
    /// MAN-234: input samples `process` replaced with silence since
    /// construction.
    bad_samples: u64,
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
            bad_samples: 0,
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

    /// Input samples `process` has replaced with silence since construction
    /// because a component was NaN, infinite or beyond
    /// `sample_guard::MAX_SAMPLE_MAGNITUDE`. SPEC §1.3 step 0 (MAN-234).
    pub fn bad_samples(&self) -> u64 {
        self.bad_samples
    }

    /// Feed input IQ; returns however many hops became available. SPEC §1.3.
    /// Step 0 (MAN-234): an unusable sample (`sample_guard::is_usable`) is
    /// replaced with `0+0j` as it enters the window and counted in
    /// `bad_samples`; usable samples are never written.
    pub fn process(&mut self, iq: &[Complex32]) -> Vec<HopOutput> {
        let start = self.buf.len();
        self.buf.extend_from_slice(iq);
        self.bad_samples += sample_guard::replace_unusable(&mut self.buf[start..]);
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

    fn assert_hops_bitwise_equal(a: &[HopOutput], b: &[HopOutput]) {
        assert_eq!(a.len(), b.len());
        for (ha, hb) in a.iter().zip(b.iter()) {
            assert_eq!(ha.m, hb.m);
            for (xa, xb) in ha.x.iter().zip(hb.x.iter()) {
                assert_eq!(xa.re.to_bits(), xb.re.to_bits());
                assert_eq!(xa.im.to_bits(), xb.im.to_bits());
            }
            for (pa, pb) in ha.power.iter().zip(hb.power.iter()) {
                assert_eq!(pa.to_bits(), pb.to_bits());
            }
        }
    }

    #[test]
    fn a_bad_sample_is_replaced_before_it_reaches_any_hop() {
        // MAN-234: one NaN mid-stream must not reach channel power.
        let mut iq = tone(6_000.0, 20_000, 1.0, FS);
        iq[9_000].re = f32::NAN;
        let mut ch = Channelizer::new(FS, 0.0).unwrap();
        let hops = ch.process(&iq);
        assert!(!hops.is_empty());
        for hop in &hops {
            assert!(hop.power.iter().all(|p| p.is_finite()), "hop {}", hop.m);
        }
        assert_eq!(ch.bad_samples(), 1);
    }

    #[test]
    fn a_bad_sample_produces_the_same_hops_as_silence() {
        // MAN-234 D3: the replacement is exactly `0+0j`, so a bad sample's
        // hops equal those of the same input with that sample zeroed.
        let clean = tone(6_000.0, 20_000, 1.0, FS);
        let mut zeroed = clean.clone();
        zeroed[9_000] = Complex32::new(0.0, 0.0);
        for bad in [f32::INFINITY, f32::MAX] {
            let mut corrupt = clean.clone();
            corrupt[9_000].re = bad;
            let mut ch_bad = Channelizer::new(FS, 0.0).unwrap();
            let mut ch_zero = Channelizer::new(FS, 0.0).unwrap();
            let hops_bad = ch_bad.process(&corrupt);
            let hops_zero = ch_zero.process(&zeroed);
            assert_hops_bitwise_equal(&hops_bad, &hops_zero);
            assert_eq!(ch_bad.bad_samples(), 1);
            assert_eq!(ch_zero.bad_samples(), 0);
        }
    }

    #[test]
    fn bad_samples_accumulate_across_process_calls() {
        let mut a = tone(6_000.0, 5_000, 1.0, FS);
        let mut b = tone(6_000.0, 5_000, 1.0, FS);
        a[100].im = f32::NAN;
        b[4_000].re = f32::NEG_INFINITY;
        let mut ch = Channelizer::new(FS, 0.0).unwrap();
        ch.process(&a);
        assert_eq!(ch.bad_samples(), 1);
        ch.process(&b);
        assert_eq!(ch.bad_samples(), 2);

        // One bad sample whose chunk is split across a call boundary is
        // counted once, and the samples already in the window are not
        // re-checked or re-counted.
        let mut whole = tone(6_000.0, 20_000, 1.0, FS);
        whole[10_000].re = f32::NAN;
        let mut split = Channelizer::new(FS, 0.0).unwrap();
        split.process(&whole[..9_999]);
        split.process(&whole[9_999..10_002]);
        split.process(&whole[10_002..]);
        assert_eq!(split.bad_samples(), 1);
    }

    #[test]
    fn full_scale_bound_keeps_every_power_finite() {
        // MAN-234 D2: at the bound, DC (coherent worst case) and
        // alternating-sign input keep every channel power finite.
        let b = sample_guard::MAX_SAMPLE_MAGNITUDE;
        let mut ch = Channelizer::new(FS, 0.0).unwrap();
        let len = ch.filter_len() * 2;
        let dc = vec![Complex32::new(b, b); len];
        let alt: Vec<Complex32> = (0..len)
            .map(|i| {
                let v = if i % 2 == 0 { b } else { -b };
                Complex32::new(v, -v)
            })
            .collect();
        let mut hops = ch.process(&dc);
        hops.extend(ch.process(&alt));
        assert!(!hops.is_empty());
        for hop in &hops {
            assert!(hop.power.iter().all(|p| p.is_finite()), "hop {}", hop.m);
        }
        assert_eq!(ch.bad_samples(), 0);
    }

    /// MAN-114: a clean tone at `freq_hz`, channelized at `fs`; returns the
    /// `phase_position` (unwrapped near `k0`) of the summed
    /// `phase_lag_product` of each post-warm-up hop's strongest channel
    /// among `k0 - 1..=k0 + 1`.
    fn phase_position_of_tone(fs: f64, freq_hz: f64, k0: usize) -> f64 {
        let mut ch = Channelizer::new(fs, 0.0).unwrap();
        let n = ch.n_channels();
        let iq = tone(freq_hz, 2 * ch.filter_len() + 64 * ch.hop(), 1.0, fs);
        let hops = ch.process(&iq);
        let warmup = ch.filter_len() / ch.hop() + 1;
        let mut z = Complex64::new(0.0, 0.0);
        for pair in hops[warmup..].windows(2) {
            let k = [(k0 + n - 1) % n, k0, (k0 + 1) % n]
                .into_iter()
                .max_by(|&a, &b| pair[1].power[a].total_cmp(&pair[1].power[b]))
                .unwrap();
            z += phase_lag_product(&pair[0], &pair[1], k).expect("consecutive full hops");
        }
        phase_position(z, k0 as f64).expect("a tone gives a non-zero sum")
    }

    #[test]
    fn phase_position_is_exact_for_a_clean_tone_across_the_channel() {
        // MAN-114: the dB parabola's S-curve bias (up to ~12 Hz on a clean
        // tone) is absent from the phase estimator on every table, on even
        // and odd channels, at every fractional offset.
        for fs in [48_000.0, 96_000.0, 192_000.0] {
            for k0 in [64usize, 65] {
                for step in 0..=20 {
                    let frac = -0.5 + 0.05 * step as f64;
                    let truth = k0 as f64 + frac;
                    let got = phase_position_of_tone(fs, truth * CHANNEL_SPACING_HZ, k0);
                    let err_hz = (got - truth) * CHANNEL_SPACING_HZ;
                    assert!(
                        err_hz.abs() < 0.05,
                        "fs {fs} k0 {k0} frac {frac:.2}: phase position {got} vs {truth} \
                         ({err_hz:+.4} Hz)"
                    );
                }
            }
        }
    }

    #[test]
    fn phase_position_handles_negative_frequency_channels() {
        // -6000 Hz is channel N-64 (SPEC §1.1's f(k)); 0.3 channel below it.
        let n = (FS / CHANNEL_SPACING_HZ) as usize;
        let k0 = n - 64;
        let truth = k0 as f64 - 0.3;
        let got = phase_position_of_tone(FS, -6_000.0 - 0.3 * CHANNEL_SPACING_HZ, k0);
        let err_hz = (got - truth) * CHANNEL_SPACING_HZ;
        assert!(
            err_hz.abs() < 0.05,
            "phase position {got} vs {truth} ({err_hz:+.4} Hz)"
        );
    }

    #[test]
    fn phase_lag_product_none_on_hop_gap_or_power_only_hop() {
        let full = |m: u64| HopOutput {
            m,
            x: vec![Complex32::new(1.0, 0.5); 8],
            power: vec![1.25; 8],
        };
        assert!(phase_lag_product(&full(4), &full(5), 3).is_some());
        // Not consecutive: a hop gap (or a reset stream) breaks the lag.
        assert_eq!(phase_lag_product(&full(4), &full(6), 3), None);
        assert_eq!(phase_lag_product(&full(5), &full(5), 3), None);
        // Power-only hops (an empty `x`) carry no phase.
        let power_only = |m: u64| HopOutput {
            m,
            x: Vec::new(),
            power: vec![1.25; 8],
        };
        assert_eq!(phase_lag_product(&full(4), &power_only(5), 3), None);
        assert_eq!(phase_lag_product(&power_only(4), &full(5), 3), None);
        // Out-of-range channel.
        assert_eq!(phase_lag_product(&full(4), &full(5), 8), None);
    }

    #[test]
    fn phase_position_unwraps_near_reference() {
        // arg = 2π·0.25 is position 1.0 (mod 4); nearest 9.1 is 9.0.
        let z = Complex64::from_polar(2.5, std::f64::consts::TAU * 0.25);
        let p = phase_position(z, 9.1).unwrap();
        assert!((p - 9.0).abs() < 1e-12, "got {p}");
        // Near a negative-side reference too: nearest 1022.6 is 1021.0
        // (1021 ≡ 1 mod 4) rather than 1025.0.
        let p = phase_position(z, 1022.6).unwrap();
        assert!((p - 1021.0).abs() < 1e-9, "got {p}");
        assert_eq!(phase_position(Complex64::new(0.0, 0.0), 9.1), None);
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

    #[test]
    fn odd_channel_sign_correction_recovers_a_phase_continuous_odd_channel_tone() {
        // Codex review, PR #161's MAN-168: a centered tone on an odd
        // channel comes out of the channelizer with a spurious per-hop
        // sign flip (`(-1)^(m*k)`) that a magnitude-only consumer never
        // noticed; a consumer using the complex spectrum directly (the
        // narrowband refiner) sees severe self-cancellation without this
        // correction. Verify the corrected odd channel matches an even
        // channel's amplitude exactly, using the real channelizer.
        use crate::refine::Refiner;
        let fs = 96_000.0;
        let mean = |k: usize, corrected: bool| -> f32 {
            let mut ch = Channelizer::new(fs, 0.0).unwrap();
            let freq_hz = ch.channel_freq_hz(k);
            let sig = tone(freq_hz, 60_000, 1.0, fs);
            let hops = ch.process(&sig);
            let mut r = Refiner::new(30.0, 0.0);
            let mut sum = 0.0f32;
            let mut count = 0u32;
            for h in hops.iter().skip(20) {
                let x = if corrected {
                    h.x[k] * odd_channel_sign_correction(h.m, k)
                } else {
                    h.x[k]
                };
                sum += r.push(x);
                count += 1;
            }
            sum / count as f32
        };
        let even_amp = mean(64, true);
        let odd_uncorrected = mean(65, false);
        let odd_corrected = mean(65, true);
        assert!(
            odd_uncorrected < even_amp * 0.5,
            "sanity: an uncorrected odd channel must show real self-cancellation, got \
             odd={odd_uncorrected} even={even_amp}"
        );
        assert!(
            (odd_corrected - even_amp).abs() < 1e-3,
            "corrected odd channel must match the even channel's amplitude exactly, got \
             odd_corrected={odd_corrected} even={even_amp}"
        );
    }
}
