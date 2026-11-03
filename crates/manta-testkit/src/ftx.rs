//! MAN-323: synthetic FT8/FT4 transmissions for the FTx skimmer's tests.
//!
//! The tone strings below came from ft8_lib's own encoder (kgoba/ft8_lib at
//! `9fec6ca`, MIT, vendored by pancetta-ft8): `make gen_ft8`, then
//! `gen_ft8 "CQ W1AW FN31" out.wav 1234` (and `-ft4` for the FT4 string,
//! `"CQ K1ABC FN42"` for the second FT8 one), copying the printed
//! `FSK tones:` line. [`gfsk_iq`] is a port of ft8_lib's `synth_gfsk`
//! (`ft8/encode.c`'s pulse and phase recursion, MIT) to complex IQ, so a test
//! can place a transmission anywhere in an IQ stream without a binary
//! fixture.

use crate::noise::{add_unit_awgn, amplitude_for_snr_2500};
use num_complex::Complex32;
use std::f64::consts::PI;

/// `CQ W1AW FN31` as 79 FT8 tones (0-7).
pub const CQ_W1AW_FN31_FT8: &str =
    "3140652000000001006774623106034423003140652734564663621004325777226765323140652";

/// `CQ K1ABC FN42` as 79 FT8 tones (0-7).
pub const CQ_K1ABC_FN42_FT8: &str =
    "3140652000000001005476704606021533433140652736011047517007334745455133543140652";

/// `CQ W1AW FN31` as 105 FT4 tones (0-3), ramp symbols included.
pub const CQ_W1AW_FN31_FT4: &str = "001321033112330313110232212220101210231223330230203303123110012113123102132323123110123332031003212132010";

/// Which protocol's symbol timing and Gaussian shaping to synthesise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// 0.160 s symbols, BT 2.0, 6.25 Hz tone spacing.
    Ft8,
    /// 0.048 s symbols, BT 1.0, 20.833 Hz tone spacing.
    Ft4,
}

impl Protocol {
    fn symbol_period_s(self) -> f64 {
        match self {
            Protocol::Ft8 => 0.160,
            Protocol::Ft4 => 0.048,
        }
    }

    fn symbol_bt(self) -> f64 {
        match self {
            Protocol::Ft8 => 2.0,
            Protocol::Ft4 => 1.0,
        }
    }
}

/// Parse a tone string into symbol values.
pub fn tones(s: &str) -> Vec<u8> {
    s.bytes().map(|b| b - b'0').collect()
}

/// Abramowitz-Stegun 7.1.26 error function (max error 1.5e-7).
fn erf(x: f64) -> f64 {
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let x = x.abs();
    let t = 1.0 / (1.0 + 0.327_591_1 * x);
    let y = 1.0
        - (((((1.061_405_429 * t - 1.453_152_027) * t) + 1.421_413_741) * t - 0.284_496_736) * t
            + 0.254_829_592)
            * t
            * (-x * x).exp();
    sign * y
}

/// ft8_lib's `gfsk_pulse`: the frequency pulse over three symbols.
fn gfsk_pulse(n_spsym: usize, bt: f64) -> Vec<f64> {
    // pi * sqrt(2 / ln 2)
    const K: f64 = 5.336_446;
    (0..3 * n_spsym)
        .map(|i| {
            let t = i as f64 / n_spsym as f64 - 1.5;
            (erf(K * bt * (t + 0.5)) - erf(K * bt * (t - 0.5))) / 2.0
        })
        .collect()
}

/// A unit-amplitude complex GFSK transmission of `tones` with tone 0 at
/// `offset_hz` from the stream centre (negative below it), starting at
/// sample `start_sample` of a `len`-sample buffer at `fs` S/s. Samples
/// before the start and after the end are zero; a transmission running
/// past `len` is cut there. Shaping follows ft8_lib's `synth_gfsk`: the
/// Gaussian frequency pulse, dummy first/last symbols, and a 1/8-symbol
/// raised-cosine amplitude ramp at each end.
pub fn gfsk_iq(
    tones: &[u8],
    fs: f64,
    offset_hz: f64,
    start_sample: usize,
    len: usize,
    protocol: Protocol,
) -> Vec<Complex32> {
    let n_spsym = (0.5 + fs * protocol.symbol_period_s()) as usize;
    let n_sym = tones.len();
    let n_wave = n_sym * n_spsym;
    let dphi_peak = 2.0 * PI / n_spsym as f64; // hmod = 1
    let mut dphi = vec![2.0 * PI * offset_hz / fs; n_wave + 2 * n_spsym];
    let pulse = gfsk_pulse(n_spsym, protocol.symbol_bt());
    for (i, &tone) in tones.iter().enumerate() {
        let ib = i * n_spsym;
        for (j, p) in pulse.iter().enumerate() {
            dphi[j + ib] += dphi_peak * f64::from(tone) * p;
        }
    }
    for j in 0..2 * n_spsym {
        dphi[j] += dphi_peak * pulse[j + n_spsym] * f64::from(tones[0]);
        dphi[j + n_sym * n_spsym] += dphi_peak * pulse[j] * f64::from(tones[n_sym - 1]);
    }
    let n_ramp = n_spsym / 8;
    let mut out = vec![Complex32::new(0.0, 0.0); len];
    let mut phi = 0.0f64;
    for k in 0..n_wave {
        let idx = start_sample + k;
        if idx >= len {
            break;
        }
        let env = if k < n_ramp {
            (1.0 - (2.0 * PI * k as f64 / (2 * n_ramp) as f64).cos()) / 2.0
        } else if k >= n_wave - n_ramp {
            let i = n_wave - 1 - k;
            (1.0 - (2.0 * PI * i as f64 / (2 * n_ramp) as f64).cos()) / 2.0
        } else {
            1.0
        };
        out[idx] = Complex32::new((env * phi.cos()) as f32, (env * phi.sin()) as f32);
        phi = (phi + dphi[k + n_spsym]) % (2.0 * PI);
    }
    out
}

