//! Generator cross-check gate (MAN-206).
//!
//! Compares the testkit's production routes (`scene`: direct NCO and the
//! 129-tap real-passband route with coppa's Watterson fader) with the
//! independent generator (`baseband`) through the independent estimators
//! (`measure`), on envelope durations, spectra, total power (including the
//! independent `HEADROOM` against `scene::MASTER_SCALE`),
//! reference-bandwidth SNR, noise whiteness and fading autocorrelation.
//!
//! Ordering is enforced by a type: the only public way to get an
//! independent scene for decoding is [`independent_scene`], which takes the
//! [`Verified`] token that only a passing [`verified`] hands out. A failing
//! dimension cannot be skipped by test order.
//!
//! Verified domain: keying and spectra at 96 kS/s, fading at 8 kS/s,
//! |offset| >= [`VERIFIED_MIN_OFFSET_HZ`] for the 129-tap route, scenes of 3
//! to 60 s. Tolerances are the named constants below. Most hold at least
//! twice the measured disagreement; noise whiteness, ensemble power and the
//! full sweep's worst fading row hold less (1.4x to 1.6x). The decision
//! record lists the worst measurement per dimension
//! (docs/DECISIONS/2026-11-03-man206-independent-generator-and-scene-minimizer.md).

use crate::baseband::{self, FadeProfile, IndependentScene, Timing, ToneSpec};
use crate::keyer::Jitter;
use crate::measure;
use crate::scene::{self, QsbSine, Route, SignalSpec, WattersonFade};
use crate::vectors::VectorSpec;
use anyhow::Result;
use coppa_channel::watterson::WattersonPreset;
use num_complex::{Complex32, Complex64};
use std::fmt;
use std::sync::OnceLock;

/// Envelope: on-run start and duration agree within this many samples.
pub const ENVELOPE_SAMPLE_TOL: f64 = 1.0;
/// Envelope: largest per-sample magnitude difference, in carrier units.
pub const ENVELOPE_MAG_TOL: f64 = 1e-6;
/// Total power ratio, dB.
pub const POWER_DB_TOL: f64 = 0.01;
/// ±100 Hz main-band power ratio, dB.
pub const MAIN_BAND_DB_TOL: f64 = 0.01;
/// Per-bin PSD ratio over bins within [`PER_BIN_RANGE_DB`] of the peak, dB.
pub const PER_BIN_DB_TOL: f64 = 0.1;
/// Range below the PSD peak that the per-bin check covers, dB.
pub const PER_BIN_RANGE_DB: f64 = 60.0;
/// Largest allowed mirror image, dBc.
pub const IMAGE_DBC_MAX: f64 = -75.0;
/// The 129-tap route's group delay, samples (exact).
pub const HILBERT_GROUP_DELAY: usize = 64;
/// Achieved 2500 Hz SNR vs the request, dB.
pub const SNR_REQUEST_DB_TOL: f64 = 0.3;
/// Achieved 2500 Hz SNR, production vs independent, dB.
pub const SNR_PAIR_DB_TOL: f64 = 0.2;
/// 1 kHz noise band powers vs their mean, dB.
pub const WHITENESS_DB_TOL: f64 = 0.5;
/// Fading autocorrelation vs `exp(-2π²σ²τ²)`.
pub const FADE_THEORY_TOL: f64 = 0.03;
/// Fading autocorrelation, production vs independent.
pub const FADE_PAIR_TOL: f64 = 0.03;
/// Ensemble power: `|mean - 1| <= ENSEMBLE_SE_MULT * SE`.
pub const ENSEMBLE_SE_MULT: f64 = 4.0;
/// Ensemble power: largest allowed standard error.
pub const ENSEMBLE_SE_MAX: f64 = 0.06;
/// The 129-tap real-passband route is verified only this far from DC.
pub const VERIFIED_MIN_OFFSET_HZ: f64 = 2000.0;

/// On-run threshold, fraction of the plateau magnitude.
pub const ON_RUN_FRACTION: f64 = 0.3;

const KEY_FS: f64 = 96_000.0;
const FADE_FS: f64 = 8_000.0;
const FADE_CARRIER_HZ: f64 = 2_000.0;
const FADE_SECONDS: f64 = 60.0;
const FADE_DECIMATE: usize = 8;
/// Realizations per preset in the compact gate. 32 left the Good preset's
/// ensemble standard error at 0.0601 against the 0.06 ceiling, so the gate
/// averages 48 (MAN-206 D4: the averaging went up, not the bound).
pub const FADE_SEEDS: u64 = 48;

