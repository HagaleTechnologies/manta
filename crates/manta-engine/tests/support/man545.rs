//! MAN-545 test support: the offline evidence-level fuser of the legacy and
//! HSMM engines (item 16 of `docs/DECISIONS/2026-09-09-decoder-recall-research.md`),
//! substitution unit. It builds each engine's station transcript the way the
//! bench joins tracks, aligns the two transcripts with a deterministic
//! Levenshtein alignment, and rewrites legacy `CharDecoded` events at
//! substitution columns under one of the pre-registered policies. It never
//! touches the live decode path: `manta run`/`manta bench` run one decoder.
//!
//! Item 16's invariants hold for every policy (ceilings included):
//! (a) fusing a branch with a byte-identical copy of itself changes nothing
//! (`oracle-legacy-alts` reads no second branch, so it has none to copy);
//! (b) alignment never erases an unknown span: a legacy `?` is never
//! deleted (the substitution policies never change it; a span policy
//! replaces a region holding one only by an HSMM side with the same number
//! of non-space characters), and `?` never replaces a real glyph.
#![allow(dead_code)]

use manta_decode::events::DecoderEvent;
use manta_decode::tree::Glyph;
use std::collections::BTreeMap;

/// The bench's station match window, in Hz (`crates/manta-cli/src/bench.rs`).
pub const MATCH_HZ: f64 = 150.0;
/// The bench's 500 Hz to 2500 Hz shift, as the bench computes it (f32 widened).
pub const RBN_CORR_DB: f64 = 6.989_7_f32 as f64;
/// Calibration bins over [0, 1] of a raw confidence or mass (MAN-545 D6).
pub const BINS: usize = 20;
/// The beam's glyphless survivor (`beam.rs`): an unknown character.
pub const UNKNOWN: char = '?';
/// Joins two tracks' transcripts; never a decoded character.
pub const SEPARATOR: char = '\n';

/// One transcript position: a character, a word space or a track separator.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub c: char,
    pub conf: f32,
    pub alts: Vec<(char, f32)>,
    pub track: Option<u32>,
    /// Index of the source event in the engine's event stream (`None` for a
    /// separator).
    pub ev: Option<usize>,
    /// The source event's `sample_ts` (0 for a separator).
    pub ts: u64,
}

/// Each track's frequency: the last `TrackMeta`, else its `TrackPromoted`.
pub fn track_freqs(events: &[DecoderEvent]) -> BTreeMap<u32, f64> {
    let mut promoted = BTreeMap::new();
    let mut meta = BTreeMap::new();
    for e in events {
        match e {
            DecoderEvent::TrackPromoted {
                track_id, freq_hz, ..
            } => {
                promoted.insert(*track_id, *freq_hz);
            }
            DecoderEvent::TrackMeta {
                track_id, freq_hz, ..
            } => {
                meta.insert(*track_id, *freq_hz);
            }
            _ => {}
        }
    }
    promoted.extend(meta);
    promoted
}

fn text_char(glyph: &Glyph) -> Option<char> {
    glyph.text_char().map(|c| c.to_ascii_uppercase())
}

/// One track's items, built as `events_to_text` builds its text: text
/// characters only (prosigns dropped), a word boundary as a single space,
/// no leading or trailing space.
fn track_items(events: &[DecoderEvent], track: u32) -> Vec<Item> {
    let mut out: Vec<Item> = Vec::new();
    for (i, e) in events.iter().enumerate() {
        match e {
            DecoderEvent::CharDecoded {
                track_id,
                sample_ts,
                glyph,
                confidence,
                alternatives,
            } if *track_id == track => {
                if let Some(c) = text_char(glyph) {
                    out.push(Item {
                        c,
                        conf: *confidence,
                        alts: alternatives
                            .iter()
                            .filter_map(|(g, m)| text_char(g).map(|a| (a, *m)))
                            .collect(),
                        track: Some(track),
                        ev: Some(i),
                        ts: *sample_ts,
                    });
                }
            }
            DecoderEvent::WordBoundary {
                track_id,
                sample_ts,
                confidence,
            } if *track_id == track && out.last().is_some_and(|it| it.c != ' ') => {
                out.push(Item {
                    c: ' ',
                    conf: *confidence,
                    alts: Vec::new(),
                    track: Some(track),
                    ev: Some(i),
                    ts: *sample_ts,
                });
            }
            _ => {}
        }
    }
    if out.last().is_some_and(|it| it.c == ' ') {
        out.pop();
    }
    out
}

