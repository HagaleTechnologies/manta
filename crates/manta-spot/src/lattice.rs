//! MAN-200: a bounded callsign evidence lattice per self-identification
//! episode (investigation 9 of docs/DECISIONS/2026-09-09-decoder-recall-research.md).
//!
//! Damaged copies of one station's CQ/DE/UP self-ID -- `W1AV`, `W1EW` and
//! `M1AW` for `W1AW` -- are grouped into an episode (one input, one
//! channel, the 90 s window), lined up character by character with
//! insertions and deletions, and voted per position. Votes count copies,
//! not confidence (the legacy engine's confidence barely separates a wrong
//! character from a right one): each copy votes 1 for its top glyph, 1 for
//! "nothing here" where the alignment gives it a gap, and 1 for unknown
//! where it decoded the `?` placeholder; a listed alternative adds its
//! score relative to the top glyph's, at most 1.
//!
//! A position is decided only by a strict majority with a clear lead over
//! every rival, unknown included. Any undecided position makes the explicit
//! unknown hypothesis win, and nothing is confirmed. No glyph is ever
//! invented: every winning glyph is one some copy read or listed. Copies of
//! the same samples (overlapping spans) are one observation, and
//! message-distinctness reuses the repetition gate's own rule.

use std::collections::BTreeMap;

use crate::gate::{
    classify_new_occurrence, overlaps, promote_orphaned_leader, MIN_MESSAGE_TIME_GAP_SECONDS,
    SAME_SIGNAL_MAX_HZ, WINDOW_SECONDS,
};
use crate::variant::{levenshtein, MAX_EDIT_DISTANCE};

/// D5: the lead a decided position needs over every other candidate. For
/// top-1 copies it reads "at least one more copy than any rival".
pub const MIN_COLUMN_MARGIN: f64 = 0.5;
/// D6: a copy joins an episode only within this many edits of every member
/// (the variant arbiter's own confusability bound).
pub const MAX_JOIN_EDITS: usize = MAX_EDIT_DISTANCE;
/// D6: the exchange grammar's shortest call token.
pub const MIN_COPY_CHARS: usize = 3;
/// D13: observations kept per episode; the oldest is evicted.
pub const MAX_OBSERVATIONS: usize = 16;
/// D13: episodes kept; the one with the oldest newest observation goes.
pub const MAX_EPISODES: usize = 4096;

/// One decoded character with its alternatives.
#[derive(Debug, Clone, PartialEq)]
pub struct CharEvidence {
    pub glyph: char,
    pub confidence: f32,
    pub alternatives: Vec<(char, f32)>,
}

/// One self-ID copy offered to the lattice.
#[derive(Debug, Clone, PartialEq)]
pub struct Copy {
    pub text: String,
    pub chars: Vec<CharEvidence>,
    /// D2: first character's `sample_ts` to the `WordBoundary`'s.
    pub span: (u64, u64),
    pub track_id: u32,
    pub word_seq: u64,
    pub freq_hz: f64,
}

/// An episode's identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct EpisodeKey {
    pub input: u32,
    pub id: u64,
}

/// What `EvidenceLattice::add` did with a copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddOutcome {
    /// Stored as a new observation of this episode.
    Added(EpisodeKey),
    /// Its span overlaps a copy already in this episode: the same samples,
    /// no new observation and no vote (D7).
    Duplicate(EpisodeKey),
}

/// A confirmed fusion.
#[derive(Debug, Clone, PartialEq)]
pub struct Fused {
    pub call: String,
    /// Message-distinct observations within `MAX_JOIN_EDITS` of `call`.
    pub reps: u32,
    pub observations: u32,
    /// Observations that read `call` exactly.
    pub exact_copies: u32,
    /// The smallest lead of a decided position.
    pub min_margin: f64,
    /// Per winning position, the mean top confidence of the copies whose
    /// top glyph won (the largest alternative score when only alternatives
    /// named it).
    pub char_confidences: Vec<f32>,
    /// The distinct texts of observations that are not `call`.
    pub disagreeing: Vec<String>,
}

