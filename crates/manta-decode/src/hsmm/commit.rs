//! Partial-traceback commit and posterior margins. SPEC v2 §4.8–4.9.
use super::token::{glyph_rank, Token};
use super::{Committed, HsmmConfig};
use crate::tree::Glyph;

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

/// Confidence and alternatives for entry `j` of the best token (tokens
/// must be sorted by `token_order`, best first).
pub fn margin(tokens: &[Token], j: usize, cfg: &HsmmConfig) -> (f32, Vec<(Glyph, f32)>) {
    let best = &tokens[0];
    let g = best.hist.get(j).map(|e| e.glyph);
    let mut alts: Vec<(Option<Glyph>, f32)> = Vec::new();
    // Codex review, PR #161 round 2: `s_alt` (the runner-up score the
    // margin is computed against) must include every token that
    // DISAGREES with `best` at entry `j` -- including one that hasn't
    // appended entry `j` at all yet (`tg` outer `None`, e.g. it's shorter
    // or behind on a forced commit). The old code only ever folded
    // `alts`' own scores into `s_alt`, and `alts` only ever holds tokens
    // with an actual glyph/boundary value to report as an alternative --
    // an undecided token has none, so it was silently excluded from
    // `s_alt` too, letting a real, still-live, near-tied competitor
    // inflate confidence toward sigmoid(4kappa/kappa) ~= 0.982 instead of
    // the ~0.5 its true score gap implies. Track `s_alt` independently of
    // `alts` so every disagreeing token counts toward the margin, while
    // `alts` (the reported alternative list) still only ever contains
    // tokens with a real glyph/boundary to show.
    let mut s_alt = f32::NEG_INFINITY;
    for t in tokens.iter().skip(1) {
        let tg = t.hist.get(j).map(|e| e.glyph);
        if tg != g {
            s_alt = s_alt.max(t.score);
            if let Some(og) = tg.flatten() {
                if !alts.iter().any(|(a, _)| *a == Some(og)) {
                    alts.push((Some(og), t.score));
                }
            } else if tg.is_some() && !alts.iter().any(|(a, _)| a.is_none()) {
                alts.push((None, t.score));
            }
        }
    }
    let s_alt = if s_alt.is_finite() {
        s_alt
    } else {
        best.score - 4.0 * cfg.conf_kappa
    };
    let confidence = sigmoid((best.score - s_alt) / cfg.conf_kappa);
    alts.sort_by(|a, b| {
        b.1.total_cmp(&a.1)
            .then_with(|| glyph_rank(a.0).cmp(&glyph_rank(b.0)))
    });
    let alternatives = alts
        .into_iter()
        .filter_map(|(g, s)| g.map(|g| (g, sigmoid((s - best.score) / cfg.conf_kappa))))
        .take(2)
        .collect();
    (confidence, alternatives)
}