/// One measured comparison.
#[derive(Debug, Clone)]
pub struct CheckRow {
    pub dimension: &'static str,
    pub case: String,
    pub measured: f64,
    pub tolerance: f64,
    pub pass: bool,
}

/// Every row the gate measured.
#[derive(Debug, Clone, Default)]
pub struct CrossCheckReport {
    pub rows: Vec<CheckRow>,
}

impl CrossCheckReport {
    /// True when every row passed.
    pub fn passed(&self) -> bool {
        !self.rows.is_empty() && self.rows.iter().all(|r| r.pass)
    }

    /// Rows of `dimension` that failed.
    pub fn failed(&self, dimension: &str) -> Vec<&CheckRow> {
        self.rows
            .iter()
            .filter(|r| r.dimension == dimension && !r.pass)
            .collect()
    }

    /// Record `measured <= tolerance`.
    fn at_most(
        &mut self,
        dimension: &'static str,
        case: impl Into<String>,
        measured: f64,
        tol: f64,
    ) {
        self.rows.push(CheckRow {
            dimension,
            case: case.into(),
            measured,
            tolerance: tol,
            pass: measured.is_finite() && measured <= tol,
        });
    }
}

impl fmt::Display for CrossCheckReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "{:<10} {:<6} {:>14} {:>12}  case",
            "dimension", "result", "measured", "tolerance"
        )?;
        for r in &self.rows {
            writeln!(
                f,
                "{:<10} {:<6} {:>14.6e} {:>12.4e}  {}",
                r.dimension,
                if r.pass { "pass" } else { "FAIL" },
                r.measured,
                r.tolerance,
                r.case
            )?;
        }
        Ok(())
    }
}

/// Proof that the gate passed in this process. No public constructor.
#[derive(Debug)]
pub struct Verified {
    report: CrossCheckReport,
}

impl Verified {
    /// The passing report.
    pub fn report(&self) -> &CrossCheckReport {
        &self.report
    }
}

/// Run the compact gate once per process. `Ok` only when every row passed.
pub fn verified() -> Result<&'static Verified, &'static CrossCheckReport> {
    static GATE: OnceLock<std::result::Result<Verified, CrossCheckReport>> = OnceLock::new();
    match GATE.get_or_init(|| {
        let report = run_compact();
        if report.passed() {
            Ok(Verified { report })
        } else {
            Err(report)
        }
    }) {
        Ok(v) => Ok(v),
        Err(r) => Err(r),
    }
}

/// The one mapping from a production signal to the independent types.
pub(crate) fn tone_from_signal(sig: &SignalSpec) -> ToneSpec {
    ToneSpec {
        text: sig.text.clone(),
        loop_text: sig.loop_text,
        timing: Timing {
            wpm: sig.wpm as f64,
            char_wpm: sig.char_wpm.map(|c| c as f64),
            weight: sig.weight as f64,
            dit_units: sig.dit_units as f64,
            element_gap_units: sig.element_gap_units as f64,
            char_gap_units: sig.char_gap_units as f64,
            word_gap_units: sig.word_gap_units as f64,
            rise_s: sig.rise_ms / 1000.0,
            jitter: sig.jitter.map(|j| (j.sigma as f64, j.seed)),
        },
        offset_hz: sig.offset_hz,
        snr_2500_db: sig.snr_2500_db as f64,
        qsb_rate_hz: sig.qsb.map(|q| q.rate_hz as f64),
        fade: sig.watterson.map(|w| {
            let p = match w.preset {
                WattersonPreset::Good => FadeProfile::Good,
                WattersonPreset::Moderate => FadeProfile::Moderate,
                WattersonPreset::Poor => FadeProfile::Poor,
            };
            (p, w.seed)
        }),
    }
}

/// Render `spec`'s signals (with `spec.rise_ms`, as `vectors::render` does)
/// through the independent generator, with its own noise stream seeded
/// from `spec.noise_seed`. Requires the gate's [`Verified`] token.
pub fn independent_scene(
    v: &Verified,
    spec: &VectorSpec,
    duration_s: f64,
) -> Result<(Vec<Complex32>, Vec<String>)> {
    anyhow::ensure!(v.report.passed(), "cross-check report did not pass");
    let tones = spec
        .signals
        .iter()
        .map(|s| {
            tone_from_signal(&SignalSpec {
                rise_ms: spec.rise_ms,
                ..s.clone()
            })
        })
        .collect();
    baseband::render(&IndependentScene {
        fs: spec.fs,
        duration_s,
        noise_seed: Some(spec.noise_seed),
        tones,
    })
}

