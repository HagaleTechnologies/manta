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

fn default_metrics_bind_addr() -> String {
    "127.0.0.1".to_string()
}

/// MAN-159: dry-run is ON unless an operator explicitly turns it off.
/// Enabling a `[[rbn_uplink]]` block is already a deliberate act, but it
/// should not *also* silently mean "start transmitting to a real RBN
/// target" -- especially before MAN-90 verifies what a real target
/// accepts. Mirrors legacy Aggregator's "prevent sending false spots
/// during testing" checkbox, which is only useful if it is the default.
fn default_dry_run() -> bool {
    true
}

/// The operator-configured band index in RBN's `CALL-N-#` convention (MAN-89
/// / D4), stripped back off for lookups that want the operator's actual
/// callsign rather than the node's wire identity. Returns `call` unchanged
/// when there is no valid SSID -- the definition of "valid" here is the same
/// one `check_operator_callsign` enforces, and the two must not drift.
pub fn strip_ssid(call: &str) -> &str {
    match call.split_once('-') {
        Some((base, ssid)) if is_valid_ssid(ssid) => base,
        _ => call,
    }
}

/// One or two ASCII digits, no leading zero: `-1` through `-99` (MAN-89
/// Decision 2). Digits-only is what keeps the server's OWN generated `-#`
/// suffix unconfigurable now that `-` is no longer banned outright.
fn is_valid_ssid(ssid: &str) -> bool {
    !ssid.is_empty()
        && ssid.len() <= 2
        && ssid.chars().all(|c| c.is_ascii_digit())
        && !ssid.starts_with('0')
}

/// The shortest thing that can be a callsign: an ITU call is at minimum a
/// one-character prefix, a digit and a one-character suffix (`W1A`). Bounds
/// the slash-composed base directly; the `/`-delimited segment that must be
/// the call itself gets the stronger structural test instead, which implies
/// this length -- see `is_complete_callsign` and `check_operator_callsign`.
const MIN_CALLSIGN_LEN: usize = 3;

/// True when this ONE `/`-delimited segment has the structure of a complete
/// ITU callsign: a prefix, a separating digit, and a letter suffix that runs
/// to the END of the segment -- i.e. some digit with at least one letter
/// BEFORE it, and the segment's LAST character a letter. `W1A`, `W5AU`,
/// `4U1UN`, `3DA0RS`, `GB3LER` and multi-digit special-event forms like
/// `LZ130LO` all satisfy it, as do the digit-led special-event forms below.
///
/// Structure, not just character classes (PR #131 review, round 4): a
/// predicate that only asks for `MIN_CALLSIGN_LEN` characters plus "some
/// digit, some letter" still accepts `W12` (and therefore `W12-1` and
/// `W12/P`, whose only other segment is a portable designator). None of
/// those has a suffix, so none is a callsign -- and the typo would become
/// this node's identity on every telnet line and JSON `deCall`.
///
/// The suffix must run to the end of the segment, not merely exist somewhere
/// after a digit (PR #131 review, round 5): "a letter somewhere to the right
/// of the digit" still accepted `W1A2` (and `W1A2-1`, `W1A2/P`), where the
/// trailing digit sits OUTSIDE the suffix. ITU RR 19.68A puts a letter last
/// in every amateur callsign, so a segment ending in a digit is a typo at any
/// length.
///
/// A DIGIT-LED segment satisfies it too (PR #131 review, round 6): the
/// vendored `master.scp` carries `4AFARU`, `4GRID` and `5NNHR`, whose only
/// digit PRECEDES the first letter, so "a digit with a letter before it"
/// alone would lock those operators out of starting the daemon. The
/// digit-ending forms this predicate exists to reject are unaffected --
/// `W12` and `W1A2` still fail on the trailing-letter rule, which is what
/// separates a missing suffix from a leading numeral.
///
/// The length bound is restated explicitly for that digit-led shape: the
/// separating-digit shape implies `MIN_CALLSIGN_LEN` on its own (letter,
/// digit, letter), but a leading digit plus one letter does not. Prefix
/// (`JW/`) and portable/beacon suffix (`/P`, `/B`, `/3`) segments
/// legitimately fail this and stay accepted -- `check_operator_callsign`
/// only requires that ONE segment pass.
fn is_complete_callsign(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    let Some(first_letter) = bytes.iter().position(u8::is_ascii_alphabetic) else {
        return false;
    };
    // The suffix ENDS the segment: `rposition` alone only proves a letter
    // exists somewhere after the digit, which `W1A2` also satisfies.
    let Some(&last) = bytes.last() else {
        return false;
    };
    if !last.is_ascii_alphabetic() {
        return false;
    }
    if bytes.len() < MIN_CALLSIGN_LEN {
        return false;
    }
    // `4GRID`: a SINGLE leading numeral is the prefix, and the letters after
    // it are the whole call. `first_letter == 1` is what keeps this narrow --
    // ITU digit-led prefixes carry exactly one numeral (`4U`, `3D`, `5N`), so
    // `12A` is still a typo -- and the segment already ends in a letter, so
    // this cannot readmit a suffix-less `W12` either.
    //
    // `1..=9`, not "any digit": no ITU prefix begins with `0` (and no entry in
    // the vendored `master.scp` does either), so `0AB` is a mistyped identity
    // rather than a digit-led special-event call. Restricting the leading
    // numeral keeps this shortcut from being the hole the finding it answers
    // warned against, without touching the separating-digit rule below --
    // `0A1B` still passes there exactly as it did before.
    if matches!(bytes[0], b'1'..=b'9') && first_letter == 1 {
        return true;
    }
    let last_letter = bytes.len() - 1;
    bytes
        .iter()
        .enumerate()
        .any(|(i, b)| b.is_ascii_digit() && i > first_letter && i < last_letter)
}

/// Validates an operator-supplied station identity -- `[server].station_callsign`
/// and MAN-32's `login_callsign`. Shared by both so the rule, and the
/// line-injection concern it guards against (see
/// `ServerConfig::station_callsign`'s doc comment), can't drift between them.
///
/// MAN-45 (PR #63 round-8 finding): this deliberately does NOT reuse
/// `manta_spot::grammar::is_plausible`. That grammar is a cheap prefilter for
/// garbled DECODER OUTPUT (ARCHITECTURE §6.2) -- 3-7 alphanumerics with a
/// fixed portable-suffix allowlist -- and it rejects real operator callsigns
/// the bundled `cty.dat` itself recognizes: `JW/LB2PG` (DXCC prefix override,
/// base too short once split) and `GB3LER/B` (beacon suffix, not in the
/// allowlist). Being over-narrow is correct for the decoder (a false accept
/// costs a bogus spot) and wrong here (a false reject means the station
/// cannot start at all).
///
/// MAN-89 / D4: an optional trailing `-N` SSID is accepted, so a multi-band
/// node can identify each band the way RBN's own spotters do (`W5AU-1` ->
/// `W5AU-1-#` on the wire). `N` is 1-99, digits only.
///
/// What this still guarantees, which is the actual reason this field is
/// validated: the value contains nothing but ASCII alphanumerics, `/`, and a
/// digits-only trailing SSID -- so it can never carry a control character,
/// whitespace, or the server's own generated `-#` suffix into a telnet line
/// or a JSON `deCall`.
///
/// Deliberately NOT gated on `cty.dat`: a newly-issued callsign absent from
/// the bundled snapshot would be rejected, a worse failure than the one this
/// fixes, and it would couple config deserialization to table loading.
fn check_operator_callsign(call: &str) -> Result<(), String> {
    let (base, ssid) = match call.split_once('-') {
        Some((b, s)) => (b, Some(s)),
        None => (call, None),
    };
    if ssid.is_some_and(|s| !is_valid_ssid(s)) {
        return Err(format!(
            "{} is not a plausible callsign (an SSID must be -1 through -99)",
            crate::human::quoted(call)
        ));
    }
    // Base length: `MIN_CALLSIGN_LEN` (the call itself cannot be shorter, so
    // neither can the base that contains it) to 20 (a prefix/base/suffix
    // triple with room to spare) -- an outer bound on what gets interpolated
    // into every output line, not a claim about callsign structure.
    if base.len() < MIN_CALLSIGN_LEN || base.len() > 20 {
        return Err(format!(
            "{} is not a plausible callsign (length)",
            crate::human::quoted(call)
        ));
    }
    if !base.chars().all(|c| c.is_ascii_alphanumeric() || c == '/') {
        // Covers control characters, CR/LF, whitespace and non-ASCII in one rule.
        return Err(format!(
            "{} is not a plausible callsign \
             (only A-Z, 0-9, '/' and a trailing -N SSID are allowed)",
            crate::human::quoted(call)
        ));
    }
    // At most prefix/base/suffix, each non-empty and no longer than any real
    // designator.
    let segments: Vec<&str> = base.split('/').collect();
    if segments.len() > 3 || segments.iter().any(|s| s.is_empty() || s.len() > 10) {
        return Err(format!(
            "{} is not a plausible callsign (structure)",
            crate::human::quoted(call)
        ));
    }
    // The letter-and-digit rule belongs to the SEGMENT that is the actual
    // call, not to the whole string (PR #131 review): applied whole-string it
    // accepts `ABC/123` and `A/1/B`, where no single segment can be a
    // callsign, and that malformed identity then goes out on every telnet and
    // JSON spot. Requiring only that ONE segment satisfy it keeps prefix
    // (`JW/`) and portable/beacon suffix (`/P`, `/B`) segments -- which
    // legitimately carry letters or digits alone -- accepted.
    // That segment must also have the ITU call STRUCTURE, not merely a
    // qualifying length and one of each character class (PR #131 review,
    // rounds 3, 4 and 5) -- prefix, separating digit, and a letter suffix
    // running to the END of the segment; see `is_complete_callsign`.
    if !segments.iter().any(|s| is_complete_callsign(s)) {
        return Err(format!(
            "{} is not a plausible callsign (one segment must be a complete \
             callsign: a prefix, a separating digit, and a letter suffix, as in \
             {MIN_CALLSIGN_LEN}-character W1A)",
            crate::human::quoted(call)
        ));
    }
    Ok(())
}

