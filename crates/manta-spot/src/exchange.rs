//! Exchange grammar: who is sending, who is called, who is mentioned
//! (MAN-201). ARCHITECTURE §6.1.
//!
//! A callsign can be copied perfectly as the station *called* and still be
//! wrong to report as the station *sending*. `interpret` reads a window of
//! decoded words and gives every callsign occurrence exactly one `Role`:
//! `self_id` (the sending station identifying itself), `addressee` (the
//! station called), `relayed_mention` (another station named inside a
//! station's own turn) or `uncertain`.
//!
//! The recognizer is a single left-to-right pass over the window's tokens,
//! with bounded lookback inside the current turn and one token of lookahead.
//! It holds no state between calls: `Validator` re-runs it over the bounded
//! word window at each boundary, as it ran `context::parse` before.
//!
//! Token classes (whole word, uppercased, first match wins): turn ends (`K`,
//! `KN`, `(` -- the decoder's `KN` --, `BK`, `AR`, `SK`), `CQ`, `TEST`, the
//! contest fillers of `context::CQ_FILLER`, `DE`, `UP`, the sign-offs of
//! `context::TEST_SIGN_OFF`, a whole-word RST (`message::is_rst_word`), a
//! serial (1 to 4 of `[0-9TN]` with a digit), a query (`?`, `AGN`, `AGN?`,
//! `RPT`), a Q-code (`Q` plus two letters, optional `?`: no callsign starts
//! with `Q`), a correction (five or more `E`, or `HH`), `V`, `T`, then a
//! call-shaped word (`[A-Z0-9/]{3,15}`, not a `grammar` convention). A run is
//! a maximal sequence of identical call words in one turn.
//!
//! Turns. A turn ends after a turn-end word, and after a word whose
//! `ExchangeWord::ends_turn` is set (an `AR`/`SK` prosign, or a silence the
//! caller measured). No frame or mention is read across a turn end, so a
//! stale `CQ` cannot type a later, unrelated turn's call.
//!
//! Roles, first match wins, for a run `[a..=b]` (`back` is the frame reached
//! by scanning back from `a`, `fwd` the token after `b` in the same turn):
//!
//! 1. `fwd` is a correction, or `b` is `corrected`: `uncertain`.
//! 2. `fwd` is `DE`: `b` is the `addressee`; earlier members are `self_id`
//!    when `back` frames them, else `addressee` (`W1AW W1AW DE K1ABC`).
//! 3. `back` is `DE` (typed `CQ` when a `CQ`, or a non-sign-off `TEST`,
//!    leads straight into the `DE`, or `fwd` is `CQ`), a `CQ` reached through
//!    contest fillers, a non-sign-off `TEST`, or `V V V`: `self_id`.
//! 4. `back` is a sign-off `TEST` (`TU TEST <call>`): `uncertain`, the next
//!    caller or a CQer (MAN-104).
//! 5. `fwd` is `UP`: `self_id`, spot type `De`.
//! 6. `fwd` is a report, serial or query: `addressee` (`<call> 5NN`).
//! 7. `back` is a sign-off (`TU <call>`): `self_id` with no spot type.
//! 8. Otherwise `relayed_mention` when an earlier occurrence in the same turn
//!    is the `self_id` of a different, plausible call, else `uncertain`.
//!
//! A run is settled once a word follows it, its last word ends a turn, or
//! the caller says the window is complete. Every rule reads only the words
//! before the run, the run and the one token after it, so a settled role
//! never changes as words are appended. See
//! docs/DECISIONS/2026-11-01-man201-callsign-roles.md.

use crate::context::{SpotType, CQ_FILLER, TEST_SIGN_OFF};
use crate::{grammar, message};
use std::ops::Range;

/// What a callsign occurrence is to the transmission it was heard in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The sending station identifying itself (`DE <call>`, `CQ <call>`).
    SelfId,
    /// The station being called (`<call> DE <other>`, `<call> 5NN`).
    Addressee,
    /// Another station named inside a station's own turn (`DE K1ABC OP
    /// <call>`).
    RelayedMention,
    /// No rule decides it (`<call> T`, a bare call, a corrected call).
    Uncertain,
}

