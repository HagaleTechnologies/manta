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
    /// `sh/dx [N | Nm] [BAND band] [CW | RTTY]` -- replay retained spot
    /// history, scoped by count or minutes, band and mode (MAN-92; see
    /// docs/DECISIONS/2026-10-10-man92-telnet-commands.md).
    ShowDx(ShowDxQuery),
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
    /// `sh/version` / `show version` -- reply with manta's name and
    /// version (MAN-92).
    ShowVersion,
    /// Anything else, including a known command with malformed arguments:
    /// answered with a fixed `Unknown command` line (MAN-92; it used to get
    /// no reply at all), never disconnects the client.
    Unknown,
}

/// One `sh/dx` request. Request-local: nothing here persists into the
/// connection's live stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShowDxQuery {
    pub selection: HistorySelection,
    /// An allocation name from `band::allocations()` (e.g. `"20m"`) --
    /// never client text.
    pub band: Option<&'static str>,
    pub mode: Option<QueryMode>,
}

/// Aggregator manual v6.0 §10.5: `sh/dx XX` is a count, `sh/dx XXm` a
/// minutes window -- `20m` here never means the 20-metre band (that is
/// the `BAND` extension).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistorySelection {
    /// The newest `n` matching entries; `None` means the server default.
    Count(Option<usize>),
    /// Every matching entry heard within the last `secs` seconds (the
    /// client's minutes times 60, overflow-checked at parse time).
    Window { secs: i64 },
}

/// The `" CW"`/`" RTTY"` suffix Aggregator §10.5 allows on every `sh/dx`
/// form. manta decodes CW only, so `Cw` matches every spot and `Rtty` none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryMode {
    Cw,
    Rtty,
}

impl ShowDxQuery {
    /// Bare `sh/dx`: the server-default count, any band, any mode.
    pub const DEFAULT: Self = Self {
        selection: HistorySelection::Count(None),
        band: None,
        mode: None,
    };
}

pub fn parse(line: &str) -> Command {
    // NUL is a token separator alongside whitespace, not part of a token
    // (MAN-86/PR #128 review): RFC 854 encodes Enter as `CR NUL`, so a
    // macOS-`telnet`-style client's line arrives here as `BYE\r\0` --
    // `split_whitespace` alone leaves a trailing `\0` token that turns
    // every command from such a client into `Unknown`.
    let tokens: Vec<String> = line
        .replace('/', " ")
        .split(|c: char| c.is_whitespace() || c == '\u{0}')
        .filter(|t| !t.is_empty())
        .map(|t| t.to_uppercase())
        .collect();
    let t: Vec<&str> = tokens.iter().map(String::as_str).collect();

    match t.as_slice() {
        ["SH" | "SHOW", "DX", rest @ ..] => {
            parse_show_dx(rest).map_or(Command::Unknown, Command::ShowDx)
        }
        ["SH" | "SHOW", "VERSION"] => Command::ShowVersion,
        ["SET", "DX", "FILTER", "UNIQUE", ">", n] => match n.parse() {
            Ok(min) => Command::SetFilterUnique { min },
            Err(_) => Command::Unknown,
        },
        ["SKIMMER", "SETT"] | ["SETT"] => Command::Sett,
        ["BYE"] => Command::Bye,
        _ => Command::Unknown,
    }
}

/// `[N | Nm] [BAND band] [CW | RTTY]`, in that order, every token
/// consumed -- anything left over (a duplicate option, a reversed order,
/// trailing junk) makes the whole line `None`, never a partial match.
fn parse_show_dx(mut rest: &[&str]) -> Option<ShowDxQuery> {
    let mut query = ShowDxQuery::DEFAULT;
    if let [first, tail @ ..] = rest {
        // A token that isn't a well-formed count or window (`BAND`, `CW`,
        // `banana`, an overflow) is left for the clauses below, none of
        // which accept a numeric-looking token, so it still ends up
        // rejected rather than defaulted.
        if let Some(selection) = parse_selection(first) {
            query.selection = selection;
            rest = tail;
        }
    }
    if let ["BAND", name, tail @ ..] = rest {
        query.band = Some(parse_band(name)?);
        rest = tail;
    }
    if let [mode, tail @ ..] = rest {
        query.mode = Some(match *mode {
            "CW" => QueryMode::Cw,
            "RTTY" => QueryMode::Rtty,
            _ => return None,
        });
        rest = tail;
    }
    rest.is_empty().then_some(query)
}

