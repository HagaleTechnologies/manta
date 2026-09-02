//! `[server]` TOML config table. ARCHITECTURE §8: "server ports, ...
//! station callsign (spotter ID)" in the single daemon TOML config.
//! Naming follows `docs/SPEC-decode-core.md` §9's convention
//! (`lower_snake_case`, `_port` unit suffix, table name = subsystem name).

use serde::{Deserialize, Deserializer};

fn default_telnet_port() -> u16 {
    7300
}

fn default_json_port() -> u16 {
    7301
}

fn default_metrics_port() -> u16 {
    7302
}

fn default_bind_addr() -> String {
    "0.0.0.0".to_string()
}

fn deserialize_station_callsign<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let call = String::deserialize(deserializer)?;
    if !manta_spot::grammar::is_plausible(&call) {
        return Err(serde::de::Error::custom(format!(
            "station_callsign {call:?} is not a plausible callsign"
        )));
    }
    Ok(call)
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// The spotter's own callsign, used as the telnet `DX de <call>-#:`
    /// identity and the JSON stream's `deCall`. No sensible default --
    /// every real cluster/spot-stream node identifies itself. Validated
    /// (via `manta_spot::grammar::is_plausible`, the same grammar the
    /// decode pipeline itself uses) at deserialize time: an empty,
    /// control-character-laden, or malformed value would otherwise be
    /// interpolated straight into every telnet line and JSON `deCall`
    /// unescaped -- e.g. a callsign containing `\r\n` could forge
    /// additional bogus cluster lines.
    #[serde(deserialize_with = "deserialize_station_callsign")]
    pub station_callsign: String,
    #[serde(default = "default_bind_addr")]
    pub bind_addr: String,
    #[serde(default = "default_telnet_port")]
    pub telnet_port: u16,
    /// Shared TCP JSON Lines / WebSocket port, per ARCHITECTURE §7's "tcp/ws
    /// :7301" -- one listener accepts both; `manta_server::json_stream`
    /// distinguishes a WebSocket client from a raw JSON Lines client by
    /// peeking the connection's first bytes for an HTTP `GET` upgrade
    /// request before either side has sent anything.
    #[serde(default = "default_json_port")]
    pub json_port: u16,
    #[serde(default = "default_metrics_port")]
    pub metrics_port: u16,
}

/// The real on-disk daemon config file's shape: a `[server]` TOML table
/// (this is the file `manta listen --server-config <path>` reads) --
/// distinct from `ServerConfig` itself so that struct can stay a plain,
/// directly-deserializable value everywhere else (tests, future in-process
/// construction) without every caller needing to know about the table
/// wrapper. ARCHITECTURE §8: "Single TOML config... server ports." That
/// same single daemon TOML also carries `[detector]`/`[decode]`/`[input]`/
/// `[spot]` and other tables this crate doesn't model (SPEC §9) --
/// deliberately NOT `deny_unknown_fields` here, unlike `ServerConfig`
/// itself: this wrapper only needs to reject a typo INSIDE `[server]`,
/// which `ServerConfig`'s own `deny_unknown_fields` already does. Denying
/// unknown fields at THIS level too (an earlier version did) rejected
/// every other real, valid table in the unified config, making
/// `--server-config` unusable with the actual daemon config this repo's
/// own docs describe (round-11 review finding).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DaemonConfigFile {
    pub server: ServerConfig,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_station_callsign_with_default_ports() {
        let cfg: ServerConfig = toml::from_str(
            r#"
            station_callsign = "W3XYZ"
            "#,
        )
        .unwrap();

        assert_eq!(cfg.station_callsign, "W3XYZ");
        assert_eq!(cfg.telnet_port, 7300);
        assert_eq!(cfg.json_port, 7301);
        assert_eq!(cfg.metrics_port, 7302);
        assert_eq!(cfg.bind_addr, "0.0.0.0");
    }

    #[test]
    fn explicit_ports_override_defaults() {
        let cfg: ServerConfig = toml::from_str(
            r#"
            station_callsign = "W3XYZ"
            telnet_port = 17300
            json_port = 17301
            metrics_port = 17302
            bind_addr = "127.0.0.1"
            "#,
        )
        .unwrap();

        assert_eq!(cfg.telnet_port, 17300);
        assert_eq!(cfg.json_port, 17301);
        assert_eq!(cfg.metrics_port, 17302);
        assert_eq!(cfg.bind_addr, "127.0.0.1");
    }

    #[test]
    fn missing_station_callsign_is_a_parse_error() {
        let result: Result<ServerConfig, _> = toml::from_str("");
        assert!(result.is_err());
    }

    #[test]
    fn implausible_station_callsign_is_rejected() {
        for bad in ["", "W3XYZ-#", "W3XYZ\r\nEVIL LINE", "not a callsign"] {
            let result: Result<ServerConfig, _> =
                toml::from_str(&format!(r#"station_callsign = {bad:?}"#));
            assert!(result.is_err(), "{bad:?} should have been rejected");
        }
    }

    #[test]
    fn unknown_server_config_key_is_a_parse_error() {
        // Regression (round-6 review): a typo'd key (e.g. `bind_address`
        // instead of `bind_addr`) must not silently parse and fall back to
        // that field's default -- for `bind_addr` specifically, silently
        // keeping the "0.0.0.0" default instead of the operator's intended
        // restriction unexpectedly exposes all three listeners publicly.
        let result: Result<ServerConfig, _> = toml::from_str(
            r#"
            station_callsign = "W3XYZ"
            bind_address = "127.0.0.1"
            "#,
        );
        assert!(result.is_err(), "unknown key should have been rejected");
    }

    #[test]
    fn daemon_config_file_parses_the_real_server_table() {
        let file: DaemonConfigFile = toml::from_str(
            r#"
            [server]
            station_callsign = "W3XYZ"
            telnet_port = 17300
            "#,
        )
        .unwrap();

        assert_eq!(file.server.station_callsign, "W3XYZ");
        assert_eq!(file.server.telnet_port, 17300);
        assert_eq!(file.server.json_port, 7301);
    }

    #[test]
    fn daemon_config_file_permits_the_other_daemon_tables() {
        // Regression (round-11 review): ARCHITECTURE §8 / SPEC §9 describe
        // ONE daemon TOML with multiple top-level tables ([detector],
        // [decode], [input], [spot], [server], ...) -- manta-server only
        // models [server], but the file's OWN top-level
        // `deny_unknown_fields` (a round-6 fix meant for typos INSIDE
        // [server]) rejected every other real, valid table too, making
        // --server-config unusable with the actual unified daemon config
        // the rest of this repo's docs describe.
        let file: DaemonConfigFile = toml::from_str(
            r#"
            [detector]
            on_snr_db = 6.0

            [decode]
            timing_sigma = 0.25

            [server]
            station_callsign = "W3XYZ"
            "#,
        )
        .expect("unrelated daemon tables alongside [server] must not be rejected");

        assert_eq!(file.server.station_callsign, "W3XYZ");
    }

    #[test]
    fn server_config_still_rejects_unknown_keys_within_its_own_table() {
        // The round-6 fix's actual intent (catching a typo like
        // bind_address vs bind_addr) must survive -- only the TOP-LEVEL
        // wrapper's over-broad deny_unknown_fields was wrong.
        let result: Result<DaemonConfigFile, _> = toml::from_str(
            r#"
            [server]
            station_callsign = "W3XYZ"
            bind_address = "127.0.0.1"
            "#,
        );
        assert!(
            result.is_err(),
            "a typo'd key inside [server] must still be rejected"
        );
    }
}