impl Role {
    /// The snake-case label the ticket and decision record use.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SelfId => "self_id",
            Self::Addressee => "addressee",
            Self::RelayedMention => "relayed_mention",
            Self::Uncertain => "uncertain",
        }
    }
}

/// The exchange form a `self_id` occurrence was heard in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frame {
    Cq,
    De,
    Up,
    Beacon,
    /// `TU <call>`: a self-ID with no spot form.
    SignOff,
}

impl Frame {
    /// The spot type this frame spots as, if any.
    pub fn spot_type(self) -> Option<SpotType> {
        match self {
            Self::Cq => Some(SpotType::Cq),
            Self::De | Self::Up => Some(SpotType::De),
            Self::Beacon => Some(SpotType::Beacon),
            Self::SignOff => None,
        }
    }
}

/// Which rule decided an occurrence's role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    AfterDe,
    AfterCq,
    AfterTest,
    BeaconPreamble,
    BeforeUp,
    AfterSignOff,
    BeforeDe,
    BeforeReport,
    BeforeQuery,
    TestSignOff,
    Corrected,
    MentionInTurn,
    NoFrame,
}

/// One decoded word as the grammar sees it.
#[derive(Debug, Clone, Copy)]
pub struct ExchangeWord<'a> {
    pub text: &'a str,
    /// A turn ends after this word (an `AR`/`SK` prosign glyph, or a silence
    /// the caller measured).
    pub ends_turn: bool,
    /// An `<ERR>` prosign arrived right after this word.
    pub corrected: bool,
}

/// One callsign occurrence and its role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occurrence {
    /// Index into the input slice.
    pub index: usize,
    /// The word, uppercased.
    pub call: String,
    pub role: Role,
    /// `Some` only for `Role::SelfId`.
    pub frame: Option<Frame>,
    pub basis: Basis,
    /// False while a later word could still change the role.
    pub settled: bool,
    /// Word indices that decided the role and frame: the framing words, the
    /// call run, and the token after it when that token decided.
    pub span: Range<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok {
    TurnEnd,
    Cq,
    Test,
    Filler,
    De,
    Up,
    SignOff,
    Report,
    Serial,
    Query,
    QCode,
    Correction,
    V,
    T,
    Call,
    Other,
}

fn classify(word: &str) -> Tok {
    let w = word.to_ascii_uppercase();
    let w = w.as_str();
    if matches!(w, "K" | "KN" | "(" | "BK" | "AR" | "SK") {
        return Tok::TurnEnd;
    }
    match w {
        "CQ" => return Tok::Cq,
        "TEST" => return Tok::Test,
        "DE" => return Tok::De,
        "UP" => return Tok::Up,
        "V" => return Tok::V,
        "T" => return Tok::T,
        "?" | "AGN" | "AGN?" | "RPT" => return Tok::Query,
        "HH" => return Tok::Correction,
        _ => {}
    }
    if CQ_FILLER.split('|').any(|f| f == w) {
        return Tok::Filler;
    }
    if TEST_SIGN_OFF.contains(&w) {
        return Tok::SignOff;
    }
    if message::is_rst_word(w) {
        return Tok::Report;
    }
    if (1..=4).contains(&w.len())
        && w.bytes()
            .all(|b| b.is_ascii_digit() || b == b'T' || b == b'N')
        && w.bytes().any(|b| b.is_ascii_digit())
    {
        return Tok::Serial;
    }
    if is_q_code(w) {
        return Tok::QCode;
    }
    if w.len() >= 5 && w.bytes().all(|b| b == b'E') {
        return Tok::Correction;
    }
    if (3..=15).contains(&w.len())
        && w.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'/')
        && !grammar::is_convention(w)
    {
        return Tok::Call;
    }
    Tok::Other
}

