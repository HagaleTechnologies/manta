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
    /// Dah:dit ratio, threaded into `KeyerSpec::weight`. SPEC v2 §8.2 VR6.
    /// 3.0 (standard Morse) for every vector before VR6.
    pub weight: f32,
    /// Inter-character gap in dit units, threaded into
    /// `KeyerSpec::char_gap_units`. SPEC v2 §8.2 VR7. 3.0 (standard) for
    /// every vector before VR7.
    pub char_gap_units: f32,
    /// Inter-word gap in dit units, threaded into
    /// `KeyerSpec::word_gap_units`. SPEC v2 §8.2 VR7. 7.0 (standard) for
    /// every vector before VR7.
    pub word_gap_units: f32,
    /// Raised-cosine rise/fall for this signal's keying, threaded into
    /// `KeyerSpec::rise_ms`. SPEC v2 §8.2 VR4 (0.5 ms "hard edges"). 5.0 ms
    /// (standard) for every vector before VR4.
    ///
    /// This is a true per-signal knob only for callers that build
    /// `SignalSpec`s and call `render_scene` directly. A signal reached via
    /// `VectorSpec::render()`/`render_v9_drift()` has this value silently
    /// overridden by `VectorSpec::rise_ms` (see that field's doc comment) --
    /// there is currently no way to give two signals in the same
    /// `VectorSpec`-based vector different edge shapes.
    pub rise_ms: f64,
    /// Dot mark length in dit units, threaded into `KeyerSpec::dit_units`.
    /// 1.0 (standard) for every vector before MAN-315's VW3/VW4.
    pub dit_units: f32,
    /// Element gap in dit units, threaded into
    /// `KeyerSpec::element_gap_units`. 1.0 (standard) for every vector
    /// before MAN-315's VW3/VW4.
    pub element_gap_units: f32,
}

/// Sinusoidal QSB envelope multiplier applied on top of the keyed envelope.
/// SPEC §7 V6: `0.55 + 0.45 * sin(2*pi*rate_hz*t)`.
#[derive(Debug, Clone, Copy)]
pub struct QsbSine {
    pub rate_hz: f32,
}

/// Watterson HF fading applied to this signal only, via coppa's real,
/// currently-shipped `watterson_preset()` (one-shot, real-domain) -- the
/// streaming `WattersonChannel` the SPEC-watterson §6 design doc proposed
/// was never implemented in coppa; vector generation is offline/batch, so
/// the streaming requirement doesn't apply.
#[derive(Debug, Clone, Copy)]
pub struct WattersonFade {
    pub preset: coppa_channel::watterson::WattersonPreset,
    pub seed: u64,
}

/// Headroom scale applied to signal+noise after mixing (float32 WAV; keeps
/// peaks well under 1.0 for i16 export later).
pub const MASTER_SCALE: f32 = 0.05;

/// Which generation route a signal takes. `render_signal` picks
/// `RealPassband` exactly when the signal has a Watterson fade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Complex NCO placement (V1-V3, V6).
    Direct,
    /// Real passband tone, optional Watterson fade, 129-tap Hilbert back to
    /// complex baseband (64-sample group delay).
    RealPassband,
}

/// One signal's unscaled contribution (no noise, no `MASTER_SCALE`) and its
/// keyed text, on the route `render_scene` uses for it. MAN-206.
pub fn render_signal(
    sig: &SignalSpec,
    fs: f64,
    duration_s: f64,
) -> Result<(Vec<Complex32>, String)> {
    let route = if sig.watterson.is_some() {
        Route::RealPassband
    } else {
        Route::Direct
    };
    render_signal_with_route(sig, fs, duration_s, route)
}

