//! MAN-301: parse the RBN relay's AK1A `DX de` lines -- the inverse of
//! `crate::rbn::format_line` for the fields grading needs.
//!
//! A line looks like (MAN-88's live capture, `rbn.rs`'s `LIVE_RBN_CAPTURE`):
//!
//! ```text
//! DX de S53A-#:   14011.90  N8II           CW    21 dB  25 WPM  CQ      0236Z
//! ```
//!
//! The parse is token-based, not column-based: strip `DX de `, split the
//! spotter off at the first `:`, then read whitespace-separated tokens
//! (frequency, call, mode, `<snr> dB`, `<n> WPM|BPS`, type, `HHMMZ`).
//! Column drift -- an over-long call, a 2 m frequency, a wide SNR -- moves
//! fields right without changing their order, so tokens survive it where
//! fixed columns would not. `round_trips_format_line` pins the parser to
//! manta's own renderer.
//!
//! Only CW reports are accepted: port 7000 also carries RTTY (speed in
//! `BPS`), and any other mode is not something manta decodes.

/// One accepted CW report from the feed.
#[derive(Debug, Clone, PartialEq)]
pub struct DxLine {
    /// The reporting skimmer, uppercase, with RBN's `-#` suffix stripped
    /// (`S53A-#` -> `S53A`; `KM3T-2-#` -> `KM3T-2`).
    pub spotter: String,
    /// Frequency in kHz as RBN prints it (finite and positive).
    pub freq_khz: f64,
    /// The spotted call, uppercase.
    pub call: String,
    /// The mode column; always `CW` for a line [`parse_dx_line`] accepts.
    pub mode: String,
    /// Speed in WPM. Always present on an accepted line (the unit must be
    /// `WPM`); an `Option` so a consumer never has to invent a speed.
    pub wpm: Option<u32>,
}

/// What a feed line is, for the feed reader: not a spot line at all
/// (banner, prompt, `To ALL` talk, a malformed spot), a well-formed spot
/// line in a mode manta does not grade, or a CW report.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LineKind {
    NotSpot,
    NonCw,
    Cw(DxLine),
}

/// Parses one feed line into a CW report, or `None` for anything else:
/// non-`DX de` lines, RTTY (`BPS`) and other modes, and malformed lines
/// (non-numeric, NaN, infinite, zero or negative frequency; non-numeric
/// speed or SNR; no `HHMMZ` time).
pub fn parse_dx_line(line: &str) -> Option<DxLine> {
    match classify(line) {
        LineKind::Cw(dx) => Some(dx),
        LineKind::NotSpot | LineKind::NonCw => None,
    }
}

/// RBN appends `-#` to every skimmer's call; the grading engine compares
/// spotters without it, uppercase (the rule `shadow-compare.py` applies).
fn normalize_spotter(raw: &str) -> String {
    let upper = raw.trim().to_ascii_uppercase();
    upper.strip_suffix("-#").unwrap_or(&upper).to_string()
}

/// The AK1A callsign column's width (RBN columns 27-41; `rbn.rs`).
const CALL_COLUMN_WIDTH: usize = 15;

/// `HHMMZ`: four digits and a `Z`. The date-less time is not used (a live
/// report's time is its arrival time); requiring it rejects a truncated line.
fn is_hhmmz(token: &str) -> bool {
    let b = token.as_bytes();
    b.len() == 5 && b[..4].iter().all(u8::is_ascii_digit) && b[4] == b'Z'
}