/// `QRZ`, `QRZ?`, `QRL?`, `QSY`: ITU reserves the Q-series for codes, so no
/// callsign starts with `Q`.
fn is_q_code(w: &str) -> bool {
    let body = w.strip_suffix('?').unwrap_or(w);
    body.len() == 3 && body.bytes().all(|b| b.is_ascii_uppercase()) && body.starts_with('Q')
}

/// The frame found by scanning back from a run's first word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Back {
    /// `DE` directly before; `true` when a `CQ` or non-sign-off `TEST` leads
    /// straight into that `DE`.
    De(bool),
    Cq,
    Test,
    TestSignOff,
    Beacon,
    SignOff,
    None,
}

struct Window<'w, 'a> {
    words: &'w [ExchangeWord<'a>],
    toks: Vec<Tok>,
}

impl Window<'_, '_> {
    /// Whether word `i - 1` is in the same turn as word `i`.
    fn joined_to_previous(&self, i: usize) -> bool {
        i > 0 && !self.words[i - 1].ends_turn && self.toks[i - 1] != Tok::TurnEnd
    }

    fn upper(&self, i: usize) -> String {
        self.words[i].text.to_ascii_uppercase()
    }

    /// Start of the contiguous `CQ`/`TEST`/filler run ending at `end`
    /// (inclusive), within `end`'s turn.
    fn framing_run_start(&self, end: usize) -> usize {
        let mut start = end;
        while self.joined_to_previous(start)
            && matches!(self.toks[start - 1], Tok::Cq | Tok::Test | Tok::Filler)
        {
            start -= 1;
        }
        start
    }

    /// MAN-104: a `TEST` at `start` is a run station's trailing sign-off
    /// (`TU TEST`, `<mycall> TEST`) when the word before it, in its turn, is
    /// a sign-off word or a plausible call other than `call`.
    fn test_is_a_sign_off(&self, start: usize, call: &str) -> bool {
        if !self.joined_to_previous(start) {
            return false;
        }
        let prev = self.upper(start - 1);
        (prev != call && grammar::is_plausible(&prev)) || TEST_SIGN_OFF.contains(&prev.as_str())
    }

    /// Start of the run of identical call words ending at `end`, within its
    /// turn.
    fn call_run_start(&self, end: usize) -> usize {
        let mut start = end;
        while self.joined_to_previous(start)
            && self.toks[start - 1] == Tok::Call
            && !self.words[start - 1].corrected
            && self.words[start - 1]
                .text
                .eq_ignore_ascii_case(self.words[end].text)
        {
            start -= 1;
        }
        start
    }

    /// The frame for a run starting at `a`, and the first word of that
    /// frame. A correction (`EEEEE`, `HH`, or an `<ERR>`-flagged run)
    /// directly before is skipped once, with the run it corrects.
    fn back(&self, a: usize, call: &str) -> (Back, usize) {
        let mut p = a;
        let mut skipped = false;
        loop {
            if !self.joined_to_previous(p) {
                return (Back::None, a);
            }
            let j = p - 1;
            match self.toks[j] {
                Tok::De => {
                    let run = self.framing_run_start(j);
                    let promoted = run < j
                        && (self.toks[run..j].contains(&Tok::Cq)
                            || (self.toks[run] == Tok::Test
                                && !self.test_is_a_sign_off(run, call)));
                    let start = if promoted { run } else { j };
                    return (Back::De(promoted), start);
                }
                Tok::Cq | Tok::Test | Tok::Filler => {
                    let run = self.framing_run_start(j);
                    let back = if self.toks[run..=j].contains(&Tok::Cq) {
                        Back::Cq
                    } else if self.toks[run] == Tok::Test {
                        if self.test_is_a_sign_off(run, call) {
                            Back::TestSignOff
                        } else {
                            Back::Test
                        }
                    } else {
                        Back::None
                    };
                    return (back, run);
                }
                Tok::V => {
                    let preamble = j >= 2
                        && self.joined_to_previous(j)
                        && self.joined_to_previous(j - 1)
                        && self.toks[j - 1] == Tok::V
                        && self.toks[j - 2] == Tok::V;
                    return if preamble {
                        (Back::Beacon, j - 2)
                    } else {
                        (Back::None, a)
                    };
                }
                Tok::SignOff => return (Back::SignOff, j),
                Tok::Correction if !skipped => {
                    skipped = true;
                    p = j;
                    if self.joined_to_previous(p) && self.toks[p - 1] == Tok::Call {
                        p = self.call_run_start(p - 1);
                    }
                }
                Tok::Call if self.words[j].corrected && !skipped => {
                    skipped = true;
                    p = self.call_run_start(j);
                }
                _ => return (Back::None, a),
            }
        }
    }
}