fn deserialize_station_callsign<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let call = String::deserialize(deserializer)?;
    check_operator_callsign(&call)
        .map_err(|e| serde::de::Error::custom(format!("station_callsign {e}")))?;
    // Normalized here so config casing never reaches the wire:
    // `cty::Table::lookup` and `Validator::allowlist` both uppercase
    // internally, and RBN cluster lines are conventionally uppercase --
    // `deCall: "w3xyz"` would be this one boundary leaking its own
    // formatting downstream.
    Ok(call.to_ascii_uppercase())
}

fn deserialize_optional_callsign<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let call: Option<String> = Option::deserialize(deserializer)?;
    match call {
        Some(call) => {
            check_operator_callsign(&call)
                .map_err(|e| serde::de::Error::custom(format!("login_callsign {e}")))?;
            Ok(Some(call.to_ascii_uppercase()))
        }
        None => Ok(None),
    }
}

/// Operator free-text (name, QTH) is interpolated verbatim into the
/// greeting banner every client sees (MAN-86), so it carries exactly the
/// line-injection concern `ServerConfig::station_callsign` documents: an
/// embedded CR/LF would forge additional cluster lines, and other control
/// characters could manipulate a terminal client's output. Rejected at
/// deserialize time rather than escaped at render time so an operator finds
/// out at daemon start, not silently on the wire.
fn check_operator_text(field: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    if value.chars().any(|c| c.is_control()) {
        return Err(format!("{field} must not contain control characters"));
    }
    Ok(())
}

/// Maidenhead locator: two letters A-R, two digits, optionally two letters
/// A-X (`maidenhead::square` is the one parser). A malformed grid is worse
/// than an absent one -- Aggregator, and every JSON spot's `deGrid`, would
/// carry garbage as this node's location.
fn check_grid(value: &str) -> Result<(), String> {
    crate::maidenhead::square(value)
        .map(|_| ())
        .ok_or_else(|| not_a_grid(value))
}

fn not_a_grid(value: &str) -> String {
    format!(
        "{} is not a Maidenhead grid square",
        crate::human::quoted(value)
    )
}

fn deserialize_optional_operator_name<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value: Option<String> = Option::deserialize(deserializer)?;
    if let Some(value) = &value {
        check_operator_text("operator_name", value).map_err(serde::de::Error::custom)?;
    }
    Ok(value)
}

fn deserialize_optional_operator_qth<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value: Option<String> = Option::deserialize(deserializer)?;
    if let Some(value) = &value {
        check_operator_text("operator_qth", value).map_err(serde::de::Error::custom)?;
    }
    Ok(value)
}

fn deserialize_optional_grid<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value: Option<String> = Option::deserialize(deserializer)?;
    if let Some(value) = &value {
        check_grid(value).map_err(serde::de::Error::custom)?;
    }
    Ok(value)
}

/// A decimal-degree coordinate within `-limit..=limit`. NaN and the
/// infinities are rejected explicitly: TOML spells them `nan`/`inf`, and
/// serde hands them to an `f64` like any other number.
fn check_coordinate(key: &str, value: f64, limit: f64) -> Result<(), String> {
    if !value.is_finite() || value.abs() > limit {
        return Err(format!(
            "{key} must be a number from -{limit} to {limit}, got {value}"
        ));
    }
    Ok(())
}

fn check_history_minutes(value: u32) -> Result<(), String> {
    if value > crate::bus::MAX_HISTORY_MINUTES {
        return Err(format!(
            "history_minutes must be a whole number of minutes from 0 to {}, got {value}",
            crate::bus::MAX_HISTORY_MINUTES
        ));
    }
    Ok(())
}

fn deserialize_optional_history_minutes<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: Deserializer<'de>,
{
    let value: Option<u32> = Option::deserialize(deserializer)?;
    if let Some(value) = value {
        check_history_minutes(value).map_err(serde::de::Error::custom)?;
    }
    Ok(value)
}

fn deserialize_optional_latitude<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value: Option<f64> = Option::deserialize(deserializer)?;
    if let Some(value) = value {
        check_coordinate("operator_lat", value, 90.0).map_err(serde::de::Error::custom)?;
    }
    Ok(value)
}