/// Emit the consensus prefix and any entry older than the lookahead;
/// remove emitted entries from every token; drop tokens that disagree
/// with a forced commit. `tokens` is sorted best-first on entry and
/// stays sorted.
///
/// [Task 8 fix, execution 2026-09-09]: `sealed` is a list, owned by
/// `HsmmDecoder` and threaded through every call, of `(sample_ts, glyph)`
/// pairs this decoder has ever actually committed. `HsmmDecoder::push`
/// keeps a handful of past `Anchor`s alive (up to `reach_sil` hops back,
/// SPEC v2 §4.6) so a later hop can still generate a candidate token by
/// transitioning directly from an *older* anchor's frozen token snapshot.
/// For a still-`AfterMark` ancestor sitting in one of those old anchors,
/// SPEC v2 §4.3's flat Silence duration prior stays satisfiable across a
/// *wide*, still-growing window ([10u, 80u] hops) -- so at every
/// subsequent anchor step, `HsmmDecoder::push` re-derives a *fresh*
/// candidate closing the SAME gap with a *larger* `d`, and
/// `Token::successor` stamps that fresh `HistEntry` with `born_hop =
/// end_hop` (the current hop, correct per its own doc comment -- this is
/// a new, better-evidenced re-estimate of "how long the gap has run so
/// far"). Because `born_hop` legitimately keeps advancing on every
/// re-derivation, a watermark keyed on it (the fix's first cut) cannot
/// tell "the same decision, re-estimated" apart from "a genuinely new
/// decision", and the `forced` check below re-fires on literally every
/// subsequent anchor step for as long as the gap keeps growing --
/// reproduced directly: an isolated "C" followed by a long trailing
/// silence committed the same glyph, at the same `sample_ts`, six times
/// over (three during the run, three more at `finish`), and the full
/// "CQ TEST W5AU W5AU TEST" scene came out as
/// "CCCQQQQ TESTT T WW5AUU WW5AUU TEST".
///
/// `HistEntry::sample_ts`, by contrast, is pinned to the *anchor's own*
/// `sample_ts` (`Token::successor`'s `seg_start_ts` parameter) -- fixed
/// once when that ancestor anchor was created, identical across every
/// re-derivation from it no matter how large `d` grows. Sealing on
/// `(sample_ts, is_char)` instead correctly recognizes every re-derived
/// candidate as the same already-decided instant and strips it before
/// the consensus/forced check ever sees it, while still letting a
/// legitimately different word-boundary entry at that same instant
/// (same `sample_ts`, `glyph: None` vs `Some(_)`, since a closing
/// transition emits at most one of each) commit in its own right once
/// evidence resolves it.
///
/// [Task 8 fix, review round 6, Codex on PR #161]: the seal key is
/// `(sample_ts, is_char)` -- whether the entry is a character at all,
/// NOT the specific glyph -- because a retained older anchor re-deriving
/// an already-committed gap under a different node/speed hypothesis can
/// disagree with the winning hypothesis on WHICH character it is while
/// agreeing on WHEN and THAT it's a character. Sealing on the exact
/// `(sample_ts, glyph)` pair (the original design) only strips a
/// re-derivation that happens to name the same glyph already committed;
/// once the winning hypothesis's token falls outside its valid
/// silence-duration window and stops being re-derived, a runner-up
/// lineage's disagreeing glyph at that same `sample_ts` was never in
/// `sealed` at all and could surface as a second, contradictory
/// `CharDecoded` for an instant already resolved -- the validator then
/// appends both characters into corrupted spot text. At most one
/// character and one word-boundary decision can ever be correct for a
/// given `sample_ts` (a closing transition emits at most one of each),
/// so sealing by kind rather than by value is exactly as permissive as
/// the real invariant requires.
///
/// [Task 8 fix, review round 2]: the strip runs at the *top of every loop
/// iteration*, not just once on entry. Committing one entry can reveal a
/// new front entry on some token that a *different* token/anchor lineage
/// already sealed independently in an earlier call (e.g. one surviving
/// token holds `[X@ts1, Y@ts2, ...]` while the beam already committed a
/// different glyph `X'` at `ts1` from a sibling token, sealing `(ts1,
/// X')` -- this token's own, disagreeing `(ts1, X)` front entry must be
/// silently dropped, not re-committed as a duplicate/contradictory event,
/// once it becomes the front after some earlier removal in this same
/// call). Re-checking every iteration catches that without needing a
/// second, separately-maintained check.
pub fn commit(
    tokens: &mut Vec<Token>,
    now_hop: u64,
    cfg: &HsmmConfig,
    sealed: &mut Vec<(u64, bool)>,
) -> Vec<Committed> {
    let mut out = Vec::new();
    loop {
        for t in tokens.iter_mut() {
            while t
                .hist
                .first()
                .is_some_and(|e| sealed.contains(&(e.sample_ts, e.glyph.is_some())))
            {
                t.hist.remove(0);
            }
        }
        if tokens.is_empty() || tokens[0].hist.is_empty() {
            break;
        }
        let head = tokens[0].hist[0];
        let consensus = tokens
            .iter()
            .all(|t| t.hist.first().map(|e| e.glyph) == Some(head.glyph));
        // `saturating_sub`: `now_hop` is always >= any live token's
        // `born_hop` (both derived from the same monotonic evidence hop
        // counter), but make that invariant explicit rather than relying
        // on it silently.
        let forced =
            now_hop.saturating_sub(head.born_hop) as f32 > cfg.lookahead_dits * tokens[0].u;
        if !consensus && !forced {
            break;
        }
        let (confidence, alternatives) = margin(tokens, 0, cfg);
        out.push(Committed {
            glyph: head.glyph,
            sample_ts: head.sample_ts,
            confidence,
            alternatives,
        });
        tokens.retain(|t| t.hist.first().map(|e| e.glyph) == Some(head.glyph));
        for t in tokens.iter_mut() {
            t.hist.remove(0);
        }
        sealed.push((head.sample_ts, head.glyph.is_some()));
    }
    out
}
