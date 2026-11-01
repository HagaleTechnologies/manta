//! MAN-320 D10: parse a local skimmer's `DX de` lines for the skimmer
//! relay. Two layouts arrive on a skimmer's telnet port: RBN's AK1A layout
//! with a mode column (CW Skimmer Server, a manta with `line_format =
//! "rbn"`), and CW Skimmer's/manta's `skimmer` layout without one:
//!
//! ```text
//! DX de S53A-#:   14011.90  N8II           CW    21 dB  25 WPM  CQ      0236Z
//! DX de W3XYZ-#:  14027.1  JA1ABC         23 dB  28 WPM  CQ      0236Z
//! ```
//!
//! Token-based like `rbn_compare::ak1a::classify`, whose spotter and time
//! rules it reuses; that parser's own behaviour is unchanged. Unlike it,
//! this one keeps the SNR and the spot type, which the relayed line
//! carries.

use crate::rbn_compare::ak1a::{is_hhmmz, normalize_spotter, CALL_COLUMN_WIDTH};
use manta_spot::SpotType;

/// What one skimmer line is, for the relay.
#[derive(Debug, Clone, PartialEq)]
pub enum RelayLine {
    /// Not a spot line: a banner, prompt, talk, or a malformed spot.
    NotSpot,
    /// A well-formed spot line in another mode (RTTY in `BPS`, FT8 ...).
    NotCw,
    /// A CW spot.
    Cw(SkimmerSpot),
}

/// One CW spot a skimmer reported.
#[derive(Debug, Clone, PartialEq)]
pub struct SkimmerSpot {
    /// The `DX de` spotter, uppercase, `-#` stripped (`ak1a`'s rule).
    pub spotter: String,
    /// Frequency in kHz as printed (finite and positive).
    pub freq_khz: f64,
    /// The spotted call, uppercase.
    pub call: String,
    /// The skimmer's `dB` figure, verbatim (500 Hz convention, D8).
    pub snr_db: i32,
    pub wpm: u32,
    pub spot_type: SpotType,
}

fn is_mode_name(token: &str) -> bool {
    matches!(token, "CW" | "RTTY" | "FT8" | "FT4" | "PSK31" | "PSK63")
}

fn is_int(token: &str) -> bool {
    token.parse::<i32>().is_ok()
}

/// The type tokens between the speed unit and the time (`CQ`, `DE`,
/// `BEACON`, `NCDXF B`); anything else (`DX`, none) is `Unknown`.
fn spot_type_of(tokens: &[&str]) -> SpotType {
    match tokens {
        ["CQ"] => SpotType::Cq,
        ["DE"] => SpotType::De,
        ["BEACON"] | ["NCDXF", "B"] => SpotType::Beacon,
        _ => SpotType::Unknown,
    }
}

