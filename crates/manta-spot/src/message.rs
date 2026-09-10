//! Secondary message content carried alongside an already-valid spot: the
//! most recent RST signal report and the QRL? frequency query (MAN-33).
//! ARCHITECTURE §6.1a.
//!
//! Distinct from `context` by design: `context` classifies a *callsign
//! candidate* and gates nothing else, while these two are per-track
//! annotations that ride on a spot the pipeline has already validated.
//! Neither one gates any of ARCHITECTURE §6's five steps -- a callsign that
//! fails grammar/cty/repetition is not rescued by carrying an RST.
//!
//! Legacy precedent: CW Skimmer's band map shows "the most recent RST" under a
//! 599 label and flags QRL? so the operator notices a new station on a crowded
//! band (CW Skimmer manual, Band map).
//!
//! Deliberately lightweight, in the same spirit as `context`: see this
//! module's tests for the accepted limitations (a three-digit serial number is
//! RST-shaped; a `QRL?` whose `?` did not survive the decoder reads as a bare
//! `QRL` response and is not flagged).

use regex::Regex;
use std::sync::LazyLock;

/// A signal report. R is 1-5, S and T are 1-9 -- `0` never appears in a valid
/// RST, which is why the only cut number this needs is `N` = 9 (the `T` = 0
/// cut, universal elsewhere in CW, is unreachable inside an RST). Restricting
/// the substitution to `N` keeps `A`/`U`/`V`/`E` cut letters from turning
/// callsign fragments into spurious reports.
static RST_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b([1-5])([1-9N])([1-9N])\b").unwrap());

/// The QRL *query*. The trailing `?` IS required (review finding on PR #159):
/// bare `QRL` and `QRL?` are opposite halves of the same exchange -- `QRL`
/// asserts "the frequency **is** in use" (a response), `QRL?` asks "is it in
/// use?" (the query). Only the interrogative form is CW Skimmer's band-map
/// "QRL?" cue, and `Spot::qrl_query` is named for the query, so flagging a
/// bare response as a query would permanently misreport it to JSON consumers.
///
/// Accepted cost of requiring the `?`: the decoder's `?` is overloaded --
/// `manta_decode::beam` emits `Glyph::Char('?')` at confidence 0.0 for a
/// character whose beam survivors carry no glyph (SPEC §4.4.4) -- so a `QRL?`
/// whose `?` was lost decodes as bare `QRL` and is (correctly, given the
/// evidence actually decoded) not flagged, while a bare `QRL` followed by an
/// unresolvable character can read as a query. Both are one-character
/// misreadings of the decoded text; neither invents an interrogative the text
/// does not show.
static QRL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bQRL\?").unwrap());

/// Resolves the one cut number an RST can contain.
fn uncut(c: char) -> char {
    match c.to_ascii_uppercase() {
        'N' => '9',
        d => d,
    }
}

/// The last RST-shaped token in `text`, normalized to three ASCII digits
/// (`"5NN"` -> `"599"`), or `None`. Last-wins: `Validator` calls this once per
/// completed word, so "last in this word" composes into "most recently
/// decoded on this track" (MAN-33 plan decision D4).
pub fn parse_rst(text: &str) -> Option<String> {
    RST_RE.captures_iter(text).last().map(|caps| {
        (1..=3)
            .map(|i| uncut(caps[i].chars().next().unwrap()))
            .collect()
    })
}

/// True if `text` contains a QRL query token: the literal `QRL?`, with the
/// trailing `?` **required** -- bare `QRL` is the response ("the frequency is
/// in use"), not the query, and does not match. See `QRL_RE` on why the `?` is
/// required.
pub fn is_qrl_query(text: &str) -> bool {
    QRL_RE.is_match(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_plain_and_cut_number_rst() {
        for (text, want) in [
            ("5NN", "599"),
            ("599", "599"),
            ("TU 5NN", "599"),
            ("579", "579"),
            ("5N9", "599"),
        ] {
            assert_eq!(parse_rst(text).as_deref(), Some(want), "text was {text:?}");
        }
    }

    #[test]
    fn rejects_non_rst_tokens() {
        // 0 can never appear in a valid RST (R 1-5, S 1-9, T 1-9), so 069/509/590
        // are rejected outright. 5NNN/5NN9 fail the word boundary. V3E pins the
        // decision NOT to accept A/U/V/E cut numbers (see the plan's D2).
        for text in [
            "5NN9",
            "5NNN",
            "K5ARH",
            "W1AW",
            "CQ TEST K5ARH",
            "069",
            "509",
            "590",
            "NNN",
            "K5A",
            "V3E",
            "<AR>",
        ] {
            assert_eq!(parse_rst(text), None, "text was {text:?}");
        }
    }

    #[test]
    fn the_most_recent_rst_wins() {
        assert_eq!(parse_rst("5NN TU 339").as_deref(), Some("339"));
        assert_eq!(parse_rst("339 TU 5NN").as_deref(), Some("599"));
    }

    #[test]
    fn a_three_digit_serial_can_be_mistaken_for_an_rst() {
        // ACCEPTED LIMITATION, pinned so a future change to it is deliberate:
        // in "5NN 199" the serial number is RST-shaped and wins under
        // last-value-wins. See the module doc and MAN-33 plan decision D2.
        assert_eq!(parse_rst("5NN 199").as_deref(), Some("199"));
        // The common form is unaffected: a serial with a 0 is not RST-shaped.
        assert_eq!(parse_rst("599 001").as_deref(), Some("599"));
    }

    #[test]
    fn recognizes_the_interrogative_qrl_form() {
        for text in ["QRL?", "QRL? DE K5ARH", "K5ARH 5NN QRL?", "QRL?K"] {
            assert!(is_qrl_query(text), "text was {text:?}");
        }
    }

    #[test]
    fn a_bare_qrl_response_is_not_a_query() {
        // Review finding (PR #159): bare `QRL` states "the frequency IS in
        // use" -- the answer, not the question. Flagging it as `qrl_query`
        // would tell JSON consumers the station sent `QRL?` when it did not.
        for text in ["QRL", "QRL DE K5ARH", "TU QRL"] {
            assert!(!is_qrl_query(text), "text was {text:?}");
        }
    }

    #[test]
    fn does_not_match_qrl_inside_another_token() {
        for text in ["QRLX", "QRLX?", "VQRL?", "CQ TEST K5ARH", "5NN"] {
            assert!(!is_qrl_query(text), "text was {text:?}");
        }
    }
}
