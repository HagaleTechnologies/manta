//! Compose keyed CW signals + noise into one IQ scene. ARCHITECTURE §9.

use crate::keyer::{key_text, key_text_loop, Jitter, KeyerSpec};
use crate::noise::{add_unit_awgn, amplitude_for_snr_2500};
use anyhow::Result;
use num_complex::Complex32;

/// One signal in a synthetic wideband scene. SPEC §7, ARCHITECTURE §9.
#[derive(Debug, Clone)]
pub struct SignalSpec {
    pub text: String,
    /// true: repeat the payload for the whole scene (SPEC §7 default payload
    /// behavior); false: key once, silence after.
    pub loop_text: bool,
    pub wpm: f32,
    pub offset_hz: f64,
    pub snr_2500_db: f32,
    pub jitter: Option<Jitter>,
    pub qsb: Option<QsbSine>,
    pub watterson: Option<WattersonFade>,
    /// SPEC §7 V10 Farnsworth: character speed, if different from `wpm`
    /// (the effective/word speed). `None` for every existing vector.
    pub char_wpm: Option<f32>,
}

/// Sinusoidal QSB envelope multiplier applied on top of the keyed envelope.
/// SPEC §7 V6: `0.55 + 0.45 * sin(2*pi*rate_hz*t)`.
#[derive(Debug, Clone, Copy)]
pub struct QsbSine {
    pub rate_hz: f32,
}

/// Watterson HF fading applied to this signal only, via coppa's real,
/// currently-shipped `watterson_preset()` (one-shot, real-domain) -- design
/// doc §6: the streaming `WattersonChannel` SPEC-decode-core.md assumes was
/// never implemented in coppa; vector generation is offline/batch, so the
/// streaming requirement doesn't apply.
#[derive(Debug, Clone, Copy)]
pub struct WattersonFade {
    pub preset: coppa_channel::watterson::WattersonPreset,
    pub seed: u64,
}

/// Headroom scale applied to signal+noise after mixing (float32 WAV; keeps
/// peaks well under 1.0 for i16 export later).
pub const MASTER_SCALE: f32 = 0.05;

