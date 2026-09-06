//! RBN-format ("DX de ...") line rendering for the telnet cluster server.
//! ARCHITECTURE §7.
//!
//! MAN-88: the line is a true fixed-column AK1A layout, not just fields in
//! the right order -- every field boundary is anchored to an absolute
//! column, matching a live RBN capture from `telnet.reversebeacon.net:7000`
//! byte-for-byte (see `LIVE_RBN_CAPTURE` in the tests below and
//! `docs/DECISIONS/2026-09-06-man88-ak1a-column-layout.md`).

use manta_spot::{Spot, SpotType};

fn spot_type_label(spot_type: SpotType) -> &'static str {
    match spot_type {
        SpotType::Cq => "CQ",
        SpotType::De => "DE",
        SpotType::Beacon => "BEACON",
        SpotType::Unknown => "",
    }
}

/// Which wire layout `format_line` renders.
///
/// Both variants share the fixed-column AK1A geometry MAN-88 measured
/// against a live RBN capture; they differ only in whether the 6-column
/// mode field is present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LineFormat {
    /// The RBN relay layout: mode column present, time at column 71.
    /// What downstream loggers see on telnet.reversebeacon.net today, and
    /// manta's default.
    #[default]
    Rbn,
    /// The CW-Skimmer-native layout: no mode column, time at column 67.
    /// For operators running manta behind W3OA's Aggregator, which
    /// consumes CW Skimmer's own line rather than the RBN relay's.
    ///
    /// CAVEAT (MAN-88, Decision 2): this byte layout is *derived*, not
    /// measured. The only primary source for CW Skimmer's own line
    /// (a pdftotext dump of the CW Skimmer manual, made during the
    /// 2026-09-05 review) was a scratch artifact and is not preserved in
    /// this repo or the thoughts pool. What every surviving source agrees
    /// on is qualitative -- "no mode field; CQ/DE/blank" -- so this
    /// variant deletes the 6-wide mode field and substitutes the layout's
    /// standard 2-space separator, shifting everything after the callsign
    /// column left by 4. MAN-86 (the `SKIMMER/SETT` handshake) is where a
    /// real Aggregator/CW Skimmer capture will land; correcting this is a
    /// one-constant change plus a test-vector update.
    Skimmer,
}

/// The frequency field's last character lands on this 1-indexed column,
/// matching the live RBN capture. Equivalent to the classic AK1A 10-wide
/// spotter field: `DX de ` (6) + a 7-character base callsign + `-#:` (3)
/// is exactly 16 columns, leaving 8 for a 5-digit-MHz frequency.
const FREQ_END_COL: usize = 24;

/// Minimum width of the callsign column (columns 27-41), so the mode field
/// starts at column 42.
const CALL_COL_WIDTH: usize = 15;