/// A station's transcript: the tracks within [`MATCH_HZ`] of `f_hz`, in
/// ascending track id, joined by one separator (`manta_testkit::cer::joined_cer`).
pub fn station_items(events: &[DecoderEvent], freqs: &BTreeMap<u32, f64>, f_hz: f64) -> Vec<Item> {
    let mut out = Vec::new();
    for (&track, &freq) in freqs {
        if (freq - f_hz).abs() > MATCH_HZ {
            continue;
        }
        let items = track_items(events, track);
        if items.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(Item {
                c: SEPARATOR,
                conf: 1.0,
                alts: Vec::new(),
                track: None,
                ev: None,
                ts: 0,
            });
        }
        out.extend(items);
    }
    out
}

/// Uppercase with single spaces, trimmed: the truth's alignment form.
pub fn normalize(text: &str) -> Vec<char> {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_uppercase()
        .chars()
        .collect()
}

pub fn chars(items: &[Item]) -> Vec<char> {
    items.iter().map(|it| it.c).collect()
}

/// One alignment column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Match,
    Sub,
    /// A character only the left (legacy) side has.
    LeftOnly,
    /// A character only the right side has.
    RightOnly,
}

/// Levenshtein alignment with a fixed tie order on the traceback (diagonal,
/// then left-only, then right-only), so equal-cost pairs always align the
/// same way. Columns carry the left and right indices they cover.
pub fn align(left: &[char], right: &[char]) -> Vec<(Op, Option<usize>, Option<usize>)> {
    let (n, m) = (left.len(), right.len());
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    d[0] = (0..=m).collect();
    for i in 1..=n {
        for j in 1..=m {
            let sub = d[i - 1][j - 1] + usize::from(left[i - 1] != right[j - 1]);
            d[i][j] = sub.min(d[i - 1][j] + 1).min(d[i][j - 1] + 1);
        }
    }
    let mut out = Vec::new();
    let (mut i, mut j) = (n, m);
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && d[i][j] == d[i - 1][j - 1] + usize::from(left[i - 1] != right[j - 1]) {
            let op = if left[i - 1] == right[j - 1] {
                Op::Match
            } else {
                Op::Sub
            };
            out.push((op, Some(i - 1), Some(j - 1)));
            i -= 1;
            j -= 1;
        } else if i > 0 && d[i][j] == d[i - 1][j] + 1 {
            out.push((Op::LeftOnly, Some(i - 1), None));
            i -= 1;
        } else {
            out.push((Op::RightOnly, None, Some(j - 1)));
            j -= 1;
        }
    }
    out.reverse();
    out
}

/// Which engine a calibration table belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Engine {
    Legacy,
    Hsmm,
}

/// The calibration bin of a raw confidence or mass: NaN or negative to 0,
/// 1.0 and above to the last bin.
pub fn bin(x: f32) -> usize {
    if x.is_nan() || x < 0.0 {
        return 0;
    }
    ((x as f64 * BINS as f64).floor() as usize).min(BINS - 1)
}

/// One truth label: a winner character, a winner space or an alternative,
/// its raw value and whether it was right.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Label {
    WinChar { conf: f32, right: bool },
    WinSpace { conf: f32, right: bool },
    Alt { mass: f32, right: bool },
}

