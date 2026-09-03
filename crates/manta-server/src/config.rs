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
    /// Overrides the per-source-IP connection quota (MAN-61) applied
    /// uniformly across the telnet, JSON/WS, and metrics listeners --
    /// `None` (the default, field omitted) uses each listener's own
    /// built-in default (16/16/8). Needed for the documented reverse-proxy
    /// TLS-termination deployment (`docs/RUNBOOKS/network-exposure.md`):
    /// every client behind the proxy shares the proxy's own IP as far as
    /// `peer.ip()` is concerned, so the built-in per-IP defaults would
    /// otherwise cap TOTAL concurrent clients at the quota instead of the
    /// listener's real capacity (PR #81 review, round 1). `0` means "no
    /// per-IP cap" -- only the listener's total connection ceiling
    /// applies, and the operator relies on the reverse proxy's own
    /// connection-rate limiting instead (MAN-61's ticket's own
    /// third-option disposition, now actionable for this topology).
    #[serde(default)]
    pub max_connections_per_ip: Option<usize>,
}

/// One `[[rbn_uplink]]` TOML array-of-tables entry -- MAN-32/MAN-42.
/// Outbound telnet client that logs into an RBN spot-collection endpoint
/// and forwards manta's spots there. `DaemonConfigFile.rbn_uplink` holds
/// zero or more of these -- MAN-42 extended the original MAN-32 single
/// optional table to a `Vec` so operators can forward to more than one
/// target; a config with the table omitted entirely still means the
/// uplink is off, so existing single-node operators see no behavior
/// change. Scoped `deny_unknown_fields` the same way `ServerConfig` is (see
/// `DaemonConfigFile`'s doc comment on why the wrapper itself is NOT):
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

/// The real on-disk daemon config file's shape: a `[server]` TOML table
/// (this is the file `manta listen --server-config <path>` reads) --
/// distinct from `ServerConfig` itself so that struct can stay a plain,
/// directly-deserializable value everywhere else (tests, future in-process
/// construction) without every caller needing to know about the table
/// wrapper. ARCHITECTURE §8: "Single TOML config... server ports." That
/// same single daemon TOML also carries `[detector]`/`[decode]`/`[input]`/
/// `[spot]` and other tables this crate doesn't model (SPEC §9) --
/// deliberately NOT `deny_unknown_fields` here, unlike `ServerConfig`/
/// `RbnUplinkConfig` themselves: this wrapper only needs to reject a typo
/// INSIDE `[server]`/`[[rbn_uplink]]`, which those structs' own
/// `deny_unknown_fields` already does. Denying unknown fields at THIS
/// level too (an earlier version did) rejected every other real, valid
/// table in the unified config, making `--server-config` unusable with
/// the actual daemon config this repo's own docs describe (round-11
/// review finding).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DaemonConfigFile {
    pub server: ServerConfig,
    /// MAN-42: zero or more `[[rbn_uplink]]` array-of-tables entries --
    /// changed from MAN-32's single optional `[rbn_uplink]` table. No real
    /// deployed config used the old single-table syntax, so this is a
    /// direct breaking change to the TOML shape rather than a migration.
    #[serde(default)]
    pub rbn_uplink: Vec<RbnUplinkConfig>,
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

    // MAN-32/MAN-42: [[rbn_uplink]] array-of-tables.

    #[test]
    fn uplink_table_omitted_parses_as_empty_vec() {
        let file: DaemonConfigFile = toml::from_str(
            r#"
            [server]
            station_callsign = "W3XYZ"
            "#,
        )
        .unwrap();
        assert!(file.rbn_uplink.is_empty());
    }

    #[test]
    fn uplink_table_requires_target_when_present() {
        let result: Result<DaemonConfigFile, _> = toml::from_str(
            r#"
            [server]
            station_callsign = "W3XYZ"
            [[rbn_uplink]]
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
            [[rbn_uplink]]
            enabled = true
            target_host = "example.invalid"
            target_port = 7300
            "#,
        )
        .unwrap();
        assert_eq!(file.rbn_uplink.len(), 1);
        let uplink = &file.rbn_uplink[0];
        assert!(uplink.enabled);
        assert_eq!(uplink.target_host, "example.invalid");
        assert_eq!(uplink.target_port, 7300);
        assert!(!uplink.dry_run);
        assert_eq!(uplink.login_callsign, None);
    }

    #[test]
    fn two_rbn_uplink_tables_parse_into_a_vec_of_two() {
        let file: DaemonConfigFile = toml::from_str(
            r#"
            [server]
            station_callsign = "W3XYZ"
            [[rbn_uplink]]
            enabled = true
            target_host = "rbn1.example"
            target_port = 7300
            [[rbn_uplink]]
            enabled = true
            target_host = "rbn2.example"
            target_port = 7301
            "#,
        )
        .unwrap();
        assert_eq!(file.rbn_uplink.len(), 2);
        assert_eq!(file.rbn_uplink[0].target_host, "rbn1.example");
        assert_eq!(file.rbn_uplink[1].target_host, "rbn2.example");
    }

    #[test]
    fn single_bracket_rbn_uplink_table_is_a_parse_error() {
        // The old MAN-32 single-table syntax no longer parses -- a
        // `Vec<RbnUplinkConfig>` field can't deserialize from a bare
        // `[rbn_uplink]` table, only from `[[rbn_uplink]]` array-of-tables.
        let result: Result<DaemonConfigFile, _> = toml::from_str(
            r#"
            [server]
            station_callsign = "W3XYZ"
            [rbn_uplink]
            enabled = true
            target_host = "example.invalid"
            target_port = 7300
            "#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn uplink_unknown_key_is_a_parse_error() {
        // Mirrors ServerConfig's own deny_unknown_fields rule (round-6
        // review on MAN-12/PR#63): a typo'd key here is safety-relevant in
        // a way ServerConfig's typos aren't -- e.g. `dry-run` instead of
        // `dry_run` would otherwise silently parse as the untouched
        // dry_run=false default and start transmitting real spots to RBN
        // instead of failing loudly.
        let result: Result<DaemonConfigFile, _> = toml::from_str(
            r#"
            [server]
            station_callsign = "W3XYZ"
            [[rbn_uplink]]
            enabled = true
            target_host = "example.invalid"
            target_port = 7300
            dry-run = true
            "#,
        );
        assert!(result.is_err(), "unknown key should have been rejected");
    }

    #[test]
    fn uplink_rejects_implausible_login_callsign() {
        let result: Result<DaemonConfigFile, _> = toml::from_str(
            r#"
            [server]
            station_callsign = "W3XYZ"
            [[rbn_uplink]]
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
