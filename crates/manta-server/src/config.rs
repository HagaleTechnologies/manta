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

fn default_dry_run() -> bool {
    false
}

/// Shared by `deserialize_station_callsign` (required) and
/// `deserialize_optional_callsign` (MAN-32's `login_callsign`, optional) so
/// the plausibility rule -- and the line-injection concern it guards
/// against, see `ServerConfig::station_callsign`'s doc comment -- can't
/// drift between the two call sites.
fn check_plausible(call: &str) -> Result<(), String> {
    if !manta_spot::grammar::is_plausible(call) {
        return Err(format!("{call:?} is not a plausible callsign"));
    }
    Ok(())
}

fn deserialize_station_callsign<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let call = String::deserialize(deserializer)?;
    check_plausible(&call)
        .map_err(|e| serde::de::Error::custom(format!("station_callsign {e}")))?;
    Ok(call)
}

fn deserialize_optional_callsign<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let call: Option<String> = Option::deserialize(deserializer)?;
    if let Some(call) = &call {
        check_plausible(call)
            .map_err(|e| serde::de::Error::custom(format!("login_callsign {e}")))?;
    }
    Ok(call)
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
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
/// wrapper. ARCHITECTURE §8: "Single TOML config... server ports."
/// `[rbn_uplink]` TOML table -- MAN-32. Outbound telnet client that logs
/// into RBN's own spot-collection endpoint and forwards manta's spots
/// there. Absent from a config file entirely (`None`) means the uplink is
/// off; existing single-node operators see no behavior change.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RbnUplinkConfig {
    pub enabled: bool,
    pub target_host: String,
    pub target_port: u16,
    /// Defaults to `[server].station_callsign` when omitted -- see
    /// `effective_login_callsign`.
    #[serde(default, deserialize_with = "deserialize_optional_callsign")]
    pub login_callsign: Option<String>,
    /// When true, the connection is still made (so operators can validate
    /// connectivity/login) but spot lines are not transmitted --
    /// legacy Aggregator's "prevent sending false spots during testing"
    /// checkbox, folded into MAN-32's scope per
    /// `docs/DECISIONS/2026-09-01-legacy-capability-matrix.md:91`.
    #[serde(default = "default_dry_run")]
    pub dry_run: bool,
}

impl RbnUplinkConfig {
    pub fn effective_login_callsign<'a>(&'a self, station_callsign: &'a str) -> &'a str {
        self.login_callsign.as_deref().unwrap_or(station_callsign)
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DaemonConfigFile {
    pub server: ServerConfig,
    pub rbn_uplink: Option<RbnUplinkConfig>,
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

    // MAN-32: [rbn_uplink] table.

    #[test]
    fn uplink_disabled_by_default_when_table_omitted() {
        let file: DaemonConfigFile = toml::from_str(
            r#"
            [server]
            station_callsign = "W3XYZ"
            "#,
        )
        .unwrap();
        assert!(file.rbn_uplink.is_none());
    }

    #[test]
    fn uplink_table_requires_target_when_present() {
        let result: Result<DaemonConfigFile, _> = toml::from_str(
            r#"
            [server]
            station_callsign = "W3XYZ"
            [rbn_uplink]
            enabled = true
            "#,
        );
        assert!(
            result.is_err(),
            "enabled=true with no target should fail to parse"
        );
    }

    #[test]
    fn uplink_table_parses_with_defaults() {
        let file: DaemonConfigFile = toml::from_str(
            r#"
            [server]
            station_callsign = "W3XYZ"
            [rbn_uplink]
            enabled = true
            target_host = "example.invalid"
            target_port = 7300
            "#,
        )
        .unwrap();
        let uplink = file.rbn_uplink.unwrap();
        assert!(uplink.enabled);
        assert_eq!(uplink.target_host, "example.invalid");
        assert_eq!(uplink.target_port, 7300);
        assert!(!uplink.dry_run);
        assert_eq!(uplink.login_callsign, None);
    }

    #[test]
    fn uplink_rejects_implausible_login_callsign() {
        let result: Result<DaemonConfigFile, _> = toml::from_str(
            r#"
            [server]
            station_callsign = "W3XYZ"
            [rbn_uplink]
            enabled = true
            target_host = "example.invalid"
            target_port = 7300
            login_callsign = "W3XYZ\r\nEVIL LINE"
            "#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn uplink_effective_login_callsign_falls_back_to_station_callsign() {
        let uplink = RbnUplinkConfig {
            enabled: true,
            target_host: "example.invalid".to_string(),
            target_port: 7300,
            login_callsign: None,
            dry_run: false,
        };
        assert_eq!(uplink.effective_login_callsign("W3XYZ"), "W3XYZ");

        let uplink_override = RbnUplinkConfig {
            login_callsign: Some("W3XYZ-2".to_string()),
            ..uplink
        };
        assert_eq!(uplink_override.effective_login_callsign("W3XYZ"), "W3XYZ-2");
    }
}