/// Labels one engine's station items against the truth: a winner is right
/// when it aligns to the truth as a match; an alternative is right when its
/// item aligns to a truth position holding the alternative's glyph.
pub fn truth_labels(truth: &[char], items: &[Item]) -> Vec<Label> {
    let mut out = Vec::new();
    for (op, li, ti) in align(&chars(items), truth) {
        let Some(li) = li else { continue };
        let it = &items[li];
        if it.c == SEPARATOR {
            continue;
        }
        let right = op == Op::Match;
        if it.c == ' ' {
            out.push(Label::WinSpace {
                conf: it.conf,
                right,
            });
            continue;
        }
        out.push(Label::WinChar {
            conf: it.conf,
            right,
        });
        if let Some(ti) = ti {
            for &(a, mass) in &it.alts {
                out.push(Label::Alt {
                    mass,
                    right: a == truth[ti],
                });
            }
        }
    }
    out
}

/// Per engine: winner characters, winner spaces and alternatives, each as
/// `(k right, n)` per bin; probabilities are Laplace-smoothed, (k+1)/(n+2).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Calibration {
    pub tables: BTreeMap<Engine, [[(u64, u64); BINS]; 3]>,
}

impl Calibration {
    /// Fits from `(trial, engine, label)` rows, leaving out `exclude_trial`
    /// (cross-fitting: trial t's tables never see trial t's truth).
    pub fn fit(labels: &[(u32, Engine, Label)], exclude_trial: Option<u32>) -> Self {
        let mut tables: BTreeMap<Engine, [[(u64, u64); BINS]; 3]> = BTreeMap::new();
        for &(trial, engine, label) in labels {
            if Some(trial) == exclude_trial {
                continue;
            }
            let (table, x, right) = match label {
                Label::WinChar { conf, right } => (0, conf, right),
                Label::WinSpace { conf, right } => (1, conf, right),
                Label::Alt { mass, right } => (2, mass, right),
            };
            let cell = &mut tables.entry(engine).or_insert([[(0, 0); BINS]; 3])[table][bin(x)];
            cell.0 += u64::from(right);
            cell.1 += 1;
        }
        Calibration { tables }
    }

    fn p(&self, engine: Engine, table: usize, x: f32) -> f64 {
        let (k, n) = self
            .tables
            .get(&engine)
            .map_or((0, 0), |t| t[table][bin(x)]);
        (k as f64 + 1.0) / (n as f64 + 2.0)
    }

    /// Calibrated probability that a winner item is right.
    pub fn p_win(&self, engine: Engine, item: &Item) -> f64 {
        self.p(engine, if item.c == ' ' { 1 } else { 0 }, item.conf)
    }

    /// Calibrated probability that an alternative of this mass is right.
    pub fn p_alt(&self, engine: Engine, mass: f32) -> f64 {
        self.p(engine, 2, mass)
    }
}

/// The pre-registered policies (MAN-545 D4). Every one keeps legacy's
/// tracks, track ids and non-text events. The substitution policies change
/// only the glyph and confidence of legacy characters; the span policies
/// replace a whole disagreement region (D5). The duplicate-branch controls
/// (`dup-sub`, `dup-span`) are `CalSub`/`CalSpan` given legacy as both
/// branches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Runs the fusion path and changes nothing.
    Noop,
    /// Ceiling: a legacy substitution against the truth takes a legacy
    /// runner-up equal to the truth glyph. No HSMM.
    OracleLegacyAlts,
    /// Ceiling: at a legacy-vs-HSMM substitution column, the truth glyph if
    /// any proposal (both winners, both engines' alternatives) is it.
    OracleSub,
    /// Realizable: at a substitution column, the glyph with the highest
    /// summed calibrated probability; ties keep legacy.
    CalSub,
    /// Ceiling: per region, HSMM's side when it brings the station's
    /// transcript strictly closer to the truth; ties keep legacy.
    OracleSpan,
    /// Realizable: per region, HSMM's side when its mean calibrated log-odds
    /// is strictly higher than legacy's; an empty side scores minus the other.
    CalSpan,
}

/// One station's inputs to a fuser.
pub struct StationInput<'a> {
    pub legacy: &'a [Item],
    pub hsmm: &'a [Item],
    /// The keyed truth (normalized); only the oracle ceilings read it.
    pub truth: &'a [char],
}

/// A replacement for one legacy character event.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Replace {
    pub ev: usize,
    pub c: char,
    pub conf: f32,
}

