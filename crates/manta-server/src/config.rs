//! `[server]` TOML config table. ARCHITECTURE §8: "server ports, ...
//! station callsign (spotter ID)" in the single daemon TOML config.
//! Naming follows `docs/SPEC-decode-core.md` §9's convention
//! (`lower_snake_case`, `_port` unit suffix, table name = subsystem name).

use serde::Deserialize;

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

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ServerConfig {
    /// The spotter's own callsign, used as the telnet `DX de <call>-#:`
    /// identity and the JSON stream's `deCall`. No sensible default --
    /// every real cluster/spot-stream node identifies itself.
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
}
