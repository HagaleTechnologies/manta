//! `SKIMMER/SETT` reply model. MAN-86 / decision D1
//! (`docs/DECISIONS/2026-09-06-broad-review-decisions.md`): Aggregator will
//! not forward spots from a source that doesn't answer SETT (Aggregator
//! manual v6.0 §9.2). Wire format pinned by
//! `docs/DECISIONS/2026-09-07-man86-aggregator-sett-handshake.md`.

use std::fmt;

/// CW Skimmer's validation-level scale (RttySkimServ manual: 0=minimal,
/// 1=normal, 2=aggressive, 3=paranoid; "the RBN recommends a value of 1").
/// manta's own validator (`manta-spot`) has no equivalent scale, so only
/// the RBN-recommended level is ever emitted today -- an enum rather than a
/// literal so a real mapping stays a local change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationLevel {
    Minimal,
    Normal,
    Aggressive,
    Paranoid,
}

impl fmt::Display for ValidationLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ValidationLevel::Minimal => "vlMinimal",
            ValidationLevel::Normal => "vlNormal",
            ValidationLevel::Aggressive => "vlAggressive",
            ValidationLevel::Paranoid => "vlParanoid",
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SettSettings {
    pub validation_level: ValidationLevel,
    /// The optional `CQ` token -- CW Skimmer manual: "the CQ filter, IF
    /// ENABLED". Always false today; manta spots CQ, DE and beacon types.
    pub cq_only: bool,
    /// Decodable segments as `(lo_hz, hi_hz)`, ascending. Rendered in kHz
    /// to one decimal, comma-separated with no spaces.
    pub segments: Vec<(f64, f64)>,
}

impl fmt::Display for SettSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SETT: {}", self.validation_level)?;
        if self.cq_only {
            f.write_str(" CQ")?;
        }
        if !self.segments.is_empty() {
            f.write_str(" ")?;
            for (i, (lo, hi)) in self.segments.iter().enumerate() {
                if i > 0 {
                    f.write_str(",")?;
                }
                write!(f, "{:.1}-{:.1}", lo / 1000.0, hi / 1000.0)?;
            }
        }
        Ok(())
    }
}