/// Renders one spot as a fixed-column AK1A `DX de` cluster line, e.g.
/// `DX de W3XYZ-#:  14027.10  JA1ABC         CW    23 dB  28 WPM  CQ      0312Z`.
///
/// `unix_ts_secs` is the spot's wall-clock time (UTC); converting from the
/// decoder's sample-count timestamp happens at the caller, not here (see
/// `manta_spot::validator::Spot`'s doc comment on why `Spot` itself carries
/// no wall-clock time).
///
/// Every field after the spotter identity is anchored to an absolute
/// column (MAN-88): the identity and callsign fields are MINIMUM widths
/// with a guaranteed one-space separator, never truncated -- an oversized
/// value (e.g. MAN-28's Watch List bypasses callsign-grammar validation)
/// shifts the rest of the line right rather than corrupting a field or
/// forging a shorter identity.
pub fn format_line(
    spot: &Spot,
    spotter_call: &str,
    unix_ts_secs: i64,
    line_format: LineFormat,
) -> String {
    let freq_khz = spot.freq_hz / 1000.0;
    let secs_of_day = unix_ts_secs.rem_euclid(86_400);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;

    let identity = format!("DX de {spotter_call}-#:");
    // `{:.2}` is a *minimum* width: a 2 m frequency (`144110.00`, 9 chars)
    // widens the field rather than losing a digit, and the identity
    // padding below absorbs it so column 24 still holds.
    let freq = format!("{freq_khz:.2}");
    // Decision 3: anchor the frequency's last char to FREQ_END_COL, but
    // never let the two fields abut -- truncating an operator's own
    // callsign would forge a wrong spotter ID, and abutting would corrupt
    // the spotter token for whitespace-splitting parsers.
    let freq_gap = FREQ_END_COL
        .saturating_sub(identity.chars().count() + freq.chars().count())
        .max(1);
    // Same rule for the callsign column: MAN-28's Watch List bypasses the
    // callsign grammar, so an allowlisted entry can exceed 15 columns.
    let call_gap = CALL_COL_WIDTH
        .saturating_sub(spot.callsign.chars().count())
        .max(1);
    // The mode field's own trailing padding is the separator to the SNR
    // field -- there is no extra gap in the RBN layout (columns 42-47).
    let mode = match line_format {
        LineFormat::Rbn => "CW    ",
        LineFormat::Skimmer => "  ",
    };

    format!(
        "{identity}{:freq_gap$}{freq}  {call}{:call_gap$}{mode}\
         {snr:>2} dB  {wpm:>2} WPM  {ctx:<6}  {hour:02}{minute:02}Z",
        "",
        "",
        call = spot.callsign,
        snr = spot.snr_db.round() as i32,
        wpm = spot.wpm.round() as i32,
        ctx = spot_type_label(spot.spot_type),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one live RBN line available as evidence, captured from
    /// telnet.reversebeacon.net:7000 on 2026-09-06 02:36Z and quoted verbatim
    /// in the 2026-09-05 lens-2 operations review. MAN-88 Scenario 1: manta's
    /// line must be byte-identical to this.
    const LIVE_RBN_CAPTURE: &str =
        "DX de S53A-#:   14011.90  N8II           CW    21 dB  25 WPM  CQ      0236Z";

    fn capture_spot() -> Spot {
        Spot {
            callsign: "N8II".to_string(),
            freq_hz: 14_011_900.0,
            snr_db: 21.0,
            wpm: 25.0,
            spot_type: SpotType::Cq,
            confidence: 0.9,
            track_id: 1,
            sample_ts: 0,
        }
    }

    /// Returns the 1-indexed column `needle` starts at.
    fn col_of(line: &str, needle: &str) -> usize {
        line.find(needle).expect("field missing from line") + 1
    }

    #[test]
    fn matches_the_live_rbn_capture_byte_for_byte() {
        let line = format_line(&capture_spot(), "S53A", 2 * 3600 + 36 * 60, LineFormat::Rbn);
        assert_eq!(line, LIVE_RBN_CAPTURE);
    }

    #[test]
    fn frequency_carries_two_decimals_of_khz_and_ends_at_column_24() {
        let mut spot = capture_spot();
        spot.freq_hz = 14_011_910.0; // the 10 Hz RBN now carries
        let line = format_line(&spot, "S53A", 0, LineFormat::Rbn);
        assert!(line.contains("14011.91"), "line was: {line}");
        assert_eq!(col_of(&line, "14011.91") + "14011.91".len() - 1, 24);
    }

    #[test]
    fn time_lands_at_column_71_for_every_realistic_spotter_length() {
        for spotter in ["W4X", "W5AU", "W3XYZ", "DL8LAS"] {
            let line = format_line(
                &capture_spot(),
                spotter,
                2 * 3600 + 36 * 60,
                LineFormat::Rbn,
            );
            assert_eq!(col_of(&line, "0236Z"), 71, "spotter {spotter}: {line}");
        }
    }

    #[test]
    fn callsign_column_is_15_wide_so_the_mode_field_starts_at_column_42() {
        for call in ["A1A", "N8II", "JA1ABC", "VK9/W3XYZ/QRP1"] {
            let mut spot = capture_spot();
            spot.callsign = call.to_string();
            let line = format_line(&spot, "S53A", 2 * 3600 + 36 * 60, LineFormat::Rbn);
            assert_eq!(col_of(&line, "CW"), 42, "call {call}: {line}");
            assert_eq!(col_of(&line, "0236Z"), 71, "call {call}: {line}");
        }
    }

    #[test]
    fn every_spot_type_label_fits_the_six_wide_type_field() {
        // BEACON is exactly 6 characters -- the width's binding constraint.
        for spot_type in [
            SpotType::Cq,
            SpotType::De,
            SpotType::Beacon,
            SpotType::Unknown,
        ] {
            let mut spot = capture_spot();
            spot.spot_type = spot_type;
            let line = format_line(&spot, "S53A", 2 * 3600 + 36 * 60, LineFormat::Rbn);
            assert_eq!(col_of(&line, "0236Z"), 71, "{spot_type:?}: {line}");
        }
    }

    #[test]
    fn low_and_high_bands_keep_the_frequency_field_anchored() {
        for freq_hz in [1_822_500.0, 3_573_600.0, 50_110_000.0, 144_110_000.0] {
            let mut spot = capture_spot();
            spot.freq_hz = freq_hz;
            let line = format_line(&spot, "S53A", 2 * 3600 + 36 * 60, LineFormat::Rbn);
            assert_eq!(col_of(&line, "0236Z"), 71, "{freq_hz} Hz: {line}");
        }
    }

    /// MAN-28's Watch List bypasses the callsign grammar entirely
    /// (`manta_spot::validator`), so an allowlisted entry longer than the
    /// 15-wide column can reach the renderer. It must shift the rest of the
    /// line right, never truncate and never abut the mode field.
    #[test]
    fn an_over_long_callsign_shifts_right_but_never_abuts_the_mode_field() {
        let mut spot = capture_spot();
        spot.callsign = "SOMEVERYLONGWATCHLISTCALL".to_string();
        let line = format_line(&spot, "S53A", 2 * 3600 + 36 * 60, LineFormat::Rbn);
        assert!(
            line.contains("SOMEVERYLONGWATCHLISTCALL CW"),
            "line was: {line}"
        );
    }

    /// Decision 3: an identity too long for columns 1-16 shifts the line right
    /// with exactly one separating space rather than truncating the operator's
    /// own callsign or running into the frequency.
    #[test]
    fn an_over_long_spotter_identity_shifts_right_but_never_abuts_the_frequency() {
        let line = format_line(
            &capture_spot(),
            "K5ARH/QRP",
            2 * 3600 + 36 * 60,
            LineFormat::Rbn,
        );
        assert!(
            line.contains("DX de K5ARH/QRP-#: 14011.90"),
            "line was: {line}"
        );
    }

    #[test]
    fn midnight_wraps_to_zero_zulu() {
        let line = format_line(&capture_spot(), "S53A", 0, LineFormat::Rbn);
        assert!(line.ends_with("0000Z"), "line was: {line}");
    }

    #[test]
    fn the_skimmer_layout_drops_the_mode_column_and_moves_time_to_column_67() {
        let line = format_line(
            &capture_spot(),
            "S53A",
            2 * 3600 + 36 * 60,
            LineFormat::Skimmer,
        );
        assert!(!line.contains(" CW "), "mode column still present: {line}");
        assert_eq!(col_of(&line, "0236Z"), 67, "line was: {line}");
        // Columns 1-41 are identical to the RBN layout.
        let rbn = format_line(&capture_spot(), "S53A", 2 * 3600 + 36 * 60, LineFormat::Rbn);
        assert_eq!(line[..41], rbn[..41]);
    }
}