/// The lattice's answer for one episode.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Confirmed(Fused),
    /// Position `position` of the alignment is undecided: unknown wins.
    Unknown {
        position: usize,
    },
    /// Every position decided, but too few separate messages (or a winner
    /// shorter than `MIN_COPY_CHARS`).
    Insufficient {
        reps: u32,
        required: u32,
    },
}

/// Additions and evictions, for diagnostics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LatticeCounts {
    pub added: u64,
    pub duplicates: u64,
    pub observations_evicted: u64,
    pub episodes_evicted: u64,
}

struct Episode {
    anchor_freq_hz: f64,
    /// The gate's `(sample_ts, word_seq, track_id, counted, anchor_ts)`
    /// shape, index-aligned with `copies`.
    occurrences: Vec<(u64, u64, u32, bool, u64)>,
    copies: Vec<Copy>,
    last_seen_by_track: BTreeMap<u32, (u64, u64, u64)>,
}

impl Episode {
    fn newest(&self) -> u64 {
        self.copies.iter().map(|c| c.span.1).max().unwrap_or(0)
    }
}

/// A column candidate. Ordered so a tie picks a glyph first, deterministically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Cand {
    Glyph(char),
    Gap,
    Unknown,
}

/// The episodes of every input. See the module doc.
pub struct EvidenceLattice {
    window_samples: u64,
    time_gap_samples: u64,
    episodes: BTreeMap<(u32, u64), Episode>,
    next_id: u64,
    counts: LatticeCounts,
}

impl EvidenceLattice {
    pub fn new(fs: f64) -> Self {
        Self {
            window_samples: (WINDOW_SECONDS * fs) as u64,
            time_gap_samples: (MIN_MESSAGE_TIME_GAP_SECONDS * fs) as u64,
            episodes: BTreeMap::new(),
            next_id: 0,
            counts: LatticeCounts::default(),
        }
    }