fn proposals(item: &Item) -> impl Iterator<Item = (char, f32)> + '_ {
    std::iter::once((item.c, item.conf)).chain(item.alts.iter().copied())
}

/// D7b: a column may change legacy's glyph only when both glyphs are real
/// characters (never a space, a separator or `?`). The new event needs a
/// finite positive confidence, the validator's evidence predicate.
fn replaceable(old: char, new: char, conf: f32) -> bool {
    let real = |c: char| c != ' ' && c != SEPARATOR && c != UNKNOWN;
    real(old) && real(new) && old != new && conf.is_finite() && conf > 0.0
}

/// The legacy character events one station's policy rewrites.
pub fn station_replacements(
    policy: Policy,
    st: &StationInput,
    cal: Option<&Calibration>,
) -> Vec<Replace> {
    let mut out = Vec::new();
    match policy {
        Policy::Noop | Policy::OracleSpan | Policy::CalSpan => {}
        Policy::OracleLegacyAlts => {
            for (op, li, ti) in align(&chars(st.legacy), st.truth) {
                if op != Op::Sub {
                    continue;
                }
                let (it, truth) = (&st.legacy[li.unwrap()], st.truth[ti.unwrap()]);
                if let Some(&(_, mass)) = it.alts.iter().find(|(a, _)| *a == truth) {
                    if replaceable(it.c, truth, mass) {
                        out.push(Replace {
                            ev: it.ev.unwrap(),
                            c: truth,
                            conf: mass,
                        });
                    }
                }
            }
        }
        Policy::OracleSub => {
            let truth_of: BTreeMap<usize, char> = align(&chars(st.legacy), st.truth)
                .into_iter()
                .filter_map(|(_, li, ti)| Some((li?, st.truth[ti?])))
                .collect();
            for (op, li, hi) in align(&chars(st.legacy), &chars(st.hsmm)) {
                if op != Op::Sub {
                    continue;
                }
                let (l, h) = (&st.legacy[li.unwrap()], &st.hsmm[hi.unwrap()]);
                let Some(&truth) = truth_of.get(&li.unwrap()) else {
                    continue;
                };
                let found = proposals(l).chain(proposals(h)).find(|(g, _)| *g == truth);
                if let Some((g, conf)) = found {
                    if replaceable(l.c, g, conf) {
                        out.push(Replace {
                            ev: l.ev.unwrap(),
                            c: g,
                            conf,
                        });
                    }
                }
            }
        }
        Policy::CalSub => {
            let flat = Calibration::default();
            let cal = cal.unwrap_or(&flat);
            for (op, li, hi) in align(&chars(st.legacy), &chars(st.hsmm)) {
                if op != Op::Sub {
                    continue;
                }
                let (l, h) = (&st.legacy[li.unwrap()], &st.hsmm[hi.unwrap()]);
                if let Some((g, conf)) = cal_sub_pick(cal, l, h) {
                    if replaceable(l.c, g, conf) {
                        out.push(Replace {
                            ev: l.ev.unwrap(),
                            c: g,
                            conf,
                        });
                    }
                }
            }
        }
    }
    out
}

/// One engine's calibrated probability for its own proposal of `g`, with
/// the proposal's raw value: the winner table when `g` is its winner, the
/// alternatives table when `g` is one of its alternatives, else 0.
fn engine_support(cal: &Calibration, engine: Engine, item: &Item, g: char) -> (f64, f32) {
    if item.c == g {
        return (cal.p_win(engine, item), item.conf);
    }
    item.alts
        .iter()
        .filter(|(a, _)| *a == g)
        .map(|&(_, m)| (cal.p_alt(engine, m), m))
        .fold((0.0, 0.0), |best, x| if x.0 > best.0 { x } else { best })
}

