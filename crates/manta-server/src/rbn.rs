//! RBN-format ("DX de ...") line rendering for the telnet cluster server.
//! ARCHITECTURE §7.

use manta_spot::{Spot, SpotType};

/// MAN-102 / decision D3 (2026-09-06 broad review): RBN and CW Skimmer
/// quote spot SNR in a 500 Hz reference bandwidth; manta's detector
/// measures it in 2500 Hz (SPEC §2.3). `10*log10(2500/500) = 6.9897 dB`.
/// Applied ONLY here, at the telnet/uplink wire boundary -- the JSON
/// stream keeps the native 2500 Hz value plus an explicit `snrRefHz` (see
/// `spot_message.rs`), and nothing inside the decode pipeline changes.
const RBN_REF_BW_CORRECTION_DB: f32 = 6.989_7;

fn spot_type_label(spot_type: SpotType) -> &'static str {
    match spot_type {
        SpotType::Cq => "CQ",
        SpotType::De => "DE",
        SpotType::Beacon => "BEACON",
        SpotType::Unknown => "",
    }
}

/// Renders one spot as a standard RBN `DX de` cluster line, e.g.
/// `DX de W3XYZ-#:  14027.1  JA1ABC   CW  30 dB  28 WPM  CQ  0312Z`.
///
/// `unix_ts_secs` is the spot's wall-clock time (UTC); converting from the
/// decoder's sample-count timestamp happens at the caller, not here (see
/// `manta_spot::validator::Spot`'s doc comment on why `Spot` itself carries
/// no wall-clock time). `snr` is `Spot.snr_db` (SPEC §2.3's native 2500 Hz
/// measurement) converted to the 500 Hz RBN/CW Skimmer reference bandwidth
/// -- see `RBN_REF_BW_CORRECTION_DB`.
pub fn format_line(spot: &Spot, spotter_call: &str, unix_ts_secs: i64) -> String {
    let freq_khz = spot.freq_hz / 1000.0;
    let secs_of_day = unix_ts_secs.rem_euclid(86_400);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;

    format!(
        "DX de {spotter}-#:{freq:>9.1}  {call:<8} CW  {snr:>2} dB  {wpm:>2} WPM  {ctx}  {hour:02}{minute:02}Z",
        spotter = spotter_call,
        freq = freq_khz,
        call = spot.callsign,
        snr = (spot.snr_db + RBN_REF_BW_CORRECTION_DB).round() as i32,
        wpm = spot.wpm.round() as i32,
        ctx = spot_type_label(spot.spot_type),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_spot() -> Spot {
        Spot {
            callsign: "JA1ABC".to_string(),
            freq_hz: 14_027_100.0,
            snr_db: 23.0,
            wpm: 28.0,
            spot_type: SpotType::Cq,
            confidence: 0.9,
            track_id: 1,
            sample_ts: 0,
        }
    }

    #[test]
    fn formats_the_architecture_doc_example_verbatim() {
        // 03:12 UTC == 11520 seconds past midnight. MAN-102/D3: the wire
        // line reports SNR in RBN/CW Skimmer's 500 Hz reference bandwidth,
        // not the pipeline's native 2500 Hz -- sample_spot's 2500 Hz value
        // (see below) plus 6.9897 dB rounds to 30 dB(500).
        let line = format_line(&sample_spot(), "W3XYZ", 11_520);
        assert_eq!(
            line,
            "DX de W3XYZ-#:  14027.1  JA1ABC   CW  30 dB  28 WPM  CQ  0312Z"
        );
    }

    #[test]
    fn wire_snr_is_referenced_to_500hz() {
        // D3/MAN-102: the pipeline measures SNR in 2500 Hz; RBN and CW
        // Skimmer quote 500 Hz. sample_spot's 2500 Hz value (see above) +
        // 10*log10(5) rounds to 30 dB(500).
        let line = format_line(&sample_spot(), "W3XYZ", 11_520);
        assert!(line.contains("  30 dB  "), "line was: {line}");
    }

    #[test]
    fn a_negative_snr_spot_still_renders_a_sane_wire_value() {
        let mut spot = sample_spot();
        spot.snr_db = -4.0; // 2500 Hz
        let line = format_line(&spot, "W3XYZ", 11_520);
        assert!(line.contains("   3 dB  "), "line was: {line}");
    }

    #[test]
    fn de_spot_uses_de_label() {
        let mut spot = sample_spot();
        spot.spot_type = SpotType::De;
        let line = format_line(&spot, "W3XYZ", 11_520);
        assert!(line.contains("  DE  0312Z"), "line was: {line}");
    }

    #[test]
    fn midnight_wraps_to_zero_zulu() {
        let line = format_line(&sample_spot(), "W3XYZ", 0);
        assert!(line.ends_with("0000Z"), "line was: {line}");
    }

    #[test]
    fn a_long_portable_call_is_separated_from_the_mode_field() {
        let mut spot = sample_spot();
        spot.callsign = "K5ARH/QRP".to_string(); // 9 chars, exceeds the 8-wide call column
        let line = format_line(&spot, "W3XYZ", 11_520);
        assert!(
            line.contains("K5ARH/QRP CW"),
            "call and mode ran together: {line}"
        );
    }
}