/// One signal's noise-free contribution from the independent generator,
/// for checks outside the gate (the receive-side analytic conversion).
/// Requires the gate's [`Verified`] token.
pub fn independent_tone(
    v: &Verified,
    sig: &SignalSpec,
    fs: f64,
    duration_s: f64,
) -> Result<(Vec<Complex32>, String)> {
    anyhow::ensure!(v.report.passed(), "cross-check report did not pass");
    baseband::render_tone(&tone_from_signal(sig), fs, duration_s)
}

fn signal(text: &str, loop_text: bool, wpm: f32, offset_hz: f64, snr: f32) -> SignalSpec {
    SignalSpec {
        text: text.into(),
        loop_text,
        wpm,
        offset_hz,
        snr_2500_db: snr,
        jitter: None,
        qsb: None,
        watterson: None,
        char_wpm: None,
        weight: 3.0,
        char_gap_units: 3.0,
        word_gap_units: 7.0,
        rise_ms: 5.0,
        dit_units: 1.0,
        element_gap_units: 1.0,
    }
}

fn db(ratio: f64) -> f64 {
    10.0 * ratio.log10()
}

/// The envelope cases of the compact gate.
pub(crate) fn envelope_cases() -> Vec<(&'static str, SignalSpec, f64)> {
    let v1 = signal("CQ CQ DE W1AW W1AW K", true, 20.0, 12_340.0, 20.0);
    let jit = SignalSpec {
        jitter: Some(Jitter {
            sigma: 0.08,
            seed: 0x206,
        }),
        ..signal("CQ TEST JA1ABC JA1ABC TEST", true, 35.0, 12_340.0, 15.0)
    };
    let farns = SignalSpec {
        char_wpm: Some(25.0),
        ..signal("CQ CQ DE G3ABC G3ABC K", true, 15.0, 12_340.0, 20.0)
    };
    let odd = SignalSpec {
        weight: 2.6,
        char_gap_units: 2.5,
        word_gap_units: 5.0,
        dit_units: 0.8,
        element_gap_units: 1.2,
        rise_ms: 0.5,
        jitter: Some(Jitter {
            sigma: 0.08,
            seed: 0x2060,
        }),
        ..signal("CQ TEST K1ABC K1ABC TEST", true, 28.0, -8_200.0, 20.0)
    };
    let once = signal("DE N1AA 5NN TU", false, 24.0, 5_600.0, 20.0);
    vec![
        ("V1 timing loop 10 s", v1, 10.0),
        ("35 WPM 8 % jitter loop 10 s", jit, 10.0),
        ("Farnsworth 15/25 10 s", farns, 10.0),
        (
            "weight 2.6, gaps 2.5/5, dit 0.8, elem 1.2, 0.5 ms, jitter",
            odd,
            10.0,
        ),
        ("key_text once", once, 5.0),
    ]
}

/// Envelope durations: production direct route vs the independent tone.
pub(crate) fn check_envelope(
    r: &mut CrossCheckReport,
    case: &str,
    sig: &SignalSpec,
    tone: &ToneSpec,
    fs: f64,
    duration_s: f64,
) -> Result<()> {
    let (p, ptext) = scene::render_signal_with_route(sig, fs, duration_s, Route::Direct)?;
    let (q, qtext) = baseband::render_tone(tone, fs, duration_s)?;
    let ap = crate::noise::amplitude_for_snr_2500(sig.snr_2500_db, fs) as f64;
    let aq = baseband::amplitude(tone.snr_2500_db, fs);
    let mp: Vec<f64> = p.iter().map(|c| c.norm() as f64 / ap).collect();
    let mq: Vec<f64> = q.iter().map(|c| c.norm() as f64 / aq).collect();
    // 0.3 of the plateau: never exactly on a sample of a raised-cosine
    // ramp (0.5 is, at the ramp midpoint, so rounding flips it).
    let rp = measure::on_runs(&mp, ON_RUN_FRACTION);
    let rq = measure::on_runs(&mq, ON_RUN_FRACTION);
    r.at_most(
        "envelope",
        format!("{case}: on-run count {} vs {}", rp.len(), rq.len()),
        (rp.len() as f64 - rq.len() as f64).abs(),
        0.0,
    );
    // Pairwise comparison is meaningful only when the counts agree.
    let counts_agree = rp.len() == rq.len();
    let mut worst = if counts_agree { 0.0f64 } else { f64::NAN };
    for (a, b) in rp.iter().zip(&rq).filter(|_| counts_agree) {
        worst = worst
            .max((a.0 as f64 - b.0 as f64).abs())
            .max((a.1 as f64 - b.1 as f64).abs());
    }
    r.at_most(
        "envelope",
        format!("{case}: start/duration, samples"),
        worst,
        ENVELOPE_SAMPLE_TOL,
    );
    let mag = mp
        .iter()
        .zip(&mq)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f64, f64::max);
    r.at_most(
        "envelope",
        format!("{case}: max magnitude difference"),
        mag,
        ENVELOPE_MAG_TOL,
    );
    r.at_most(
        "envelope",
        format!("{case}: keyed text equal"),
        if ptext == qtext { 0.0 } else { 1.0 },
        0.0,
    );
    Ok(())
}