/// The segments manta is actually decoding right now: the live passband
/// (`centre ± rate/2`) clipped to each amateur allocation it overlaps.
/// Skimmer Server reports what is CURRENTLY decodable, not what is
/// configured (verified against a real two-SkimServ SETT capture) -- for
/// manta, "currently decodable" IS the passband.
pub fn segments_for_passband(center_freq_hz: f64, sample_rate_hz: f64) -> Vec<(f64, f64)> {
    if !center_freq_hz.is_finite() || center_freq_hz <= 0.0 || !sample_rate_hz.is_finite() {
        return Vec::new();
    }
    let half = sample_rate_hz.abs() / 2.0;
    let (lo, hi) = (center_freq_hz - half, center_freq_hz + half);
    let clipped: Vec<(f64, f64)> = crate::band::allocations()
        .iter()
        .filter_map(|&(_, band_lo, band_hi)| {
            let (l, h) = (lo.max(band_lo), hi.min(band_hi));
            (l < h).then_some((l, h))
        })
        .collect();
    // A passband in no amateur allocation at all still gets a parseable,
    // truthful answer -- an empty list reads as "decoding nothing".
    if clipped.is_empty() {
        vec![(lo, hi)]
    } else {
        clipped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_the_cw_skimmer_manual_example_shape() {
        // CW Skimmer manual, Telnet Commands: `SETT: vlNormal CQ 14000.0-14070.0`.
        // manta has no CQ-only mode, so the optional CQ token is absent --
        // exactly as in the real 2015 SkimSrv capture with CqOnly=0
        // (skimmertalk 2015-December/001624).
        let s = SettSettings {
            validation_level: ValidationLevel::Normal,
            cq_only: false,
            segments: vec![(14_000_000.0, 14_070_000.0)],
        };
        assert_eq!(s.to_string(), "SETT: vlNormal 14000.0-14070.0");
    }

    #[test]
    fn formats_multiple_segments_comma_separated_without_spaces() {
        // Verbatim shape from a real two-SkimServ capture:
        // `SETT: vlNormal 7000.0-7040.0,14000.0-14070.0`
        let s = SettSettings {
            validation_level: ValidationLevel::Normal,
            cq_only: false,
            segments: vec![(7_000_000.0, 7_040_000.0), (14_000_000.0, 14_070_000.0)],
        };
        assert_eq!(
            s.to_string(),
            "SETT: vlNormal 7000.0-7040.0,14000.0-14070.0"
        );
    }

    #[test]
    fn cq_token_appears_between_level_and_segments_when_enabled() {
        // Not reachable today (manta has no CQ-only mode) but the field
        // exists so adding one later is a one-line change, not a format
        // rewrite.
        let s = SettSettings {
            validation_level: ValidationLevel::Normal,
            cq_only: true,
            segments: vec![(14_000_000.0, 14_070_000.0)],
        };
        assert_eq!(s.to_string(), "SETT: vlNormal CQ 14000.0-14070.0");
    }

    #[test]
    fn segments_for_passband_clip_to_the_enclosing_amateur_allocation() {
        // 192 kS/s centred on 14.091 MHz spans 13995.0-14187.0 kHz; the part
        // below 14000.0 is outside the 20m allocation and must not be claimed.
        let segs = segments_for_passband(14_091_000.0, 192_000.0);
        assert_eq!(segs, vec![(14_000_000.0, 14_187_000.0)]);
    }

    #[test]
    fn a_passband_wider_than_its_band_is_clipped_to_the_allocation_on_both_edges() {
        // 100 kHz around 10.125 MHz overruns the 50 kHz-wide 30m allocation
        // at BOTH ends; neither overrun may be claimed.
        let segs = segments_for_passband(10_125_000.0, 100_000.0);
        assert_eq!(segs, vec![(10_100_000.0, 10_150_000.0)]);
    }

    #[test]
    fn multiple_overlapped_allocations_produce_multiple_ascending_segments() {
        // No single real SDR passband straddles two amateur allocations --
        // they are tens of MHz apart -- so this is unreachable with one
        // source today. It is asserted anyway because the multi-segment
        // shape IS the real Skimmer Server wire format (`SETT: vlNormal
        // 7000.0-7040.0,14000.0-14070.0`, skimmertalk 2015-December/001624),
        // and MAN-13's multi-source model will reach it. Uses an
        // artificially wide span rather than pretending a real receiver
        // could do this.
        let segs = segments_for_passband(10_500_000.0, 8_000_000.0);
        assert_eq!(
            segs,
            vec![
                (7_000_000.0, 7_300_000.0),
                (10_100_000.0, 10_150_000.0),
                (14_000_000.0, 14_350_000.0),
            ]
        );
    }

    #[test]
    fn a_passband_in_no_amateur_allocation_falls_back_to_the_raw_passband() {
        // A receiver test tone / bogus --dial-freq-hz must still produce a
        // parseable reply -- an empty segment list reads as "decoding
        // nothing".
        let segs = segments_for_passband(15_000_000.0, 48_000.0);
        assert_eq!(segs, vec![(14_976_000.0, 15_024_000.0)]);
    }

    #[test]
    fn a_zero_or_non_finite_centre_frequency_yields_no_claimed_segment() {
        // `AudioIqSource::center_freq_hz()` returns 0.0 when --dial-freq-hz
        // was not given; claiming a segment around DC would be a lie.
        assert!(segments_for_passband(0.0, 48_000.0).is_empty());
        assert!(segments_for_passband(f64::NAN, 48_000.0).is_empty());
    }

    #[test]
    fn an_empty_segment_list_still_renders_a_well_formed_reply() {
        let s = SettSettings {
            validation_level: ValidationLevel::Normal,
            cq_only: false,
            segments: vec![],
        };
        assert_eq!(s.to_string(), "SETT: vlNormal");
    }
}