    /// Stores `copy` in its episode (D6, D7) and freezes its
    /// message-distinct verdict, as the repetition gate does.
    pub fn add(
        &mut self,
        input: u32,
        copy: Copy,
        is_track_active: impl Fn(u32) -> bool,
    ) -> AddOutcome {
        let cutoff = copy.span.1.saturating_sub(self.window_samples);
        let span = (copy.span.0.min(copy.span.1), copy.span.1);
        let eligible: Vec<(u64, usize)> = self
            .episodes
            .range((input, 0)..=(input, u64::MAX))
            .filter(|(_, ep)| (copy.freq_hz - ep.anchor_freq_hz).abs() < SAME_SIGNAL_MAX_HZ)
            .filter_map(|(&(_, id), ep)| {
                let live: Vec<&Copy> = ep.copies.iter().filter(|c| c.span.1 >= cutoff).collect();
                if live.is_empty() {
                    return None;
                }
                let worst = live
                    .iter()
                    .map(|c| levenshtein(&c.text, &copy.text))
                    .max()
                    .unwrap_or(0);
                Some((id, worst))
            })
            .collect();
        // D7: the same samples, near enough to be the same call, are a
        // duplicate of the first arrival. Its track's watermark still takes
        // the overlapped copy's anchor under the never-regress rule, as
        // `RepetitionGate::record_span` does (Codex review, PR #500).
        for &(id, _) in &eligible {
            let ep = self.episodes.get_mut(&(input, id)).expect("eligible");
            let overlapped = ep.copies.iter().position(|c| {
                c.span.1 >= cutoff
                    && overlaps(c.span, span)
                    && levenshtein(&c.text, &copy.text) <= MAX_JOIN_EDITS
            });
            if let Some(i) = overlapped {
                let anchor_ts = ep.occurrences[i].4;
                ep.last_seen_by_track
                    .entry(copy.track_id)
                    .and_modify(|e| {
                        if span.1 >= e.0 {
                            *e = (span.1, copy.word_seq, anchor_ts);
                        }
                    })
                    .or_insert((span.1, copy.word_seq, anchor_ts));
                self.counts.duplicates += 1;
                return AddOutcome::Duplicate(EpisodeKey { input, id });
            }
        }
        let joined = eligible
            .iter()
            .filter(|&&(_, worst)| worst <= MAX_JOIN_EDITS)
            .min_by(|a, b| {
                let na = self.episodes[&(input, a.0)].newest();
                let nb = self.episodes[&(input, b.0)].newest();
                a.1.cmp(&b.1).then(nb.cmp(&na)).then(a.0.cmp(&b.0))
            })
            .map(|&(id, _)| id);
        let id = match joined {
            Some(id) => id,
            None => {
                if self.episodes.len() >= MAX_EPISODES {
                    let oldest = self
                        .episodes
                        .iter()
                        .min_by_key(|(k, ep)| (ep.newest(), **k))
                        .map(|(k, _)| *k);
                    if let Some(k) = oldest {
                        self.episodes.remove(&k);
                        self.counts.episodes_evicted += 1;
                    }
                }
                let id = self.next_id;
                self.next_id += 1;
                self.episodes.insert(
                    (input, id),
                    Episode {
                        anchor_freq_hz: copy.freq_hz,
                        occurrences: Vec::new(),
                        copies: Vec::new(),
                        last_seen_by_track: BTreeMap::new(),
                    },
                );
                id
            }
        };
        let time_gap_samples = self.time_gap_samples;
        let ep = self
            .episodes
            .get_mut(&(input, id))
            .expect("just found or made");
        if copy.span.1 >= ep.newest() {
            ep.anchor_freq_hz = copy.freq_hz;
        }
        let own_last_seen = ep.last_seen_by_track.get(&copy.track_id).copied();
        let (counted, anchor_ts) = classify_new_occurrence(
            &ep.occurrences,
            own_last_seen,
            copy.word_seq,
            span.1,
            copy.track_id,
            time_gap_samples,
            &is_track_active,
        );
        ep.last_seen_by_track
            .entry(copy.track_id)
            .and_modify(|e| {
                if span.1 >= e.0 {
                    *e = (span.1, copy.word_seq, anchor_ts);
                }
            })
            .or_insert((span.1, copy.word_seq, anchor_ts));
        ep.occurrences
            .push((span.1, copy.word_seq, copy.track_id, counted, anchor_ts));
        ep.copies.push(Copy { span, ..copy });
        while ep.copies.len() > MAX_OBSERVATIONS {
            ep.copies.remove(0);
            ep.occurrences.remove(0);
            self.counts.observations_evicted += 1;
        }
        Self::prune(ep, cutoff);
        self.counts.added += 1;
        AddOutcome::Added(EpisodeKey { input, id })
    }

    fn prune(ep: &mut Episode, cutoff: u64) {
        let mut i = 0;
        while i < ep.copies.len() {
            if ep.copies[i].span.1 < cutoff {
                ep.copies.remove(i);
                ep.occurrences.remove(i);
            } else {
                i += 1;
            }
        }
        ep.last_seen_by_track
            .retain(|_, &mut (ts, ..)| ts >= cutoff);
        promote_orphaned_leader(&mut ep.occurrences);
    }

    /// Message-distinct observations of the episode inside the window.
    pub fn repetitions(&self, key: EpisodeKey, now_ts: u64) -> u32 {
        let cutoff = now_ts.saturating_sub(self.window_samples);
        self.episodes
            .get(&(key.input, key.id))
            .map(|ep| {
                ep.occurrences
                    .iter()
                    .filter(|&&(ts, _, _, counted, _)| counted && ts >= cutoff)
                    .count() as u32
            })
            .unwrap_or(0)
    }

