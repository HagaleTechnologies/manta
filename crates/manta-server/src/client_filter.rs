//! A telnet connection's own spot filter (MAN-546): `set/dx/filter cq`,
//! `band <list>` and `snr <n>`. One value per connection, held in that
//! connection's task, so no other client sees or changes it; see
//! docs/DECISIONS/2026-11-10-man546-telnet-client-filters.md.

use manta_spot::{Spot, SpotType};

/// Allocations from `band::allocations()`, one bit per table index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BandSet(u16);

impl BandSet {
    /// Adds the allocation at `index` in `band::allocations()`. An index
    /// past the table (or past the 16 bits this set holds) is ignored.
    pub fn insert(&mut self, index: usize) {
        debug_assert!(index < crate::band::allocations().len() && index < 16);
        if let Some(bit) = 1u16.checked_shl(index as u32) {
            self.0 |= bit;
        }
    }

    pub fn contains(self, index: usize) -> bool {
        1u16.checked_shl(index as u32)
            .is_some_and(|bit| self.0 & bit != 0)
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// The cq/band/snr filter one telnet connection set with `set/dx/filter`.
/// `Default` passes every spot. A clause left at its default does not
/// filter: no band clause means any band, `unknown` included.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SpotFilter {
    /// Only `SpotType::Cq` passes; DE, beacon and untyped spots do not.
    pub cq_only: bool,
    /// Only spots inside one of these allocations pass. A spot outside
    /// every allocation never passes a band clause.
    pub bands: BandSet,
    /// Compared with `rbn::wire_snr_db`, the 500 Hz whole-dB figure the
    /// row prints, so a row reading `10 dB` passes `snr 10`.
    pub min_snr_db: Option<i32>,
}

impl SpotFilter {
    /// Whether this connection's filter lets `spot` through.
    pub fn passes(&self, spot: &Spot) -> bool {
        (!self.cq_only || spot.spot_type == SpotType::Cq)
            && (self.bands.is_empty()
                || crate::band::allocation_index_for_freq_hz(spot.freq_hz)
                    .is_some_and(|i| self.bands.contains(i)))
            && self
                .min_snr_db
                .is_none_or(|min| crate::rbn::wire_snr_db(spot.snr_db) >= min)
    }
}

/// The clauses in command form and fixed order (`cq`, then `band` with
/// names in `band::allocations()` order, then `snr <n>`), or `off` for the
/// default. Serves the `Filter set: ...` ack and the audit log, and the
/// text is a command the client can send again.
impl std::fmt::Display for SpotFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut clauses: Vec<String> = Vec::new();
        if self.cq_only {
            clauses.push("cq".to_string());
        }
        if !self.bands.is_empty() {
            let names: Vec<&str> = crate::band::allocations()
                .iter()
                .enumerate()
                .filter(|(i, _)| self.bands.contains(*i))
                .map(|(_, (name, _, _))| *name)
                .collect();
            clauses.push(format!("band {}", names.join(",")));
        }
        if let Some(min) = self.min_snr_db {
            clauses.push(format!("snr {min}"));
        }
        if clauses.is_empty() {
            f.write_str("off")
        } else {
            f.write_str(&clauses.join(" "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use manta_spot::{EmissionState, SpotMode};

    fn index_of(name: &str) -> usize {
        crate::band::allocations()
            .iter()
            .position(|(n, _, _)| *n == name)
            .expect("allocation name")
    }

    fn bands(names: &[&str]) -> BandSet {
        let mut set = BandSet::default();
        for name in names {
            set.insert(index_of(name));
        }
        set
    }

    fn spot(spot_type: SpotType, freq_hz: f64, snr_db: f32) -> Spot {
        Spot {
            callsign: "K1ABC".to_string(),
            freq_hz,
            snr_db,
            wpm: 28.0,
            spot_type,
            confidence: 0.9,
            emission_state: EmissionState::Confirmed,
            track_id: 1,
            sample_ts: 0,
            rst: None,
            qrl_query: false,
            mode: SpotMode::Cw,
            baud: None,
        }
    }

    const ALL_TYPES: [SpotType; 4] = [
        SpotType::Cq,
        SpotType::De,
        SpotType::Beacon,
        SpotType::Unknown,
    ];

    #[test]
    fn the_default_filter_passes_every_spot() {
        let filter = SpotFilter::default();
        for spot_type in ALL_TYPES {
            assert!(filter.passes(&spot(spot_type, 14_027_000.0, 5.0)));
            assert!(filter.passes(&spot(spot_type, 15_000_000.0, 5.0)));
            assert!(filter.passes(&spot(spot_type, 7_027_000.0, -40.0)));
        }
    }

    #[test]
    fn cq_only_passes_only_the_cq_type() {
        let filter = SpotFilter {
            cq_only: true,
            ..SpotFilter::default()
        };
        assert!(filter.passes(&spot(SpotType::Cq, 14_027_000.0, 5.0)));
        for spot_type in [SpotType::De, SpotType::Beacon, SpotType::Unknown] {
            assert!(!filter.passes(&spot(spot_type, 14_027_000.0, 5.0)));
        }
    }

    #[test]
    fn a_band_clause_passes_listed_allocations_with_inclusive_edges() {
        let filter = SpotFilter {
            bands: bands(&["40m", "20m"]),
            ..SpotFilter::default()
        };
        let passes = |freq| filter.passes(&spot(SpotType::Cq, freq, 5.0));
        assert!(passes(7_000_000.0));
        assert!(passes(7_300_000.0));
        assert!(!passes(10_120_000.0));
        assert!(passes(14_350_000.0));
        assert!(!passes(15_000_000.0));
    }

    #[test]
    fn the_snr_clause_compares_the_rounded_500_hz_figure() {
        let at_least = |min| SpotFilter {
            min_snr_db: Some(min),
            ..SpotFilter::default()
        };
        let passes =
            |filter: SpotFilter, snr| filter.passes(&spot(SpotType::Cq, 14_027_000.0, snr));
        assert!(passes(at_least(10), 2.9));
        assert!(passes(at_least(10), 3.0));
        assert!(!passes(at_least(10), 2.5));
        assert!(passes(at_least(-5), -12.0));
        assert!(!passes(at_least(-5), -12.6));
    }

    #[test]
    fn clauses_combine() {
        let filter = SpotFilter {
            cq_only: true,
            bands: bands(&["40m"]),
            min_snr_db: Some(10),
        };
        assert!(!filter.passes(&spot(SpotType::De, 7_027_000.0, 12.0)));
        assert!(!filter.passes(&spot(SpotType::Cq, 14_027_000.0, 12.0)));
        assert!(!filter.passes(&spot(SpotType::Cq, 7_027_000.0, 2.5)));
        assert!(filter.passes(&spot(SpotType::Cq, 7_027_000.0, 2.9)));
    }

    #[test]
    fn display_renders_clauses_in_fixed_order() {
        assert_eq!(SpotFilter::default().to_string(), "off");
        let cq = SpotFilter {
            cq_only: true,
            ..SpotFilter::default()
        };
        assert_eq!(cq.to_string(), "cq");
        let band = SpotFilter {
            bands: bands(&["20m", "40m"]),
            ..SpotFilter::default()
        };
        assert_eq!(band.to_string(), "band 40m,20m");
        let all = SpotFilter {
            cq_only: true,
            bands: bands(&["20m", "40m"]),
            min_snr_db: Some(10),
        };
        assert_eq!(all.to_string(), "cq band 40m,20m snr 10");
        let negative = SpotFilter {
            min_snr_db: Some(-3),
            ..SpotFilter::default()
        };
        assert_eq!(negative.to_string(), "snr -3");
        let every: Vec<&str> = crate::band::allocations()
            .iter()
            .map(|(name, _, _)| *name)
            .collect();
        let mut reversed = every.clone();
        reversed.reverse();
        let every_band = SpotFilter {
            bands: bands(&reversed),
            ..SpotFilter::default()
        };
        assert_eq!(every_band.to_string(), format!("band {}", every.join(",")));
    }

    #[test]
    fn the_band_set_fits_the_allocation_table() {
        assert!(crate::band::allocations().len() <= 16);
    }
}
