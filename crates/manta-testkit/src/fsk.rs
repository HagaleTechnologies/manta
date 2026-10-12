//! Continuous-phase two-tone FSK (RTTY) generator, keyed by Baudot (ITA2)
//! framed text. MAN-314: the scenes the RTTY detector (`manta_dsp::fsk`)
//! is tested against. A separate adder rather than a `SignalSpec` field, so
//! no existing CW scene or golden vector changes.

use crate::noise::amplitude_for_snr_2500;
use crate::scene::MASTER_SCALE;
use num_complex::Complex32;

/// One RTTY signal added to an existing analytic IQ buffer.
#[derive(Debug, Clone)]
pub struct FskSpec {
    /// Mark (upper RF tone, amateur convention) offset from the IQ centre.
    pub mark_offset_hz: f64,
    /// Space = mark - shift.
    pub shift_hz: f64,
    /// 45.45 / 50 / 75 ...
    pub baud: f64,
    pub snr_2500_db: f32,
    /// Looped for the whole buffer; Baudot with LTRS/FIGS shifts. Empty
    /// text keys a constant mark (idle).
    pub text: String,
    /// Silent before `start_s`, keyed from `start_s` on.
    pub start_s: f64,
    /// Watterson fading for this signal only, as in `scene::WattersonFade`.
    pub watterson: Option<(coppa_channel::watterson::WattersonPreset, u64)>,
    /// Initial phase only; deterministic.
    pub seed: u64,
}

/// ITA2 letters-case code (bit 0 = first data bit sent).
fn ita2_letter(c: char) -> Option<u8> {
    Some(match c {
        'A' => 0x03,
        'B' => 0x19,
        'C' => 0x0E,
        'D' => 0x09,
        'E' => 0x01,
        'F' => 0x0D,
        'G' => 0x1A,
        'H' => 0x14,
        'I' => 0x06,
        'J' => 0x0B,
        'K' => 0x0F,
        'L' => 0x12,
        'M' => 0x1C,
        'N' => 0x0C,
        'O' => 0x18,
        'P' => 0x16,
        'Q' => 0x17,
        'R' => 0x0A,
        'S' => 0x05,
        'T' => 0x10,
        'U' => 0x07,
        'V' => 0x1E,
        'W' => 0x13,
        'X' => 0x1D,
        'Y' => 0x15,
        'Z' => 0x11,
        _ => return None,
    })
}

/// ITA2 figures-case code (US TTY).
fn ita2_figure(c: char) -> Option<u8> {
    Some(match c {
        '1' => 0x17,
        '2' => 0x13,
        '3' => 0x01,
        '4' => 0x0A,
        '5' => 0x10,
        '6' => 0x15,
        '7' => 0x07,
        '8' => 0x06,
        '9' => 0x18,
        '0' => 0x16,
        '-' => 0x03,
        '?' => 0x19,
        '/' => 0x1D,
        '.' => 0x1C,
        ',' => 0x0C,
        _ => return None,
    })
}

const LTRS: u8 = 0x1F;
const FIGS: u8 = 0x1B;
const SPACE: u8 = 0x04;
const CR: u8 = 0x08;
const LF: u8 = 0x02;

/// Baudot codes for `text`, inserting LTRS/FIGS shifts as needed (starts in
/// letters case). Unknown characters are skipped.
fn baudot_codes(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut figs = false;
    for c in text.chars().map(|c| c.to_ascii_uppercase()) {
        match c {
            ' ' => out.push(SPACE),
            '\r' => out.push(CR),
            '\n' => out.push(LF),
            _ => {
                if let Some(code) = ita2_letter(c) {
                    if figs {
                        out.push(LTRS);
                        figs = false;
                    }
                    out.push(code);
                } else if let Some(code) = ita2_figure(c) {
                    if !figs {
                        out.push(FIGS);
                        figs = true;
                    }
                    out.push(code);
                }
            }
        }
    }
    out
}

/// Half-bit keying symbols (true = mark) for `text`: per character a start
/// bit (space), five data bits LSB first, 1.5 stop bits (mark).
pub fn baudot_half_bits(text: &str) -> Vec<bool> {
    let mut halves = Vec::new();
    for code in baudot_codes(text) {
        halves.extend([false, false]);
        for b in 0..5 {
            let bit = (code >> b) & 1 == 1;
            halves.extend([bit, bit]);
        }
        halves.extend([true, true, true]);
    }
    halves
}