/// Total power, ±100 Hz main band, per-bin PSD and image of `prod`
/// (shifted back by `delay` samples) vs `ind`, both noise-free.
pub(crate) fn check_power_and_spectra(
    r: &mut CrossCheckReport,
    case: &str,
    prod: &[Complex32],
    ind: &[Complex32],
    fs: f64,
    f0: f64,
    delay: usize,
) {
    let n = prod.len().min(ind.len()).saturating_sub(delay);
    let p = &prod[delay..delay + n];
    let q = &ind[..n];
    r.at_most(
        "power",
        format!("{case}: total power, dB"),
        db(measure::total_power(p) / measure::total_power(q)).abs(),
        POWER_DB_TOL,
    );
    let pp = measure::welch_psd(p, fs, 1 << 14);
    let pq = measure::welch_psd(q, fs, 1 << 14);
    let main_p = pp.band_power(f0 - 100.0, f0 + 100.0);
    let main_q = pq.band_power(f0 - 100.0, f0 + 100.0);
    r.at_most(
        "spectra",
        format!("{case}: main band, dB"),
        db(main_p / main_q).abs(),
        MAIN_BAND_DB_TOL,
    );
    let per_bin = pq
        .bins_within_db_of_peak(PER_BIN_RANGE_DB)
        .into_iter()
        .map(|k| db(pp.bins[k].max(1e-300) / pq.bins[k]).abs())
        .fold(0.0f64, f64::max);
    r.at_most(
        "spectra",
        format!("{case}: per bin within {PER_BIN_RANGE_DB} dB of peak, dB"),
        per_bin,
        PER_BIN_DB_TOL,
    );
    let image = pp.band_power(-f0 - 100.0, -f0 + 100.0);
    r.at_most(
        "image",
        format!("{case}: production image, dBc"),
        db(image.max(1e-300) / main_p),
        IMAGE_DBC_MAX,
    );
}

/// Power/spectra/image (and group delay for the real route) of one signal
/// on `route` vs the independent tone.
pub(crate) fn check_route(
    r: &mut CrossCheckReport,
    case: &str,
    sig: &SignalSpec,
    route: Route,
    duration_s: f64,
) -> Result<()> {
    let (p, _) = scene::render_signal_with_route(sig, KEY_FS, duration_s, route)?;
    let (q, _) = baseband::render_tone(&tone_from_signal(sig), KEY_FS, duration_s)?;
    let delay = match route {
        Route::Direct => 0,
        Route::RealPassband => {
            let lag = measure::group_delay_samples(&q, &p, 4 * HILBERT_GROUP_DELAY);
            r.at_most(
                "delay",
                format!("{case}: group delay {lag} vs {HILBERT_GROUP_DELAY} samples"),
                (lag - HILBERT_GROUP_DELAY as i64).abs() as f64,
                0.0,
            );
            HILBERT_GROUP_DELAY
        }
    };
    check_power_and_spectra(r, case, &p, &q, KEY_FS, sig.offset_hz, delay);
    Ok(())
}

/// Total power of one noise-free scene through `scene::render_scene`
/// (`MASTER_SCALE`) and the independent scene renderer (`HEADROOM`). The
/// per-signal rows compare unscaled contributions and the SNR rows cancel
/// the scale, so this row is the one that compares the two scales (on the
/// signal path; the noise floors agree through the SNR rows).
/// `scale_error_db` scales the independent scene (the row's negative
/// control).
pub(crate) fn check_scene_scale(
    r: &mut CrossCheckReport,
    case: &str,
    sig: &SignalSpec,
    duration_s: f64,
    scale_error_db: f64,
) -> Result<()> {
    let (p, _) = scene::render_scene(std::slice::from_ref(sig), KEY_FS, duration_s, None)?;
    let (mut q, _) = baseband::render(&IndependentScene {
        fs: KEY_FS,
        duration_s,
        noise_seed: None,
        tones: vec![tone_from_signal(sig)],
    })?;
    let gain = 10f64.powf(scale_error_db / 20.0) as f32;
    for z in &mut q {
        *z *= gain;
    }
    r.at_most(
        "power",
        format!("{case}: scene total power, HEADROOM vs MASTER_SCALE, dB"),
        db(measure::total_power(&p) / measure::total_power(&q)).abs(),
        POWER_DB_TOL,
    );
    Ok(())
}