/// `render_signal` on an explicit route. `RealPassband` with
/// `watterson: None` skips only the fader, so the cross-check can measure
/// the Hilbert route on its own. Only the direct route applies `qsb`, so
/// `qsb` on the real route is an error (no shipped vector combines QSB with
/// fading).
pub fn render_signal_with_route(
    sig: &SignalSpec,
    fs: f64,
    duration_s: f64,
    route: Route,
) -> Result<(Vec<Complex32>, String)> {
    if route == Route::RealPassband && sig.qsb.is_some() {
        anyhow::bail!("QSB on the real-passband (faded) route is not implemented");
    }
    let n = (duration_s * fs).round() as usize;
    let mut acc = vec![Complex32::new(0.0, 0.0); n];
    let spec = KeyerSpec {
        wpm: sig.wpm,
        char_wpm: sig.char_wpm,
        rise_ms: sig.rise_ms,
        jitter: sig.jitter,
        weight: sig.weight,
        char_gap_units: sig.char_gap_units,
        word_gap_units: sig.word_gap_units,
        dit_units: sig.dit_units,
        element_gap_units: sig.element_gap_units,
    };
    let (env, text) = if sig.loop_text {
        key_text_loop(&sig.text, &spec, fs, duration_s)?
    } else {
        key_text(&sig.text, &spec, fs)?
    };
    let amp = amplitude_for_snr_2500(sig.snr_2500_db, fs);

    if route == Route::RealPassband {
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
        let faded = match sig.watterson {
            Some(fade) => {
                coppa_channel::watterson::watterson_preset(&real, fs as f32, fade.preset, fade.seed)
            }
            None => real,
        };
        // MAN-273 D4: fixture synthesis stays on the M1 129-tap design so
        // widening the live-decode default cannot move a golden-vector
        // byte. This route's verified domain is |offset| >= 2 kHz at
        // 96 kS/s (image about -80 dBc at 2 kHz, -12.8 dBc at 600 Hz;
        // MAN-206). V8w's `s09` sits at -324.6 Hz, outside it, and leaves a
        // mirror image the decoder promotes; `crosscheck` pins that one
        // exception so a new near-DC faded signal fails loudly.
        let mut analytic = manta_dsp::hilbert::HilbertTransformer::with_taps(
            manta_dsp::hilbert::HILBERT_TAPS_M1_LEGACY,
        )
        .process(&faded);
        if sig.offset_hz < 0.0 {
            for a in &mut analytic {
                *a = a.conj();
            }
        }
        for (out, a) in acc.iter_mut().zip(analytic.iter()) {
            *out += a;
        }
        return Ok((acc, text));
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
    Ok((acc, text))
}

/// Sum contributions in slice order into a zeroed buffer of `n` samples,
/// add unit AWGN when `noise_seed` is set, then apply `MASTER_SCALE`.
pub fn mix(contributions: &[&[Complex32]], n: usize, noise_seed: Option<u64>) -> Vec<Complex32> {
    let mut acc = vec![Complex32::new(0.0, 0.0); n];
    for c in contributions {
        accumulate(&mut acc, c);
    }
    finish(&mut acc, noise_seed);
    acc
}

/// `acc += contribution`, sample by sample (the order `mix` sums in).
pub(crate) fn accumulate(acc: &mut [Complex32], contribution: &[Complex32]) {
    for (out, a) in acc.iter_mut().zip(contribution.iter()) {
        *out += a;
    }
}

/// Noise, then `MASTER_SCALE`: the tail of `mix`.
pub(crate) fn finish(acc: &mut [Complex32], noise_seed: Option<u64>) {
    if let Some(seed) = noise_seed {
        add_unit_awgn(acc, seed);
    }
    for s in acc.iter_mut() {
        *s *= MASTER_SCALE;
    }
}

/// Render signals (slice order) + optional noise, then MASTER_SCALE.
/// Returns the scene and the keyed text of each signal. ARCHITECTURE §9.
/// Same arithmetic as `mix` over every `render_signal`, but each
/// contribution is added and dropped in turn, so a 50-signal scene holds
/// one contribution at a time.
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
        let (c, text) = render_signal(sig, fs, duration_s)?;
        accumulate(&mut acc, &c);
        texts.push(text);
    }
    finish(&mut acc, noise_seed);
    Ok((acc, texts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vectors;

    /// MAN-206: digests of `render_scene(&spec.signals, spec.fs, d,
    /// Some(spec.noise_seed))`, computed at d1ddf99 before the per-signal
    /// split.
    #[test]
    fn render_scene_split_keeps_direct_route_bytes() {
        for (spec, want) in [
            (vectors::v1(), 0xb689_3351_782c_1da3u64),
            (vectors::v6(), 0x912c_6564_8c14_91c1),
            (vectors::v7(), 0xe9c4_7623_22c3_0d1a),
            (vectors::v8(), 0x6fac_e2d0_35cf_5a59),
            (vectors::vr4(), 0x44ce_2d62_f984_63c2),
            (vectors::vw3(), 0x10d1_2485_40b3_f7d0),
        ] {
            let (s, _) = render_scene(&spec.signals, spec.fs, 20.0, Some(spec.noise_seed)).unwrap();
            assert_eq!(crate::repro::samples_digest(&s), want, "{}", spec.name);
        }
    }

    /// Faded route: coppa's FFT is SIMD-dispatched, so these are pinned on
    /// the target they were measured on only (MAN-206 D13).
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    #[test]
    fn render_scene_split_keeps_faded_route_bytes() {
        let v4 = vectors::v4();
        let (s, _) = render_scene(&v4.signals, v4.fs, 5.0, Some(v4.noise_seed)).unwrap();
        assert_eq!(
            crate::repro::samples_digest(&s),
            0x4488_7052_75d1_ff00,
            "v4"
        );
        let v8w = vectors::v8w();
        let (s, _) = render_scene(&v8w.signals[..3], v8w.fs, 5.0, Some(v8w.noise_seed)).unwrap();
        assert_eq!(
            crate::repro::samples_digest(&s),
            0xa4c7_276f_77c1_22a2,
            "v8w first 3"
        );
    }

    #[test]
    fn render_signal_contributions_sum_to_render_scene() {
        let v8 = vectors::v8();
        let parts: Vec<Vec<Complex32>> = v8
            .signals
            .iter()
            .map(|s| render_signal(s, v8.fs, 5.0).unwrap().0)
            .collect();
        let refs: Vec<&[Complex32]> = parts.iter().map(|p| p.as_slice()).collect();
        let n = (5.0 * v8.fs).round() as usize;
        let mixed = mix(&refs, n, Some(v8.noise_seed));
        let (want, _) = render_scene(&v8.signals, v8.fs, 5.0, Some(v8.noise_seed)).unwrap();
        assert_eq!(mixed, want);
    }

    #[test]
    fn real_passband_route_without_fade_is_a_delayed_tone() {
        let sig = vectors::v1().signals[0].clone();
        let (d, _) = render_signal_with_route(&sig, 96_000.0, 2.0, Route::Direct).unwrap();
        let (r, _) = render_signal_with_route(&sig, 96_000.0, 2.0, Route::RealPassband).unwrap();
        let first = |x: &[Complex32]| {
            let peak = x.iter().map(|c| c.norm()).fold(0.0f32, f32::max);
            x.iter().position(|c| c.norm() >= 0.5 * peak).unwrap()
        };
        assert_eq!(first(&r) - first(&d), 64);
    }

    #[test]
    fn render_signal_with_route_direct_matches_render_signal_for_unfaded() {
        let sig = vectors::v2().signals[0].clone();
        let a = render_signal(&sig, 96_000.0, 3.0).unwrap();
        let b = render_signal_with_route(&sig, 96_000.0, 3.0, Route::Direct).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn achieved_snr_matches_request() {
        // Measure key-down signal power vs noise power in the 2500 Hz
        // convention; must be within 0.3 dB of the request (pinned decision 3).
        let fs = 96_000.0;
        let sig = SignalSpec {
            dit_units: 1.0,
            element_gap_units: 1.0,
            text: "CQ CQ DE W1AW W1AW K".into(),
            loop_text: true,
            wpm: 20.0,
            offset_hz: 12_340.0,
            snr_2500_db: 20.0,
            jitter: None,
            qsb: None,
            watterson: None,
            char_wpm: None,
            weight: 3.0,
            char_gap_units: 3.0,
            word_gap_units: 7.0,
            rise_ms: 5.0,
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
            dit_units: 1.0,
            element_gap_units: 1.0,
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
            weight: 3.0,
            char_gap_units: 3.0,
            word_gap_units: 7.0,
            rise_ms: 5.0,
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
            dit_units: 1.0,
            element_gap_units: 1.0,
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
            weight: 3.0,
            char_gap_units: 3.0,
            word_gap_units: 7.0,
            rise_ms: 5.0,
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
