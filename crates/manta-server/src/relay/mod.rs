//! MAN-320: relaying the operator's other local decoders through manta's
//! RBN uplink. A `[[skimmer_relay]]` ([`skimmer`]) logs in to another CW
//! skimmer's telnet server and forwards the spots that skimmer made under
//! its own call; a `[[wsjtx_relay]]` ([`wsjtx`]) listens for WSJT-X's UDP
//! decodes and forwards new FT8/FT4 `CQ` decodes as digital spots.
//!
//! A relayed spot is a [`RelaySpot`] on the bus's relay channel
//! ([`crate::bus::SpotBus::publish_relay`]), which only the uplink reads: it
//! never reaches manta's telnet, JSON/WebSocket, stdout or live-state
//! outputs (D4), and it skips manta's validator, being another decoder's
//! finished output (D5). The uplink renders it with the relay's own spotter
//! call ([`crate::rbn::format_relay_line`]) through the same `spot_types`,
//! `dry_run` and ceiling gates as manta's own spots (D6). See
//! `docs/DECISIONS/2026-10-27-man320-relay-local-skimmers-and-wsjtx.md`.

use std::sync::Arc;

pub mod skimmer;
pub mod skimmer_line;
pub mod wsjtx;
pub mod wsjtx_proto;

/// One spot another local program decoded, on its way to the RBN uplink
/// only (MAN-320 D4).
#[derive(Debug, Clone, PartialEq)]
pub struct RelaySpot {
    /// The `DX de` identity: the relay's configured `callsign` (D3).
    pub spotter: Arc<str>,
    /// The spotted call, uppercase; passed [`plausible_relayed_call`].
    pub callsign: String,
    /// Frequency in Hz, finite and positive.
    pub freq_hz: f64,
    pub mode: RelayMode,
    /// As the source quoted it: 500 Hz for a CW skimmer, WSJT-X's own
    /// 2500 Hz for FT8/FT4 (D8). Never corrected.
    pub snr_db: i32,
    pub spot_type: manta_spot::SpotType,
    /// Arrival time, Unix seconds (D9).
    pub unix_ts: i64,
}

/// The mode a relayed spot goes out in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RelayMode {
    /// CW, at the skimmer's quoted speed.
    Cw {
        wpm: u32,
    },
    Ft8,
    Ft4,
}

/// D5's structural call check for a relayed (not validator-checked) call:
/// 3-20 ASCII letters, digits and `/`, with at least one letter and one
/// digit. Nothing that passes can put a space, CR/LF or control byte into
/// the uplink's AK1A line.
pub fn plausible_relayed_call(call: &str) -> bool {
    (3..=20).contains(&call.len())
        && call.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'/')
        && call.bytes().any(|b| b.is_ascii_alphabetic())
        && call.bytes().any(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plausible_relayed_call_accepts_real_calls_and_rejects_injection() {
        for ok in ["K1ABC", "JW/LB2PG", "GB3LER/B", "VK9/W3XYZ/QRP1A", "N8II"] {
            assert!(plausible_relayed_call(ok), "{ok}");
        }
        for bad in [
            "",
            "AB",
            "ABCDEF",
            "123",
            "K1 ABC",
            "K1ABC\r\nDX de EVIL-#:",
            "<...>",
            "K1ABCDEFGHIJKLMNOPQRS",
            "K1ÄBC",
        ] {
            assert!(!plausible_relayed_call(bad), "{bad:?}");
        }
    }
}