/// Assigns every callsign occurrence in `words` one role. `complete` says
/// no further word will follow (the track is closing), which settles the
/// window's last run. Pure and deterministic: no clock, no hashing.
pub fn interpret(words: &[ExchangeWord<'_>], complete: bool) -> Vec<Occurrence> {
    let w = Window {
        words,
        toks: words.iter().map(|w| classify(w.text)).collect(),
    };
    let n = words.len();
    let mut out: Vec<Occurrence> = Vec::new();
    let mut a = 0;
    while a < n {
        if w.toks[a] != Tok::Call {
            a += 1;
            continue;
        }
        // Maximal run of identical calls in one turn; a corrected word ends it.
        let mut b = a;
        while b + 1 < n
            && w.toks[b + 1] == Tok::Call
            && !words[b].ends_turn
            && !words[b].corrected
            && words[b + 1].text.eq_ignore_ascii_case(words[a].text)
        {
            b += 1;
        }
        let call = w.upper(a);
        let (back, frame_start) = w.back(a, &call);
        let fwd = if words[b].ends_turn {
            Some(Tok::TurnEnd)
        } else {
            w.toks.get(b + 1).copied()
        };
        let settled = b + 1 < n || words[b].ends_turn || complete;
        let fwd_end = if b + 1 < n && !words[b].ends_turn {
            b + 2
        } else {
            b + 1
        };
        let back_frame = match back {
            Back::De(true) => Some((Frame::Cq, Basis::AfterDe)),
            Back::De(false) if fwd == Some(Tok::Cq) => Some((Frame::Cq, Basis::AfterDe)),
            Back::De(false) => Some((Frame::De, Basis::AfterDe)),
            Back::Cq => Some((Frame::Cq, Basis::AfterCq)),
            Back::Test => Some((Frame::Cq, Basis::AfterTest)),
            Back::Beacon => Some((Frame::Beacon, Basis::BeaconPreamble)),
            _ => None,
        };
        // Rule 8's test, read before `push` borrows `out`: an earlier
        // occurrence in this turn is another plausible station's self-ID.
        let mention = out.iter().any(|o| {
            o.role == Role::SelfId
                && o.call != call
                && grammar::is_plausible(&o.call)
                && (o.index + 1..=a).all(|i| w.joined_to_previous(i))
        });
        let mut push = |i: usize, role: Role, frame: Option<Frame>, basis: Basis, end: usize| {
            out.push(Occurrence {
                index: i,
                call: call.clone(),
                role,
                frame,
                basis,
                settled,
                span: frame_start..end,
            })
        };
        // 1. A correction withdraws the run.
        if fwd == Some(Tok::Correction) || words[b].corrected {
            for i in a..=b {
                push(i, Role::Uncertain, None, Basis::Corrected, fwd_end);
            }
        // 2. A call directly before `DE` is the station called.
        } else if fwd == Some(Tok::De) {
            for i in a..b {
                match (back_frame, back) {
                    (Some((frame, basis)), _) => push(i, Role::SelfId, Some(frame), basis, b + 1),
                    (None, Back::SignOff) => push(
                        i,
                        Role::SelfId,
                        Some(Frame::SignOff),
                        Basis::AfterSignOff,
                        b + 1,
                    ),
                    _ => push(i, Role::Addressee, None, Basis::BeforeDe, fwd_end),
                }
            }
            push(b, Role::Addressee, None, Basis::BeforeDe, fwd_end);
        // 3. `DE`, `CQ`, `TEST` or `V V V` frames a self-ID.
        } else if let Some((frame, basis)) = back_frame {
            let end = if fwd == Some(Tok::Cq) { fwd_end } else { b + 1 };
            for i in a..=b {
                push(i, Role::SelfId, Some(frame), basis, end);
            }
        // 4. `TU TEST <call>`: the next caller, or a CQer.
        } else if back == Back::TestSignOff {
            for i in a..=b {
                push(i, Role::Uncertain, None, Basis::TestSignOff, b + 1);
            }
        // 5. `<call> UP`.
        } else if fwd == Some(Tok::Up) {
            for i in a..=b {
                push(i, Role::SelfId, Some(Frame::Up), Basis::BeforeUp, fwd_end);
            }
        // 6. `<call> 5NN`, `<call> 001`, `<call> ?`: sent to that station.
        } else if matches!(fwd, Some(Tok::Report | Tok::Serial)) {
            for i in a..=b {
                push(i, Role::Addressee, None, Basis::BeforeReport, fwd_end);
            }
        } else if fwd == Some(Tok::Query) {
            for i in a..=b {
                push(i, Role::Addressee, None, Basis::BeforeQuery, fwd_end);
            }
        // 7. `TU <call>`: the sender signing, with no spot form.
        } else if back == Back::SignOff {
            for i in a..=b {
                push(
                    i,
                    Role::SelfId,
                    Some(Frame::SignOff),
                    Basis::AfterSignOff,
                    b + 1,
                );
            }
        // 8. Named after another station's own self-ID in the same turn.
        } else {
            let (role, basis) = if mention {
                (Role::RelayedMention, Basis::MentionInTurn)
            } else {
                (Role::Uncertain, Basis::NoFrame)
            };
            for i in a..=b {
                push(i, role, None, basis, b + 1);
            }
        }
        a = b + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use Role::*;

    type Row = (String, Role, Option<SpotType>, bool);

    fn words(text: &str) -> Vec<ExchangeWord<'_>> {
        text.split_whitespace()
            .map(|text| ExchangeWord {
                text,
                ends_turn: false,
                corrected: false,
            })
            .collect()
    }

    fn rows(occ: &[Occurrence]) -> Vec<Row> {
        occ.iter()
            .map(|o| {
                (
                    o.call.clone(),
                    o.role,
                    o.frame.and_then(Frame::spot_type),
                    o.settled,
                )
            })
            .collect()
    }

    fn roles(text: &str, complete: bool) -> Vec<Row> {
        rows(&interpret(&words(text), complete))
    }

    /// (call, role, spot type) for a complete window.
    fn roles3(text: &str) -> Vec<(String, Role, Option<SpotType>)> {
        roles(text, true)
            .into_iter()
            .map(|(c, r, t, _)| (c, r, t))
            .collect()
    }

    fn r(call: &str, role: Role, ty: Option<SpotType>) -> (String, Role, Option<SpotType>) {
        (call.to_string(), role, ty)
    }

    const CQ: Option<SpotType> = Some(SpotType::Cq);
    const DE: Option<SpotType> = Some(SpotType::De);
    const BEACON: Option<SpotType> = Some(SpotType::Beacon);

    /// The controlled role corpus: every occurrence of every row, labelled
    /// independently. Its gate is zero role inversions.
    fn corpus() -> Vec<(&'static str, Vec<(String, Role, Option<SpotType>)>)> {
        vec![
            (
                "DE W1AW DE K1ABC",
                vec![r("W1AW", Addressee, None), r("K1ABC", SelfId, DE)],
            ),
            ("CQ DX W1AW", vec![r("W1AW", SelfId, CQ)]),
            (
                "CQ CQ W5XYZ W5XYZ K 5NN 05 TU 73 GL OM DE K1ABC K1ABC K",
                vec![
                    r("W5XYZ", SelfId, CQ),
                    r("W5XYZ", SelfId, CQ),
                    r("K1ABC", SelfId, DE),
                    r("K1ABC", SelfId, DE),
                ],
            ),
            (
                "W1AW W1AW DE K1ABC K1ABC K",
                vec![
                    r("W1AW", Addressee, None),
                    r("W1AW", Addressee, None),
                    r("K1ABC", SelfId, DE),
                    r("K1ABC", SelfId, DE),
                ],
            ),
            (
                "TEST W5AU W5AU W5AU DE K1ABC",
                vec![
                    r("W5AU", SelfId, CQ),
                    r("W5AU", SelfId, CQ),
                    r("W5AU", Addressee, None),
                    r("K1ABC", SelfId, DE),
                ],
            ),
            (
                "CQ W1AW DE K1ABC",
                vec![r("W1AW", Addressee, None), r("K1ABC", SelfId, DE)],
            ),
            (
                "TU W5AU TEST K1ABC",
                vec![r("W5AU", SelfId, None), r("K1ABC", Uncertain, None)],
            ),
            (
                "DE W1AX EEEEE W1AW K",
                vec![r("W1AX", Uncertain, None), r("W1AW", SelfId, DE)],
            ),
            (
                "DE K1ABC K1ABC OP W1AW K",
                vec![
                    r("K1ABC", SelfId, DE),
                    r("K1ABC", SelfId, DE),
                    r("W1AW", RelayedMention, None),
                ],
            ),
            ("K1ABC 5NN 05", vec![r("K1ABC", Addressee, None)]),
            ("K5ARH T", vec![r("K5ARH", Uncertain, None)]),
            ("V V V K5ARH", vec![r("K5ARH", SelfId, BEACON)]),
            ("K1ABC UP", vec![r("K1ABC", SelfId, DE)]),
            ("DE K5ARH CQ", vec![r("K5ARH", SelfId, CQ)]),
            ("QRZ DE K1ABC", vec![r("K1ABC", SelfId, DE)]),
            // MAN-104 contest framing.
            ("CQ WPX CQ WPX W5AU", vec![r("W5AU", SelfId, CQ)]),
            ("TEST TEST W5AU", vec![r("W5AU", SelfId, CQ)]),
            ("5NN 04 TEST W5AU", vec![r("W5AU", SelfId, CQ)]),
            (
                "TU TEST K1ABC K1ABC 5NN 05",
                vec![r("K1ABC", Uncertain, None), r("K1ABC", Uncertain, None)],
            ),
            ("TU TEST DE K1ABC", vec![r("K1ABC", SelfId, DE)]),
            (
                "TU W5AU TEST DE K1ABC",
                vec![r("W5AU", SelfId, None), r("K1ABC", SelfId, DE)],
            ),
            ("TEST DE W5AU", vec![r("W5AU", SelfId, CQ)]),
            (
                "W5AU TEST DE W5AU",
                vec![r("W5AU", Uncertain, None), r("W5AU", SelfId, CQ)],
            ),
            ("K1ABC ?", vec![r("K1ABC", Addressee, None)]),
            (
                "TU W1AW DE K1ABC",
                vec![r("W1AW", Addressee, None), r("K1ABC", SelfId, DE)],
            ),
            ("W1AW CQ", vec![r("W1AW", Uncertain, None)]),
            (
                "CQ POTA W1AW",
                vec![r("POTA", SelfId, CQ), r("W1AW", Uncertain, None)],
            ),
            // MAN-48 glued forms: a framing word glued inside one decoded
            // word frames nothing.
            (
                "NOISE/DE W1AW T",
                vec![r("NOISE/DE", Uncertain, None), r("W1AW", Uncertain, None)],
            ),
            (
                "DE/NOISE W1AW T",
                vec![r("DE/NOISE", Uncertain, None), r("W1AW", Uncertain, None)],
            ),
            ("-CQ W1AW T", vec![r("W1AW", Uncertain, None)]),
            (
                "DE K5ARH NOISE/CQ",
                vec![r("K5ARH", SelfId, DE), r("NOISE/CQ", RelayedMention, None)],
            ),
        ]
    }

    #[test]
    fn scenario_1_the_addressee_of_a_call_is_not_spotted_as_its_transmitter() {
        assert_eq!(
            roles("DE W1AW DE K1ABC", true),
            vec![
                ("W1AW".into(), Addressee, None, true),
                ("K1ABC".into(), SelfId, DE, true),
            ]
        );
        let open = roles("DE W1AW DE K1ABC", false);
        assert!(open[0].3, "W1AW is settled by the DE after it");
        assert!(!open[1].3, "K1ABC, the window's tail, is pending");
    }

    #[test]
    fn scenario_2_a_bare_cq_with_a_call_is_self_id_cq() {
        assert_eq!(
            roles("CQ DX W1AW", true),
            vec![("W1AW".into(), SelfId, CQ, true)]
        );
    }

    #[test]
    fn role_corpus() {
        for (text, want) in corpus() {
            assert_eq!(roles3(text), want, "window {text:?}");
        }
        for filler in CQ_FILLER.split('|') {
            let text = format!("CQ {filler} W1AW");
            assert_eq!(roles3(&text), vec![r("W1AW", SelfId, CQ)], "{text:?}");
        }
    }

    #[test]
    fn a_cq_frames_only_the_call_it_reaches_through_filler() {
        assert_eq!(
            roles3("CQ CQ W5XYZ K DE K1ABC"),
            vec![r("W5XYZ", SelfId, CQ), r("K1ABC", SelfId, DE)]
        );
        assert_eq!(roles3("CQ 73 DE K1ABC"), vec![r("K1ABC", SelfId, DE)]);
        assert_eq!(roles3("CQ CQ DE K1ABC"), vec![r("K1ABC", SelfId, CQ)]);
    }

    #[test]
    fn a_turn_end_closes_frames_and_settles_the_call_before_it() {
        for end in ["K", "KN", "(", "BK", "AR", "SK"] {
            let text = format!("CQ {end} W1AW");
            assert_eq!(roles3(&text), vec![r("W1AW", Uncertain, None)], "{text:?}");
            let text = format!("DE W1AW {end}");
            assert_eq!(
                roles(&text, false),
                vec![("W1AW".into(), SelfId, DE, true)],
                "{text:?}"
            );
        }
        // An `ends_turn` flag on the call settles it, and a following `DE`
        // (the next turn) does not make it an addressee.
        let mut ws = words("DE W1AW");
        ws[1].ends_turn = true;
        assert_eq!(
            rows(&interpret(&ws, false)),
            vec![("W1AW".into(), SelfId, DE, true)]
        );
        let mut ws = words("DE W1AW DE K1ABC");
        ws[1].ends_turn = true;
        assert_eq!(
            rows(&interpret(&ws, true)),
            vec![
                ("W1AW".into(), SelfId, DE, true),
                ("K1ABC".into(), SelfId, DE, true),
            ]
        );
        // A flagged turn end also stops a CQ framing across it.
        let mut ws = words("CQ CQ DE K1ABC");
        ws[1].ends_turn = true;
        assert_eq!(
            rows(&interpret(&ws, true)),
            vec![("K1ABC".into(), SelfId, DE, true)]
        );
    }

    #[test]
    fn the_tail_is_pending_until_a_next_word_a_turn_end_or_completion() {
        assert_eq!(
            roles("DE K5ARH", false),
            vec![("K5ARH".into(), SelfId, DE, false)]
        );
        assert_eq!(
            roles("DE K5ARH K5ARH", false),
            vec![
                ("K5ARH".into(), SelfId, DE, false),
                ("K5ARH".into(), SelfId, DE, false),
            ]
        );
        assert!(roles("DE K5ARH 73", false)[0].3);
        assert!(roles("DE K5ARH K", false)[0].3);
        assert!(roles("DE K5ARH", true)[0].3);
    }

    #[test]
    fn a_correction_withdraws_the_previous_call_and_passes_its_frame_on() {
        for marker in ["EEEEE", "EEEEEEEE", "HH"] {
            let text = format!("DE W1AX {marker} W1AW K");
            assert_eq!(
                roles3(&text),
                vec![r("W1AX", Uncertain, None), r("W1AW", SelfId, DE)],
                "{text:?}"
            );
        }
        let mut ws = words("DE W1AX W1AW K");
        ws[1].corrected = true;
        assert_eq!(
            rows(&interpret(&ws, true))
                .into_iter()
                .map(|(c, r, t, _)| (c, r, t))
                .collect::<Vec<_>>(),
            vec![r("W1AX", Uncertain, None), r("W1AW", SelfId, DE)]
        );
    }

    #[test]
    fn relayed_mention_needs_a_plausible_self_id_in_the_same_turn() {
        assert_eq!(
            roles3("CQ DX FILLER1 FILLER2 K5ARH T"),
            vec![
                r("FILLER1", SelfId, CQ),
                r("FILLER2", Uncertain, None),
                r("K5ARH", Uncertain, None),
            ]
        );
        assert_eq!(
            roles3("W1AW TNX DE K1ABC"),
            vec![r("W1AW", Uncertain, None), r("K1ABC", SelfId, DE)]
        );
        // A turn end between the self-ID and the call: not a mention.
        assert_eq!(
            roles3("DE K1ABC K OP W1AW"),
            vec![r("K1ABC", SelfId, DE), r("W1AW", Uncertain, None)]
        );
    }

    #[test]
    fn a_settled_role_never_changes_as_words_are_appended() {
        for (text, _) in corpus() {
            let full = interpret(&words(text), true);
            let all = words(text);
            for len in 1..=all.len() {
                for o in interpret(&all[..len], false)
                    .into_iter()
                    .filter(|o| o.settled)
                {
                    let f = full.iter().find(|f| f.index == o.index).unwrap();
                    assert_eq!(
                        (o.role, o.frame, o.basis),
                        (f.role, f.frame, f.basis),
                        "{text:?} prefix {len}, word {}",
                        o.index
                    );
                }
            }
        }
    }

    #[test]
    fn every_call_shaped_non_keyword_word_gets_exactly_one_occurrence() {
        for (text, _) in corpus() {
            let ws = words(text);
            let calls = ws.iter().filter(|w| classify(w.text) == Tok::Call).count();
            let occ = interpret(&ws, true);
            assert_eq!(occ.len(), calls, "{text:?}");
            let mut idx: Vec<usize> = occ.iter().map(|o| o.index).collect();
            idx.dedup();
            assert_eq!(idx.len(), calls, "{text:?}");
        }
    }

    #[test]
    fn no_keyword_or_q_code_is_a_call_occurrence() {
        let mut keywords: Vec<&str> = CQ_FILLER.split('|').collect();
        keywords.extend([
            "TU", "TNX", "QSL", "5NN", "599", "AGN", "RPT", "QRZ", "QRZ?", "QRL?", "QSY", "K",
            "KN", "BK", "AR", "SK", "3NN", "4NN", "UP", "DE",
        ]);
        for k in keywords {
            assert!(interpret(&words(k), true).is_empty(), "{k:?}");
            assert_ne!(classify(k), Tok::Call, "{k:?}");
        }
    }

    #[test]
    fn interpret_is_deterministic() {
        for (text, _) in corpus() {
            assert_eq!(
                interpret(&words(text), true),
                interpret(&words(text), true),
                "{text:?}"
            );
        }
    }

    #[test]
    fn role_labels() {
        assert_eq!(SelfId.as_str(), "self_id");
        assert_eq!(Addressee.as_str(), "addressee");
        assert_eq!(RelayedMention.as_str(), "relayed_mention");
        assert_eq!(Uncertain.as_str(), "uncertain");
    }
}