/// Reference-bandwidth SNR and noise whiteness. `amp_error_db` scales the
/// independent carrier (the gate's negative control).
pub(crate) fn check_snr(
    r: &mut CrossCheckReport,
    case: &str,
    sig: &SignalSpec,
    duration_s: f64,
    noise_seed: u64,
    amp_error_db: f64,
) -> Result<()> {
    let fs = KEY_FS;
    let (pc, _) = scene::render_scene(std::slice::from_ref(sig), fs, duration_s, None)?;
    let (pn, _) = scene::render_scene(&[], fs, duration_s, Some(noise_seed))?;
    let mut tone = tone_from_signal(sig);
    tone.snr_2500_db += amp_error_db;
    let scene_of = |tones: Vec<ToneSpec>, noise_seed| {
        baseband::render(&IndependentScene {
            fs,
            duration_s,
            noise_seed,
            tones,
        })
    };
    let (qc, _) = scene_of(vec![tone], None)?;
    let (qn, _) = scene_of(vec![], Some(noise_seed))?;
    let want = sig.snr_2500_db as f64;
    let sp = measure::snr_2500_db(&pc, &pn, fs, sig.offset_hz);
    let sq = measure::snr_2500_db(&qc, &qn, fs, sig.offset_hz);
    r.at_most(
        "snr",
        format!("{case}: production vs request {want} dB"),
        (sp - want).abs(),
        SNR_REQUEST_DB_TOL,
    );
    r.at_most(
        "snr",
        format!("{case}: independent vs request {want} dB"),
        (sq - want).abs(),
        SNR_REQUEST_DB_TOL,
    );
    r.at_most(
        "snr",
        format!("{case}: production vs independent"),
        (sp - sq).abs(),
        SNR_PAIR_DB_TOL,
    );
    for (who, noise) in [("production", &pn), ("independent", &qn)] {
        let psd = measure::welch_psd(noise, fs, 1 << 12);
        let bands: Vec<f64> = (0..96)
            .map(|k| {
                let lo = -48_000.0 + 1000.0 * k as f64;
                psd.band_power(lo, lo + 1000.0 - 1e-9)
            })
            .collect();
        let mean = bands.iter().sum::<f64>() / bands.len() as f64;
        let worst = bands
            .iter()
            .map(|b| db(b / mean).abs())
            .fold(0.0f64, f64::max);
        r.at_most(
            "whiteness",
            format!("{case}: {who} 1 kHz bands vs mean, dB"),
            worst,
            WHITENESS_DB_TOL,
        );
    }
    Ok(())
}

fn fade_signal(preset: WattersonPreset, seed: u64) -> SignalSpec {
    // One 60 s dah: "T" at 0.06 WPM is 3 * 1.2 / 0.06 = 60 s.
    SignalSpec {
        watterson: Some(WattersonFade { preset, seed }),
        ..signal("T", false, 0.06, FADE_CARRIER_HZ, 20.0)
    }
}

/// Complex gain of a faded constant carrier: `out / (A e^{j2π f (i - d)/fs})`,
/// decimated to the fader's 1 kHz rate, edges trimmed.
fn demodulate(out: &[Complex32], amp: f64, delay: usize) -> Vec<Complex64> {
    let skip = 800;
    (delay + skip..out.len().saturating_sub(skip))
        .step_by(FADE_DECIMATE)
        .map(|i| {
            let c = Complex64::new(out[i].re as f64, out[i].im as f64);
            let cyc = FADE_CARRIER_HZ * (i - delay) as f64 / FADE_FS;
            c / Complex64::from_polar(amp, std::f64::consts::TAU * (cyc - cyc.floor()))
        })
        .collect()
}

pub(crate) fn production_fade_gains(
    preset: WattersonPreset,
    seeds: u64,
) -> Result<Vec<Vec<Complex64>>> {
    let amp = crate::noise::amplitude_for_snr_2500(20.0, FADE_FS) as f64;
    (0..seeds)
        .map(|s| {
            let sig = fade_signal(preset, 0x2060_0000 + s);
            let (out, _) = scene::render_signal(&sig, FADE_FS, FADE_SECONDS)?;
            Ok(demodulate(&out, amp, HILBERT_GROUP_DELAY))
        })
        .collect()
}

