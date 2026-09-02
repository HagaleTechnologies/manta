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
        assert_eq!(parse("bye"), Command::Unknown);
        assert_eq!(parse(""), Command::Unknown);
        assert_eq!(parse("set dx filter unique > banana"), Command::Unknown);
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