fn deserialize_optional_longitude<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value: Option<f64> = Option::deserialize(deserializer)?;
    if let Some(value) = value {
        check_coordinate("operator_lon", value, 180.0).map_err(serde::de::Error::custom)?;
    }
    Ok(value)
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// The spotter's own callsign, used as the telnet `DX de <call>-#:`
    /// identity and the JSON stream's `deCall`. No sensible default --
    /// every real cluster/spot-stream node identifies itself. Validated
    /// (via `check_operator_callsign`, a purpose-built operator-identity
    /// validator -- MAN-45/MAN-89, NOT `manta_spot::grammar::is_plausible`,
    /// which is a narrower decoder-output prefilter) at deserialize time:
    /// an empty, control-character-laden, or malformed value would
    /// otherwise be interpolated straight into every telnet line and JSON
    /// `deCall` unescaped -- e.g. a callsign containing `\r\n` could forge
    /// additional bogus cluster lines. Accepts `CALL`, `CALL/SUFFIX` (a DXCC
    /// prefix override or portable designator), and an optional trailing
    /// `-N` per-band SSID (MAN-89 / D4), e.g. `W5AU-1`, matching RBN's own
    /// `CALL-N-#` multi-band node convention. Normalized to uppercase.
    #[serde(deserialize_with = "deserialize_station_callsign")]
    pub station_callsign: String,
    /// Address the telnet and JSON/WebSocket listeners bind. Defaults to
    /// `0.0.0.0`: both are public by design, as RBN nodes run
    /// (ARCHITECTURE §7, D14). Does NOT apply to the metrics listener --
    /// see `metrics_bind_addr`.
    #[serde(default = "default_bind_addr")]
    pub bind_addr: String,
    /// Address the metrics listener (`GET /metrics`, `GET /healthz`) binds
    /// (MAN-132). Defaults to `127.0.0.1`: the endpoint has no
    /// authentication and is operational tooling, not an RBN-facing
    /// surface (D14). Deliberately a SEPARATE field that never follows
    /// `bind_addr` -- widening the public listeners must not widen this
    /// one, and narrowing this one must not take them offline (the
    /// trap `docs/RUNBOOKS/network-exposure.md` warned about while the
    /// address was shared). Same per-listener reasoning as
    /// `json_max_connections_per_ip`.
    #[serde(default = "default_metrics_bind_addr")]
    pub metrics_bind_addr: String,
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
    /// Overrides the metrics listener's per-source-IP response budget
    /// (MAN-64), separate from `metrics_max_connections_per_ip` above,
    /// which bounds concurrent connections. Over-budget complete requests
    /// get an empty 429 instead of normal response rendering and body writes.
    /// This does not limit connection acceptance, task creation, header
    /// reads, or the rate of 429 responses. Each admitted connection charges
    /// the budget once, before routing a complete request or on EOF, a
    /// header-read error, or a header timeout. Empty and malformed attempts
    /// consume budget but are not refused based on it; a TCP liveness probe
    /// consumes a charge just like `GET /metrics`. `None` (field omitted)
    /// uses the built-in default (`metrics_http::MAX_METRICS_REQUESTS_PER_IP`
    /// per `metrics_http::METRICS_REQUEST_RATE_WINDOW`). `0` means no
    /// per-IP cap -- and note that UNLIKE telnet/JSON there is no
    /// per-connection budget left underneath it (this endpoint serves one
    /// request per connection, see `metrics_http::write_response`'s
    /// `Connection: close`), so `0` disables this rate bounding on this
    /// listener entirely; only the total connection ceiling and the
    /// per-IP connection quota still apply. Raise it for a federated or
    /// multi-scraper deployment where every scrape (or a shared liveness
    /// probe) reaches manta from one address; see
    /// `docs/RUNBOOKS/network-exposure.md`.
    #[serde(default)]
    pub metrics_max_requests_per_ip: Option<u32>,
    /// MAN-122 periodic status line cadence, seconds. `None` (field omitted)
    /// uses `status::DEFAULT_STATUS_INTERVAL` (60 s); `0` disables the status
    /// line entirely, for operators who ship logs by the byte.
    #[serde(default)]
    pub status_interval_secs: Option<u64>,
    /// Telnet history window in heard-time minutes (default 30, range 0..=1440).
    /// Always retains the newest 50 publications, never more than 10,000.
    /// Zero restores count-only history of 50 spots. Changes need a restart.
    /// See `docs/DECISIONS/2026-10-11-man93-sh-dx-history-window.md`.
    #[serde(default, deserialize_with = "deserialize_optional_history_minutes")]
    pub history_minutes: Option<u32>,
    /// Which telnet wire layout the INBOUND cluster server emits
    /// (MAN-88). Defaults to `"rbn"`, the RBN relay's fixed-column AK1A
    /// layout that downstream loggers parse today. `"skimmer"` selects
    /// the CW-Skimmer-native layout (no mode column) that W3OA's
    /// Aggregator consumes -- set it only when manta is running behind
    /// an Aggregator install.
    ///
    /// Deliberately scoped to `[server]` and NOT applied to
    /// `[[rbn_uplink]]` (MAN-88 Decision 1): an uplink's peer is an RBN
    /// spot-collection endpoint, not an Aggregator reading from manta,
    /// and MAN-90 has not yet verified what that endpoint accepts --
    /// letting an inbound-listener key silently change the bytes on that
    /// unverified path is exactly the coupling
    /// `docs/DECISIONS/2026-09-06-broad-review-decisions.md` D2 warns
    /// against. `uplink::forward_loop` pins `LineFormat::Rbn`.
    #[serde(default)]
    pub line_format: crate::rbn::LineFormat,
    // The five `operator_*` keys below feed the telnet greeting banner
    // (`operator_name`/`operator_qth`/`operator_grid`, MAN-86) and the
    // station position every JSON spot carries (`operator_grid`/
    // `operator_lat`/`operator_lon` as `deGrid`/`deLat`/`deLon`, MAN-138;
    // see `ServerConfig::station_location`). They are ALSO listed, with
    // these same defaults and validation rules, in
    // `docs/SPEC-decode-core.md` §9's `[server]` block -- the repository's
    // canonical config-key table, and the only place an operator who has
    // not read this source file will look for them. Renaming a key,
    // changing a default, or changing a validation rule here means
    // changing it there too; each field's own doc comment below restates
    // the rule so the two can be diffed by eye.
    /// Operator's given name, e.g. `"Art"`. **Optional; default: absent**
    /// (the field is omitted from the greeting line rather than rendered
    /// as an empty placeholder). Appears in the telnet greeting banner CW
    /// Skimmer clients and RBN Aggregator read on connect (MAN-86) --
    /// Aggregator's Combined Skimmers tab (manual v6.0 §9.2) exists
    /// precisely to hand-enter this same information for sources that
    /// can't supply it. **Validated at deserialize time** by
    /// `check_operator_text`: must be non-empty after trimming and must
    /// contain no control characters, because it is interpolated verbatim
    /// into every connecting client's banner (an embedded CR/LF would
    /// forge cluster lines). A bad value fails daemon start.
    #[serde(default, deserialize_with = "deserialize_optional_operator_name")]
    pub operator_name: Option<String>,
    /// Operator's QTH as free text, e.g. `"Richmond Hill, ON"`.
    /// **Optional; default: absent**, dropped from the greeting line when
    /// unset. **Validated at deserialize time** by the same
    /// `check_operator_text` rule as `operator_name` above -- non-empty
    /// after trimming, no control characters -- and for the same
    /// line-injection reason.
    #[serde(default, deserialize_with = "deserialize_optional_operator_qth")]
    pub operator_qth: Option<String>,
    /// Maidenhead grid square, e.g. `"FN03GW"`. **Optional; default:
    /// absent**, dropped from the greeting line when unset. **Validated
    /// at deserialize time** by `check_grid`: exactly 4 or 6 characters,
    /// field letters `A`-`R`, square digits `0`-`9`, and (when 6)
    /// subsquare letters `A`-`X`; case-insensitive. A malformed grid is
    /// worse than an absent one -- Aggregator would record garbage as
    /// this node's location. Also every JSON spot's `deGrid`, in its
    /// canonical spelling (`FN03gw`), and -- unless `operator_lat`/
    /// `operator_lon` give an exact point -- the square's centre as
    /// `deLat`/`deLon` (MAN-138).
    #[serde(default, deserialize_with = "deserialize_optional_grid")]
    pub operator_grid: Option<String>,
    /// Station latitude in decimal degrees, north positive, e.g.
    /// `43.9375`. **Optional; default: absent.** **Validated at
    /// deserialize time** by `check_coordinate`: a finite number from -90
    /// to 90. Every JSON spot's `deLat` (MAN-138). Set together with
    /// `operator_lon` or not at all; alongside `operator_grid` the point
    /// must lie inside that square -- both checked by
    /// `ServerConfig::station_location`.
    #[serde(default, deserialize_with = "deserialize_optional_latitude")]
    pub operator_lat: Option<f64>,
    /// Station longitude in decimal degrees, east positive (west is
    /// negative), e.g. `-79.4583`. **Optional; default: absent.**
    /// **Validated at deserialize time** by `check_coordinate`: a finite
    /// number from -180 to 180. Every JSON spot's `deLon` (MAN-138). The
    /// same pair and inside-the-grid rules as `operator_lat`.
    #[serde(default, deserialize_with = "deserialize_optional_longitude")]
    pub operator_lon: Option<f64>,
}

/// This station's position as every JSON spot reports it -- `deGrid`,
/// `deLat`, `deLon` (MAN-138) -- resolved from `operator_grid`/
/// `operator_lat`/`operator_lon` by `ServerConfig::station_location`.
/// All three fields or none: a station either has a configured position
/// or every spot says `null`.
#[derive(Debug, Clone, PartialEq)]
pub struct StationLocation {
    pub grid: String,
    pub lat: f64,
    pub lon: f64,
}

impl ServerConfig {
    /// The cross-key rules serde's per-field validators cannot express:
    /// lat and lon come as a pair, and a point given alongside a grid must
    /// lie inside it -- otherwise `deGrid` and `deLat`/`deLon` would
    /// disagree on the wire. `Ok(None)` when no location key is set.
    ///
    /// Re-runs the per-field checks too, so a hand-built `ServerConfig`
    /// that never went through serde gets an `Err`, never a panic or a
    /// clamped position.
    pub fn station_location(&self) -> Result<Option<StationLocation>, String> {
        let point = match (self.operator_lat, self.operator_lon) {
            (None, None) => None,
            (Some(lat), Some(lon)) => {
                check_coordinate("operator_lat", lat, 90.0)?;
                check_coordinate("operator_lon", lon, 180.0)?;
                Some((lat, lon))
            }
            (Some(_), None) => {
                return Err(
                    "operator_lat is set but operator_lon is not -- set both, or neither".into(),
                )
            }
            (None, Some(_)) => {
                return Err(
                    "operator_lon is set but operator_lat is not -- set both, or neither".into(),
                )
            }
        };
        let Some(grid) = &self.operator_grid else {
            return Ok(point.map(|(lat, lon)| StationLocation {
                grid: crate::maidenhead::locator(lat, lon),
                lat,
                lon,
            }));
        };
        // `check_grid`'s rule and message: a hand-built config never went
        // through `deserialize_optional_grid`.
        let (Some(sq), Some(canonical)) = (
            crate::maidenhead::square(grid),
            crate::maidenhead::canonical(grid),
        ) else {
            return Err(not_a_grid(grid));
        };
        let (lat, lon) = match point {
            None => sq.center(),
            Some((lat, lon)) if sq.contains(lat, lon) => (lat, lon),
            Some((lat, lon)) => return Err(outside_grid_message(grid, &sq, lat, lon)),
        };
        Ok(Some(StationLocation {
            grid: canonical,
            lat,
            lon,
        }))
    }
}

/// `station_location`'s error for a point that is not inside the
/// configured square, naming the square's bounds so the operator can see
/// which of the two values is wrong.
fn outside_grid_message(grid: &str, sq: &crate::maidenhead::Square, lat: f64, lon: f64) -> String {
    format!(
        "operator_lat = {lat}, operator_lon = {lon} is outside operator_grid {} \
         (latitude {:.4} to {:.4}, longitude {:.4} to {:.4}) -- fix whichever is wrong, or \
         set only one of operator_grid and operator_lat/operator_lon",
        crate::human::quoted(grid),
        sq.south,
        sq.south + sq.height,
        sq.west,
        sq.west + sq.width,
    )
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
/// this only needs to reject a typo INSIDE one `[[rbn_uplink]]` block.
/// Since MAN-159 made `dry_run` default to `true`, a typo like `dry-run`
/// no longer risks silently transmitting -- it now risks the mirrored
/// confusion of an operator who wrote `dry-run = false` intending to go
/// live and silently staying in dry-run instead. Failing loudly on the
/// typo is the right answer in both directions.
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
    /// Which spot types this target is sent (MAN-91). Omitted means
    /// `cq_beacon`, RBN's own submission rule; `all` forwards every type.
    /// Applies only to this uplink: telnet/JSON still carry every spot.
    #[serde(default)]
    pub spot_types: UplinkSpotTypes,
}

