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
    /// Overrides the telnet listener's per-source-IP connection quota
    /// (MAN-61) -- `None` (default, field omitted) uses the built-in
    /// default (16). `0` means "no per-IP cap" (only
    /// `MAX_TELNET_CONNECTIONS`'s total ceiling still applies). See
    /// `json_max_connections_per_ip`'s doc comment for why these three
    /// are separate, per-listener fields rather than one shared knob
    /// (PR #81 review, round 3).
    #[serde(default)]
    pub telnet_max_connections_per_ip: Option<usize>,
    /// Overrides the JSON/WS listener's per-source-IP connection quota
    /// (MAN-61) -- `None` (default, field omitted) uses the built-in
    /// default (16). `0` means "no per-IP cap" (only
    /// `MAX_JSON_STREAM_CONNECTIONS`'s total ceiling still applies).
    /// Needed for the documented reverse-proxy TLS-termination deployment
    /// (`docs/RUNBOOKS/network-exposure.md`): every client behind the
    /// proxy shares the proxy's own IP as far as `peer.ip()` is
    /// concerned, so the built-in per-IP default would otherwise cap
    /// TOTAL concurrent clients at the quota instead of the listener's
    /// real capacity.
    ///
    /// Deliberately a SEPARATE field from `telnet_max_connections_per_ip`/
    /// `metrics_max_connections_per_ip`, not one shared override (PR #81
    /// review, round 3, correcting round 1's initial single-knob design):
    /// the runbook's reverse-proxy setup only fronts the JSON/WS port --
    /// telnet and metrics stay directly exposed. A single shared override
    /// set to disable the JSON/WS quota would ALSO disable it on those
    /// still-directly-exposed listeners, undoing MAN-61's protection on
    /// listeners that were never behind the proxy.
    #[serde(default)]
    pub json_max_connections_per_ip: Option<usize>,
    /// Overrides the metrics listener's per-source-IP connection quota
    /// (MAN-61) -- `None` (default, field omitted) uses the built-in
    /// default (8). `0` means "no per-IP cap" (only
    /// `MAX_METRICS_CONNECTIONS`'s total ceiling still applies). See
    /// `json_max_connections_per_ip`'s doc comment for why these three
    /// are separate, per-listener fields.
    #[serde(default)]
    pub metrics_max_connections_per_ip: Option<usize>,
    /// Overrides the telnet listener's per-source-IP AGGREGATE command
    /// rate budget (MAN-57) -- separate from `telnet_max_connections_per_ip`
    /// above, which bounds concurrent connections, not command rate.
    /// `None` (default, field omitted) uses the built-in default
    /// (`telnet::MAX_TELNET_COMMANDS` per `telnet::COMMAND_RATE_WINDOW`).
    /// `0` means no per-IP aggregate cap (only each connection's own
    /// per-connection budget still applies). Needed for the same
    /// reverse-proxy deployment `json_max_connections_per_ip` documents:
    /// see `rate_limit::IpRateLimiter::new_with_override`'s doc comment.
    #[serde(default)]
    pub telnet_max_commands_per_ip: Option<u32>,
    /// Overrides the JSON/WS listener's per-source-IP AGGREGATE Ping rate
    /// budget (MAN-57) -- separate from `json_max_connections_per_ip`
    /// above, which bounds concurrent connections, not Ping rate. `None`
    /// (default, field omitted) uses the built-in default
    /// (`json_stream::MAX_INBOUND_PINGS` per `json_stream::PING_RATE_WINDOW`).
    /// `0` means no per-IP aggregate cap (only each connection's own
    /// per-connection budget still applies). See
    /// `rate_limit::IpRateLimiter::new_with_override`'s doc comment.
    #[serde(default)]
    pub json_max_pings_per_ip: Option<u32>,
}

/// One `[[rbn_uplink]]` TOML array-of-tables entry -- MAN-32/MAN-42.
/// Outbound telnet client that logs into an RBN spot-collection endpoint
/// and forwards manta's spots there. `manta_cli::config::ConfigFile.
/// rbn_uplink` (MAN-74; formerly `manta_server::config::DaemonConfigFile`,
/// removed) holds zero or more of these -- MAN-42 extended the original
/// MAN-32 single optional table to a `Vec` so operators can forward to
/// more than one target; a config with the table omitted entirely still
/// means the uplink is off, so existing single-node operators see no
/// behavior change. Scoped `deny_unknown_fields` the same way
/// `ServerConfig` is, but unlike the unified `ConfigFile` wrapper around
/// both (which is deliberately NOT -- see that type's own doc comment):
/// this only needs to reject a typo INSIDE one `[[rbn_uplink]]` block, and
/// doing so is specifically safety-relevant here -- an operator typo like
/// `dry-run` instead of `dry_run` would otherwise silently parse as the
/// untouched `dry_run = false` default and start transmitting real spots
/// to RBN.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
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

    /// PR #81 review, round 3: the three per-IP quota overrides are
    /// independent fields, not one shared knob -- setting only
    /// `json_max_connections_per_ip` must leave the other two `None`.
    #[test]
    fn per_ip_quota_overrides_default_to_none_and_are_independent() {
        let cfg: ServerConfig = toml::from_str(
            r#"
            station_callsign = "W3XYZ"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.telnet_max_connections_per_ip, None);
        assert_eq!(cfg.json_max_connections_per_ip, None);
        assert_eq!(cfg.metrics_max_connections_per_ip, None);

        let cfg: ServerConfig = toml::from_str(
            r#"
            station_callsign = "W3XYZ"
            json_max_connections_per_ip = 0
            "#,
        )
        .unwrap();
        assert_eq!(cfg.telnet_max_connections_per_ip, None);
        assert_eq!(cfg.json_max_connections_per_ip, Some(0));
        assert_eq!(cfg.metrics_max_connections_per_ip, None);
    }

    /// MAN-57: separate from the per-IP connection quota above --
    /// `telnet_max_commands_per_ip`/`json_max_pings_per_ip` override the
    /// per-IP AGGREGATE rate budget, not the connection count. Independent
    /// fields for the same reason the connection quota overrides are.
    #[test]
    fn per_ip_rate_overrides_default_to_none_and_are_independent() {
        let cfg: ServerConfig = toml::from_str(
            r#"
            station_callsign = "W3XYZ"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.telnet_max_commands_per_ip, None);
        assert_eq!(cfg.json_max_pings_per_ip, None);

        let cfg: ServerConfig = toml::from_str(
            r#"
            station_callsign = "W3XYZ"
            json_max_pings_per_ip = 0
            "#,
        )
        .unwrap();
        assert_eq!(cfg.telnet_max_commands_per_ip, None);
        assert_eq!(cfg.json_max_pings_per_ip, Some(0));
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

    // MAN-32/MAN-42: [[rbn_uplink]] array-of-tables. MAN-74: the old
    // `DaemonConfigFile`-wrapper tests that used to live here (unified
    // top-level parsing, cross-table permissiveness, array-of-tables
    // deserialization) moved to `manta-cli::config`'s tests -- that crate
    // now owns the one unified daemon-TOML type; this crate keeps owning
    // only `ServerConfig`/`RbnUplinkConfig` themselves.

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