/// `cal-sub` at one column: the glyph with the highest summed calibrated
/// support; ties keep legacy. Returns the new glyph and the raw confidence
/// of its best-calibrated proposal, or `None` to keep legacy. A duplicated
/// branch doubles every support, so the argmax and the confidence stay.
pub fn cal_sub_pick(cal: &Calibration, l: &Item, h: &Item) -> Option<(char, f32)> {
    let support = |g: char| {
        let (pl, cl) = engine_support(cal, Engine::Legacy, l, g);
        let (ph, ch) = engine_support(cal, Engine::Hsmm, h, g);
        (pl + ph, if pl >= ph { cl } else { ch })
    };
    let keep = support(l.c).0;
    let mut best: Option<(char, f64, f32)> = None;
    for (g, _) in proposals(l).chain(proposals(h)) {
        if g == l.c || g == UNKNOWN {
            continue;
        }
        let (s, conf) = support(g);
        if s > keep && best.is_none_or(|(_, bs, _)| s > bs) {
            best = Some((g, s, conf));
        }
    }
    best.map(|(g, _, conf)| (g, conf))
}

/// Applies replacements to a legacy event stream: glyph and confidence
/// only, so tracks, timestamps and every other event stay as they were.
pub fn apply(events: &[DecoderEvent], replacements: &[Replace]) -> Vec<DecoderEvent> {
    let by_ev: BTreeMap<usize, &Replace> = replacements.iter().map(|r| (r.ev, r)).collect();
    events
        .iter()
        .enumerate()
        .map(|(i, e)| match (e, by_ev.get(&i)) {
            (
                DecoderEvent::CharDecoded {
                    track_id,
                    sample_ts,
                    ..
                },
                Some(r),
            ) => DecoderEvent::char_decoded(*track_id, *sample_ts, Glyph::Char(r.c), r.conf),
            _ => e.clone(),
        })
        .collect()
}

/// Fuses one recording: every station's replacements, applied once (a
/// character two stations share takes the first station's choice).
pub fn fuse(
    policy: Policy,
    legacy_events: &[DecoderEvent],
    stations: &[StationInput],
    cal: Option<&Calibration>,
) -> Vec<DecoderEvent> {
    if matches!(policy, Policy::OracleSpan | Policy::CalSpan) {
        let mut edits: Vec<SpanEdit> = Vec::new();
        for st in stations {
            for e in station_span_edits(policy, st, cal) {
                // Two edits conflict when they remove a shared event, when
                // one anchors to an event the other removes, or when both
                // insert on the same side of one anchor.
                let conflicts = |x: &SpanEdit| {
                    x.remove.iter().any(|r| e.remove.contains(r))
                        || x.remove.contains(&e.at)
                        || e.remove.contains(&x.at)
                        || (x.at == e.at && x.before == e.before)
                };
                if !edits.iter().any(conflicts) {
                    edits.push(e);
                }
            }
        }
        return apply_spans(legacy_events, &edits);
    }
    let mut all: Vec<Replace> = Vec::new();
    for st in stations {
        for r in station_replacements(policy, st, cal) {
            if !all.iter().any(|x| x.ev == r.ev) {
                all.push(r);
            }
        }
    }
    apply(legacy_events, &all)
}

/// One disagreement region (D5): a maximal run of non-matching columns in
/// the legacy-vs-HSMM alignment, as item indices on each side, with the
/// legacy items just before and just after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    pub l: Vec<usize>,
    pub h: Vec<usize>,
    pub before: Option<usize>,
    pub after: Option<usize>,
}

pub fn regions(left: &[Item], right: &[Item]) -> Vec<Region> {
    let mut out = Vec::new();
    let mut last_l: Option<usize> = None;
    let mut cur: Option<Region> = None;
    for (op, li, hi) in align(&chars(left), &chars(right)) {
        if op == Op::Match {
            if let Some(mut r) = cur.take() {
                r.after = li;
                out.push(r);
            }
            last_l = li;
            continue;
        }
        let r = cur.get_or_insert_with(|| Region {
            l: Vec::new(),
            h: Vec::new(),
            before: last_l,
            after: None,
        });
        if let Some(i) = li {
            r.l.push(i);
        }
        if let Some(j) = hi {
            r.h.push(j);
        }
        if li.is_some() {
            last_l = li;
        }
    }
    out.extend(cur);
    out
}