/// Which validated spots an `[[rbn_uplink]]` target is sent -- MAN-91.
/// RBN only accepts a non-beacon station that is calling CQ or TEST, so the
/// default forwards `Cq` (which `manta-spot`'s context parser also assigns
/// to `CQ TEST <call>`) and `Beacon` only, and holds back `De` (a station
/// answering someone else) and `Unknown` (no CQ evidence at all). `all` is
/// the operator override for a collector that wants every type. A spot held
/// back here is counted in the target's `suppressed` counter, alongside
/// dry-run, per ARCHITECTURE §8's "every suppressed item is counted".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UplinkSpotTypes {
    /// CQ and beacon spots only -- RBN's own rule, and the default.
    #[default]
    CqBeacon,
    /// Every spot type, including DE and untyped spots.
    All,
}

impl UplinkSpotTypes {
    /// Whether a spot of type `t` is sent to the target.
    pub fn forwards(self, t: manta_spot::SpotType) -> bool {
        use manta_spot::SpotType;
        match self {
            UplinkSpotTypes::All => true,
            UplinkSpotTypes::CqBeacon => matches!(t, SpotType::Cq | SpotType::Beacon),
        }
    }

    /// The TOML spelling, for `config check` and the startup log.
    pub fn as_str(self) -> &'static str {
        match self {
            UplinkSpotTypes::CqBeacon => "cq_beacon",
            UplinkSpotTypes::All => "all",
        }
    }
}

impl RbnUplinkConfig {
    pub fn effective_login_callsign<'a>(&'a self, station_callsign: &'a str) -> &'a str {
        self.login_callsign.as_deref().unwrap_or(station_callsign)
    }
}

fn default_rbn_feed_host() -> String {
    "telnet.reversebeacon.net".to_string()
}

/// MAN-301: the `[rbn_compare]` table -- live grading of every published
/// spot against what RBN's public CW telnet feed heard within
/// `window_secs` of it. Off unless `enabled = true` or `--rbn-compare`.
/// Every key needs a restart (MAN-78's SIGHUP reload does not apply it).
///
/// `deny_unknown_fields` for the reason `RbnUplinkConfig` gives: a typo
/// such as `window-secs` must fail loudly rather than silently grading
/// with the default. Deliberately NOT a field of `DaemonConfigFile`:
/// strictness about which tables exist stays in `manta-cli`'s loader
/// (MAN-74's lesson, see `DaemonConfigFile`'s doc comment).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RbnCompareConfig {
    /// Default `false`: grading opens an outbound connection to a public
    /// service, so it is an explicit operator choice (MAN-301).
    pub enabled: bool,
    /// RBN's public telnet feed. Default `telnet.reversebeacon.net`, the
    /// host RBN documents for its aggregate feed.
    pub feed_host: String,
    /// Default 7000: RBN's CW and RTTY feed (manta keeps CW only); 7001 is
    /// the digital-modes feed, which carries no CW.
    pub feed_port: u16,
    /// The callsign sent at the feed's login prompt -- the only line manta
    /// ever writes to it. Defaults to `[server].station_callsign` verbatim,
    /// the same rule as `[[rbn_uplink]].login_callsign`; see
    /// `effective_login_callsign`.
    #[serde(deserialize_with = "deserialize_optional_callsign")]
    pub login_callsign: Option<String>,
    /// How far either side of a spot's time an RBN report still counts, in
    /// seconds. Default 180: RBN skimmers re-spot a CQing station every few
    /// minutes, so a shorter window misses real reports and a longer one
    /// pairs unrelated activity. Bounded 30 to 3600 by `validate`.
    pub window_secs: i64,
    /// A spot further than this from RBN's modal frequency, in Hz, is a
    /// frequency disagreement. Default 500 Hz: RBN's own dupe/QSY threshold
    /// is of this order, and skimmer calibration spread is well inside it.
    pub freq_tol_hz: f64,
    /// A speed disagreement needs manta's WPM to differ from RBN's median
    /// by more than this many WPM. Default 3: skimmer speed estimates
    /// commonly differ by 1-2 WPM on the same signal.
    pub speed_tol_wpm: f64,
    /// ...and by more than this percent of RBN's median. Default 20%: at
    /// high speeds a few WPM is ordinary estimator spread.
    pub speed_tol_pct: f64,
    /// Only these RBN skimmers' reports count. Default empty: every
    /// skimmer counts. `--rbn-spotter` replaces this list.
    #[serde(deserialize_with = "deserialize_callsign_list_spotters")]
    pub spotters: Vec<String>,
    /// Never count these skimmers. Default empty; this station's own
    /// callsign, with and without its SSID, is always excluded on top
    /// (`grade_config`), so a node never confirms itself.
    #[serde(deserialize_with = "deserialize_callsign_list_exclude")]
    pub exclude_spotters: Vec<String>,
}

impl Default for RbnCompareConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            feed_host: default_rbn_feed_host(),
            feed_port: 7000,
            login_callsign: None,
            window_secs: 180,
            freq_tol_hz: 500.0,
            speed_tol_wpm: 3.0,
            speed_tol_pct: 20.0,
            spotters: Vec::new(),
            exclude_spotters: Vec::new(),
        }
    }
}

/// `[rbn_compare]`'s `window_secs` bounds (MAN-301).
pub const RBN_COMPARE_WINDOW_SECS: std::ops::RangeInclusive<i64> = 30..=3600;

/// A tolerance must be a finite number from 0 to `max`.
fn check_tolerance(key: &str, value: f64, max: f64) -> Result<(), String> {
    if !value.is_finite() || !(0.0..=max).contains(&value) {
        return Err(format!(
            "[rbn_compare].{key} must be a number from 0 to {max}, got {value}"
        ));
    }
    Ok(())
}

/// A callsign list entry: `check_operator_callsign`, then uppercased.
fn normalize_callsign_list(key: &str, calls: Vec<String>) -> Result<Vec<String>, String> {
    calls
        .into_iter()
        .map(|call| {
            check_operator_callsign(&call).map_err(|e| format!("{key} {e}"))?;
            Ok(call.to_ascii_uppercase())
        })
        .collect()
}

fn deserialize_callsign_list_spotters<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    normalize_callsign_list("spotters", Vec::<String>::deserialize(deserializer)?)
        .map_err(serde::de::Error::custom)
}

fn deserialize_callsign_list_exclude<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    normalize_callsign_list(
        "exclude_spotters",
        Vec::<String>::deserialize(deserializer)?,
    )
    .map_err(serde::de::Error::custom)
}

/// `--rbn-spotter`'s value check: the same rule and normalisation as an
/// entry of `[rbn_compare].spotters`.
pub fn normalize_rbn_spotter(call: &str) -> Result<String, String> {
    check_operator_callsign(call)?;
    Ok(call.to_ascii_uppercase())
}

/// An RBN spotter or exclusion entry as the grader compares it: uppercase,
/// without the RBN `-#` skimmer suffix.
fn grade_call(call: &str) -> String {
    let upper = call.to_ascii_uppercase();
    upper
        .strip_suffix("-#")
        .map(str::to_string)
        .unwrap_or(upper)
}

impl RbnCompareConfig {
    pub fn effective_login_callsign<'a>(&'a self, station_callsign: &'a str) -> &'a str {
        self.login_callsign.as_deref().unwrap_or(station_callsign)
    }

    /// The range and shape checks serde's types cannot express. Every
    /// message names the key as `[rbn_compare].<key>`; the loader prefixes
    /// the file. Re-checks the callsign fields too, so a hand-built config
    /// that never went through serde gets an `Err`, never a bad login line.
    pub fn validate(&self) -> Result<(), String> {
        if !RBN_COMPARE_WINDOW_SECS.contains(&self.window_secs) {
            return Err(format!(
                "[rbn_compare].window_secs must be {} to {}, got {}",
                RBN_COMPARE_WINDOW_SECS.start(),
                RBN_COMPARE_WINDOW_SECS.end(),
                self.window_secs
            ));
        }
        check_tolerance("freq_tol_hz", self.freq_tol_hz, 10_000.0)?;
        check_tolerance("speed_tol_wpm", self.speed_tol_wpm, 20.0)?;
        check_tolerance("speed_tol_pct", self.speed_tol_pct, 100.0)?;
        if self.feed_port == 0 {
            return Err("[rbn_compare].feed_port must be between 1 and 65535, got 0".into());
        }
        if self.feed_host.is_empty()
            || self
                .feed_host
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(format!(
                "[rbn_compare].feed_host must be a host name or address without spaces or \
                 control characters, got {}",
                crate::human::quoted(&self.feed_host)
            ));
        }
        if let Some(call) = &self.login_callsign {
            check_operator_callsign(call)
                .map_err(|e| format!("[rbn_compare].login_callsign {e}"))?;
        }
        for (key, list) in [
            ("spotters", &self.spotters),
            ("exclude_spotters", &self.exclude_spotters),
        ] {
            for call in list {
                check_operator_callsign(call).map_err(|e| format!("[rbn_compare].{key} {e}"))?;
            }
        }
        Ok(())
    }

    /// The `spotters` entry that is this station's own callsign (with or
    /// without its SSID), if any. Its reports are always excluded
    /// (`grade_config`), so listing it would grade every spot `none`.
    pub fn own_call_spotter<'a>(&'a self, station_call: &str) -> Option<&'a str> {
        let own = [
            grade_call(station_call),
            grade_call(strip_ssid(station_call)),
        ];
        self.spotters
            .iter()
            .map(String::as_str)
            .find(|s| own.contains(&grade_call(s)))
    }

    /// The refusal for `own_call_spotter`, naming the entry as listed.
    pub fn check_spotters_against_station(&self, station_call: &str) -> Result<(), String> {
        match self.own_call_spotter(station_call) {
            Some(call) => Err(format!(
                "[rbn_compare].spotters lists {call}, this station's own callsign: its reports \
                 are always excluded, so every spot would grade none"
            )),
            None => Ok(()),
        }
    }

    /// The grading engine's view of this table: callsign sets uppercased
    /// with any RBN `-#` suffix stripped, and this station's own callsign,
    /// with and without its SSID, always excluded.
    pub fn grade_config(&self, station_call: &str) -> crate::rbn_compare::grade::GradeConfig {
        let mut exclude_spotters: std::collections::BTreeSet<String> = self
            .exclude_spotters
            .iter()
            .map(|c| grade_call(c))
            .collect();
        exclude_spotters.insert(grade_call(station_call));
        exclude_spotters.insert(grade_call(strip_ssid(station_call)));
        crate::rbn_compare::grade::GradeConfig {
            window_secs: self.window_secs,
            freq_tol_hz: self.freq_tol_hz,
            speed_tol_wpm: self.speed_tol_wpm,
            speed_tol_pct: self.speed_tol_pct,
            spotters: self.spotters.iter().map(|c| grade_call(c)).collect(),
            exclude_spotters,
        }
    }
}