    /// Aligns the episode's in-window observations and decides every
    /// position (D4, D5, D8).
    pub fn evaluate(&self, key: EpisodeKey, now_ts: u64, required_reps: u32) -> Verdict {
        let cutoff = now_ts.saturating_sub(self.window_samples);
        let Some(ep) = self.episodes.get(&(key.input, key.id)) else {
            return Verdict::Insufficient {
                reps: 0,
                required: required_reps,
            };
        };
        let mut obs: Vec<(&Copy, bool)> = ep
            .copies
            .iter()
            .zip(&ep.occurrences)
            .filter(|(c, _)| c.span.1 >= cutoff)
            .map(|(c, o)| (c, o.3))
            .collect();
        obs.sort_by_key(|(c, _)| (c.span.0, c.track_id, c.word_seq));
        if obs.is_empty() {
            return Verdict::Insufficient {
                reps: 0,
                required: required_reps,
            };
        }
        let columns = align(obs.iter().map(|(c, _)| *c));
        let n = obs.len() as f64;
        let mut call = String::new();
        let mut char_confidences = Vec::new();
        let mut min_margin = f64::INFINITY;
        for (position, col) in columns.iter().enumerate() {
            match decide_column(&col.votes, n) {
                None => return Verdict::Unknown { position },
                Some((Cand::Gap, margin)) => min_margin = min_margin.min(margin),
                Some((Cand::Glyph(g), margin)) => {
                    min_margin = min_margin.min(margin);
                    call.push(g);
                    let tops: Vec<f32> = col
                        .members
                        .iter()
                        .filter(|e| e.glyph == g)
                        .map(|e| e.confidence)
                        .collect();
                    let conf = if tops.is_empty() {
                        col.members
                            .iter()
                            .flat_map(|e| e.alternatives.iter())
                            .filter(|&&(a, _)| a == g)
                            .map(|&(_, s)| s)
                            .fold(0.0f32, f32::max)
                    } else {
                        tops.iter().sum::<f32>() / tops.len() as f32
                    };
                    char_confidences.push(conf);
                }
                Some((Cand::Unknown, _)) => return Verdict::Unknown { position },
            }
        }
        let reps = obs
            .iter()
            .filter(|(c, counted)| *counted && levenshtein(&c.text, &call) <= MAX_JOIN_EDITS)
            .count() as u32;
        if call.chars().count() < MIN_COPY_CHARS || reps < required_reps {
            return Verdict::Insufficient {
                reps,
                required: required_reps,
            };
        }
        let mut disagreeing: Vec<String> = obs
            .iter()
            .filter(|(c, _)| c.text != call)
            .map(|(c, _)| c.text.clone())
            .collect();
        disagreeing.sort();
        disagreeing.dedup();
        Verdict::Confirmed(Fused {
            exact_copies: obs.iter().filter(|(c, _)| c.text == call).count() as u32,
            observations: obs.len() as u32,
            call,
            reps,
            min_margin,
            char_confidences,
            disagreeing,
        })
    }

    /// Forgets copies older than the window, and empty episodes.
    pub fn sweep(&mut self, now_ts: u64) {
        let cutoff = now_ts.saturating_sub(self.window_samples);
        self.episodes.retain(|_, ep| {
            Self::prune(ep, cutoff);
            !ep.copies.is_empty()
        });
    }

    pub fn counts(&self) -> LatticeCounts {
        self.counts
    }

    /// Episodes held.
    pub fn len(&self) -> usize {
        self.episodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.episodes.is_empty()
    }
}

struct Column {
    votes: BTreeMap<Cand, f64>,
    /// The character evidence aligned here, for the winner's confidence.
    members: Vec<CharEvidence>,
}

impl Column {
    fn top(&self) -> Option<Cand> {
        let mut best: Option<(Cand, f64)> = None;
        for (&c, &v) in &self.votes {
            if best.is_none_or(|(_, bv)| v > bv) {
                best = Some((c, v));
            }
        }
        best.map(|(c, _)| c)
    }

    fn vote_char(&mut self, e: &CharEvidence) {
        if e.glyph == '?' {
            *self.votes.entry(Cand::Unknown).or_default() += 1.0;
        } else {
            *self.votes.entry(Cand::Glyph(e.glyph)).or_default() += 1.0;
            let top = f64::from(e.confidence);
            if top.is_finite() && top > 0.0 {
                for &(g, a) in &e.alternatives {
                    if g == e.glyph || g == '?' {
                        continue;
                    }
                    let a = f64::from(a);
                    if a.is_finite() && a > 0.0 {
                        *self.votes.entry(Cand::Glyph(g)).or_default() += (a / top).min(1.0);
                    }
                }
            }
        }
        self.members.push(e.clone());
    }
}