pub(crate) fn independent_fade_gains(
    preset: WattersonPreset,
    seeds: u64,
    sigma_scale: f64,
) -> Result<Vec<Vec<Complex64>>> {
    let amp = baseband::amplitude(20.0, FADE_FS);
    (0..seeds)
        .map(|s| {
            let tone = tone_from_signal(&fade_signal(preset, 0x2061_0000 + s));
            let (out, _) = baseband::render_tone_with(&tone, FADE_FS, FADE_SECONDS, sigma_scale)?;
            Ok(demodulate(&out, amp, 0))
        })
        .collect()
}

fn preset_name(p: WattersonPreset) -> &'static str {
    match p {
        WattersonPreset::Good => "Good",
        WattersonPreset::Moderate => "Moderate",
        WattersonPreset::Poor => "Poor",
    }
}

fn spread_hz(p: WattersonPreset) -> f64 {
    tone_from_signal(&fade_signal(p, 0))
        .fade
        .map(|(f, _)| f.spread_hz())
        .unwrap_or(0.0)
}

/// Lags, seconds, at which a preset is compared.
pub(crate) fn fade_lags_s(p: WattersonPreset) -> [f64; 3] {
    match p {
        WattersonPreset::Poor => [0.1, 0.2, 0.5],
        WattersonPreset::Moderate => [0.2, 0.4, 1.0],
        WattersonPreset::Good => [0.5, 1.0, 2.0],
    }
}

/// Fading autocorrelation against theory and between generators, and each
/// generator's ensemble power.
pub(crate) fn check_fading(
    r: &mut CrossCheckReport,
    preset: WattersonPreset,
    prod: &[Vec<Complex64>],
    ind: &[Vec<Complex64>],
) {
    let name = preset_name(preset);
    let sigma = spread_hz(preset) / 2.0;
    let rate = FADE_FS / FADE_DECIMATE as f64;
    let lags_s = fade_lags_s(preset);
    let lags: Vec<usize> = lags_s.iter().map(|t| (t * rate).round() as usize).collect();
    let rp = measure::fade_autocorr(prod, &lags);
    let rq = measure::fade_autocorr(ind, &lags);
    for (k, tau) in lags_s.iter().enumerate() {
        let theory = (-2.0 * std::f64::consts::PI.powi(2) * sigma * sigma * tau * tau).exp();
        r.at_most(
            "fading",
            format!(
                "{name} τ={tau} s: production {:.3} vs theory {theory:.3}",
                rp[k]
            ),
            (rp[k] - theory).abs(),
            FADE_THEORY_TOL,
        );
        r.at_most(
            "fading",
            format!(
                "{name} τ={tau} s: independent {:.3} vs theory {theory:.3}",
                rq[k]
            ),
            (rq[k] - theory).abs(),
            FADE_THEORY_TOL,
        );
        r.at_most(
            "fading",
            format!("{name} τ={tau} s: production vs independent"),
            (rp[k] - rq[k]).abs(),
            FADE_PAIR_TOL,
        );
    }
    for (who, g) in [("production", prod), ("independent", ind)] {
        let (mean, se) = measure::ensemble_power(g);
        r.at_most(
            "ensemble",
            format!("{name} {who}: |mean {mean:.3} - 1| in SE {se:.3}"),
            (mean - 1.0).abs() / se.max(1e-12),
            ENSEMBLE_SE_MULT,
        );
        r.at_most(
            "ensemble",
            format!("{name} {who}: standard error"),
            se,
            ENSEMBLE_SE_MAX,
        );
    }
}

/// The Morse table row: the decoder's table against the independent ITU
/// table. `+` is the documented exception (manta keys `.-.-.` as AR).
pub(crate) fn check_morse_table(r: &mut CrossCheckReport) {
    let mismatches = baseband::itu_characters()
        .into_iter()
        .filter(|(c, _)| *c != '+')
        .filter(|(c, p)| manta_decode::tree::pattern_for(*c) != Some(*p))
        .count();
    r.at_most(
        "morse",
        "decoder table vs ITU-R M.1677-1 (except '+')",
        mismatches as f64,
        0.0,
    );
}

