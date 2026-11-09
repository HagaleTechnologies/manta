//! MAN-541: the RBN skew reading. Turns the live comparator's per-spot
//! frequency offsets ([`super::grade::SpotGrade::freq_offset_hz`]) into a
//! per-band skew in parts per million with its precision, a suggested
//! `freq_correction_ppm` per config entry, and the held over-limit state
//! behind `/healthz`'s `WARN rbn skew` line and
//! `manta_rbn_compare_skew_warning`.
//!
//! Pure functions plus a small tracker, so `metrics.rs` only stores the
//! samples and calls in (the `health.rs` precedent). The decisions behind
//! every constant are in `docs/DECISIONS/2026-11-09-man541-rbn-skew.md`.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use super::grade::RBN_RESOLUTION_HZ;

/// A band reading (and a band's contribution to a suggestion) needs this
/// many graded spots in the window (D5).
pub const MIN_SKEW_SAMPLES: usize = 20;
/// A band warns once it has been over the limit this long without a break
/// (D6).
pub const SKEW_HOLD: Duration = Duration::from_secs(1800);
/// `[rbn_compare].skew_warn_ppm`'s default.
pub const DEFAULT_SKEW_WARN_PPM: f64 = 2.0;
/// A band warns only when its `ppm_error` is at most limit / this (D6).
pub const WARN_PRECISION_DIVISOR: f64 = 3.0;
/// `calibrate --from-rbn` saves only a pooled reading at least this
/// precise (D8).
pub const MAX_SAVE_ERROR_PPM: f64 = 1.0;

/// One graded spot's offset from RBN's modal frequency, and that modal
/// frequency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub offset_hz: i64,
    pub modal_hz: i64,
}

/// One band's reading over a set of samples.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BandReading {
    pub samples: usize,
    /// Median of the Hz offsets, exactly as
    /// `manta_rbn_compare_freq_offset_median_hz` computes it.
    pub median_offset_hz: f64,
    /// Median of the per-spot ppm; `None` under [`MIN_SKEW_SAMPLES`].
    pub median_ppm: Option<f64>,
    /// [`ppm_error`] for the reading; `None` under [`MIN_SKEW_SAMPLES`].
    pub ppm_error: Option<f64>,
}

/// One spot's skew: `offset / modal × 10⁶`. Positive means manta reads
/// high. `None` for a non-positive modal frequency.
pub fn sample_ppm(s: &Sample) -> Option<f64> {
    (s.modal_hz > 0).then(|| s.offset_hz as f64 / s.modal_hz as f64 * 1e6)
}

/// The precision of a median over `samples` spots near `freq_hz`: half of
/// RBN's 100 Hz resolution over `√n`, in ppm of the frequency (D4).
pub fn ppm_error(samples: usize, freq_hz: f64) -> f64 {
    (RBN_RESOLUTION_HZ / 2.0) / (samples as f64).sqrt() / freq_hz * 1e6
}

/// Median; an even count averages the middle two. `0.0` for no values.
pub fn median_i64(values: &mut [i64]) -> f64 {
    values.sort_unstable();
    let n = values.len();
    if n == 0 {
        0.0
    } else if n % 2 == 1 {
        values[n / 2] as f64
    } else {
        (values[n / 2 - 1] as f64 + values[n / 2] as f64) / 2.0
    }
}

/// Median of finite values; an even count averages the middle two.
/// `None` for no values.
pub fn median_f64(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let n = values.len();
    Some(if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    })
}

