//! Re-spot suppression. ARCHITECTURE §6.5. `sample_ts`-based (SPEC
//! -decode-core.md §6 rule 2). `BTreeMap`, never `HashMap` (rule 3).

use crate::context::SpotType;
use std::collections::BTreeMap;

const FREQ_BUCKET_HZ: f64 = 300.0;
const SUPPRESSION_SECONDS: f64 = 600.0;
const SNR_IMPROVEMENT_DB: f32 = 6.0;

struct LastSpot {
    sample_ts: u64,
    snr_db: f32,
    spot_type: SpotType,
}

pub struct Dedupe {
    suppression_window_samples: u64,
    last: BTreeMap<(String, i64), LastSpot>,
}

impl Dedupe {
    pub fn new(fs: f64) -> Self {
        Self {
            suppression_window_samples: (SUPPRESSION_SECONDS * fs) as u64,
            last: BTreeMap::new(),
        }
    }

    fn bucket(freq_hz: f64) -> i64 {
        (freq_hz / FREQ_BUCKET_HZ).round() as i64
    }

    /// True if a spot for this `(callsign, freq_hz)` should be emitted now
    /// -- no prior spot, the suppression window has elapsed, SNR improved
    /// by at least `SNR_IMPROVEMENT_DB`, or the spot type changed. Records
    /// the new spot as the latest one when it returns true.
    pub fn should_emit(
        &mut self,
        callsign: &str,
        freq_hz: f64,
        snr_db: f32,
        spot_type: SpotType,
        sample_ts: u64,
    ) -> bool {
        let key = (callsign.to_string(), Self::bucket(freq_hz));
        let emit = match self.last.get(&key) {
            None => true,
            Some(prev) => {
                sample_ts.saturating_sub(prev.sample_ts) >= self.suppression_window_samples
                    || snr_db - prev.snr_db >= SNR_IMPROVEMENT_DB
                    || spot_type != prev.spot_type
            }
        };
        if emit {
            self.last.insert(
                key,
                LastSpot {
                    sample_ts,
                    snr_db,
                    spot_type,
                },
            );
        }
        emit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 96_000.0;

    #[test]
    fn first_spot_always_emits() {
        let mut d = Dedupe::new(FS);
        assert!(d.should_emit("K5ARH", 14_027_000.0, 20.0, SpotType::Cq, 0));
    }

    #[test]
    fn immediate_repeat_same_snr_and_type_is_suppressed() {
        let mut d = Dedupe::new(FS);
        d.should_emit("K5ARH", 14_027_000.0, 20.0, SpotType::Cq, 0);
        assert!(!d.should_emit("K5ARH", 14_027_000.0, 20.0, SpotType::Cq, 1000));
    }

    #[test]
    fn snr_jump_overrides_suppression() {
        let mut d = Dedupe::new(FS);
        d.should_emit("K5ARH", 14_027_000.0, 20.0, SpotType::Cq, 0);
        assert!(d.should_emit("K5ARH", 14_027_000.0, 26.0, SpotType::Cq, 1000));
    }

    #[test]
    fn type_change_overrides_suppression() {
        let mut d = Dedupe::new(FS);
        d.should_emit("K5ARH", 14_027_000.0, 20.0, SpotType::Cq, 0);
        assert!(d.should_emit("K5ARH", 14_027_000.0, 20.0, SpotType::De, 1000));
    }

    #[test]
    fn window_elapsing_overrides_suppression() {
        let mut d = Dedupe::new(FS);
        d.should_emit("K5ARH", 14_027_000.0, 20.0, SpotType::Cq, 0);
        let window_samples = (SUPPRESSION_SECONDS * FS) as u64;
        assert!(d.should_emit(
            "K5ARH",
            14_027_000.0,
            20.0,
            SpotType::Cq,
            window_samples + 1
        ));
    }

    #[test]
    fn different_freq_bucket_is_independent() {
        let mut d = Dedupe::new(FS);
        d.should_emit("K5ARH", 14_027_000.0, 20.0, SpotType::Cq, 0);
        assert!(d.should_emit("K5ARH", 14_030_000.0, 20.0, SpotType::Cq, 1000));
    }
}
