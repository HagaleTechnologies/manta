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

fn default_ws_port() -> u16 {
    7303
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
    /// TCP JSON Lines port. ARCHITECTURE §7's diagram labels this "tcp/ws
    /// :7301" as one shared port; this implementation gives WebSocket its
    /// own `ws_port` (default 7303) instead of protocol-sniffing a shared
    /// socket -- simpler and avoids the failure modes of guessing whether
    /// the first bytes on a connection are an HTTP upgrade or raw JSON.
    #[serde(default = "default_json_port")]
    pub json_port: u16,
    #[serde(default = "default_ws_port")]
    pub ws_port: u16,
    #[serde(default = "default_metrics_port")]
    pub metrics_port: u16,
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
        assert_eq!(cfg.ws_port, 7303);
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
            ws_port = 17303
            metrics_port = 17302
            bind_addr = "127.0.0.1"
            "#,
        )
        .unwrap();

        assert_eq!(cfg.telnet_port, 17300);
        assert_eq!(cfg.json_port, 17301);
        assert_eq!(cfg.ws_port, 17303);
        assert_eq!(cfg.metrics_port, 17302);
        assert_eq!(cfg.bind_addr, "127.0.0.1");
    }

    #[test]
    fn missing_station_callsign_is_a_parse_error() {
        let result: Result<ServerConfig, _> = toml::from_str("");
        assert!(result.is_err());
    }
}
