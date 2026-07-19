//! **Deprecated** as of M2 sub-project 1 (`skimmer-dsp::channelizer`
//! implements the real WOLA polyphase filterbank, SPEC §1.3). Kept
//! compiled and tested for now as a reference/fallback -- not wired into
//! `skimmer-engine` anymore as of that sub-project. Candidate for removal
//! once the channelizer path has run cleanly for a few months; see
//! `docs/DECISIONS/2026-07-18-m2-pfb-channelizer-pins.md`.
//!
//! Single-channel extractor: one PFB channel computed directly (M0 shim).
//! Mix by -offset, prototype lowpass, decimate by N/4 to 375 Hz.

use crate::proto::{design_prototype, TAPS_PER_BRANCH};
use num_complex::Complex32;

const CHANNEL_SPACING_HZ: f64 = 93.75; // SPEC §1.1

/// One PFB channel computed directly: mix + prototype FIR + decimate to
/// 375 Hz. M0 shim, superseded by the full PFB at M2. SPEC §1.1, §1.3.
pub struct SingleChannelExtractor {
    taps: Vec<f32>,
    hop: usize,
    fs: f64,
    offset_hz: f64,
    /// Mixed samples not yet consumed; `read` indexes the next window start.
    /// Samples before `read` are dead and get compacted away.
    buf: Vec<Complex32>,
    read: usize,
    /// Total input samples seen (NCO phase reference).
    n_in: u64,
}

impl SingleChannelExtractor {
    /// A channel extractor centered at `offset_hz` from the input's DC, for
    /// a supported table rate (`fs/93.75` a power of two). SPEC §1.1.
    pub fn new(fs: f64, offset_hz: f64) -> Result<Self, String> {
        let nf = fs / CHANNEL_SPACING_HZ;
        let n = nf.round() as usize;
        if (nf - n as f64).abs() > 1e-9 || !n.is_power_of_two() {
            return Err(format!(
                "unsupported sample rate {fs}: fs/93.75 must be a power of two"
            ));
        }
        Ok(SingleChannelExtractor {
            taps: design_prototype(n, TAPS_PER_BRANCH),
            hop: n / 4,
            fs,
            offset_hz,
            buf: Vec::new(),
            read: 0,
            n_in: 0,
        })
    }

    /// Input samples consumed per output sample (N/4). SPEC §1.1.
    pub fn hop(&self) -> usize {
        self.hop
    }

    /// Prototype filter length in taps (L*N). SPEC §1.2. A causal FIR filter of
    /// this length has no valid output representing a true signal instant
    /// earlier than `(filter_len()-1)/2` input samples into a recording with no
    /// prior history — see the M0 lead-in-padding fix in skimmer-engine.
    pub fn filter_len(&self) -> usize {
        self.taps.len()
    }