pub(crate) fn classify(line: &str) -> LineKind {
    let Some(rest) = line.trim().strip_prefix("DX de ") else {
        return LineKind::NotSpot;
    };
    let Some((spotter, fields)) = rest.split_once(':') else {
        return LineKind::NotSpot;
    };
    let spotter = normalize_spotter(spotter);
    if spotter.is_empty() || spotter.contains(char::is_whitespace) {
        return LineKind::NotSpot;
    }
    let mut tokens: Vec<&str> = fields.split_whitespace().collect();
    // A call that fills its 15-wide column exactly abuts the mode field
    // with a zero-width gap (`format_line`'s "a fit, not an overflow"
    // rule): `VK9/W3XYZ/QRP1ACW`. Only then is `dB` the fourth token;
    // split the merged token at the column width.
    if tokens.len() > 3 && tokens[3] == "dB" && tokens[1].len() > CALL_COLUMN_WIDTH {
        if let Some((call, mode)) = tokens[1].split_at_checked(CALL_COLUMN_WIDTH) {
            tokens.splice(1..2, [call, mode]);
        }
    }
    // freq, call, mode, snr, "dB", speed, unit, [type...], time
    if tokens.len() < 8 {
        return LineKind::NotSpot;
    }
    let freq_khz = match tokens[0].parse::<f64>() {
        Ok(f) if f.is_finite() && f > 0.0 => f,
        _ => return LineKind::NotSpot,
    };
    let call = tokens[1].to_ascii_uppercase();
    let mode = tokens[2];
    if tokens[3].parse::<i32>().is_err() || tokens[4] != "dB" {
        return LineKind::NotSpot;
    }
    let speed = tokens[5];
    let unit = tokens[6];
    if !is_hhmmz(tokens[tokens.len() - 1]) {
        return LineKind::NotSpot;
    }
    if mode != "CW" || unit != "WPM" {
        // A well-formed spot line in another mode (RTTY in BPS, FT8 ...).
        return if speed.parse::<u32>().is_ok() {
            LineKind::NonCw
        } else {
            LineKind::NotSpot
        };
    }
    let Ok(wpm) = speed.parse::<u32>() else {
        return LineKind::NotSpot;
    };
    LineKind::Cw(DxLine {
        spotter,
        freq_khz,
        call,
        mode: mode.to_string(),
        wpm: Some(wpm),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rbn::{format_line, LineFormat};
    use manta_spot::{Spot, SpotType};

    /// Copied verbatim from `rbn.rs`'s private `tests::LIVE_RBN_CAPTURE`
    /// (MAN-88): the one byte-captured line from
    /// telnet.reversebeacon.net:7000, 2026-09-06 02:36Z.
    const LIVE_RBN_CAPTURE: &str =
        "DX de S53A-#:   14011.90  N8II           CW    21 dB  25 WPM  CQ      0236Z";

    fn spot(call: &str, freq_hz: f64, wpm: f32, spot_type: SpotType) -> Spot {
        Spot {
            callsign: call.to_string(),
            freq_hz,
            snr_db: 14.0,
            wpm,
            spot_type,
            confidence: 0.9,
            track_id: 1,
            sample_ts: 0,
        }
    }

    #[test]
    fn parses_the_live_rbn_capture() {
        let dx = parse_dx_line(LIVE_RBN_CAPTURE).expect("capture parses");
        assert_eq!(dx.spotter, "S53A");
        assert!((dx.freq_khz - 14011.90).abs() < 1e-9, "{}", dx.freq_khz);
        assert_eq!(dx.call, "N8II");
        assert_eq!(dx.mode, "CW");
        assert_eq!(dx.wpm, Some(25));
        // The same line with telnet's CRLF still parses.
        assert_eq!(parse_dx_line(&format!("{LIVE_RBN_CAPTURE}\r\n")), Some(dx));
    }

    #[test]
    fn round_trips_format_line() {
        let cases = [
            (spot("N8II", 14_011_900.0, 25.0, SpotType::Cq), "S53A"),
            (spot("JA1ABC", 7_023_450.0, 31.4, SpotType::De), "W3XYZ"),
            (
                spot("4U1UN", 14_100_000.0, 22.0, SpotType::Beacon),
                "DL8LAS",
            ),
            (spot("K1ABC", 3_525_010.0, 18.0, SpotType::Unknown), "W4X"),
            // 15 characters: fills the callsign column exactly.
            (
                spot("VK9/W3XYZ/QRP1A", 21_025_000.0, 28.0, SpotType::Cq),
                "S53A",
            ),
            // Longer than the column: shifts the tail right.
            (
                spot("VK9/W3XYZ/QRP1ABCD", 28_025_500.0, 40.0, SpotType::Cq),
                "S53A",
            ),
            // SSID spotter, and a 2 m frequency overrunning its column.
            (spot("G4ABC", 144_110_000.0, 12.0, SpotType::Cq), "KM3T-2"),
        ];
        for (s, spotter) in cases {
            let line = format_line(&s, spotter, 2 * 3600 + 36 * 60, LineFormat::Rbn);
            let dx = parse_dx_line(&line).unwrap_or_else(|| panic!("no parse: {line}"));
            assert_eq!(dx.call, s.callsign, "{line}");
            assert_eq!(dx.spotter, spotter, "{line}");
            assert!(
                (dx.freq_khz - s.freq_hz / 1000.0).abs() < 0.005 + 1e-9,
                "{line}: {}",
                dx.freq_khz
            );
            assert_eq!(dx.wpm, Some(s.wpm.round() as u32), "{line}");
            assert_eq!(dx.mode, "CW");
        }
    }

    #[test]
    fn non_cw_lines_are_rejected() {
        let rtty = "DX de W3OA-#:   14080.00  K1ABC          RTTY  15 dB  45 BPS  CQ      0236Z";
        let ft8 = "DX de W3OA-#:   14074.00  K1ABC          FT8   -5 dB  25 WPM  CQ      0236Z";
        assert_eq!(parse_dx_line(rtty), None);
        assert_eq!(parse_dx_line(ft8), None);
        assert_eq!(classify(rtty), LineKind::NonCw);
        assert_eq!(classify(ft8), LineKind::NonCw);
        // CW in the mode column but BPS as the unit is not a CW report.
        let cw_bps = "DX de W3OA-#:   14011.00  K1ABC          CW    15 dB  45 BPS  CQ      0236Z";
        assert_eq!(parse_dx_line(cw_bps), None);
    }

    #[test]
    fn non_dx_lines_are_ignored() {
        for line in [
            "Please enter your call: ",
            "Please enter your call:\r\n",
            "Welcome to the Reverse Beacon Network telnet server",
            "",
            "\r\n",
            "To ALL de W3LPL: CQ test in 10 minutes 0236Z",
            "W3XYZ-1 de RELAY 11-Oct-2026 2359Z >",
            "DX de S53A-#:",
            "DX de S53A-# 14011.90 N8II CW 21 dB 25 WPM CQ 0236Z",
        ] {
            assert_eq!(parse_dx_line(line), None, "{line:?}");
            assert_eq!(classify(line), LineKind::NotSpot, "{line:?}");
        }
    }

    #[test]
    fn malformed_numbers_are_rejected() {
        let with = |freq: &str, wpm: &str| {
            format!("DX de S53A-#:   {freq}  N8II           CW    21 dB  {wpm} WPM  CQ      0236Z")
        };
        assert!(parse_dx_line(&with("14011.90", "25")).is_some());
        for (freq, wpm) in [
            ("14O11.90", "25"),
            ("NaN", "25"),
            ("inf", "25"),
            ("-14011.90", "25"),
            ("0.00", "25"),
            ("14011.90", "2x"),
            ("14011.90", "-25"),
            ("14011.90", "25.5"),
        ] {
            let line = with(freq, wpm);
            assert_eq!(parse_dx_line(&line), None, "{line}");
        }
        // A non-numeric SNR or a missing time also rejects the line.
        assert_eq!(
            parse_dx_line(
                "DX de S53A-#:   14011.90  N8II           CW    xx dB  25 WPM  CQ      0236Z"
            ),
            None
        );
        assert_eq!(
            parse_dx_line("DX de S53A-#:   14011.90  N8II           CW    21 dB  25 WPM  CQ"),
            None
        );
    }
}
