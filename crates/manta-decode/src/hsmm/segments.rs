//! Segment types, duration and type priors. SPEC v2 §4.2–4.4.
use super::token::Phase;
use super::HsmmConfig;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SegType {
    Dit,
    Dah,
    EGap,
    CGap,
    WGap,
    Silence,
}

impl SegType {
    pub const ALL: [SegType; 6] = [
        SegType::Dit,
        SegType::Dah,
        SegType::EGap,
        SegType::CGap,
        SegType::WGap,
        SegType::Silence,
    ];

    pub fn is_mark(self) -> bool {
        matches!(self, SegType::Dit | SegType::Dah)
    }

    /// Nominal length in dits.
    pub fn k(self) -> f32 {
        match self {
            SegType::Dit | SegType::EGap => 1.0,
            SegType::Dah | SegType::CGap => 3.0,
            SegType::WGap => 7.0,
            SegType::Silence => 10.0,
        }
    }

    pub fn updates_speed(self) -> bool {
        !matches!(self, SegType::WGap | SegType::Silence)
    }

    /// The phase a token must be in to take this segment. Named per the
    /// task's "Produces" interface; the brief's Step 3/4 reference code
    /// calls this `from_phase` but never declares it in the public API —
    /// `ends_phase` is the contract Task 8 consumes, so that name wins.
    pub fn ends_phase(self) -> Phase {
        if self.is_mark() {
            Phase::AfterSpace
        } else {
            Phase::AfterMark
        }
    }

    pub fn log_type_prior(self, cfg: &HsmmConfig) -> f32 {
        match self {
            SegType::Dit => 0.6f32.ln() + cfg.mark_insert_penalty,
            SegType::Dah => 0.4f32.ln() + cfg.mark_insert_penalty,
            SegType::EGap => 0.62f32.ln(),
            SegType::CGap => 0.28f32.ln(),
            SegType::WGap => 0.10f32.ln(),
            SegType::Silence => 0.10f32.ln() - 4.0,
        }
    }

    /// Log-normal duration prior about k*u; None outside [0.6, 1.5]*nominal
    /// (Silence: flat on [10u, 80u]). SPEC v2 §4.3.
    pub fn log_dur_prior(self, d: u32, u: f32, cfg: &HsmmConfig) -> Option<f32> {
        let d = d as f32;
        if self == SegType::Silence {
            return if d >= 10.0 * u && d <= 80.0 * u {
                Some(0.0)
            } else {
                None
            };
        }
        let nom = self.k() * u;
        if d < 0.6 * nom || d > 1.5 * nom {
            return None;
        }
        let x = (d / nom).ln();
        Some(-(x * x) / (2.0 * cfg.dur_sigma * cfg.dur_sigma))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_prior_peaks_at_nominal_and_is_bounded() {
        let cfg = HsmmConfig::default();
        assert_eq!(SegType::Dah.log_dur_prior(39, 13.0, &cfg), Some(0.0));
        // [Task 7 fix]: brief's assertion (`< -1.0`) is unsatisfiable given the
        // spec-correct formula (SPEC v2 §4.3: `-(ln(d/(k*u)))^2 / (2*sigma^2)`,
        // sigma = dur_sigma = 0.22 default). At d=30, nom=3*13=39:
        // ln(30/39) = -0.262364, squared / (2*0.22^2) = 0.711105, so the value
        // is -0.7111053 (verified by direct computation) -- well short of
        // -1.0. Relaxed to a bound the correct formula actually satisfies
        // while still asserting a meaningful penalty away from the peak.
        assert!(SegType::Dah.log_dur_prior(30, 13.0, &cfg).unwrap() < -0.5);
        assert_eq!(SegType::Dah.log_dur_prior(20, 13.0, &cfg), None); // < 0.6 * 39
        assert_eq!(SegType::Dah.log_dur_prior(60, 13.0, &cfg), None); // > 1.5 * 39
        assert_eq!(SegType::Silence.log_dur_prior(200, 13.0, &cfg), Some(0.0));
        assert_eq!(SegType::Silence.log_dur_prior(100, 13.0, &cfg), None); // < 10u
    }

    #[test]
    fn mark_type_prior_includes_insertion_penalty() {
        let cfg = HsmmConfig::default();
        assert!((SegType::Dit.log_type_prior(&cfg) - (0.6f32.ln() - 1.5)).abs() < 1e-6);
        assert!((SegType::WGap.log_type_prior(&cfg) - 0.10f32.ln()).abs() < 1e-6);
    }
}
