//! Telnet cluster command grammar. ARCHITECTURE §7: "enough command
//! grammar (`sh/dx`, filters) for common clients not to choke" -- per the
//! MAN-12 ticket's 2026-09-02 clarification, `sh/dx` and filter commands
//! like `set dx filter unique > 1` are real, in-scope behavior here, not
//! just "accept the line and don't disconnect."
//!
//! Accepts both slash-separated (`sh/dx`, `set/dx/filter`) and
//! space-separated (`sh dx`, `set dx filter`) forms, case-insensitively --
//! real AK1A-descended cluster clients use both conventions.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `sh/dx` or `sh/dx/<n>` -- replay the last `count` spots (server
    /// picks a default when the client didn't specify one).
    ShowDx { count: Option<usize> },
    /// `set dx filter unique > <n>` -- suppress spots for a callsign
    /// until it's been seen more than `min` times on this bus.
    SetFilterUnique { min: u32 },
    /// `SKIMMER/SETT` (or bare `SETT`) -- Aggregator's handshake probe.
    /// Per Aggregator manual v6.0 §9.2, a source that never answers this
    /// has its spots dropped entirely, so this is the gate on manta being
    /// usable behind a stock Aggregator at all (MAN-86).
    Sett,
    /// `BYE` -- the client is done; reply `CU AGN!` and close the
    /// connection (CW Skimmer manual, Telnet Commands; Aggregator manual
    /// v6.0 §10.5 lists it on Aggregator's own local user port too).
    Bye,
    /// Anything else: accepted (never disconnects the client) but not
    /// acted on.
    Unknown,
}

pub fn parse(line: &str) -> Command {
    let tokens: Vec<String> = line
        .trim()
        .replace('/', " ")
        .split_whitespace()
        .map(|t| t.to_uppercase())
        .collect();
    let t: Vec<&str> = tokens.iter().map(String::as_str).collect();

    match t.as_slice() {
        ["SH", "DX"] | ["SHOW", "DX"] => Command::ShowDx { count: None },
        ["SH", "DX", n] | ["SHOW", "DX", n] => match n.parse() {
            Ok(count) => Command::ShowDx { count: Some(count) },
            Err(_) => Command::Unknown,
        },
        ["SET", "DX", "FILTER", "UNIQUE", ">", n] => match n.parse() {
            Ok(min) => Command::SetFilterUnique { min },
            Err(_) => Command::Unknown,
        },
        ["SKIMMER", "SETT"] | ["SETT"] => Command::Sett,
        ["BYE"] => Command::Bye,
        _ => Command::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_slash_form_sh_dx() {
        assert_eq!(parse("sh/dx"), Command::ShowDx { count: None });
    }

    #[test]
    fn parses_space_form_show_dx() {
        assert_eq!(parse("show dx"), Command::ShowDx { count: None });
    }

    #[test]
    fn parses_sh_dx_with_a_count() {
        assert_eq!(parse("sh/dx/20"), Command::ShowDx { count: Some(20) });
    }

    #[test]
    fn parses_set_dx_filter_unique_slash_form() {
        assert_eq!(
            parse("set/dx/filter/unique/>/1"),
            Command::SetFilterUnique { min: 1 }
        );
    }

    #[test]
    fn parses_set_dx_filter_unique_space_form() {
        assert_eq!(
            parse("set dx filter unique > 3"),
            Command::SetFilterUnique { min: 3 }
        );
    }

    #[test]
    fn is_case_insensitive() {
        assert_eq!(parse("SH/DX"), Command::ShowDx { count: None });
    }

    #[test]
    fn unrecognized_command_is_unknown_not_an_error() {
        // MAN-86 deliberately promoted `bye` out of `Unknown` (see
        // `parses_bye_case_insensitively` below) -- don't "restore" it here.
        assert_eq!(parse(""), Command::Unknown);
        assert_eq!(parse("set dx filter unique > banana"), Command::Unknown);
    }

    #[test]
    fn parses_skimmer_sett_in_both_slash_and_space_form() {
        // Aggregator sends `SKIMMER/SETT` (Aggregator manual v6.0 §3.1); the
        // manuals also refer to it in prose as bare "the SETT command"
        // (§6.1, §9.2), so accept both rather than gambling on one spelling.
        assert_eq!(parse("SKIMMER/SETT"), Command::Sett);
        assert_eq!(parse("skimmer/sett"), Command::Sett);
        assert_eq!(parse("SKIMMER SETT"), Command::Sett);
        assert_eq!(parse("sett"), Command::Sett);
    }

    #[test]
    fn parses_bye_case_insensitively() {
        // Aggregator manual v6.0 §10.5 lists "BYE or bye" on its own local
        // user port -- both spellings are real.
        assert_eq!(parse("BYE"), Command::Bye);
        assert_eq!(parse("bye"), Command::Bye);
        assert_eq!(parse("bye\r\n"), Command::Bye);
    }

    #[test]
    fn sett_with_trailing_junk_is_unknown_not_sett() {
        // Matches the existing malformed-argument rule (`sh/dx/banana` is
        // Unknown, not a bare `sh/dx`) -- a command shape we don't
        // understand must not be silently treated as one we do.
        assert_eq!(parse("skimmer/sett/14000"), Command::Unknown);
        assert_eq!(parse("bye now"), Command::Unknown);
    }

    #[test]
    fn an_explicit_malformed_sh_dx_count_is_unknown_not_the_bare_default() {
        // A malformed EXPLICIT count (`sh/dx/banana`, a negative number, an
        // overflow) must be distinguishable from the client simply not
        // specifying one (`sh/dx`, which legitimately means
        // `count: None`) -- silently mapping both to `None` (the prior
        // behavior via `.parse().ok()`) makes a client's mistake behave
        // exactly like a bare `sh/dx`, matching the filter parser's
        // existing malformed-input handling just above (round-15 review
        // finding).
        assert_eq!(parse("sh/dx/banana"), Command::Unknown);
        assert_eq!(parse("sh/dx/-1"), Command::Unknown);
        assert_eq!(parse("sh/dx/99999999999999999999"), Command::Unknown);
    }
}
