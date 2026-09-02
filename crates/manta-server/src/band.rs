//! HF amateur-band lookup by frequency, for the JSON spot stream's `band`
//! field (dispensa `contracts/spots/spots.v1.schema.json`, ADR-0006 band
//! conventions).

const BANDS_HZ: &[(&str, f64, f64)] = &[
    ("2200m", 135_700.0, 137_800.0),
    ("630m", 472_000.0, 479_000.0),
    ("160m", 1_800_000.0, 2_000_000.0),
    ("80m", 3_500_000.0, 4_000_000.0),
    ("60m", 5_330_000.0, 5_407_000.0),
    ("40m", 7_000_000.0, 7_300_000.0),
    ("30m", 10_100_000.0, 10_150_000.0),
    ("20m", 14_000_000.0, 14_350_000.0),
    ("17m", 18_068_000.0, 18_168_000.0),
    ("15m", 21_000_000.0, 21_450_000.0),
    ("12m", 24_890_000.0, 24_990_000.0),
    ("10m", 28_000_000.0, 29_700_000.0),
    ("6m", 50_000_000.0, 54_000_000.0),
];

/// `"unknown"` for anything outside a recognized amateur HF/6m allocation
/// rather than panicking or guessing -- manta may be tuned to a segment
/// that doesn't map cleanly (e.g. a receiver test tone).
pub fn band_for_freq_hz(freq_hz: f64) -> &'static str {
    BANDS_HZ
        .iter()
        .find(|(_, lo, hi)| freq_hz >= *lo && freq_hz <= *hi)
        .map(|(name, _, _)| *name)
        .unwrap_or("unknown")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_20m_cw_segment() {
        assert_eq!(band_for_freq_hz(14_027_100.0), "20m");
    }

    #[test]
    fn classifies_40m_cw_segment() {
        assert_eq!(band_for_freq_hz(7_027_000.0), "40m");
    }

    #[test]
    fn classifies_band_edges_inclusive() {
        assert_eq!(band_for_freq_hz(14_000_000.0), "20m");
        assert_eq!(band_for_freq_hz(14_350_000.0), "20m");
    }

    #[test]
    fn frequency_outside_any_band_is_unknown() {
        assert_eq!(band_for_freq_hz(1_000.0), "unknown");
        assert_eq!(band_for_freq_hz(15_000_000.0), "unknown");
    }
}