/// Builds the confusion network: the first copy seeds one column per
/// character, each next copy is aligned to the columns' current top
/// candidates by Levenshtein (ties: diagonal, then deletion, then
/// insertion). An insertion adds a column where earlier copies have a gap.
fn align<'a>(copies: impl Iterator<Item = &'a Copy>) -> Vec<Column> {
    let mut columns: Vec<Column> = Vec::new();
    let mut seen = 0usize;
    for copy in copies {
        let chars = &copy.chars;
        if seen == 0 {
            for e in chars {
                let mut col = Column {
                    votes: BTreeMap::new(),
                    members: Vec::new(),
                };
                col.vote_char(e);
                columns.push(col);
            }
            seen = 1;
            continue;
        }
        let tops: Vec<Option<Cand>> = columns.iter().map(Column::top).collect();
        let (m, k) = (columns.len(), chars.len());
        let sub = |i: usize, j: usize| -> usize {
            let g = chars[j].glyph;
            match tops[i] {
                _ if g == '?' => 0,
                Some(Cand::Glyph(t)) if t == g => 0,
                _ => 1,
            }
        };
        // dp[i][j]: cost of aligning the first i columns with the first j chars.
        let mut dp = vec![vec![0usize; k + 1]; m + 1];
        for (i, row) in dp.iter_mut().enumerate() {
            row[0] = i;
        }
        for (j, cell) in dp[0].iter_mut().enumerate() {
            *cell = j;
        }
        for i in 1..=m {
            for j in 1..=k {
                dp[i][j] = (dp[i - 1][j - 1] + sub(i - 1, j - 1))
                    .min(dp[i - 1][j] + 1)
                    .min(dp[i][j - 1] + 1);
            }
        }
        // Trace back: per column, Some(char index) or None (a gap); plus
        // insertions before column i.
        let mut assign: Vec<Option<usize>> = vec![None; m];
        let mut inserts: Vec<(usize, usize)> = Vec::new(); // (before column i, char j)
        let (mut i, mut j) = (m, k);
        while i > 0 || j > 0 {
            if i > 0 && j > 0 && dp[i][j] == dp[i - 1][j - 1] + sub(i - 1, j - 1) {
                assign[i - 1] = Some(j - 1);
                i -= 1;
                j -= 1;
            } else if i > 0 && dp[i][j] == dp[i - 1][j] + 1 {
                i -= 1;
            } else {
                inserts.push((i, j - 1));
                j -= 1;
            }
        }
        for (ci, a) in assign.iter().enumerate() {
            match a {
                Some(j) => columns[ci].vote_char(&chars[*j]),
                None => *columns[ci].votes.entry(Cand::Gap).or_default() += 1.0,
            }
        }
        // Insert new columns, from the back so indices stay valid. Within
        // one gap, `inserts` is in reverse char order, so each insert at
        // the same index pushes the later char right.
        for (before, jc) in inserts {
            let mut col = Column {
                votes: BTreeMap::new(),
                members: Vec::new(),
            };
            col.votes.insert(Cand::Gap, seen as f64);
            col.vote_char(&chars[jc]);
            columns.insert(before, col);
        }
        seen += 1;
    }
    columns
}