/// Replaces one region's legacy events with HSMM's items on a legacy track.
#[derive(Debug, Clone, PartialEq)]
pub struct SpanEdit {
    /// Legacy event indices removed.
    pub remove: Vec<usize>,
    /// The legacy event the insertion is anchored to.
    pub at: usize,
    /// Insert before `at` (else after it).
    pub before: bool,
    pub track: u32,
    /// HSMM's items: (character or ' ', raw confidence, HSMM `sample_ts`).
    pub insert: Vec<(char, f32, u64)>,
}

fn non_space(items: &[&Item]) -> usize {
    items.iter().filter(|it| it.c != ' ').count()
}

/// The D5/D7 checks every span policy applies before any replacement: no
/// separator on either side; HSMM's side holds no `?` and only finite
/// positive confidences; a legacy `?` is kept unless HSMM's side has the
/// same number of non-space characters; an empty legacy side needs a
/// legacy anchor next to it. Returns the edit, or `None` to keep legacy.
fn span_edit(st: &StationInput, r: &Region) -> Option<SpanEdit> {
    let l: Vec<&Item> = r.l.iter().map(|&i| &st.legacy[i]).collect();
    let h: Vec<&Item> = r.h.iter().map(|&j| &st.hsmm[j]).collect();
    if l.iter().chain(&h).any(|it| it.c == SEPARATOR) {
        return None;
    }
    if h.iter()
        .any(|it| it.c == UNKNOWN || (it.c != ' ' && !(it.conf.is_finite() && it.conf > 0.0)))
    {
        return None;
    }
    if l.iter().any(|it| it.c == UNKNOWN) && non_space(&l) != non_space(&h) {
        return None;
    }
    let real = |i: Option<usize>| i.map(|i| &st.legacy[i]).filter(|it| it.c != SEPARATOR);
    let (at, before, track) = match l.first() {
        Some(first) => (first.ev?, true, first.track?),
        None => match (real(r.before), real(r.after)) {
            (Some(b), _) => (b.ev?, false, b.track?),
            (None, Some(a)) => (a.ev?, true, a.track?),
            (None, None) => return None,
        },
    };
    Some(SpanEdit {
        remove: l.iter().filter_map(|it| it.ev).collect(),
        at,
        before,
        track,
        insert: h.iter().map(|it| (it.c, it.conf, it.ts)).collect(),
    })
}

fn logit(p: f64) -> f64 {
    (p / (1.0 - p)).ln()
}

/// `cal-span`'s score of one side: the mean calibrated log-odds of its
/// items (winner tables); `None` for an empty side.
fn side_score(cal: &Calibration, engine: Engine, items: &[&Item]) -> Option<f64> {
    if items.is_empty() {
        return None;
    }
    let sum: f64 = items.iter().map(|it| logit(cal.p_win(engine, it))).sum();
    Some(sum / items.len() as f64)
}

/// Whether `cal-span` takes HSMM's side of a region.
pub fn cal_span_takes_hsmm(cal: &Calibration, l: &[&Item], h: &[&Item]) -> bool {
    match (
        side_score(cal, Engine::Legacy, l),
        side_score(cal, Engine::Hsmm, h),
    ) {
        (Some(sl), Some(sh)) => sh > sl,
        (Some(sl), None) => -sl > sl,
        (None, Some(sh)) => sh > -sh,
        (None, None) => false,
    }
}

/// The station transcript with one region's legacy side swapped for HSMM's.
fn with_region(st: &StationInput, r: &Region) -> Vec<char> {
    let first = r.l.first().copied().or(r.after).unwrap_or(st.legacy.len());
    let mut out: Vec<char> = Vec::new();
    let mut inserted = false;
    for (i, it) in st.legacy.iter().enumerate() {
        if !inserted && i == first {
            out.extend(r.h.iter().map(|&j| st.hsmm[j].c));
            inserted = true;
        }
        if !r.l.contains(&i) {
            out.push(it.c);
        }
    }
    if !inserted {
        out.extend(r.h.iter().map(|&j| st.hsmm[j].c));
    }
    out
}