/// The reading over one band's samples.
pub fn band_reading(samples: &[Sample]) -> BandReading {
    let mut offsets: Vec<i64> = samples.iter().map(|s| s.offset_hz).collect();
    let median_offset_hz = median_i64(&mut offsets);
    let (median_ppm, error) = if samples.len() >= MIN_SKEW_SAMPLES {
        let mut ppms: Vec<f64> = samples.iter().filter_map(sample_ppm).collect();
        let mut modals: Vec<i64> = samples.iter().map(|s| s.modal_hz).collect();
        let f_med = median_i64(&mut modals);
        match median_f64(&mut ppms) {
            Some(m) if f_med > 0.0 => (Some(m), Some(ppm_error(samples.len(), f_med))),
            _ => (None, None),
        }
    } else {
        (None, None)
    };
    BandReading {
        samples: samples.len(),
        median_offset_hz,
        median_ppm,
        ppm_error: error,
    }
}

/// Inverse-variance weighted mean of the readings that have a ppm, and
/// its error `1/√Σw` (D7). `None` when no reading qualifies.
pub fn pooled(readings: &[BandReading]) -> Option<(f64, f64)> {
    let mut sum_w = 0.0;
    let mut sum_wx = 0.0;
    for r in readings {
        if let (Some(ppm), Some(err)) = (r.median_ppm, r.ppm_error) {
            if err > 0.0 && err.is_finite() {
                let w = 1.0 / (err * err);
                sum_w += w;
                sum_wx += w * ppm;
            }
        }
    }
    (sum_w > 0.0).then(|| (sum_wx / sum_w, 1.0 / sum_w.sqrt()))
}

/// The `freq_correction_ppm` that cancels an observed skew, given the
/// value the daemon applies while it measured: the reported frequency is
/// already scaled by `1 + applied`, so the new value replaces the old one
/// and is never added to it (D2). Rounded as saved.
pub fn suggested_correction_ppm(applied_ppm: f64, observed_ppm: f64) -> f64 {
    manta_spot::round_ppm(((1.0 + applied_ppm * 1e-6) / (1.0 + observed_ppm * 1e-6) - 1.0) * 1e6)
}

/// Whether a band reading is over the limit: at least
/// [`MIN_SKEW_SAMPLES`] spots, a positive limit, an error at most a
/// [`WARN_PRECISION_DIVISOR`]th of it, and `|skew|` past it (D6).
pub fn is_over(r: &BandReading, limit_ppm: f64) -> bool {
    let (Some(ppm), Some(err)) = (r.median_ppm, r.ppm_error) else {
        return false;
    };
    r.samples >= MIN_SKEW_SAMPLES
        && limit_ppm > 0.0
        && err <= limit_ppm / WARN_PRECISION_DIVISOR
        && ppm.abs() > limit_ppm
}

/// Since when each band has been over the limit without a break.
#[derive(Debug, Default)]
pub struct SkewTracker {
    over_since: BTreeMap<&'static str, Instant>,
}

impl SkewTracker {
    /// Records one evaluation: an over band keeps its start (or starts
    /// now); a band not over is cleared.
    pub fn observe(&mut self, band: &'static str, over: bool, now: Instant) {
        if over {
            self.over_since.entry(band).or_insert(now);
        } else {
            self.over_since.remove(band);
        }
    }

    /// Forgets every band `present` says has no samples left.
    pub fn retain_bands(&mut self, present: impl Fn(&str) -> bool) {
        self.over_since.retain(|band, _| present(band));
    }

    /// How long the band has been over, `None` when it is not.
    pub fn over_for(&self, band: &str, now: Instant) -> Option<Duration> {
        self.over_since
            .get(band)
            .map(|since| now.saturating_duration_since(*since))
    }

    /// Over for [`SKEW_HOLD`] or longer.
    pub fn warning(&self, band: &str, now: Instant) -> bool {
        self.over_for(band, now).is_some_and(|d| d >= SKEW_HOLD)
    }
}

/// One input the daemon runs, as `/status` attributes spots to it.
#[derive(Debug, Clone, PartialEq)]
pub struct RbnSkewInput {
    /// The daemon's name for the input (stream).
    pub name: String,
    /// The `[[input]]` entry name; `None` for the `[input]` table.
    pub entry: Option<String>,
    /// The `freq_correction_ppm` the daemon applies to it.
    pub applied_ppm: f64,
    /// The daemon's receiver came from a command-line source flag, not
    /// from the config file.
    pub replaced_by_flag: bool,
}