/// The real on-disk daemon config file's shape: a `[server]` TOML table
/// (this is the file `manta run --config <path>` reads; `manta listen`
/// and `--server-config` remain deprecated aliases, D11/MAN-77) --
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
/// table in the unified config, making `--config` unusable with
/// the actual daemon config this repo's own docs describe (round-11
/// review finding). MAN-261: the strict top-level check (an unknown table
/// such as `[detectr]` is an error naming it) lives once in
/// `manta-cli`'s `config` module, the only crate that knows the full table
/// set; the daemon itself now parses `[server]`/`[[rbn_uplink]]` there too.
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
        assert_eq!(cfg.metrics_bind_addr, "127.0.0.1");
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
            metrics_bind_addr = "::1"
            "#,
        )
        .unwrap();

        assert_eq!(cfg.telnet_port, 17300);
        assert_eq!(cfg.json_port, 17301);
        assert_eq!(cfg.metrics_port, 17302);
        assert_eq!(cfg.bind_addr, "127.0.0.1");
        assert_eq!(cfg.metrics_bind_addr, "::1");
    }

    /// MAN-132 / D14: metrics defaults to loopback while telnet/JSON stay
    /// public, and the two addresses never follow each other.
    #[test]
    fn metrics_bind_addr_defaults_to_loopback_independently_of_bind_addr() {
        let cfg: ServerConfig = toml::from_str(r#"station_callsign = "W3XYZ""#).unwrap();
        assert_eq!(
            (cfg.bind_addr.as_str(), cfg.metrics_bind_addr.as_str()),
            ("0.0.0.0", "127.0.0.1")
        );

        let cfg: ServerConfig =
            toml::from_str("station_callsign = \"W3XYZ\"\nbind_addr = \"192.168.1.5\"\n").unwrap();
        assert_eq!(
            cfg.metrics_bind_addr, "127.0.0.1",
            "widening bind_addr must not widen metrics"
        );

        let cfg: ServerConfig =
            toml::from_str("station_callsign = \"W3XYZ\"\nmetrics_bind_addr = \"0.0.0.0\"\n")
                .unwrap();
        assert_eq!(
            (cfg.bind_addr.as_str(), cfg.metrics_bind_addr.as_str()),
            ("0.0.0.0", "0.0.0.0")
        );
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

    /// MAN-64: separate from `metrics_max_connections_per_ip` (connections)
    /// and from telnet/JSON's rate overrides (different listeners) -- the
    /// runbook's proxy/federation guidance is per-listener, so a shared knob
    /// would disable protection on listeners that were never behind a proxy
    /// (PR #81 review round 3's correction, applied to this field too).
    #[test]
    fn metrics_request_rate_override_defaults_to_none_and_is_independent() {
        let cfg: ServerConfig = toml::from_str(
            r#"
            station_callsign = "W3XYZ"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.metrics_max_requests_per_ip, None);

        let cfg: ServerConfig = toml::from_str(
            r#"
            station_callsign = "W3XYZ"
            metrics_max_requests_per_ip = 0
            "#,
        )
        .unwrap();
        assert_eq!(cfg.metrics_max_requests_per_ip, Some(0));
        assert_eq!(cfg.metrics_max_connections_per_ip, None);
        assert_eq!(cfg.telnet_max_commands_per_ip, None);
    }

    #[test]
    fn operator_identity_keys_are_optional_and_default_to_none() {
        let cfg: ServerConfig = toml::from_str(r#"station_callsign = "W3XYZ""#).unwrap();
        assert_eq!(cfg.operator_name, None);
        assert_eq!(cfg.operator_qth, None);
        assert_eq!(cfg.operator_grid, None);
    }

    #[test]
    fn operator_lat_and_lon_are_optional_and_default_to_none() {
        let cfg: ServerConfig = toml::from_str(r#"station_callsign = "W3XYZ""#).unwrap();
        assert_eq!(cfg.operator_lat, None);
        assert_eq!(cfg.operator_lon, None);
    }

    /// MAN-138: TOML integers coerce, so `operator_lon = -72` works as written.
    #[test]
    fn operator_lat_and_lon_parse_integers_and_decimals() {
        let cfg: ServerConfig = toml::from_str(
            "station_callsign = \"W1AW\"\noperator_lat = 41.714775\noperator_lon = -72\n",
        )
        .unwrap();
        assert_eq!(cfg.operator_lat, Some(41.714775));
        assert_eq!(cfg.operator_lon, Some(-72.0));
    }

    #[test]
    fn operator_lat_and_lon_reject_out_of_range_and_non_finite_values() {
        for (bad, message) in [
            (
                "operator_lat = 95",
                "operator_lat must be a number from -90 to 90",
            ),
            (
                "operator_lat = -90.5",
                "operator_lat must be a number from -90 to 90",
            ),
            (
                "operator_lon = 180.5",
                "operator_lon must be a number from -180 to 180",
            ),
            (
                "operator_lat = nan",
                "operator_lat must be a number from -90 to 90",
            ),
            (
                "operator_lon = inf",
                "operator_lon must be a number from -180 to 180",
            ),
            (
                "operator_lon = -inf",
                "operator_lon must be a number from -180 to 180",
            ),
        ] {
            let src = format!("station_callsign = \"W1AW\"\n{bad}\n");
            let err = toml::from_str::<ServerConfig>(&src)
                .expect_err(bad)
                .to_string();
            assert!(err.contains(message), "{bad}: {err}");
        }
        for good in [
            "operator_lat = 90\noperator_lon = 180",
            "operator_lat = -90\noperator_lon = -180",
        ] {
            let src = format!("station_callsign = \"W1AW\"\n{good}\n");
            assert!(toml::from_str::<ServerConfig>(&src).is_ok(), "{good}");
        }
    }

    fn server_with(location_keys: &str) -> ServerConfig {
        toml::from_str(&format!("station_callsign = \"W1AW\"\n{location_keys}"))
            .unwrap_or_else(|e| panic!("{location_keys:?}: {e}"))
    }

    #[test]
    fn station_location_is_none_when_no_location_key_is_set() {
        assert_eq!(server_with("").station_location(), Ok(None));
        // The greeting-only keys are not a position.
        let cfg = server_with("operator_qth = \"Newington, CT\"\n");
        assert_eq!(cfg.station_location(), Ok(None));
    }

    #[test]
    fn station_location_from_a_grid_alone_is_its_canonical_spelling_and_centre() {
        let loc = server_with("operator_grid = \"fn31PR\"\n")
            .station_location()
            .unwrap()
            .unwrap();
        assert_eq!(loc.grid, "FN31pr");
        assert_eq!(
            (loc.lat, loc.lon),
            crate::maidenhead::square("FN31pr").unwrap().center()
        );
    }

    #[test]
    fn station_location_from_lat_lon_alone_derives_the_six_character_grid() {
        let loc = server_with("operator_lat = 41.714775\noperator_lon = -72.72726\n")
            .station_location()
            .unwrap()
            .unwrap();
        assert_eq!(
            loc,
            StationLocation {
                grid: "FN31pr".to_string(),
                lat: 41.714775,
                lon: -72.72726,
            }
        );
    }

    #[test]
    fn station_location_with_all_three_keeps_the_configured_grid_and_point() {
        let loc = server_with(
            "operator_grid = \"FN31\"\noperator_lat = 41.714775\noperator_lon = -72.72726\n",
        )
        .station_location()
        .unwrap()
        .unwrap();
        assert_eq!(
            loc,
            StationLocation {
                grid: "FN31".to_string(),
                lat: 41.714775,
                lon: -72.72726,
            }
        );
    }

    #[test]
    fn station_location_rejects_a_point_outside_the_configured_grid() {
        let err =
            server_with("operator_grid = \"FN31pr\"\noperator_lat = 40.0\noperator_lon = -74.0\n")
                .station_location()
                .unwrap_err();
        assert!(
            err.contains(
                "operator_lat = 40, operator_lon = -74 is outside operator_grid \"FN31pr\""
            ),
            "{err}"
        );
        assert!(
            err.contains("(latitude 41.7083 to 41.7500, longitude -72.7500 to -72.6667)"),
            "{err}"
        );
    }

    #[test]
    fn station_location_rejects_lat_without_lon_and_lon_without_lat() {
        assert_eq!(
            server_with("operator_lat = 41.7\n").station_location(),
            Err("operator_lat is set but operator_lon is not -- set both, or neither".to_string())
        );
        assert_eq!(
            server_with("operator_lon = -72.7\n").station_location(),
            Err("operator_lon is set but operator_lat is not -- set both, or neither".to_string())
        );
    }

    /// A `ServerConfig` built in code skips serde's validators; the
    /// resolver must still answer `Err`, never panic or clamp.
    #[test]
    fn station_location_rejects_hand_built_invalid_values() {
        let cfg = ServerConfig {
            operator_grid: Some("ZZ99xx".to_string()),
            ..server_with("")
        };
        assert_eq!(
            cfg.station_location(),
            Err("\"ZZ99xx\" is not a Maidenhead grid square".to_string())
        );
        let cfg = ServerConfig {
            operator_lat: Some(95.0),
            operator_lon: Some(0.0),
            ..server_with("")
        };
        assert!(cfg.station_location().unwrap_err().contains("operator_lat"));
    }

    #[test]
    fn operator_identity_keys_parse_when_present() {
        let cfg: ServerConfig = toml::from_str(
            r#"
            station_callsign = "HB9H"
            operator_name = "Art"
            operator_qth = "Switzerland"
            operator_grid = "JN46la"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.operator_name.as_deref(), Some("Art"));
        assert_eq!(cfg.operator_qth.as_deref(), Some("Switzerland"));
        assert_eq!(cfg.operator_grid.as_deref(), Some("JN46la"));
    }

    #[test]
    fn operator_name_and_qth_reject_line_injection_and_control_characters() {
        // Same concern station_callsign already documents: these are
        // interpolated verbatim into the greeting banner, so an embedded
        // CRLF could forge additional cluster lines to every connecting
        // client.
        for bad in [
            "operator_name = \"Art\\r\\nDX de EVIL-#:  14000.0  N0CALL\"",
            "operator_qth = \"Bern\\nEVIL\"",
            "operator_name = \"Art\\u0007\"",
            "operator_qth = \"\"",
            "operator_name = \"   \"",
        ] {
            let src = format!("station_callsign = \"HB9H\"\n{bad}");
            assert!(
                toml::from_str::<ServerConfig>(&src).is_err(),
                "{bad} should have been rejected"
            );
        }
    }

    #[test]
    fn operator_grid_rejects_non_maidenhead_values() {
        for bad in ["ZZ99xx", "JN4", "JN46lax", "1N46la", "JN46l", ""] {
            let src = format!("station_callsign = \"HB9H\"\noperator_grid = {bad:?}");
            assert!(
                toml::from_str::<ServerConfig>(&src).is_err(),
                "{bad:?} should have been rejected as a grid square"
            );
        }
    }

    #[test]
    fn operator_grid_accepts_four_and_six_character_locators_in_either_case() {
        for good in ["FN03", "FN03GW", "fn03gw", "JN46la"] {
            let src = format!("station_callsign = \"HB9H\"\noperator_grid = {good:?}");
            assert!(
                toml::from_str::<ServerConfig>(&src).is_ok(),
                "{good:?} should have been accepted"
            );
        }
    }

    #[test]
    fn missing_station_callsign_is_a_parse_error() {
        let result: Result<ServerConfig, _> = toml::from_str("");
        assert!(result.is_err());
    }

    /// MAN-89 / D4: RBN's own per-band SSID convention. A multi-band node
    /// identifies each band as `CALL-N-#`; the operator configures the `CALL-N`
    /// half and the server appends its own `-#`. Measured against a live RBN
    /// capture: 10 of 71 observed spotters use this pattern (7x `-1-#`,
    /// 2x `-6-#`, 1x `-2-#`).
    #[test]
    fn per_band_ssid_station_callsigns_are_accepted() {
        for good in [
            "W5AU-1",
            "W5AU-2",
            "W5AU-6", // the exact shapes seen in the capture
            "AC0C-1",
            "W1NT-2", // the exact spotters seen in the capture
            "W5AU-12",
            "W5AU-99",    // two-digit SSIDs (Decision 2)
            "JW/LB2PG-1", // SSID on a DXCC-prefix-override call
            "w5au-1",     // lowercase accepted and normalized
        ] {
            let cfg: ServerConfig = toml::from_str(&format!(r#"station_callsign = {good:?}"#))
                .unwrap_or_else(|e| panic!("{good:?} should be accepted: {e}"));
            assert_eq!(cfg.station_callsign, good.to_ascii_uppercase());
        }
    }

    /// MAN-45 (PR #63 round-8 finding), fixed here together with MAN-89 per this
    /// ticket's technical notes: `station_callsign` was validated with
    /// `manta_spot::grammar::is_plausible`, a deliberately narrow prefilter for
    /// GARBLED DECODER OUTPUT. Real operator callsigns use DXCC prefix overrides
    /// and suffixes that grammar never modelled and that the bundled `cty.dat`
    /// recognizes outright.
    #[test]
    fn real_world_operator_callsigns_are_accepted() {
        for good in [
            "W3XYZ",     // plain
            "JW/LB2PG",  // DXCC prefix override (Svalbard) -- in cty.dat
            "GB3LER/B",  // beacon suffix -- in cty.dat
            "K5ARH/QRP", // the old allowlist's cases still pass
            "K5ARH/P",
            "VP2E/K5ARH/M", // prefix AND suffix
            "3DA0RS",       // digit-leading prefix
            "4U1UN",        // exact cty.dat alias, digit-leading prefix
            "LZ130LO",      // special-event call: several separating digits
            "W1A/P",        // the shortest real call, MIN_CALLSIGN_LEN exactly
            // PR #131 review round 6: digit-led special-event calls whose ONLY
            // digit precedes the first letter -- all three are in the vendored
            // master.scp, and the grammar this validator replaced accepted them.
            "4AFARU",
            "4GRID",
            "5NNHR",
            "4GRID-1",  // ... and they take a per-band SSID like any other
            "4GRID/P",  // ... and a portable suffix
            "JW/4GRID", // ... and a DXCC prefix override
            "w3xyz",    // lowercase accepted ...
        ] {
            let cfg: ServerConfig = toml::from_str(&format!(r#"station_callsign = {good:?}"#))
                .unwrap_or_else(|e| panic!("{good:?} should be accepted: {e}"));
            assert_eq!(cfg.station_callsign, good.to_ascii_uppercase()); // ... and normalized
        }
    }

    /// The true positives the old grammar caught must STILL be rejected -- the
    /// point of validating this field is that it is interpolated unescaped into
    /// every telnet line and JSON `deCall` (see `ServerConfig::station_callsign`'s
    /// doc comment). The server's own generated `-#` suffix must not be
    /// configurable, and now that `-` is no longer banned outright, that
    /// guarantee comes from the SSID being digits-only (MAN-89 Decision 2).
    #[test]
    fn malformed_or_injecting_station_callsigns_are_still_rejected() {
        for bad in [
            // MAN-45's fixtures
            "",
            "W3XYZ-#",
            "W3XYZ\r\nEVIL LINE",
            "not a callsign",
            "ZZ",
            "12345",
            "ABCDEF",
            "W1AW/",
            "/W1AW",
            "W1AW//P",
            "A/B/C/D",
            "W1AW/ABCDEFGHIJK",
            "W3XYZ\u{00c9}",
            // MAN-89's new SSID-boundary fixtures
            "W3XYZ-",    // bare trailing hyphen
            "W3XYZ-0",   // SSID 0 == no SSID in the packet convention
            "W3XYZ-01",  // leading zero: two spellings of one band index
            "W3XYZ-100", // beyond the 1-99 cap
            "W3XYZ-1-2", // only one SSID
            "W3XYZ--1",
            "W3XYZ-1A",        // digits only
            "W3XYZ- 1",        // no whitespace smuggled in behind the hyphen
            "W3XYZ-1/P",       // the SSID is last; a portable suffix cannot follow it
            "W3XYZ-1\r\nEVIL", // injection behind a valid-looking SSID
            "-1",              // no base
            "ZZ-1",            // base still has to be a callsign
            // PR #131 review: letters and digits split ACROSS segments means no
            // single segment can be the call.
            "ABC/123",
            "A/1/B",
            "ABC/123-1",
            // PR #131 review round 3: the qualifying segment must itself be a
            // complete callsign -- a two-character segment carrying the letter
            // and the digit is a typo, not a call, however long the base is.
            "A1/B",
            "W1/P",
            "W1/P-1",
            "AB/1C",
            // PR #131 review round 4: length plus "some digit, some letter" is
            // not the ITU call STRUCTURE -- a call needs a letter SUFFIX after
            // its separating digit, so a segment that ends in digits is a typo,
            // not a callsign, at any length.
            "W12",
            "W12-1",
            "W12/P",
            "AB12",
            "3DA0",
            "1W2",
            // PR #131 review round 5: the letter suffix must run to the END of
            // the segment (ITU RR 19.68A -- every amateur callsign ends in a
            // letter). A letter merely somewhere right of the separating digit
            // leaves the trailing digit outside the suffix: still a typo.
            "W1A2",
            "W1A2-1",
            "W1A2/P",
            "LZ130LO5",
            "4U1UN0",
            // PR #131 review round 6: admitting the digit-led shape must not
            // readmit anything that ends in a digit, nor drop the length floor
            // the separating-digit shape used to imply on its own.
            "4G",
            "4G-1",
            "4GRID2",
            "12A/P",
            "5NNHR9",
            // ... and the leading numeral itself is 1-9: no ITU prefix, and no
            // `master.scp` entry, begins with `0`, so this shape is a typo the
            // digit-led shortcut must not wave through.
            "0AB",
            "0AB-1",
            "0GRID",
        ] {
            let result: Result<ServerConfig, _> =
                toml::from_str(&format!(r#"station_callsign = {bad:?}"#));
            assert!(result.is_err(), "{bad:?} should have been rejected");
        }
    }

    /// GOTCHA: a NUL or other raw control character cannot be tested through the
    /// TOML fixture above -- `{:?}` renders `"\u{0}"` as `\0`, which TOML's own
    /// lexer rejects as an invalid escape, so that case would pass without the
    /// validator ever seeing it. Exercise the validator directly instead.
    #[test]
    fn control_characters_are_rejected_by_the_validator_itself() {
        for bad in ["W3XYZ\u{0}", "W3XYZ\u{7f}", "W3XYZ\t", "W3XYZ-1\u{0}"] {
            assert!(
                check_operator_callsign(bad).is_err(),
                "{bad:?} should have been rejected"
            );
        }
    }

    /// MAN-32's `login_callsign` is the same class of value -- an operator's own
    /// station identity -- so it shares the validator (MAN-89 Decision 3).
    /// `effective_login_callsign` already defaults it to `station_callsign`, so
    /// an SSID reaches this field implicitly whether or not it is declarable.
    #[test]
    fn login_callsign_shares_the_operator_callsign_rule() {
        for (call, expect_ok) in [("W3XYZ-2", true), ("JW/LB2PG", true), ("W3XYZ-#", false)] {
            let result: Result<DaemonConfigFile, _> = toml::from_str(&format!(
                r#"
                [server]
                station_callsign = "W3XYZ"
                [[rbn_uplink]]
                enabled = true
                target_host = "example.invalid"
                target_port = 7300
                login_callsign = {call:?}
                "#
            ));
            assert_eq!(result.is_ok(), expect_ok, "login_callsign {call:?}");
        }
    }

    /// The strict-superset claim, measured against the WHOLE vendored
    /// `master.scp` rather than a hand-picked fixture list.
    ///
    /// Rounds 4, 5 and 6 of the PR #131 review each narrowed
    /// `is_complete_callsign`, and round 6 found by hand-reading `master.scp`
    /// that the previous narrowing had locked real operators (`4AFARU`,
    /// `4GRID`, `5NNHR`) out of starting the daemon -- while the fixture lists
    /// above still reported the superset property as holding. Fixtures cannot
    /// catch that class of regression; the snapshot can. Every one of the
    /// ~50k real, allocated calls in it that the decoder-output grammar this
    /// validator REPLACED accepted must remain configurable as a station
    /// identity, with and without an RBN `-N` per-band SSID.
    #[test]
    fn accepts_every_master_scp_call_the_replaced_grammar_accepted() {
        let mut rejected: Vec<String> = Vec::new();
        let mut checked = 0usize;
        for call in manta_spot::MASTER_SCP
            .lines()
            .map(str::trim)
            .filter(|c| !c.is_empty() && !c.starts_with('#'))
        {
            if !manta_spot::grammar::is_plausible(call) {
                continue;
            }
            checked += 1;
            for candidate in [call.to_string(), format!("{call}-1")] {
                if check_operator_callsign(&candidate).is_err() {
                    rejected.push(candidate);
                }
            }
        }
        assert!(
            checked > 10_000,
            "test premise: the vendored snapshot should carry tens of thousands \
             of grammar-accepted calls, got {checked}"
        );
        assert!(
            rejected.is_empty(),
            "{} real master.scp callsign(s) accepted by grammar::is_plausible are \
             rejected by check_operator_callsign, e.g. {:?}",
            rejected.len(),
            &rejected[..rejected.len().min(20)]
        );
    }

    /// The SSID grammar has exactly one definition; `strip_ssid` must agree with
    /// `check_operator_callsign` about what an SSID is, or Phase 3's cty lookup
    /// would strip something the validator never accepted.
    #[test]
    fn strip_ssid_removes_only_a_valid_ssid() {
        assert_eq!(strip_ssid("W5AU-1"), "W5AU");
        assert_eq!(strip_ssid("W5AU-12"), "W5AU");
        assert_eq!(strip_ssid("JW/LB2PG-1"), "JW/LB2PG");
        assert_eq!(strip_ssid("W5AU"), "W5AU");
        assert_eq!(strip_ssid("W5AU-#"), "W5AU-#"); // not an SSID: unchanged
        assert_eq!(strip_ssid("W5AU-0"), "W5AU-0");
        assert_eq!(strip_ssid("W5AU-100"), "W5AU-100");
    }

    /// MAN-32's `login_callsign` is the same class of value -- an
    /// operator's own station identity, not decoder output -- so it gets
    /// the same validator.
    #[test]
    fn login_callsign_accepts_the_same_real_world_callsigns() {
        let file: DaemonConfigFile = toml::from_str(
            r#"
            [server]
            station_callsign = "W3XYZ"
            [[rbn_uplink]]
            enabled = true
            target_host = "example.invalid"
            target_port = 7300
            login_callsign = "JW/LB2PG"
            "#,
        )
        .expect("JW/LB2PG should be accepted as a login_callsign");
        assert_eq!(
            file.rbn_uplink[0].login_callsign.as_deref(),
            Some("JW/LB2PG")
        );
    }

    #[test]
    fn unknown_server_config_key_is_a_parse_error() {
        // Regression (round-6 review): a typo'd key (e.g. `bind_address`
        // instead of `bind_addr`) must not silently parse and fall back to
        // that field's default -- for `bind_addr` specifically, silently
        // keeping the "0.0.0.0" default instead of the operator's intended
        // restriction unexpectedly exposes the telnet and JSON listeners
        // publicly.
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
        // MAN-159: an omitted `dry_run` key must default to ON (safe),
        // not OFF. Enabling `[[rbn_uplink]]` must never start transmitting
        // to a real target without the operator opting in explicitly.
        assert!(uplink.dry_run);
        assert_eq!(uplink.login_callsign, None);
        // MAN-91: an omitted `spot_types` key means RBN's own rule.
        assert_eq!(uplink.spot_types, UplinkSpotTypes::CqBeacon);
    }

    fn parse_spot_types(value: &str) -> Result<DaemonConfigFile, toml::de::Error> {
        toml::from_str(&format!(
            r#"
            [server]
            station_callsign = "W3XYZ"
            [[rbn_uplink]]
            enabled = true
            target_host = "example.invalid"
            target_port = 7300
            spot_types = "{value}"
            "#
        ))
    }

    #[test]
    fn uplink_spot_types_all_parses() {
        let file = parse_spot_types("all").unwrap();
        assert_eq!(file.rbn_uplink[0].spot_types, UplinkSpotTypes::All);
        let file = parse_spot_types("cq_beacon").unwrap();
        assert_eq!(file.rbn_uplink[0].spot_types, UplinkSpotTypes::CqBeacon);
    }

    /// A typo must fail loudly rather than fall back to the default -- same
    /// reasoning as `line_format`.
    #[test]
    fn uplink_spot_types_rejects_an_unknown_value() {
        let err = parse_spot_types("de").unwrap_err().to_string();
        assert!(err.contains("spot_types"), "error was: {err}");
        assert!(
            err.contains("cq_beacon") && err.contains("all"),
            "error was: {err}"
        );
    }

    #[test]
    fn uplink_spot_types_forwards_table() {
        use manta_spot::SpotType;
        let cases = [
            (SpotType::Cq, true, true),
            (SpotType::Beacon, true, true),
            (SpotType::De, false, true),
            (SpotType::Unknown, false, true),
        ];
        for (t, cq_beacon, all) in cases {
            assert_eq!(UplinkSpotTypes::CqBeacon.forwards(t), cq_beacon, "{t:?}");
            assert_eq!(UplinkSpotTypes::All.forwards(t), all, "{t:?}");
        }
        assert_eq!(UplinkSpotTypes::CqBeacon.as_str(), "cq_beacon");
        assert_eq!(UplinkSpotTypes::All.as_str(), "all");
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
        // dry_run=true default (MAN-159) and leave an operator who intended
        // to go live stuck silently in dry-run instead of failing loudly.
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
    fn line_format_defaults_to_the_rbn_relay_layout() {
        let cfg: ServerConfig = toml::from_str(r#"station_callsign = "W3XYZ""#).unwrap();
        assert_eq!(cfg.line_format, crate::rbn::LineFormat::Rbn);
    }

    #[test]
    fn line_format_skimmer_selects_the_cw_skimmer_layout() {
        let cfg: ServerConfig = toml::from_str(
            r#"
            station_callsign = "W3XYZ"
            line_format = "skimmer"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.line_format, crate::rbn::LineFormat::Skimmer);
    }

    /// A typo must fail loudly at startup rather than silently falling back to
    /// the default -- same reasoning as `RbnUplinkConfig`'s `dry_run` guard.
    #[test]
    fn an_unknown_line_format_is_rejected() {
        let err = toml::from_str::<ServerConfig>(
            r#"
            station_callsign = "W3XYZ"
            line_format = "aggregator"
            "#,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("line_format") || err.to_string().contains("aggregator"),
            "error was: {err}"
        );
    }

    #[test]
    fn uplink_effective_login_callsign_falls_back_to_station_callsign() {
        let uplink = RbnUplinkConfig {
            enabled: true,
            target_host: "example.invalid".to_string(),
            target_port: 7300,
            login_callsign: None,
            dry_run: false,
            spot_types: UplinkSpotTypes::default(),
        };
        assert_eq!(uplink.effective_login_callsign("W3XYZ"), "W3XYZ");

        let uplink_override = RbnUplinkConfig {
            login_callsign: Some("W3XYZ-2".to_string()),
            ..uplink
        };
        assert_eq!(uplink_override.effective_login_callsign("W3XYZ"), "W3XYZ-2");
    }

<<<<<<< HEAD
    // ---- MAN-301: [rbn_compare]

    fn rbn_compare(body: &str) -> Result<RbnCompareConfig, String> {
        toml::from_str::<RbnCompareConfig>(body).map_err(|e| e.to_string())
    }

    #[test]
    fn rbn_compare_defaults() {
        let cfg = rbn_compare("").unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.feed_host, "telnet.reversebeacon.net");
        assert_eq!(cfg.feed_port, 7000);
        assert_eq!(cfg.login_callsign, None);
        assert_eq!(cfg.window_secs, 180);
        assert_eq!(cfg.freq_tol_hz, 500.0);
        assert_eq!(cfg.speed_tol_wpm, 3.0);
        assert_eq!(cfg.speed_tol_pct, 20.0);
        assert!(cfg.spotters.is_empty());
        assert!(cfg.exclude_spotters.is_empty());
        assert_eq!(cfg, RbnCompareConfig::default());
        assert_eq!(cfg.validate(), Ok(()));
        assert_eq!(cfg.effective_login_callsign("W3XYZ-1"), "W3XYZ-1");
    }

    #[test]
    fn rbn_compare_unknown_key_is_a_parse_error() {
        let err = rbn_compare("window-secs = 60\n").unwrap_err();
        assert!(err.contains("unknown field `window-secs`"), "{err}");
    }

    #[test]
    fn rbn_compare_rejects_out_of_range_values() {
        let cases: &[(&str, &str)] = &[
            ("window_secs = 29", "window_secs must be 30 to 3600, got 29"),
            (
                "window_secs = 3601",
                "window_secs must be 30 to 3600, got 3601",
            ),
            ("freq_tol_hz = -1.0", "freq_tol_hz"),
            ("freq_tol_hz = nan", "freq_tol_hz"),
            ("freq_tol_hz = inf", "freq_tol_hz"),
            ("freq_tol_hz = 10000.5", "freq_tol_hz"),
            ("speed_tol_wpm = -0.5", "speed_tol_wpm"),
            ("speed_tol_wpm = 20.5", "speed_tol_wpm"),
            ("speed_tol_pct = -1.0", "speed_tol_pct"),
            ("speed_tol_pct = 100.5", "speed_tol_pct"),
            ("feed_port = 0", "feed_port"),
            ("feed_host = \"\"", "feed_host"),
            ("feed_host = \"telnet. reversebeacon.net\"", "feed_host"),
            ("feed_host = \"telnet.reversebeacon.net\\n\"", "feed_host"),
        ];
        for (body, want) in cases {
            let cfg = rbn_compare(body).unwrap();
            let err = cfg.validate().unwrap_err();
            assert!(err.starts_with("[rbn_compare]."), "{body}: {err}");
            assert!(err.contains(want), "{body}: {err}");
        }
        // The bounds themselves are accepted.
        for body in [
            "window_secs = 30",
            "window_secs = 3600",
            "freq_tol_hz = 0.0",
            "freq_tol_hz = 10000.0",
            "speed_tol_wpm = 20.0",
            "speed_tol_pct = 100.0",
            "feed_port = 65535",
        ] {
            assert_eq!(rbn_compare(body).unwrap().validate(), Ok(()), "{body}");
=======
    #[test]
    fn history_minutes_is_optional_and_accepts_0_through_1440() {
        assert_eq!(server_with("").history_minutes, None);
        for minutes in [0, 30, 1440] {
            assert_eq!(
                server_with(&format!("history_minutes = {minutes}")).history_minutes,
                Some(minutes)
            );
>>>>>>> a0d514c01ca518d421b77a5db3bf0eb0653a2543
        }
    }

    #[test]
<<<<<<< HEAD
    fn rbn_compare_callsigns_are_validated_and_uppercased() {
        let cfg = rbn_compare(
            "login_callsign = \"w3xyz-1\"\nspotters = [\"k5tr\"]\n\
             exclude_spotters = [\"w1aw/p\"]\n",
        )
        .unwrap();
        assert_eq!(cfg.login_callsign.as_deref(), Some("W3XYZ-1"));
        assert_eq!(cfg.spotters, vec!["K5TR".to_string()]);
        assert_eq!(cfg.exclude_spotters, vec!["W1AW/P".to_string()]);
        for key in ["login_callsign", "spotters", "exclude_spotters"] {
            for bad in ["K5TR-#", "not a call"] {
                let body = if key == "login_callsign" {
                    format!("{key} = \"{bad}\"\n")
                } else {
                    format!("{key} = [\"{bad}\"]\n")
                };
                let err = rbn_compare(&body).unwrap_err();
                assert!(err.contains(key), "{body}: {err}");
                assert!(err.contains("not a plausible callsign"), "{body}: {err}");
            }
        }
        assert_eq!(normalize_rbn_spotter("k5tr").as_deref(), Ok("K5TR"));
        assert!(normalize_rbn_spotter("K5TR-#").is_err());
        // A hand-built config that skipped serde is still checked.
        let hand_built = RbnCompareConfig {
            spotters: vec!["not a call".into()],
            ..RbnCompareConfig::default()
        };
        assert!(hand_built.validate().unwrap_err().contains("spotters"));
    }

    #[test]
    fn rbn_compare_grade_config_adds_own_call_with_and_without_ssid() {
        let cfg = RbnCompareConfig {
            window_secs: 240,
            spotters: vec!["K5TR".into()],
            exclude_spotters: vec!["N0ABC".into()],
            ..RbnCompareConfig::default()
        };
        let grade = cfg.grade_config("W3XYZ-1");
        assert_eq!(grade.window_secs, 240);
        assert_eq!(grade.freq_tol_hz, 500.0);
        assert_eq!(grade.speed_tol_wpm, 3.0);
        assert_eq!(grade.speed_tol_pct, 20.0);
        assert_eq!(
            grade.spotters,
            ["K5TR"].into_iter().map(String::from).collect()
        );
        for call in ["W3XYZ-1", "W3XYZ", "N0ABC"] {
            assert!(grade.exclude_spotters.contains(call), "{call}");
        }
        assert_eq!(cfg.check_spotters_against_station("W3XYZ-1"), Ok(()));
        let own = RbnCompareConfig {
            spotters: vec!["K5TR".into(), "W3XYZ".into()],
            ..RbnCompareConfig::default()
        };
        assert_eq!(
            own.check_spotters_against_station("W3XYZ-1").unwrap_err(),
            "[rbn_compare].spotters lists W3XYZ, this station's own callsign: its reports are \
             always excluded, so every spot would grade none"
        );
=======
    fn history_minutes_out_of_range_is_rejected_naming_the_key() {
        for minutes in [1441, 5000] {
            let error = toml::from_str::<ServerConfig>(&format!(
                "station_callsign = \"W1AW\"\nhistory_minutes = {minutes}"
            ))
            .unwrap_err();
            assert!(error
                .to_string()
                .contains("history_minutes must be a whole number of minutes from 0 to 1440, got"));
        }
        for invalid in ["-1", "1.5", "\"30\""] {
            let error = toml::from_str::<ServerConfig>(&format!(
                "station_callsign = \"W1AW\"\nhistory_minutes = {invalid}"
            ))
            .unwrap_err();
            assert!(error.to_string().contains("history_minutes"));
        }
>>>>>>> a0d514c01ca518d421b77a5db3bf0eb0653a2543
    }
}