/// Adds `spec` to an existing analytic IQ buffer (from
/// `scene::render_scene`, i.e. already scaled by `MASTER_SCALE`).
pub fn add_fsk(iq: &mut [Complex32], fs: f64, spec: &FskSpec) {
    let halves = baudot_half_bits(&spec.text);
    let amp = amplitude_for_snr_2500(spec.snr_2500_db, fs) * MASTER_SCALE;
    let mark = spec.mark_offset_hz;
    let space = spec.mark_offset_hz - spec.shift_hz;
    // Deterministic initial phase from the seed (no RNG state needed).
    let mut phi = (spec.seed % 1000) as f64 / 1000.0 * std::f64::consts::TAU;
    let freq_at = |i: usize| -> Option<f64> {
        let t = i as f64 / fs - spec.start_s;
        if t < 0.0 {
            return None;
        }
        if halves.is_empty() {
            return Some(mark);
        }
        let h = (t * spec.baud * 2.0).floor() as usize % halves.len();
        Some(if halves[h] { mark } else { space })
    };
    match spec.watterson {
        None => {
            for (i, out) in iq.iter_mut().enumerate() {
                let Some(f) = freq_at(i) else { continue };
                let (s, c) = phi.sin_cos();
                out.re += amp * c as f32;
                out.im += amp * s as f32;
                phi = wrap(phi + std::f64::consts::TAU * f / fs);
            }
        }
        Some((preset, seed)) => {
            // Same real-domain path as `scene::render_scene`'s Watterson
            // branch: a real passband signal from the offsets' magnitudes,
            // faded, Hilbert-converted, conjugated for negative offsets.
            let negative = mark < 0.0;
            let mut real = vec![0.0f32; iq.len()];
            for (i, r) in real.iter_mut().enumerate() {
                let Some(f) = freq_at(i) else { continue };
                *r = amp / MASTER_SCALE * phi.cos() as f32;
                phi = wrap(phi + std::f64::consts::TAU * f.abs() / fs);
            }
            let faded = coppa_channel::watterson::watterson_preset(&real, fs as f32, preset, seed);
            let analytic = manta_dsp::hilbert::HilbertTransformer::new().process(&faded);
            for (out, a) in iq.iter_mut().zip(analytic.iter()) {
                let a = if negative { a.conj() } else { *a };
                *out += a * MASTER_SCALE;
            }
        }
    }
}