fn parse_selection(token: &str) -> Option<HistorySelection> {
    match token.strip_suffix('M') {
        Some(minutes) => {
            let minutes: u64 = minutes.parse().ok()?;
            let secs = i64::try_from(minutes.checked_mul(60)?).ok()?;
            Some(HistorySelection::Window { secs })
        }
        None => token.parse().ok().map(|n| HistorySelection::Count(Some(n))),
    }
}

/// `20m` or `20` (tokens arrive upper-cased) to the allocation's own name.
fn parse_band(token: &str) -> Option<&'static str> {
    crate::band::allocations()
        .iter()
        .map(|(name, _, _)| *name)
        .find(|name| name.eq_ignore_ascii_case(token) || name.strip_suffix('m') == Some(token))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(
        selection: HistorySelection,
        band: Option<&'static str>,
        mode: Option<QueryMode>,
    ) -> Command {
        Command::ShowDx(ShowDxQuery {
            selection,
            band,
            mode,
        })
    }

    fn count(n: usize) -> Command {
        query(HistorySelection::Count(Some(n)), None, None)
    }

    fn minutes(m: i64) -> HistorySelection {
        HistorySelection::Window { secs: m * 60 }
    }

    #[test]
    fn parses_slash_form_sh_dx() {
        assert_eq!(parse("sh/dx"), Command::ShowDx(ShowDxQuery::DEFAULT));
    }

    #[test]
    fn parses_space_form_show_dx() {
        assert_eq!(parse("show dx"), Command::ShowDx(ShowDxQuery::DEFAULT));
    }

    #[test]
    fn parses_sh_dx_with_a_count() {
        assert_eq!(parse("sh/dx/20"), count(20));
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
        assert_eq!(parse("SH/DX"), Command::ShowDx(ShowDxQuery::DEFAULT));
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
    fn a_cr_nul_terminated_line_parses_as_the_command_underneath_it() {
        // PR #128 review: RFC 854 encodes Enter as `CR NUL`, so a macOS
        // `telnet` client's line arrives here with a trailing NUL that
        // `split_whitespace` alone would leave as its own token, turning
        // every such command into `Unknown` -- the exact "Aggregator never
        // gets an answer" failure MAN-86 exists to remove.
        assert_eq!(parse("SKIMMER/SETT\r\u{0}"), Command::Sett);
        assert_eq!(parse("BYE\r\u{0}"), Command::Bye);
        assert_eq!(parse("sh/dx\r\u{0}"), Command::ShowDx(ShowDxQuery::DEFAULT));
        // An embedded NUL still separates tokens rather than being folded
        // into one, so a genuinely malformed line stays Unknown.
        assert_eq!(parse("bye\u{0}now"), Command::Unknown);
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

    #[test]
    fn parses_sh_version_in_every_spelling() {
        for line in [
            "sh/version",
            "show version",
            "SH/VERSION",
            "Show/Version",
            "sh version",
            "sh/version\r\u{0}",
            "  sh/version  \r\n",
        ] {
            assert_eq!(parse(line), Command::ShowVersion, "{line:?}");
        }
    }

    #[test]
    fn sh_version_with_trailing_arguments_is_unknown() {
        assert_eq!(parse("sh/version/now"), Command::Unknown);
        assert_eq!(parse("show version 1"), Command::Unknown);
        assert_eq!(parse("sh/ver"), Command::Unknown);
        assert_eq!(parse("version"), Command::Unknown);
    }

    #[test]
    fn commands_manta_does_not_implement_are_unknown() {
        for line in [
            "help",
            "set/skimmer",
            "set/nocq",
            "sh/filter",
            "   ",
            "\r\n",
        ] {
            assert_eq!(parse(line), Command::Unknown, "{line:?}");
        }
    }

    #[test]
    fn a_bare_number_is_a_count_and_a_number_with_m_is_minutes() {
        // Aggregator manual v6.0 §10.5: `sh/dx XX` is the last XX spots,
        // `sh/dx XXm` the last XX MINUTES -- neither selects a band.
        assert_eq!(
            parse("sh/dx 20 CW"),
            query(HistorySelection::Count(Some(20)), None, Some(QueryMode::Cw))
        );
        assert_eq!(parse("sh/dx 20m"), query(minutes(20), None, None));
        assert_eq!(parse("sh/dx/20M"), query(minutes(20), None, None));
        assert_eq!(
            parse("sh/dx 20m RTTY"),
            query(minutes(20), None, Some(QueryMode::Rtty))
        );
    }

    #[test]
    fn parses_every_ordered_sh_dx_form() {
        let cw = Some(QueryMode::Cw);
        let default = HistorySelection::Count(None);
        let cases: &[(&str, Command)] = &[
            ("sh/dx CW", query(default, None, cw)),
            ("sh/dx RTTY", query(default, None, Some(QueryMode::Rtty))),
            ("sh/dx BAND 20m", query(default, Some("20m"), None)),
            ("sh/dx BAND 20", query(default, Some("20m"), None)),
            ("sh/dx BAND 20m CW", query(default, Some("20m"), cw)),
            (
                "sh/dx 5 BAND 20 CW",
                query(HistorySelection::Count(Some(5)), Some("20m"), cw),
            ),
            ("sh/dx 20m BAND 20m CW", query(minutes(20), Some("20m"), cw)),
            ("sh/dx 30m BAND 40", query(minutes(30), Some("40m"), None)),
            (
                "SHOW/DX/5/BAND/20M/CW",
                query(HistorySelection::Count(Some(5)), Some("20m"), cw),
            ),
            (
                "show dx 5 band 20m cw\r\u{0}",
                query(HistorySelection::Count(Some(5)), Some("20m"), cw),
            ),
            ("sh/dx BAND 2200m", query(default, Some("2200m"), None)),
            ("sh/dx BAND 630", query(default, Some("630m"), None)),
            ("sh/dx BAND 6m", query(default, Some("6m"), None)),
            ("sh/dx 0", count(0)),
            ("sh/dx 0m", query(minutes(0), None, None)),
            ("sh/dx/+5", count(5)),
        ];
        for (line, expected) in cases {
            assert_eq!(&parse(line), expected, "{line:?}");
        }
    }

    #[test]
    fn malformed_sh_dx_arguments_are_unknown_not_a_partial_match() {
        for line in [
            "sh/dx banana",
            "sh/dx 20 SSB",
            "sh/dx BAND",
            "sh/dx BAND banana",
            "sh/dx BAND 21m",
            "sh/dx BAND 20m BAND 40m",
            "sh/dx BAND 20m 5",
            "sh/dx CW 20",
            "sh/dx CW BAND 20m",
            "sh/dx CW CW",
            "sh/dx 20 20",
            "sh/dx 20 20m",
            "sh/dx -5m",
            "sh/dx m",
            "sh/dx 20 CW junk",
            "sh/dx 99999999999999999999m",
            // Representable as minutes, overflows as seconds.
            "sh/dx 153722867280912931m",
            "sh/dx 18446744073709551615m",
        ] {
            assert_eq!(parse(line), Command::Unknown, "{line:?}");
        }
    }

    #[test]
    fn the_largest_window_that_fits_in_seconds_is_accepted() {
        let max_minutes = i64::MAX / 60;
        assert_eq!(
            parse(&format!("sh/dx {max_minutes}m")),
            query(minutes(max_minutes), None, None)
        );
    }
}