fn run(full: bool) -> Result<CrossCheckReport> {
    let mut r = CrossCheckReport::default();
    check_morse_table(&mut r);
    for (case, sig, d) in envelope_cases() {
        check_envelope(&mut r, case, &sig, &tone_from_signal(&sig), KEY_FS, d)?;
    }
    let tone_at = |f: f64| signal("CQ CQ DE W1AW W1AW K", true, 20.0, f, 20.0);
    for f in [12_340.0, -8_200.0] {
        check_route(
            &mut r,
            &format!("direct {f} Hz"),
            &tone_at(f),
            Route::Direct,
            3.0,
        )?;
    }
    let qsb = SignalSpec {
        qsb: Some(QsbSine { rate_hz: 0.2 }),
        ..tone_at(9_100.0)
    };
    check_route(
        &mut r,
        "direct 9100 Hz, V6 QSB 0.2 Hz",
        &qsb,
        Route::Direct,
        10.0,
    )?;
    check_scene_scale(
        &mut r,
        "direct 12340 Hz scene",
        &tone_at(12_340.0),
        3.0,
        0.0,
    )?;
    let mut real_offsets = vec![12_340.0, 5_600.0, 2_000.0, -8_200.0];
    if full {
        real_offsets.clear();
        for k in 0..8 {
            let f = 2_000.0 + (45_000.0 - 2_000.0) * k as f64 / 7.0;
            real_offsets.push(f);
            real_offsets.push(-f);
        }
    }
    let real_duration = if full { 120.0 } else { 3.0 };
    for f in real_offsets {
        check_route(
            &mut r,
            &format!("129-tap real route {f} Hz"),
            &tone_at(f),
            Route::RealPassband,
            real_duration,
        )?;
    }
    for snr in [20.0f32, 0.0] {
        let sig = signal("CQ CQ DE W1AW W1AW K", true, 20.0, 12_340.0, snr);
        check_snr(
            &mut r,
            &format!("{snr} dB request, 3 s"),
            &sig,
            3.0,
            0x2062,
            0.0,
        )?;
    }
    let seeds = if full { 64 } else { FADE_SEEDS };
    let mut presets = vec![WattersonPreset::Poor, WattersonPreset::Good];
    if full {
        presets.push(WattersonPreset::Moderate);
    }
    for p in presets {
        let prod = production_fade_gains(p, seeds)?;
        let ind = independent_fade_gains(p, seeds, 1.0)?;
        check_fading(&mut r, p, &prod, &ind);
    }
    Ok(r)
}

fn run_or_error_row(full: bool) -> CrossCheckReport {
    run(full).unwrap_or_else(|e| CrossCheckReport {
        rows: vec![CheckRow {
            dimension: "render",
            case: format!("a generator returned an error: {e:#}"),
            measured: f64::NAN,
            tolerance: 0.0,
            pass: false,
        }],
    })
}

/// The compact gate (about 10 to 20 s): every dimension at the cases in the
/// decision record's tolerance table.
pub fn run_compact() -> CrossCheckReport {
    run_or_error_row(false)
}