/// Render signals (slice order) + optional noise, then MASTER_SCALE.
/// Returns the scene and the keyed text of each signal. ARCHITECTURE §9.
pub fn render_scene(
    signals: &[SignalSpec],
    fs: f64,
    duration_s: f64,
    noise_seed: Option<u64>,
) -> Result<(Vec<Complex32>, Vec<String>)> {
    let n = (duration_s * fs).round() as usize;
    let mut acc = vec![Complex32::new(0.0, 0.0); n];
    let mut texts = Vec::with_capacity(signals.len());
    for sig in signals {
        let spec = KeyerSpec {
            wpm: sig.wpm,
            char_wpm: sig.char_wpm,
            rise_ms: 5.0,
            jitter: sig.jitter,
        };
        let (env, text) = if sig.loop_text {
            key_text_loop(&sig.text, &spec, fs, duration_s)?
        } else {
            key_text(&sig.text, &spec, fs)?
        };
        texts.push(text);
        let amp = amplitude_for_snr_2500(sig.snr_2500_db, fs);

        if let Some(fade) = sig.watterson {
            // Real-domain path: build a real passband tone at this
            // signal's offset, fade it with coppa's model, then
            // Hilbert-convert back to complex baseband and add directly.
            //
            // `cos` is even, so a phase accumulator driven by a *negative*
            // offset_hz produces a bit-identical real waveform to the same
            // positive offset -- sign information cannot survive a
            // real-valued tone. And the Hilbert transform of any real tone
            // always yields a positive-frequency analytic signal (the
            // negative-frequency half is exactly what the transform
            // discards). So build the tone from the offset's magnitude,
            // then conjugate the analytic result for negative offsets:
            // conj(e^{+jwt}) = e^{-jwt}, which is exactly the negative-
            // frequency tone the (possibly negative) offset asked for.
            let mut real = vec![0.0f32; n];
            let dphi = std::f64::consts::TAU * sig.offset_hz.abs() / fs;
            let mut phi = 0.0f64;
            for (i, r) in real.iter_mut().enumerate() {
                let e = env.get(i).copied().unwrap_or(0.0) * amp;
                *r = e * phi.cos() as f32;
                phi += dphi;
                if phi > std::f64::consts::PI {
                    phi -= std::f64::consts::TAU;
                } else if phi < -std::f64::consts::PI {
                    phi += std::f64::consts::TAU;
                }
            }
            let faded = coppa_channel::watterson::watterson_preset(
                &real,
                fs as f32,
                fade.preset,
                fade.seed,
            );
            let mut analytic = skimmer_dsp::hilbert::HilbertTransformer::new().process(&faded);
            if sig.offset_hz < 0.0 {
                for a in &mut analytic {
                    *a = a.conj();
                }
            }
            for (out, a) in acc.iter_mut().zip(analytic.iter()) {
                *out += a;
            }
            continue;
        }

        // AWGN-only path (V1-V3, V6): complex NCO placement.
        let dphi = std::f64::consts::TAU * sig.offset_hz / fs;
        let mut phi = 0.0f64;
        for (i, out) in acc.iter_mut().enumerate() {
            let mut e = env.get(i).copied().unwrap_or(0.0) * amp;
            if let Some(q) = sig.qsb {
                let t = i as f64 / fs;
                let mul = 0.55 + 0.45 * (std::f64::consts::TAU * q.rate_hz as f64 * t).sin();
                e *= mul as f32;
            }
            if e != 0.0 {
                let (s, c) = phi.sin_cos();
                out.re += e * c as f32;
                out.im += e * s as f32;
            }
            phi += dphi;
            if phi > std::f64::consts::PI {
                phi -= std::f64::consts::TAU;
            } else if phi < -std::f64::consts::PI {
                phi += std::f64::consts::TAU;
            }
        }
    }
    if let Some(seed) = noise_seed {
        add_unit_awgn(&mut acc, seed);
    }
    for s in &mut acc {
        *s *= MASTER_SCALE;
    }
    Ok((acc, texts))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn achieved_snr_matches_request() {
        // Measure key-down signal power vs noise power in the 2500 Hz
        // convention; must be within 0.3 dB of the request (pinned decision 3).
        let fs = 96_000.0;
        let sig = SignalSpec {
            text: "CQ CQ DE W1AW W1AW K".into(),
            loop_text: true,
            wpm: 20.0,
            offset_hz: 12_340.0,
            snr_2500_db: 20.0,
            jitter: None,
            qsb: None,
            watterson: None,
            char_wpm: None,
        };
        let (clean, _) = render_scene(std::slice::from_ref(&sig), fs, 10.0, None).unwrap();
        let (noisy_only, _) = render_scene(&[], fs, 10.0, Some(1)).unwrap();
        // Key-down mask from the clean scene:
        let plateau = clean.iter().map(|c| c.norm()).fold(0.0f32, f32::max);
        let keydown: Vec<f32> = clean
            .iter()
            .map(|c| c.norm())
            .filter(|&m| m > 0.9 * plateau)
            .collect();
        let p_sig: f64 = keydown
            .iter()
            .map(|&m| (m as f64) * (m as f64))
            .sum::<f64>()
            / keydown.len() as f64;
        let p_noise: f64 =
            noisy_only.iter().map(|c| c.norm_sqr() as f64).sum::<f64>() / noisy_only.len() as f64;
        let snr_2500 = 10.0 * (p_sig / (p_noise * 2500.0 / fs)).log10();
        assert!((snr_2500 - 20.0).abs() < 0.3, "achieved SNR {snr_2500}");
    }

    #[test]
    fn scene_is_deterministic() {
        let sig = SignalSpec {
            text: "TEST".into(),
            loop_text: false,
            wpm: 25.0,
            offset_hz: -5_000.0,
            snr_2500_db: 15.0,
            jitter: Some(crate::keyer::Jitter {
                sigma: 0.08,
                seed: 9,
            }),
            qsb: None,
            watterson: None,
            char_wpm: None,
        };
        let a = render_scene(std::slice::from_ref(&sig), 96_000.0, 3.0, Some(2)).unwrap();
        let b = render_scene(std::slice::from_ref(&sig), 96_000.0, 3.0, Some(2)).unwrap();
        assert_eq!(a.0, b.0);
        assert_eq!(a.1, b.1);
    }

    #[test]
    fn qsb_sine_modulates_envelope_amplitude() {
        let fs = 96_000.0;
        let sig = SignalSpec {
            // Ten E's as a single word: only 3-unit (180 ms) intra-word gaps
            // apply between characters, giving a 240 ms dit+gap cadence --
            // well under the 300 ms bin width below. (A single-character "E"
            // loop instead inserts a 7-unit, 420 ms inter-repetition gap
            // after every pass -- a 480 ms repeat cycle that can leave a
            // whole 0.3 s bin sitting entirely in silence regardless of the
            // QSB multiplier there; confirmed empirically that version could
            // not distinguish QSB enabled from QSB disabled.)
            text: "EEEEEEEEEE".into(),
            loop_text: true,
            wpm: 20.0,
            offset_hz: 1_000.0,
            snr_2500_db: 30.0,
            jitter: None,
            qsb: Some(QsbSine { rate_hz: 0.2 }),
            watterson: None,
            char_wpm: None,
        };
        let (samples, _) = render_scene(std::slice::from_ref(&sig), fs, 5.0, None).unwrap();
        let global_peak = samples.iter().map(|c| c.norm()).fold(0.0f32, f32::max);

        // Bin into 0.3 s windows and take each bin's peak envelope. At 20 WPM
        // this cadence repeats well inside a 0.3 s window, so every bin is
        // guaranteed to contain at least one keyed "on" pulse regardless of
        // exact keying phase -- this avoids the previous version's mistake of
        // guessing a fixed sample offset, which happened to land in ordinary
        // Morse silence rather than actually sampling the QSB envelope's
        // minimum. The true QSB minimum (multiplier 0.10, at t=3.75s where
        // sin(2*pi*0.2*t) = -1) should produce a visibly smaller bin peak
        // than the bin containing the QSB maximum (multiplier 1.0, at
        // t=1.25s where sin(2*pi*0.2*t) = 1).
        let bin_samples = (0.3 * fs) as usize;
        let min_bin_peak = samples
            .chunks(bin_samples)
            .map(|chunk| chunk.iter().map(|c| c.norm()).fold(0.0f32, f32::max))
            .fold(f32::MAX, f32::min);

        assert!(
            min_bin_peak < global_peak * 0.5,
            "expected some 0.3s bin's peak envelope well below the global peak (QSB trough): \
             global_peak={global_peak} min_bin_peak={min_bin_peak}"
        );
    }
}