/// Classifies one skimmer line (see the module doc for the layouts).
pub fn parse(line: &str) -> RelayLine {
    let Some(rest) = line.trim().strip_prefix("DX de ") else {
        return RelayLine::NotSpot;
    };
    let Some((spotter, fields)) = rest.split_once(':') else {
        return RelayLine::NotSpot;
    };
    let spotter = normalize_spotter(spotter);
    if spotter.is_empty() || spotter.contains(char::is_whitespace) {
        return RelayLine::NotSpot;
    }
    let mut tokens: Vec<&str> = fields.split_whitespace().collect();
    // A call filling its 15-wide column abuts the next field: the mode
    // (RBN layout, `dB` is then token 3) or the SNR (skimmer layout, `dB`
    // is then token 2). Split the merged token at the column width, but in
    // the RBN case only when the tail is a mode name: a longer-than-column
    // call in the skimmer layout also puts `dB` at token 3.
    for db_at in [3, 2] {
        if tokens.len() > db_at && tokens[db_at] == "dB" && tokens[1].len() > CALL_COLUMN_WIDTH {
            if let Some((call, next)) = tokens[1].split_at_checked(CALL_COLUMN_WIDTH) {
                if (db_at == 3 && is_mode_name(next)) || (db_at == 2 && is_int(next)) {
                    tokens.splice(1..2, [call, next]);
                    break;
                }
            }
        }
    }
    if tokens.len() < 2 {
        return RelayLine::NotSpot;
    }
    // Token 2 is a mode only if it is not `<int> dB`.
    let has_mode = !(tokens.len() > 3 && is_int(tokens[2]) && tokens[3] == "dB");
    let base = if has_mode { 3 } else { 2 };
    // freq, call, [mode], snr, "dB", speed, unit, [type...], time
    if tokens.len() < base + 5 {
        return RelayLine::NotSpot;
    }
    let freq_khz = match tokens[0].parse::<f64>() {
        Ok(f) if f.is_finite() && f > 0.0 => f,
        _ => return RelayLine::NotSpot,
    };
    let call = tokens[1].to_ascii_uppercase();
    let mode = if has_mode { Some(tokens[2]) } else { None };
    let Ok(snr_db) = tokens[base].parse::<i32>() else {
        return RelayLine::NotSpot;
    };
    if tokens[base + 1] != "dB" {
        return RelayLine::NotSpot;
    }
    let Ok(speed) = tokens[base + 2].parse::<u32>() else {
        return RelayLine::NotSpot;
    };
    let unit = tokens[base + 3];
    let last = tokens.len() - 1;
    if !is_hhmmz(tokens[last]) {
        return RelayLine::NotSpot;
    }
    if !matches!(mode, None | Some("CW")) || unit != "WPM" {
        return RelayLine::NotCw;
    }
    RelayLine::Cw(SkimmerSpot {
        spotter,
        freq_khz,
        call,
        snr_db,
        wpm: speed,
        spot_type: spot_type_of(&tokens[base + 4..last]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rbn::{format_line, LineFormat};
    use manta_spot::Spot;

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
            emission_state: manta_spot::EmissionState::Confirmed,
            track_id: 1,
            sample_ts: 0,
            rst: None,
            qrl_query: false,
        }
    }

    fn cases() -> Vec<(Spot, &'static str)> {
        vec![
            (spot("N8II", 14_011_900.0, 25.0, SpotType::Cq), "S53A"),
            (spot("JA1ABC", 7_023_450.0, 31.4, SpotType::De), "W3XYZ"),
            (
                spot("4U1UN", 14_100_000.0, 22.0, SpotType::Beacon),
                "DL8LAS",
            ),
            (spot("K1ABC", 3_525_010.0, 18.0, SpotType::Unknown), "W4X"),
            (
                spot("VK9/W3XYZ/QRP1A", 21_025_000.0, 28.0, SpotType::Cq),
                "S53A",
            ),
            (
                spot("VK9/W3XYZ/QRP1ABCD", 28_025_500.0, 40.0, SpotType::Cq),
                "S53A",
            ),
            (spot("G4ABC", 144_110_000.0, 12.0, SpotType::Cq), "KM3T-2"),
        ]
    }

    fn cw(line: &str) -> SkimmerSpot {
        match parse(line) {
            RelayLine::Cw(s) => s,
            other => panic!("{line:?} -> {other:?}"),
        }
    }

    #[test]
    fn parses_the_rbn_layout_with_snr_and_type() {
        let s = cw(LIVE_RBN_CAPTURE);
        assert_eq!(s.spotter, "S53A");
        assert!((s.freq_khz - 14011.90).abs() < 1e-9);
        assert_eq!(s.call, "N8II");
        assert_eq!(s.snr_db, 21);
        assert_eq!(s.wpm, 25);
        assert_eq!(s.spot_type, SpotType::Cq);
    }

    fn round_trip(format: LineFormat) {
        for (s, spotter) in cases() {
            let line = format_line(&s, spotter, 2 * 3600 + 36 * 60, format);
            let p = cw(&line);
            assert_eq!(p.call, s.callsign, "{line}");
            assert_eq!(p.spotter, spotter, "{line}");
            assert!(
                (p.freq_khz - s.freq_hz / 1000.0).abs() <= 0.005 + 1e-9,
                "{line}"
            );
            assert_eq!(p.wpm, s.wpm.round() as u32, "{line}");
            assert_eq!(p.spot_type, s.spot_type, "{line}");
            assert!(line.contains(&format!("{} dB", p.snr_db)), "{line}");
        }
    }

    #[test]
    fn parses_mantas_skimmer_layout_without_a_mode_column() {
        round_trip(LineFormat::Skimmer);
    }

    #[test]
    fn round_trips_the_rbn_layout() {
        round_trip(LineFormat::Rbn);
    }

    #[test]
    fn type_tokens_map_to_spot_types() {
        for (types, want) in [
            ("CQ", SpotType::Cq),
            ("DE", SpotType::De),
            ("BEACON", SpotType::Beacon),
            ("NCDXF B", SpotType::Beacon),
            ("", SpotType::Unknown),
            ("DX", SpotType::Unknown),
        ] {
            let line = format!("DX de S53A-#: 14011.90 N8II CW 21 dB 25 WPM {types} 0236Z");
            assert_eq!(cw(&line).spot_type, want, "{line}");
        }
    }

    #[test]
    fn ssid_spotters_are_normalized() {
        let s = cw("DX de w1abc-2-#: 14011.90 N8II CW 21 dB 25 WPM CQ 0236Z");
        assert_eq!(s.spotter, "W1ABC-2");
    }

    #[test]
    fn rtty_and_ft8_lines_are_not_cw() {
        for line in [
            "DX de S53A-#: 14080.00 N8II RTTY 21 dB 45 BPS CQ 0236Z",
            "DX de S53A-#: 14074.00 N8II FT8 21 dB 25 WPM CQ 0236Z",
            "DX de S53A-#: 14011.90 N8II CW 21 dB 45 BPS CQ 0236Z",
        ] {
            assert_eq!(parse(line), RelayLine::NotCw, "{line}");
        }
    }

    #[test]
    fn non_spot_and_malformed_lines_are_not_spots() {
        for line in [
            "",
            "Welcome to CW Skimmer",
            "Please enter your callsign: ",
            "de W1ABC-# >",
            "To ALL de S53A: hello",
            "DX de S53A-# 14011.90 N8II CW 21 dB 25 WPM CQ 0236Z",
            "DX de S53A-#: abc N8II CW 21 dB 25 WPM CQ 0236Z",
            "DX de S53A-#: NaN N8II CW 21 dB 25 WPM CQ 0236Z",
            "DX de S53A-#: inf N8II CW 21 dB 25 WPM CQ 0236Z",
            "DX de S53A-#: 0 N8II CW 21 dB 25 WPM CQ 0236Z",
            "DX de S53A-#: -14011.9 N8II CW 21 dB 25 WPM CQ 0236Z",
            "DX de S53A-#: 14011.90 N8II CW xx dB 25 WPM CQ 0236Z",
            "DX de S53A-#: 14011.90 N8II CW 21 dB xx WPM CQ 0236Z",
            "DX de S53A-#: 14011.90 N8II CW 21 dB 25 WPM CQ",
        ] {
            assert_eq!(parse(line), RelayLine::NotSpot, "{line:?}");
        }
    }
}