fn wrap(phi: f64) -> f64 {
    if phi > std::f64::consts::PI {
        phi - std::f64::consts::TAU
    } else if phi < -std::f64::consts::PI {
        phi + std::f64::consts::TAU
    } else {
        phi
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::{render_scene, SignalSpec};

    fn spec(text: &str) -> FskSpec {
        FskSpec {
            mark_offset_hz: 1_000.0,
            shift_hz: 170.0,
            baud: 45.45,
            snr_2500_db: 10.0,
            text: text.into(),
            start_s: 0.0,
            watterson: None,
            seed: 3,
        }
    }

    #[test]
    fn baudot_frame_bits_for_ryry() {
        // R = 0x0A, Y = 0x15: start (space), data LSB first, 1.5 stop (mark).
        let h = baudot_half_bits("RY");
        let bits: Vec<u8> = h.iter().map(|&b| b as u8).collect();
        #[rustfmt::skip]
        let want: Vec<u8> = vec![
            0, 0, 0, 0, 1, 1, 0, 0, 1, 1, 0, 0, 1, 1, 1,
            0, 0, 1, 1, 0, 0, 1, 1, 0, 0, 1, 1, 1, 1, 1,
        ];
        assert_eq!(bits, want);
    }

    /// Power at `f` over the buffer (single-bin DFT, normalised).
    fn tone_power(iq: &[Complex32], fs: f64, f: f64) -> f64 {
        let mut acc = num_complex::Complex64::new(0.0, 0.0);
        for (i, s) in iq.iter().enumerate() {
            let ph = -std::f64::consts::TAU * f * i as f64 / fs;
            acc += num_complex::Complex64::new(s.re as f64, s.im as f64)
                * num_complex::Complex64::from_polar(1.0, ph);
        }
        acc.norm_sqr() / (iq.len() as f64).powi(2)
    }

    #[test]
    fn tone_frequencies_match_mark_and_shift() {
        let fs = 8_000.0;
        let mut idle = vec![Complex32::new(0.0, 0.0); 16_000];
        add_fsk(&mut idle, fs, &spec(""));
        let bin = fs / idle.len() as f64;
        let peak = (-40..=40)
            .map(|d| 1_000.0 + d as f64 * bin)
            .max_by(|a, b| tone_power(&idle, fs, *a).total_cmp(&tone_power(&idle, fs, *b)))
            .unwrap();
        assert!((peak - 1_000.0).abs() <= bin, "idle peak {peak}");

        let mut keyed = vec![Complex32::new(0.0, 0.0); 16_000];
        add_fsk(&mut keyed, fs, &spec("RYRYRYRY"));
        let mut cands: Vec<(f64, f64)> = (0..1_200)
            .map(|i| 600.0 + i as f64 * bin)
            .map(|f| (f, tone_power(&keyed, fs, f)))
            .collect();
        cands.sort_by(|a, b| b.1.total_cmp(&a.1));
        let mut top: Vec<f64> = vec![cands[0].0];
        for &(c, _) in &cands[1..] {
            if (c - top[0]).abs() > 30.0 {
                top.push(c);
                break;
            }
        }
        top.sort_by(f64::total_cmp);
        // Keyed FSK is a line spectrum: the strongest lines sit a few Hz
        // from the nominal tones (measured -6.5/+5.5 Hz for RYRY at 45.45
        // Bd), so allow a quarter of the baud rate.
        let tol = 45.45 / 4.0;
        assert!((top[0] - 830.0).abs() <= tol, "space {top:?}");
        assert!((top[1] - 1_000.0).abs() <= tol, "mark {top:?}");
    }

    #[test]
    fn phase_is_continuous() {
        let fs = 8_000.0;
        let mut iq = vec![Complex32::new(0.0, 0.0); 8_000];
        add_fsk(&mut iq, fs, &spec("CQ TEST DE K5ARH 599"));
        let max_step = std::f64::consts::TAU * 1_000.0 / fs + 1e-6;
        for w in iq.windows(2) {
            let d = (w[1] * w[0].conj()).arg() as f64;
            assert!(d.abs() <= max_step + 1e-4, "phase step {d}");
        }
    }

    #[test]
    fn snr_scaling_matches_cw_convention() {
        let fs = 96_000.0;
        let cw = SignalSpec {
            text: "TTT".into(),
            loop_text: true,
            wpm: 10.0,
            offset_hz: 1_000.0,
            snr_2500_db: 10.0,
            jitter: None,
            qsb: None,
            watterson: None,
            char_wpm: None,
            weight: 3.0,
            char_gap_units: 3.0,
            word_gap_units: 7.0,
            rise_ms: 5.0,
        };
        let (cw_iq, _) = render_scene(&[cw], fs, 2.0, None).unwrap();
        let cw_peak = cw_iq.iter().map(|s| s.norm_sqr()).fold(0.0f32, f32::max);
        let (mut rtty, _) = render_scene(&[], fs, 0.5, None).unwrap();
        add_fsk(&mut rtty, fs, &spec("RY"));
        let p: f64 = rtty.iter().map(|s| s.norm_sqr() as f64).sum::<f64>() / rtty.len() as f64;
        let db = 10.0 * (p / cw_peak as f64).log10();
        assert!(db.abs() < 0.1, "rtty vs cw key-down power {db} dB");
    }

    #[test]
    fn render_is_deterministic() {
        let fs = 8_000.0;
        let mut a = vec![Complex32::new(0.0, 0.0); 4_000];
        let mut b = vec![Complex32::new(0.0, 0.0); 4_000];
        add_fsk(&mut a, fs, &spec("RY 73"));
        add_fsk(&mut b, fs, &spec("RY 73"));
        assert_eq!(a, b);
    }
}