/// Scale a unit-amplitude signal (from [`gfsk_iq`]) to `snr_db_2500` dB in
/// 2500 Hz and add seeded unit-power complex AWGN (the testkit's fixed
/// ChaCha8 sampling order, so a seed always gives the same samples).
pub fn add_awgn(iq: &mut [Complex32], fs: f64, snr_db_2500: f32, seed: u64) {
    let a = amplitude_for_snr_2500(snr_db_2500, fs);
    for s in iq.iter_mut() {
        *s *= a;
    }
    add_unit_awgn(iq, seed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tone_strings_have_the_protocol_lengths() {
        for s in [CQ_W1AW_FN31_FT8, CQ_K1ABC_FN42_FT8] {
            let t = tones(s);
            assert_eq!(t.len(), 79);
            assert!(t.iter().all(|&x| x <= 7));
            // Costas sync arrays at symbols 0, 36 and 72.
            for at in [0, 36, 72] {
                assert_eq!(&t[at..at + 7], &[3, 1, 4, 0, 6, 5, 2]);
            }
        }
        let t4 = tones(CQ_W1AW_FN31_FT4);
        assert_eq!(t4.len(), 105);
        assert!(t4.iter().all(|&x| x <= 3));
    }

    #[test]
    fn gfsk_iq_is_deterministic() {
        let t = tones(CQ_W1AW_FN31_FT8);
        let mut a = gfsk_iq(
            &t,
            48_000.0,
            24_000.0 + 1234.0,
            24_000,
            48_000 * 14,
            Protocol::Ft8,
        );
        let mut b = gfsk_iq(
            &t,
            48_000.0,
            24_000.0 + 1234.0,
            24_000,
            48_000 * 14,
            Protocol::Ft8,
        );
        add_awgn(&mut a, 48_000.0, -10.0, 7);
        add_awgn(&mut b, 48_000.0, -10.0, 7);
        assert!(a
            .iter()
            .zip(&b)
            .all(|(x, y)| x.re.to_bits() == y.re.to_bits() && x.im.to_bits() == y.im.to_bits()));
    }

    #[test]
    fn gfsk_iq_power_matches_the_requested_snr() {
        let fs = 48_000.0;
        let t = tones(CQ_W1AW_FN31_FT8);
        let start = 12_000;
        let n_wave = 79 * (0.16 * fs) as usize;
        let mut iq = gfsk_iq(
            &t,
            fs,
            1234.0,
            start,
            start + n_wave + 12_000,
            Protocol::Ft8,
        );
        assert!(iq[..start].iter().all(|s| s.norm_sqr() == 0.0));
        assert!(iq[start + n_wave..].iter().all(|s| s.norm_sqr() == 0.0));
        let snr_db = -10.0f32;
        let a = amplitude_for_snr_2500(snr_db, fs);
        for s in iq.iter_mut() {
            *s *= a;
        }
        let p_sig = iq[start..start + n_wave]
            .iter()
            .map(|s| f64::from(s.norm_sqr()))
            .sum::<f64>()
            / n_wave as f64;
        // Unit-power noise holds 2500 / fs of its power in 2500 Hz.
        let achieved = 10.0 * (p_sig / (2500.0 / fs)).log10();
        assert!(
            (achieved - f64::from(snr_db)).abs() < 0.2,
            "achieved {achieved:.2} dB"
        );
    }

    #[test]
    fn the_first_symbol_sits_on_the_offset_tone() {
        // Tone 3 (the first Costas symbol) is 3 * 6.25 Hz above the offset.
        let fs = 12_000.0;
        let t = tones(CQ_W1AW_FN31_FT8);
        let iq = gfsk_iq(&t, fs, 1000.0, 0, 79 * 1920, Protocol::Ft8);
        // Mid-symbol instantaneous frequency.
        let k = 960;
        let dphi = (iq[k + 1] * iq[k].conj()).arg() as f64;
        let f = dphi * fs / (2.0 * PI);
        assert!((f - 1018.75).abs() < 0.5, "{f} Hz");
    }
}