    /// Feed input IQ; returns however many 375 Hz channel samples became
    /// available. SPEC §1.3.
    pub fn process(&mut self, iq: &[Complex32]) -> Vec<Complex32> {
        // Mix to baseband. Phase from the absolute sample index in f64:
        // deterministic, no recurrence drift.
        self.buf.reserve(iq.len());
        for (k, s) in iq.iter().enumerate() {
            let n = (self.n_in + k as u64) as f64;
            let phi = -2.0 * std::f64::consts::PI * self.offset_hz * n / self.fs;
            let (sin, cos) = phi.sin_cos();
            let m = Complex32::new(cos as f32, sin as f32);
            self.buf.push(s * m);
        }
        self.n_in += iq.len() as u64;

        let ln = self.taps.len();
        let mut out = Vec::new();
        // Output y[t] = sum_i h[i] * x[t - i]; window [read, read+ln) with the
        // newest sample at the window end. Sequential f64 accumulation (SPEC §6.4).
        while self.read + ln <= self.buf.len() {
            let (mut re, mut im) = (0.0f64, 0.0f64);
            let w = &self.buf[self.read..self.read + ln];
            for (j, x) in w.iter().enumerate() {
                let h = self.taps[ln - 1 - j] as f64;
                re += h * x.re as f64;
                im += h * x.im as f64;
            }
            out.push(Complex32::new(re as f32, im as f32));
            self.read += self.hop;
        }
        // Samples before `read` are never used again (the next window starts
        // at `read`); compact once a filter-length's worth is dead.
        if self.read >= ln {
            self.buf.drain(..self.read);
            self.read = 0;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use num_complex::Complex32;

    const FS: f64 = 96_000.0;

    fn tone(freq: f64, n: usize, amp: f32) -> Vec<Complex32> {
        (0..n)
            .map(|i| {
                let phi = 2.0 * std::f64::consts::PI * freq * i as f64 / FS;
                Complex32::new(amp * phi.cos() as f32, amp * phi.sin() as f32)
            })
            .collect()
    }

    /// Steady-state output magnitudes (skip the filter warm-up).
    fn steady(ext: &mut SingleChannelExtractor, iq: &[Complex32]) -> Vec<f32> {
        let out = ext.process(iq);
        out[40..].iter().map(|c| c.norm()).collect()
    }

    #[test]
    fn rejects_non_table_rate() {
        assert!(SingleChannelExtractor::new(44_100.0, 0.0).is_err());
        assert!(SingleChannelExtractor::new(96_000.0, 0.0).is_ok());
        assert!(SingleChannelExtractor::new(192_000.0, 0.0).is_ok());
    }

    #[test]
    fn output_rate_is_fs_over_hop() {
        let mut ext = SingleChannelExtractor::new(FS, 12_340.0).unwrap();
        assert_eq!(ext.hop(), 256); // N = 1024, hop = N/4
        let out = ext.process(&tone(12_340.0, 96_000, 1.0)); // 1 s
                                                             // 1 s of input -> ~375 outputs (minus warm-up of ~LN/hop = 32 hops).
        assert!((340..=375).contains(&out.len()), "{} outputs", out.len());
    }

    #[test]
    fn on_channel_tone_passes_at_unity() {
        let mut ext = SingleChannelExtractor::new(FS, 12_340.0).unwrap();
        let mags = steady(&mut ext, &tone(12_340.0, 192_000, 0.5));
        for m in &mags {
            assert!((m - 0.5).abs() < 0.01, "passband magnitude {m}");
        }
    }

    #[test]
    fn tone_150hz_away_is_rejected_by_80db() {
        // SPEC §1.2: alias rejection >= 80 dB from 1.15 channels (~108 Hz) away.
        let mut ext = SingleChannelExtractor::new(FS, 12_340.0).unwrap();
        let mags = steady(&mut ext, &tone(12_340.0 + 150.0, 192_000, 1.0));
        for m in &mags {
            assert!(*m < 2e-4, "stopband leak {m}"); // -74 dB, slack for f32
        }
    }

    #[test]
    fn channel_edge_is_minus_6_db() {
        let mut ext = SingleChannelExtractor::new(FS, 12_340.0).unwrap();
        let mags = steady(&mut ext, &tone(12_340.0 + 46.875, 384_000, 1.0));
        let mean: f32 = mags.iter().sum::<f32>() / mags.len() as f32;
        // -6 dB = 0.501 in amplitude; edge tone beats at 46.875 Hz vs the
        // 375 Hz output so magnitudes are steady (complex tone at +46.875 Hz
        // in the channel), mean must sit near 0.5.
        assert!((mean - 0.5).abs() < 0.05, "edge gain {mean}");
    }

    #[test]
    fn keyed_envelope_survives_with_edges_softened() {
        // 30 ms on / 30 ms off keying (40 WPM dit rate) at channel center:
        // plateau must reach ~1.0 and troughs ~0.0 in the 375 Hz envelope.
        let n = 96_000;
        let mut iq = tone(12_340.0, n, 1.0);
        for (i, s) in iq.iter_mut().enumerate() {
            let t_ms = i as f64 * 1000.0 / FS;
            if (t_ms / 30.0) as u64 % 2 == 1 {
                *s = Complex32::new(0.0, 0.0);
            }
        }
        let mut ext = SingleChannelExtractor::new(FS, 12_340.0).unwrap();
        let mags = steady(&mut ext, &iq);
        let peak = mags.iter().cloned().fold(0.0f32, f32::max);
        let trough = mags.iter().cloned().fold(f32::MAX, f32::min);
        assert!(peak > 0.9, "plateau {peak}");
        assert!(trough < 0.1, "trough {trough}");
    }
}
