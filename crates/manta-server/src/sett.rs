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
/// clipped to each amateur allocation it overlaps. Skimmer Server reports
/// what is CURRENTLY decodable, not what is configured (verified against a
/// real two-SkimServ SETT capture) -- for manta, "currently decodable" IS
/// the passband.
///
/// `passband_hz` is the source's own RF passband as `(lo, hi)` offsets in
/// Hz from `center_freq_hz` (`IqSource::rf_passband_hz`), NOT a width
/// derived from its processing sample rate, and NOT assumed symmetric.
/// MAN-86 review: a KiwiSDR delivers `low_cut=-5000`/`high_cut=5000`
/// upsampled to 96 kS/s, and a rig's audio output delivers roughly
/// `+300 .. +3000` Hz ABOVE the dial frequency (ARCHITECTURE §3) -- reading
/// either as +/- half the sample rate advertised tens of kHz of coverage
/// Aggregator would then expect spots from, half of it on the wrong side
/// of the dial.
///
/// `freq_calibration` is the same multiplicative factor
/// (`manta_spot::calibration_factor_from_ppm`, config key
/// `input.freq_correction_ppm`) the validator applies to every emitted
/// spot frequency. MAN-86 review: without it the advertised bounds are
/// uncorrected while the spots inside them are corrected, so at the
/// supported +/-1000 ppm limit a 20 m segment is off by ~14 kHz and can
/// exclude frequencies from manta's own spot stream.
pub fn segments_for_passband(
    center_freq_hz: f64,
    passband_hz: (f64, f64),
    freq_calibration: f64,
) -> Vec<(f64, f64)> {
    let (lo_off, hi_off) = passband_hz;
    // `center_freq_hz <= 0.0` is the "this source has no RF reference"
    // case (`AudioIqSource` without `--dial-freq-hz`): claiming a segment
    // around DC would be a lie. The calibration guard is unreachable by
    // construction -- both the CLI value parser and `listen()` reject an
    // out-of-range ppm before any source is opened -- but a silently wrong
    // coverage claim is worse than none at all.
    if !center_freq_hz.is_finite()
        || center_freq_hz <= 0.0
        || !lo_off.is_finite()
        || !hi_off.is_finite()
        || hi_off <= lo_off
        || !freq_calibration.is_finite()
        || freq_calibration <= 0.0
    {
        return Vec::new();
    }
    let lo = (center_freq_hz + lo_off) * freq_calibration;
    let hi = (center_freq_hz + hi_off) * freq_calibration;
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

    /// A symmetric `centre +/- bandwidth/2` passband -- what every
    /// non-resampling source (file, SoapySDR, HPSDR) reports, and the
    /// shape these tests were written against before asymmetric
    /// rig-audio bounds existed.
    fn symmetric(bandwidth_hz: f64) -> (f64, f64) {
        (-bandwidth_hz / 2.0, bandwidth_hz / 2.0)
    }

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
        let segs = segments_for_passband(14_091_000.0, symmetric(192_000.0), 1.0);
        assert_eq!(segs, vec![(14_000_000.0, 14_187_000.0)]);
    }

    #[test]
    fn a_passband_wider_than_its_band_is_clipped_to_the_allocation_on_both_edges() {
        // 100 kHz around 10.125 MHz overruns the 50 kHz-wide 30m allocation
        // at BOTH ends; neither overrun may be claimed.
        let segs = segments_for_passband(10_125_000.0, symmetric(100_000.0), 1.0);
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
        let segs = segments_for_passband(10_500_000.0, symmetric(8_000_000.0), 1.0);
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
    fn a_narrow_rf_bandwidth_is_not_widened_to_the_processing_sample_rate() {
        // MAN-86 review: a KiwiSDR is asked for low_cut=-5000/high_cut=5000
        // and its 12 kS/s stream is upsampled to 96 kS/s, so the honest
        // answer is centre +/-5 kHz. The 96 kS/s answer (13992.0-14088.0,
        // clipped to 14000.0-14088.0) would have Aggregator expecting
        // spots from 88 kHz of spectrum manta cannot hear.
        assert_eq!(
            segments_for_passband(14_040_000.0, symmetric(10_000.0), 1.0),
            vec![(14_035_000.0, 14_045_000.0)]
        );
        assert_ne!(
            segments_for_passband(14_040_000.0, symmetric(10_000.0), 1.0),
            segments_for_passband(14_040_000.0, symmetric(96_000.0), 1.0)
        );
    }

    #[test]
    fn a_passband_in_no_amateur_allocation_falls_back_to_the_raw_passband() {
        // A receiver test tone / bogus --dial-freq-hz must still produce a
        // parseable reply -- an empty segment list reads as "decoding
        // nothing".
        let segs = segments_for_passband(15_000_000.0, symmetric(48_000.0), 1.0);
        assert_eq!(segs, vec![(14_976_000.0, 15_024_000.0)]);
    }

    #[test]
    fn a_zero_or_non_finite_centre_frequency_yields_no_claimed_segment() {
        // `AudioIqSource::center_freq_hz()` returns 0.0 when --dial-freq-hz
        // was not given; claiming a segment around DC would be a lie.
        assert!(segments_for_passband(0.0, symmetric(48_000.0), 1.0).is_empty());
        assert!(segments_for_passband(f64::NAN, symmetric(48_000.0), 1.0).is_empty());
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

    #[test]
    fn an_asymmetric_rig_audio_passband_is_advertised_only_above_the_dial() {
        // MAN-86 review: `AudioIqSource` Hilbert-transforms real rig audio,
        // so its decodable tones are POSITIVE offsets from --dial-freq-hz
        // over roughly a 3 kHz passband (ARCHITECTURE §3). Treating that as
        // +/- half the 48 kHz sample rate advertised dial +/-24 kHz -- most
        // of it spectrum the rig cannot pass, and half of it on the wrong
        // side of the dial entirely.
        let segs = segments_for_passband(14_040_000.0, (300.0, 3_000.0), 1.0);
        assert_eq!(segs, vec![(14_040_300.0, 14_043_000.0)]);
        assert_ne!(
            segs,
            segments_for_passband(14_040_000.0, symmetric(48_000.0), 1.0)
        );
    }

    #[test]
    fn frequency_calibration_moves_both_segment_edges_with_the_spots() {
        // MAN-86 review: `manta-engine::listen` multiplies every emitted
        // spot frequency by this factor, so an uncorrected segment can
        // exclude frequencies from manta's own spot stream. +1000 ppm (the
        // supported limit) is ~+14 kHz on 20 m.
        let factor = manta_spot::calibration_factor_from_ppm(1_000.0).unwrap();
        let segs = segments_for_passband(14_040_000.0, symmetric(10_000.0), factor);
        assert_eq!(
            segs,
            vec![(14_035_000.0 * factor, 14_045_000.0 * factor)],
            "both edges scale by the same factor the validator applies"
        );
        let (lo, hi) = (segs[0].0, segs[0].1);
        assert!(
            (lo - 14_049_035.0).abs() < 1.0 && (hi - 14_059_045.0).abs() < 1.0,
            "expected roughly 14049.0-14059.0 kHz, got {lo}-{hi}"
        );
        // The uncorrected answer's upper edge is BELOW the corrected
        // lower edge: every spot in the stream would fall outside it.
        let uncorrected = segments_for_passband(14_040_000.0, symmetric(10_000.0), 1.0);
        assert!(uncorrected[0].1 < lo);
    }

    #[test]
    fn a_non_finite_or_inverted_passband_yields_no_claimed_segment() {
        assert!(segments_for_passband(14_040_000.0, (f64::NAN, 3_000.0), 1.0).is_empty());
        assert!(segments_for_passband(14_040_000.0, (3_000.0, 300.0), 1.0).is_empty());
        assert!(segments_for_passband(14_040_000.0, symmetric(10_000.0), f64::NAN).is_empty());
        assert!(segments_for_passband(14_040_000.0, symmetric(10_000.0), 0.0).is_empty());
    }
}