fn edit_distance(a: &[char], b: &[char]) -> usize {
    align(a, b)
        .iter()
        .filter(|(op, _, _)| *op != Op::Match)
        .count()
}

/// The region edits one station's span policy makes.
pub fn station_span_edits(
    policy: Policy,
    st: &StationInput,
    cal: Option<&Calibration>,
) -> Vec<SpanEdit> {
    let flat = Calibration::default();
    let cal = cal.unwrap_or(&flat);
    let base = edit_distance(&chars(st.legacy), st.truth);
    let mut out = Vec::new();
    for r in regions(st.legacy, st.hsmm) {
        let Some(edit) = span_edit(st, &r) else {
            continue;
        };
        let take = match policy {
            Policy::OracleSpan => edit_distance(&with_region(st, &r), st.truth) < base,
            Policy::CalSpan => {
                let l: Vec<&Item> = r.l.iter().map(|&i| &st.legacy[i]).collect();
                let h: Vec<&Item> = r.h.iter().map(|&j| &st.hsmm[j]).collect();
                cal_span_takes_hsmm(cal, &l, &h)
            }
            _ => false,
        };
        if take {
            out.push(edit);
        }
    }
    out
}

/// Applies region edits to a legacy event stream. Inserted characters keep
/// HSMM's glyph and raw confidence; inserted spaces become `WordBoundary`
/// events with confidence 1.0. Each inserted event takes HSMM's
/// `sample_ts` clamped between its track's neighbouring text events, so
/// every track's text timestamps stay non-decreasing.
pub fn apply_spans(events: &[DecoderEvent], edits: &[SpanEdit]) -> Vec<DecoderEvent> {
    let removed: std::collections::BTreeSet<usize> = edits
        .iter()
        .flat_map(|e| e.remove.iter().copied())
        .collect();
    let mut out: Vec<(DecoderEvent, bool)> = Vec::new();
    let insert = |out: &mut Vec<(DecoderEvent, bool)>, e: &SpanEdit| {
        for &(c, conf, ts) in &e.insert {
            let ev = if c == ' ' {
                DecoderEvent::word_boundary(e.track, ts)
            } else {
                DecoderEvent::char_decoded(e.track, ts, Glyph::Char(c), conf)
            };
            out.push((ev, true));
        }
    };
    for (i, ev) in events.iter().enumerate() {
        for e in edits.iter().filter(|e| e.at == i && e.before) {
            insert(&mut out, e);
        }
        if !removed.contains(&i) {
            out.push((ev.clone(), false));
        }
        for e in edits.iter().filter(|e| e.at == i && !e.before) {
            insert(&mut out, e);
        }
    }
    clamp_inserted(out)
}

fn text_ts(ev: &mut DecoderEvent) -> Option<(u32, &mut u64)> {
    match ev {
        DecoderEvent::CharDecoded {
            track_id,
            sample_ts,
            ..
        }
        | DecoderEvent::WordBoundary {
            track_id,
            sample_ts,
            ..
        } => Some((*track_id, sample_ts)),
        _ => None,
    }
}

fn clamp_inserted(mut out: Vec<(DecoderEvent, bool)>) -> Vec<DecoderEvent> {
    // The next kept text event's timestamp on the same track, per position.
    let mut next: BTreeMap<u32, u64> = BTreeMap::new();
    let mut hi = vec![u64::MAX; out.len()];
    for (k, (ev, inserted)) in out.iter_mut().enumerate().rev() {
        if let Some((track, ts)) = text_ts(ev) {
            hi[k] = next.get(&track).copied().unwrap_or(u64::MAX);
            if !*inserted {
                next.insert(track, *ts);
            }
        }
    }
    let mut last: BTreeMap<u32, u64> = BTreeMap::new();
    for (k, (ev, inserted)) in out.iter_mut().enumerate() {
        if let Some((track, ts)) = text_ts(ev) {
            let lo = last.get(&track).copied().unwrap_or(0);
            if *inserted {
                *ts = (*ts).clamp(lo, hi[k].max(lo));
            }
            last.insert(track, (*ts).max(lo));
        }
    }
    out.into_iter().map(|(ev, _)| ev).collect()
}