/// D5: the unique highest candidate with more than half the copies and a
/// lead of at least `MIN_COLUMN_MARGIN` over every other, with that lead.
fn decide_column(votes: &BTreeMap<Cand, f64>, n: f64) -> Option<(Cand, f64)> {
    let mut ranked: Vec<(Cand, f64)> = votes.iter().map(|(&c, &v)| (c, v)).collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let (best, bv) = *ranked.first()?;
    let second = ranked.get(1).map_or(0.0, |&(_, v)| v);
    let margin = bv - second;
    (bv > n / 2.0 && margin >= MIN_COLUMN_MARGIN && margin > 0.0).then_some((best, margin))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 96_000.0;
    const F: f64 = 14_000_000.0;

    fn copy(text: &str, start: u64, end: u64, track: u32, seq: u64) -> Copy {
        Copy {
            text: text.into(),
            chars: text
                .chars()
                .map(|c| CharEvidence {
                    glyph: c,
                    confidence: if c == '?' { 0.0 } else { 0.95 },
                    alternatives: Vec::new(),
                })
                .collect(),
            span: (start, end),
            track_id: track,
            word_seq: seq,
            freq_hz: F,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn copy_alt(
        text: &str,
        start: u64,
        end: u64,
        track: u32,
        seq: u64,
        position: usize,
        alt: char,
        alt_score: f32,
        top_score: f32,
    ) -> Copy {
        let mut c = copy(text, start, end, track, seq);
        c.chars[position].confidence = top_score;
        c.chars[position].alternatives = vec![(alt, alt_score)];
        c
    }

    fn active(_: u32) -> bool {
        true
    }

    /// Separate messages: 100 000 samples and 10 word numbers apart.
    fn feed(lattice: &mut EvidenceLattice, texts: &[&str]) -> EpisodeKey {
        let mut key = None;
        for (i, t) in texts.iter().enumerate() {
            let start = 1_000 + i as u64 * 100_000;
            match lattice.add(0, copy(t, start, start + 400, 1, i as u64 * 10), active) {
                AddOutcome::Added(k) | AddOutcome::Duplicate(k) => key = Some(k),
            }
        }
        key.unwrap()
    }

    fn now(n: usize) -> u64 {
        1_000 + n as u64 * 100_000
    }

    #[test]
    fn complementary_alternatives_fuse_to_the_reading_both_list() {
        let mut l = EvidenceLattice::new(FS);
        l.add(
            0,
            copy_alt("W1AV", 0, 400, 1, 0, 3, 'W', 0.40, 0.45),
            active,
        );
        let AddOutcome::Added(key) = l.add(
            0,
            copy_alt("W1AQ", 100_000, 100_400, 1, 10, 3, 'W', 0.40, 0.45),
            active,
        ) else {
            panic!("added");
        };
        let Verdict::Confirmed(f) = l.evaluate(key, 100_400, 2) else {
            panic!("{:?}", l.evaluate(key, 100_400, 2));
        };
        assert_eq!(f.call, "W1AW");
        assert_eq!((f.reps, f.exact_copies), (2, 0));
        assert!((f.min_margin - 0.78).abs() < 0.01, "{}", f.min_margin);
    }

    #[test]
    fn top1_damage_at_different_positions_fuses_by_majority() {
        let mut l = EvidenceLattice::new(FS);
        let key = feed(&mut l, &["W1AV", "W1EW"]);
        assert_eq!(l.evaluate(key, now(2), 2), Verdict::Unknown { position: 2 });
        let key = feed(&mut EvidenceLattice::new(FS), &["W1AV", "W1EW", "M1AW"]);
        let mut l = EvidenceLattice::new(FS);
        let k2 = feed(&mut l, &["W1AV", "W1EW", "M1AW"]);
        assert_eq!(key, k2);
        let Verdict::Confirmed(f) = l.evaluate(k2, now(3), 2) else {
            panic!("confirmed");
        };
        assert_eq!((f.call.as_str(), f.reps), ("W1AW", 3));
    }

    #[test]
    fn two_disagreeing_top1_copies_leave_the_position_unknown() {
        let mut l = EvidenceLattice::new(FS);
        let key = feed(&mut l, &["W1AV", "W1AQ"]);
        assert_eq!(l.evaluate(key, now(2), 2), Verdict::Unknown { position: 3 });
    }

    #[test]
    fn no_position_is_ever_filled_with_a_glyph_no_copy_read() {
        let sets: &[&[&str]] = &[
            &["W1AV", "W1EW", "M1AW"],
            &["K5ARH", "K5ARX", "K5AQH"],
            &["W1A", "W1AWE", "W1AW"],
            &["AB2KLK", "AB2KLX", "AB2KLK"],
        ];
        for set in sets {
            let mut l = EvidenceLattice::new(FS);
            let key = feed(&mut l, set);
            if let Verdict::Confirmed(f) = l.evaluate(key, now(set.len()), 1) {
                for g in f.call.chars() {
                    assert!(set.iter().any(|t| t.contains(g)), "{g} in {set:?}");
                }
            }
        }
    }

    #[test]
    fn insertions_and_deletions_align() {
        let mut l = EvidenceLattice::new(FS);
        let key = feed(&mut l, &["W1A", "W1AWE", "W1AW"]);
        let Verdict::Confirmed(f) = l.evaluate(key, now(3), 2) else {
            panic!("{:?}", l.evaluate(key, now(3), 2));
        };
        assert_eq!(f.call, "W1AW");
    }

    #[test]
    fn an_erasure_votes_unknown() {
        let mut l = EvidenceLattice::new(FS);
        let key = feed(&mut l, &["W1A?", "W1AV"]);
        assert_eq!(l.evaluate(key, now(2), 2), Verdict::Unknown { position: 3 });
        let mut l = EvidenceLattice::new(FS);
        let key = feed(&mut l, &["W1A?", "W1AW", "W1AW"]);
        let Verdict::Confirmed(f) = l.evaluate(key, now(3), 2) else {
            panic!("confirmed");
        };
        assert_eq!(f.call, "W1AW");
    }

    #[test]
    fn a_replayed_span_is_one_observation() {
        let mut l = EvidenceLattice::new(FS);
        let AddOutcome::Added(key) = l.add(0, copy("W1AW", 100, 400, 1, 1), active) else {
            panic!("added");
        };
        assert_eq!(
            l.add(0, copy("W1AW", 150, 420, 1, 4), active),
            AddOutcome::Duplicate(key)
        );
        assert_eq!(l.repetitions(key, 420), 1);
    }

    #[test]
    fn a_duplicate_changes_neither_the_winner_nor_its_strength() {
        let mut l = EvidenceLattice::new(FS);
        let key = feed(&mut l, &["W1AV", "W1EW", "M1AW"]);
        let before = l.evaluate(key, now(3), 2);
        let again = copy("W1EW", 100_000 + 1_000, 100_000 + 1_400, 2, 0);
        assert_eq!(l.add(0, again, active), AddOutcome::Duplicate(key));
        assert_eq!(l.evaluate(key, now(3), 2), before);
    }

    /// Codex review, PR #500: a duplicate still moves its track's
    /// watermark, as `RepetitionGate::record_span` does, so that track's
    /// later message counts while the first track stays active.
    #[test]
    fn a_duplicate_track_s_later_message_counts() {
        let mut l = EvidenceLattice::new(FS);
        let AddOutcome::Added(key) = l.add(
            0,
            copy_alt("W1AV", 0, 400, 1, 0, 3, 'W', 0.40, 0.45),
            active,
        ) else {
            panic!("added");
        };
        assert_eq!(
            l.add(0, copy("W1AV", 10, 420, 2, 0), active),
            AddOutcome::Duplicate(key)
        );
        assert_eq!(
            l.add(
                0,
                copy_alt("W1AQ", 5_000, 5_400, 2, 3, 3, 'W', 0.40, 0.45),
                active
            ),
            AddOutcome::Added(key)
        );
        assert_eq!(l.repetitions(key, 5_400), 2);
        let mut gate = crate::gate::RepetitionGate::new(FS);
        gate.record_span(1, F, "W1AW", (0, 400), 0, active);
        gate.record_span(2, F, "W1AW", (10, 420), 0, active);
        assert_eq!(gate.record_span(2, F, "W1AW", (5_000, 5_400), 3, active), 2);
        let Verdict::Confirmed(f) = l.evaluate(key, 5_400, 2) else {
            panic!("{:?}", l.evaluate(key, 5_400, 2));
        };
        assert_eq!((f.call.as_str(), f.reps), ("W1AW", 2));
    }

    #[test]
    fn two_utterances_in_one_message_count_once() {
        let mut l = EvidenceLattice::new(FS);
        l.add(0, copy("W1AW", 0, 400, 1, 1), active);
        let AddOutcome::Added(key) = l.add(0, copy("W1AW", 500, 900, 1, 2), active) else {
            panic!("added");
        };
        assert_eq!(l.repetitions(key, 900), 1);
        assert_eq!(
            l.evaluate(key, 900, 2),
            Verdict::Insufficient {
                reps: 1,
                required: 2
            }
        );
    }

    #[test]
    fn a_copy_joins_only_when_close_to_every_member() {
        let mut l = EvidenceLattice::new(FS);
        let a = l.add(0, copy("W1AW", 0, 400, 1, 0), active);
        let b = l.add(0, copy("W1AV", 100_000, 100_400, 1, 10), active);
        assert_eq!(a, b);
        let c = l.add(0, copy("K5ARH", 200_000, 200_400, 1, 20), active);
        assert_ne!(a, c);
        // 2 edits from W1AW, 3 from W1AV... both members must be close.
        let d = l.add(0, copy("W1XWZ", 300_000, 300_400, 1, 30), active);
        assert_ne!(a, d);
        assert_eq!(l.len(), 3);
    }

    #[test]
    fn episodes_are_per_input_and_per_channel() {
        let mut l = EvidenceLattice::new(FS);
        let a = l.add(0, copy("W1AW", 0, 400, 1, 0), active);
        let mut far = copy("W1AW", 100_000, 100_400, 1, 10);
        far.freq_hz = F + 120.0;
        assert_ne!(a, l.add(0, far, active));
        assert_ne!(a, l.add(1, copy("W1AW", 200_000, 200_400, 2, 0), active));
        assert_eq!(l.len(), 3);
    }

    #[test]
    fn insufficient_messages_for_the_tier_are_reported() {
        let mut l = EvidenceLattice::new(FS);
        let key = feed(&mut l, &["W1AW", "W1AW", "W1AV"]);
        assert_eq!(
            l.evaluate(key, now(3), 5),
            Verdict::Insufficient {
                reps: 3,
                required: 5
            }
        );
    }

    #[test]
    fn bounds_hold_and_evictions_are_counted() {
        let mut l = EvidenceLattice::new(FS);
        for i in 0..=MAX_OBSERVATIONS as u64 {
            l.add(
                0,
                copy("W1AW", i * 1_000, i * 1_000 + 400, 1, i * 10),
                active,
            );
        }
        assert_eq!(l.counts().observations_evicted, 1);
        let mut l = EvidenceLattice::new(FS);
        for i in 0..=MAX_EPISODES as u64 {
            let mut c = copy("W1AW", i * 1_000, i * 1_000 + 400, 1, i);
            c.freq_hz = F + i as f64 * 1_000.0;
            l.add(0, c, active);
        }
        assert_eq!(l.len(), MAX_EPISODES);
        assert_eq!(l.counts().episodes_evicted, 1);
    }

    #[test]
    fn sweep_forgets_copies_older_than_the_window() {
        let mut l = EvidenceLattice::new(FS);
        l.add(0, copy("W1AW", 0, 400, 1, 0), active);
        l.sweep(400 + (WINDOW_SECONDS * FS) as u64 + 1);
        assert!(l.is_empty());
    }

    #[test]
    fn the_verdict_does_not_depend_on_arrival_order_of_separate_messages() {
        let mk = |order: &[usize]| {
            let texts = ["W1AV", "W1EW", "M1AW"];
            let mut l = EvidenceLattice::new(FS);
            let mut key = None;
            for &i in order {
                let start = 1_000 + i as u64 * 1_000_000;
                if let AddOutcome::Added(k) = l.add(
                    0,
                    copy(texts[i], start, start + 400, i as u32 + 1, 0),
                    |_| false,
                ) {
                    key = Some(k);
                }
            }
            l.evaluate(key.unwrap(), 2_001_400, 2)
        };
        assert_eq!(mk(&[0, 1, 2]), mk(&[2, 0, 1]));
    }
}