/// The full sweep: real-route offsets from 2 kHz to 45 kHz in 8 steps of
/// both signs with 120 s keying, the Moderate preset, 64 seeds.
pub fn run_full() -> CrossCheckReport {
    run_or_error_row(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vectors;
    use std::collections::BTreeSet;

    #[test]
    fn generator_cross_check_passes() {
        match verified() {
            Ok(v) => eprintln!("{}", v.report()),
            Err(report) => panic!("generator cross-check failed:\n{report}"),
        }
    }

    #[test]
    fn independent_modules_import_no_production_code() {
        let forbidden = [
            "crate::keyer",
            "crate::scene",
            "crate::noise",
            "crate::vectors",
            "crate::u01",
            "crate::gaussian_pair",
            "manta_decode",
            "manta_dsp",
            "coppa_channel",
            "super::",
        ];
        for (name, src) in [
            ("baseband.rs", include_str!("baseband.rs")),
            ("measure.rs", include_str!("measure.rs")),
        ] {
            let body = src.split("#[cfg(test)]").next().unwrap();
            for f in forbidden {
                assert!(!body.contains(f), "{name} names {f} before its tests");
            }
        }
    }

    #[test]
    fn morse_tables_agree_with_itu() {
        let mut r = CrossCheckReport::default();
        check_morse_table(&mut r);
        assert!(r.passed(), "{r}");
    }

    fn all_vectors() -> Vec<VectorSpec> {
        use vectors::*;
        vec![
            v1(),
            v2(),
            v3(),
            v4(),
            v5(),
            v6(),
            v7(),
            v8(),
            v8w(),
            v9(),
            v10(),
            v34(),
            vr1(),
            vr2(),
            vr3(),
            vr4(),
            vr5(),
            vr6a(),
            vr6b(),
            vr7(),
            vr8(),
            vw1(),
            vw2(),
            vw3(),
            vw4(),
            vw5(),
        ]
    }

    #[test]
    fn faded_signals_inside_the_verified_domain() {
        let mut outside = BTreeSet::new();
        let vs = all_vectors();
        assert_eq!(vs.len(), 26);
        for v in &vs {
            for (i, s) in v.signals.iter().enumerate() {
                if s.watterson.is_some() && s.offset_hz.abs() < VERIFIED_MIN_OFFSET_HZ {
                    outside.insert((v.name.to_string(), format!("s{i:02}")));
                }
            }
        }
        for c in crate::sensitivity::Condition::ALL {
            for (i, s) in crate::sensitivity::series_signals(c, 20.0, 0, 0)
                .iter()
                .enumerate()
            {
                if s.watterson.is_some() && s.offset_hz.abs() < VERIFIED_MIN_OFFSET_HZ {
                    outside.insert((format!("sensitivity {}", c.name()), format!("s{i:02}")));
                }
            }
        }
        let want: BTreeSet<_> = [("v8w".to_string(), "s09".to_string())]
            .into_iter()
            .collect();
        assert_eq!(outside, want);
    }

    #[test]
    fn gate_catches_a_weight_mismatch() {
        let (case, sig, d) = envelope_cases().remove(0);
        let mut tone = tone_from_signal(&sig);
        tone.timing.weight = 3.3;
        let mut r = CrossCheckReport::default();
        check_envelope(&mut r, case, &sig, &tone, KEY_FS, d).unwrap();
        assert!(!r.failed("envelope").is_empty(), "{r}");
    }

    #[test]
    fn gate_catches_the_near_dc_hilbert_image() {
        let sig = signal("CQ CQ DE W1AW W1AW K", true, 20.0, 600.0, 20.0);
        let mut r = CrossCheckReport::default();
        check_route(
            &mut r,
            "129-tap real route 600 Hz",
            &sig,
            Route::RealPassband,
            3.0,
        )
        .unwrap();
        let img = r.failed("image");
        assert_eq!(img.len(), 1, "{r}");
        assert!(img[0].measured > -20.0, "{r}");
    }

    #[test]
    fn gate_catches_doppler_sigma_equal_to_spread() {
        let p = WattersonPreset::Poor;
        let good = independent_fade_gains(p, 32, 1.0).unwrap();
        let bad = independent_fade_gains(p, 32, 2.0).unwrap();
        let mut r = CrossCheckReport::default();
        check_fading(&mut r, p, &good, &bad);
        let failed = r.failed("fading");
        assert!(failed.iter().any(|row| row.case.contains("τ=0.2")), "{r}");
    }

    #[test]
    fn gate_catches_a_one_db_amplitude_error() {
        let sig = signal("CQ CQ DE W1AW W1AW K", true, 20.0, 12_340.0, 20.0);
        let mut r = CrossCheckReport::default();
        check_snr(&mut r, "20 dB, +1 dB carrier", &sig, 3.0, 0x2062, 1.0).unwrap();
        assert!(!r.failed("snr").is_empty(), "{r}");
    }

    #[test]
    fn gate_ties_headroom_to_master_scale() {
        let rows = &verified().expect("gate passes").report().rows;
        assert!(
            rows.iter()
                .any(|row| row.dimension == "power" && row.case.contains("HEADROOM")),
            "no HEADROOM vs MASTER_SCALE row"
        );
        let sig = signal("CQ CQ DE W1AW W1AW K", true, 20.0, 12_340.0, 20.0);
        let mut r = CrossCheckReport::default();
        check_scene_scale(&mut r, "scene, +0.1 dB scale", &sig, 3.0, 0.1).unwrap();
        let failed = r.failed("power");
        assert_eq!(failed.len(), 1, "{r}");
        assert!((failed[0].measured - 0.1).abs() < 1e-3, "{r}");
    }

    #[test]
    fn report_names_dimension_case_measured_and_tolerance() {
        let mut r = CrossCheckReport::default();
        r.at_most("power", "a case", 0.5, 0.01);
        let text = r.to_string();
        assert!(
            text.contains("power") && text.contains("FAIL") && text.contains("a case"),
            "{text}"
        );
        assert!(!r.passed());
    }

    #[test]
    #[ignore = "MAN-206 full cross-check sweep; run with --ignored"]
    fn generator_cross_check_full_sweep() {
        let t = std::time::Instant::now();
        let r = run_full();
        eprintln!("{r}\nfull sweep: {:.1} s", t.elapsed().as_secs_f64());
        assert!(r.passed(), "{r}");
    }
}