/// Input index owning `track_id` among `n` inputs: input `i` owns tracks
/// `1+i, 1+i+n, …` (MAN-13 D3). `None` for track 0 or no inputs.
pub fn input_of_track(track_id: u32, n: usize) -> Option<usize> {
    (n > 0 && track_id >= 1).then(|| (track_id as usize - 1) % n)
}

/// Rounds to 0.001, never `-0.0`.
pub fn round3(v: f64) -> f64 {
    let r = (v * 1000.0).round() / 1000.0;
    if r == 0.0 {
        0.0
    } else {
        r
    }
}

/// `+3.0 ±0.6 ppm`.
pub fn skew_text(ppm: f64, error: f64) -> String {
    format!("{ppm:+.1} ±{error:.1} ppm")
}

/// `+42 Hz`.
pub fn offset_text(hz: f64) -> String {
    format!("{hz:+.0} Hz")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples(n: usize, offset_hz: i64, modal_hz: i64) -> Vec<Sample> {
        vec![
            Sample {
                offset_hz,
                modal_hz
            };
            n
        ]
    }

    /// A band reading with `n` spots reading `ppm` high at `freq_hz`.
    fn reading_at(n: usize, ppm: f64, freq_hz: i64) -> BandReading {
        let offset = (ppm * freq_hz as f64 / 1e6).round() as i64;
        let mut r = band_reading(&samples(n, offset, freq_hz));
        r.median_ppm = Some(ppm);
        r
    }

    #[test]
    fn sample_ppm_is_the_offset_over_the_modal_frequency() {
        let ppm = sample_ppm(&Sample {
            offset_hz: 42,
            modal_hz: 14_025_000,
        })
        .unwrap();
        assert!((ppm - 2.9947).abs() < 1e-4, "{ppm}");
        assert_eq!(
            sample_ppm(&Sample {
                offset_hz: 42,
                modal_hz: 0
            }),
            None
        );
    }

    #[test]
    fn ppm_error_is_half_the_rbn_step_over_root_n() {
        assert!((ppm_error(40, 14.03e6) - 0.563).abs() < 1e-3);
        assert!((ppm_error(20, 3.55e6) - 3.149).abs() < 1e-3);
    }

    #[test]
    fn a_band_reading_needs_20_samples() {
        let r = band_reading(&samples(19, 42, 14_025_000));
        assert_eq!(r.median_ppm, None);
        assert_eq!(r.ppm_error, None);
        assert_eq!(r.median_offset_hz, 42.0);
        assert_eq!(r.samples, 19);
        let r = band_reading(&samples(MIN_SKEW_SAMPLES, 42, 14_025_000));
        assert!(r.median_ppm.is_some() && r.ppm_error.is_some());
    }

    #[test]
    fn the_band_median_averages_the_middle_two() {
        let s = [
            Sample {
                offset_hz: 10,
                modal_hz: 14_000_000,
            },
            Sample {
                offset_hz: 40,
                modal_hz: 14_000_000,
            },
            Sample {
                offset_hz: 20,
                modal_hz: 14_000_000,
            },
            Sample {
                offset_hz: 30,
                modal_hz: 14_000_000,
            },
        ];
        assert_eq!(band_reading(&s).median_offset_hz, 25.0);
        assert_eq!(median_i64(&mut [3, 1, 2]), 2.0);
        assert_eq!(median_i64(&mut []), 0.0);
    }

    #[test]
    fn pooling_weights_bands_by_precision() {
        let readings = [
            reading_at(41, 3.8, 3_550_000),
            reading_at(8, 9.0, 7_025_000),
            reading_at(40, 3.0, 14_030_000),
            reading_at(24, 3.0, 21_030_000),
        ];
        let (ppm, err) = pooled(&readings).unwrap();
        assert!((ppm - 3.022).abs() < 1e-3, "{ppm}");
        assert!((err - 0.363).abs() < 1e-3, "{err}");
        assert_eq!(pooled(&[band_reading(&samples(19, 42, 14_025_000))]), None);
        assert_eq!(pooled(&[]), None);
    }

    #[test]
    fn the_suggestion_replaces_rather_than_adds() {
        assert_eq!(suggested_correction_ppm(0.0, 3.0), -3.0);
        assert_eq!(suggested_correction_ppm(-2.0, 1.0), -3.0);
        assert_eq!(suggested_correction_ppm(1.0, -0.5), 1.5);
    }

    /// MAN-127 decision 5: a receiver reading 14.2 Hz high at 10 MHz needs
    /// −1.42 ppm.
    #[test]
    fn the_suggestion_has_calibrates_sign() {
        let observed = 14.2 / 10e6 * 1e6;
        assert_eq!(suggested_correction_ppm(0.0, observed), -1.42);
    }

    #[test]
    fn warning_needs_a_reading_precise_to_a_third_of_the_limit() {
        assert!(!is_over(&reading_at(28, 3.0, 14_025_000), 2.0));
        assert!(is_over(&reading_at(29, 3.0, 14_025_000), 2.0));
        assert!(!is_over(&reading_at(113, 3.0, 7_025_000), 2.0));
        assert!(is_over(&reading_at(114, 3.0, 7_025_000), 2.0));
        assert!(is_over(&reading_at(114, -3.0, 7_025_000), 2.0));
        assert!(!is_over(&reading_at(114, 1.9, 7_025_000), 2.0));
    }

    #[test]
    fn a_zero_limit_never_warns() {
        assert!(!is_over(&reading_at(5000, 50.0, 14_025_000), 0.0));
    }

    #[test]
    fn the_tracker_warns_after_30_minutes_over() {
        let t0 = Instant::now();
        let mut t = SkewTracker::default();
        let mut s = 0;
        while s <= 1800 {
            let now = t0 + Duration::from_secs(s);
            t.observe("20m", true, now);
            if s == 1795 {
                assert!(!t.warning("20m", now));
                assert_eq!(t.over_for("20m", now), Some(Duration::from_secs(1795)));
            }
            s += 5;
        }
        assert!(t.warning("20m", t0 + SKEW_HOLD));
        assert_eq!(t.over_for("40m", t0), None);
    }

    #[test]
    fn the_tracker_clears_when_the_condition_lapses() {
        let t0 = Instant::now();
        let mut t = SkewTracker::default();
        let mut s = 0;
        while s <= 3005 {
            let now = t0 + Duration::from_secs(s);
            t.observe("20m", s != 1200, now);
            if s == 1800 || s == 3000 {
                assert!(!t.warning("20m", now), "{s}");
            }
            s += 5;
        }
        // Over again from the next step, t0+20m05s: held at t0+50m05s.
        assert!(t.warning("20m", t0 + Duration::from_secs(3005)));
    }

    #[test]
    fn the_tracker_forgets_a_band_with_no_samples() {
        let t0 = Instant::now();
        let mut t = SkewTracker::default();
        t.observe("20m", true, t0);
        t.observe("40m", true, t0);
        t.retain_bands(|b| b == "40m");
        assert_eq!(t.over_for("20m", t0), None);
        assert!(t.over_for("40m", t0).is_some());
    }

    #[test]
    fn tracks_belong_to_inputs_round_robin() {
        assert_eq!(input_of_track(1, 2), Some(0));
        assert_eq!(input_of_track(2, 2), Some(1));
        assert_eq!(input_of_track(3, 2), Some(0));
        assert_eq!(input_of_track(7, 1), Some(0));
        assert_eq!(input_of_track(0, 2), None);
        assert_eq!(input_of_track(1, 0), None);
    }
}
