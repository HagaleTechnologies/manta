//! MAN-301 grading engine: turns manta's spots plus RBN reports into
//! `spotGrade` records. Pure and deterministic: no I/O and no wall clock --
//! time comes in as Unix seconds -- so the live comparator ([`Grader`]) and
//! the offline `manta grade` replay ([`grade_batch`]) are thin drivers
//! around one implementation, and the same inputs grade byte-identically on
//! both paths. Iteration is over `BTreeMap`/`BTreeSet` only (SPEC §6
//! determinism).
//!
//! The correlation key is (call, band), not a frequency bin: a same-band
//! report 1.4 kHz away is a frequency *disagreement*, not a miss, and a
//! report on another band is `elsewhere`. `none`/`elsewhere` are only
//! claimed when the RBN feed covered the whole window; otherwise the grade
//! is `unknown`. See `docs/DECISIONS/2026-10-11-man301-live-rbn-comparison.md`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};

/// Report-buffer cap: about a contest peak's `2W + 60 s` of RBN CW traffic,
/// with headroom. Oldest reports go first; evicting one raises the coverage
/// horizon so a spot whose window reaches back past it grades `unknown`
/// rather than a false `none`.
pub const MAX_BUFFERED_REPORTS: usize = 200_000;
/// Pending-spot cap: the same bound as `bus.rs`'s `MAX_OCCURRENCE_ENTRIES`.
/// Spots submitted past it are counted ([`Grader::ungraded_full`]), not
/// graded.
pub const MAX_PENDING_SPOTS: usize = 20_000;
/// Report retention is `2 * window_secs + RETENTION_SLACK_SECS` behind the
/// close time: enough for a spot submitted (and graded) a little late.
pub const RETENTION_SLACK_SECS: i64 = 60;
/// How long past its window close [`Grader::close_due`] holds a spot whose
/// window only the open feed span covers, while no report has arrived at
/// or after the close: the feed's idle timeout plus 10 s of scheduling
/// margin. By then a feed that went silent before the close has been
/// declared down at its last line, so the silent stretch grades `unknown`
/// rather than `none`/`elsewhere`; a feed still up then was delivering
/// (non-CW) lines.
pub const FEED_SILENCE_GRACE_SECS: i64 = super::feed::FEED_IDLE_TIMEOUT.as_secs() as i64 + 10;

#[derive(Debug, Clone, PartialEq)]
pub struct GradeConfig {
    pub window_secs: i64,
    pub freq_tol_hz: f64,
    pub speed_tol_wpm: f64,
    pub speed_tol_pct: f64,
    /// Empty: every spotter counts.
    pub spotters: BTreeSet<String>,
    /// Compared after [`normalize_spotter`]; callers add their own call with
    /// and without SSID.
    pub exclude_spotters: BTreeSet<String>,
}

impl Default for GradeConfig {
    fn default() -> Self {
        Self {
            window_secs: 180,
            freq_tol_hz: 500.0,
            speed_tol_wpm: 3.0,
            speed_tol_pct: 20.0,
            spotters: BTreeSet::new(),
            exclude_spotters: BTreeSet::new(),
        }
    }
}

/// One manta spot, as its SpotMessage carries it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct GradeInput {
    pub spot_id: String,
    pub call: String,
    pub time: i64,
    pub freq_hz: i64,
    pub wpm: Option<i32>,
}

/// One CW report from RBN: live (arrival time) or archive (`date` column).
#[derive(Debug, Clone, PartialEq)]
pub struct RbnReport {
    pub spotter: String,
    pub call: String,
    pub freq_hz: f64,
    pub wpm: Option<f64>,
    pub time: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Grade {
    Match,
    Disagree,
    Elsewhere,
    None,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Disagreement {
    Frequency,
    Speed,
}

/// The `spotGrade` JSON record. Field order is the wire order. It
/// deliberately has no `dxCall`/`frequency`/`timestamp` key, so today's
/// `field-node.py`/`shadow-compare.py` never mistake it for a spot.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpotGrade {
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    pub spot_id: String,
    pub call: String,
    pub band: &'static str,
    pub spot_time: i64,
    pub window_close: i64,
    pub window_s: i64,
    pub grade: Grade,
    pub disagreements: Vec<Disagreement>,
    pub manta_freq_hz: i64,
    pub manta_wpm: Option<i32>,
    pub rbn_modal_freq_hz: Option<i64>,
    pub rbn_median_wpm: Option<f64>,
    pub rbn_skimmers: u32,
    pub rbn_reports: u32,
    pub freq_offset_hz: Option<i64>,
    pub rbn_bands: Vec<&'static str>,
    pub spotter_filter: Vec<String>,
}

/// `kind` of every grade record; spot lines have no `kind` key.
pub const SPOT_GRADE_KIND: &str = "spotGrade";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Submit {
    Queued,
    Full,
}

/// An RBN spotter call as the comparison keys it: trimmed, uppercase, and
/// without the `-#` suffix RBN appends to skimmer spotters (as
/// `shadow-compare.py` strips it), so `K5TR-#` matches `spotters = ["K5TR"]`.
/// An SSID such as `W3XYZ-1` is kept: it names a different node.
pub fn normalize_spotter(s: &str) -> String {
    let upper = s.trim().to_ascii_uppercase();
    match upper.strip_suffix("-#") {
        Some(base) => base.to_string(),
        None => upper,
    }
}

/// The spotter filter and exclusion list, normalised once.
struct SpotterFilter {
    only: BTreeSet<String>,
    exclude: BTreeSet<String>,
}

impl SpotterFilter {
    fn new(cfg: &GradeConfig) -> Self {
        Self {
            only: cfg.spotters.iter().map(|s| normalize_spotter(s)).collect(),
            exclude: cfg
                .exclude_spotters
                .iter()
                .map(|s| normalize_spotter(s))
                .collect(),
        }
    }

    fn admits_normalized(&self, spotter: &str) -> bool {
        !self.exclude.contains(spotter) && (self.only.is_empty() || self.only.contains(spotter))
    }

    fn admits(&self, spotter: &str) -> bool {
        self.admits_normalized(&normalize_spotter(spotter))
    }

    /// Normalises the report's spotter and call, or `None` when the filter
    /// rejects it. Applied before anything is stored or aggregated.
    fn admit(&self, mut r: RbnReport) -> Option<RbnReport> {
        r.spotter = normalize_spotter(&r.spotter);
        if !self.admits_normalized(&r.spotter) {
            return None;
        }
        r.call = r.call.trim().to_ascii_uppercase();
        Some(r)
    }
}

/// RBN reports by uppercase call (`BTreeMap` for deterministic iteration),
/// each call's reports in insertion order, plus a global insertion-ordered
/// queue for pruning and the cap. Live reports arrive in time order, so
/// insertion order is time order.
#[derive(Debug, Default)]
struct ReportIndex {
    by_call: BTreeMap<String, VecDeque<RbnReport>>,
    order: VecDeque<(i64, String)>,
}

impl ReportIndex {
    fn insert(&mut self, r: RbnReport) {
        self.order.push_back((r.time, r.call.clone()));
        self.by_call.entry(r.call.clone()).or_default().push_back(r);
    }

    fn len(&self) -> usize {
        self.order.len()
    }

    /// Drops the oldest-inserted report; returns its time.
    fn pop_oldest(&mut self) -> Option<i64> {
        let (time, call) = self.order.pop_front()?;
        if let Some(q) = self.by_call.get_mut(&call) {
            q.pop_front();
            if q.is_empty() {
                self.by_call.remove(&call);
            }
        }
        Some(time)
    }

    fn prune_before(&mut self, cutoff: i64) {
        while self.order.front().is_some_and(|(t, _)| *t < cutoff) {
            self.pop_oldest();
        }
    }

    fn candidates(&self, call: &str, lo: i64, hi: i64) -> Vec<&RbnReport> {
        self.by_call
            .get(call)
            .map(|q| q.iter().filter(|r| r.time >= lo && r.time <= hi).collect())
            .unwrap_or_default()
    }
}

/// When the RBN feed was known to be up: closed `(from, to)` spans, the
/// currently open span, and the report-buffer `horizon` below which reports
/// may have been pruned (so data there is gone, as if the feed were down).
/// `seen` is the latest time the feed showed it was alive (its `Up` or a
/// report's arrival).
#[derive(Debug, Clone)]
struct Coverage {
    closed: Vec<(i64, i64)>,
    open: Option<i64>,
    horizon: i64,
    seen: i64,
}

impl Coverage {
    fn empty() -> Self {
        Self {
            closed: Vec::new(),
            open: None,
            horizon: i64::MIN,
            seen: i64::MIN,
        }
    }

    fn up(&mut self, at: i64) {
        if self.open.is_none() {
            self.open = Some(at);
        }
        self.alive(at);
    }

    fn alive(&mut self, at: i64) {
        self.seen = self.seen.max(at);
    }

    fn down(&mut self, at: i64) {
        if let Some(from) = self.open.take() {
            self.closed.push((from, at.max(from)));
        }
    }

    fn raise_horizon(&mut self, to: i64) {
        self.horizon = self.horizon.max(to);
        let horizon = self.horizon;
        // A span ending below the horizon can no longer cover any window.
        self.closed.retain(|&(_, to)| to >= horizon);
    }

    /// `[lo, hi]` lies inside one span and nothing in it was pruned.
    fn covers(&self, lo: i64, hi: i64) -> bool {
        lo >= self.horizon
            && (self.closed.iter().any(|&(a, b)| a <= lo && hi <= b)
                || self.open.is_some_and(|a| a <= lo))
    }

    /// Only the open span would cover `[lo, hi]`, but the feed has not
    /// shown it was alive at `hi`: it may have gone silent inside the
    /// window, with its idle timeout not yet fired.
    fn unconfirmed(&self, lo: i64, hi: i64) -> bool {
        lo >= self.horizon && self.open.is_some_and(|a| a <= lo) && self.seen < hi
    }
}

/// Index of `band` in the band table (`band::allocations` order); unknown
/// bands sort last.
fn band_rank(band: &str) -> usize {
    let table = crate::band::allocations();
    table
        .iter()
        .position(|(name, _, _)| *name == band)
        .unwrap_or(table.len())
}

fn band_at_rank(rank: usize) -> &'static str {
    crate::band::allocations()
        .get(rank)
        .map(|(name, _, _)| *name)
        .unwrap_or("unknown")
}

/// Most common report frequency after rounding to RBN's 100 Hz resolution.
/// Ties go to the value nearest manta's frequency, then the lower one.
fn modal_freq_hz(freqs: &[f64], manta_hz: i64) -> Option<i64> {
    let mut counts: BTreeMap<i64, u32> = BTreeMap::new();
    for f in freqs {
        *counts.entry((f / 100.0).round() as i64 * 100).or_default() += 1;
    }
    // BTreeMap iterates ascending, and `min_by_key` keeps the first of
    // equal keys, so an exact tie on count and distance goes to the lower.
    counts
        .into_iter()
        .min_by_key(|&(v, n)| (std::cmp::Reverse(n), (v - manta_hz).unsigned_abs()))
        .map(|(v, _)| v)
}

/// Median; an even count averages the middle two (shadow-compare's
/// `_median`).
fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    Some(if values.len() % 2 == 1 {
        values[mid]
    } else {
        (values[mid - 1] + values[mid]) / 2.0
    })
}

fn window_of(spot: &GradeInput, cfg: &GradeConfig) -> (i64, i64) {
    (spot.time - cfg.window_secs, spot.time + cfg.window_secs)
}

/// Grades one spot from `candidates` (any superset of the relevant reports:
/// they are re-filtered here by call, window and spotter filter). `covered`
/// says the RBN feed was up for the spot's whole window.
pub fn grade_spot(
    spot: &GradeInput,
    candidates: &[&RbnReport],
    covered: bool,
    cfg: &GradeConfig,
) -> SpotGrade {
    grade_with(spot, candidates, covered, cfg, &SpotterFilter::new(cfg))
}

fn grade_with(
    spot: &GradeInput,
    candidates: &[&RbnReport],
    covered: bool,
    cfg: &GradeConfig,
    filter: &SpotterFilter,
) -> SpotGrade {
    let call = spot.call.trim().to_ascii_uppercase();
    let (lo, hi) = window_of(spot, cfg);
    let band = crate::band::band_for_freq_hz(spot.freq_hz as f64);

    let mut bands: BTreeSet<usize> = BTreeSet::new();
    let mut same: Vec<&RbnReport> = Vec::new();
    for &r in candidates {
        if r.time < lo
            || r.time > hi
            || !r.call.trim().eq_ignore_ascii_case(&call)
            || !filter.admits(&r.spotter)
        {
            continue;
        }
        let report_band = crate::band::band_for_freq_hz(r.freq_hz);
        bands.insert(band_rank(report_band));
        // Outside every band, "same band" falls back to the tolerance.
        let same_band = if band == "unknown" {
            (r.freq_hz - spot.freq_hz as f64).abs() <= cfg.freq_tol_hz
        } else {
            report_band == band
        };
        if same_band {
            same.push(r);
        }
    }

    let mut record = SpotGrade {
        kind: SPOT_GRADE_KIND,
        seq: None,
        spot_id: spot.spot_id.clone(),
        call: spot.call.clone(),
        band,
        spot_time: spot.time,
        window_close: hi,
        window_s: cfg.window_secs,
        grade: Grade::Unknown,
        disagreements: Vec::new(),
        manta_freq_hz: spot.freq_hz,
        manta_wpm: spot.wpm,
        rbn_modal_freq_hz: None,
        rbn_median_wpm: None,
        rbn_skimmers: 0,
        rbn_reports: 0,
        freq_offset_hz: None,
        rbn_bands: bands.into_iter().map(band_at_rank).collect(),
        spotter_filter: filter.only.iter().cloned().collect(),
    };

    if same.is_empty() {
        record.grade = match (record.rbn_bands.is_empty(), covered) {
            (_, false) => Grade::Unknown,
            (false, true) => Grade::Elsewhere,
            (true, true) => Grade::None,
        };
        return record;
    }

    let freqs: Vec<f64> = same.iter().map(|r| r.freq_hz).collect();
    let modal = modal_freq_hz(&freqs, spot.freq_hz);
    let mut speeds: Vec<f64> = same.iter().filter_map(|r| r.wpm).collect();
    let median_wpm = median(&mut speeds);
    let skimmers: BTreeSet<String> = same.iter().map(|r| normalize_spotter(&r.spotter)).collect();

    if let Some(m) = modal {
        if ((spot.freq_hz - m) as f64).abs() > cfg.freq_tol_hz {
            record.disagreements.push(Disagreement::Frequency);
        }
    }
    if let (Some(manta), Some(rbn)) = (spot.wpm, median_wpm) {
        let tol = cfg.speed_tol_wpm.max(cfg.speed_tol_pct / 100.0 * rbn);
        if (f64::from(manta) - rbn).abs() > tol {
            record.disagreements.push(Disagreement::Speed);
        }
    }
    record.grade = if record.disagreements.is_empty() {
        Grade::Match
    } else {
        Grade::Disagree
    };
    record.rbn_modal_freq_hz = modal;
    record.rbn_median_wpm = median_wpm;
    record.rbn_skimmers = skimmers.len() as u32;
    record.rbn_reports = same.len() as u32;
    record.freq_offset_hz = modal.map(|m| spot.freq_hz - m);
    record
}

fn sort_grades(grades: &mut [SpotGrade]) {
    grades.sort_by(|a, b| (a.window_close, &a.spot_id).cmp(&(b.window_close, &b.spot_id)));
}

/// Offline archives whose report spans overlap or lie at most this far
/// apart join into one coverage span: the live feed's idle timeout, the
/// silence it treats as an outage. Consecutive daily dumps meet at midnight
/// well inside it; a day left out between two dumps does not.
pub const ARCHIVE_JOIN_SECS: i64 = super::feed::FEED_IDLE_TIMEOUT.as_secs() as i64;

/// Offline grading (`manta grade`), one report list per archive (CSV file).
/// Each archive covers `[earliest, latest]` of its own report times (an
/// empty one covers nothing), and spans that overlap or lie at most
/// [`ARCHIVE_JOIN_SECS`] apart join, so a gap between archives is not
/// covered. Returns grades sorted by `(window_close, spot_id)`, the order
/// [`Grader::close_due`] emits.
pub fn grade_batch(
    spots: &[GradeInput],
    archives: Vec<Vec<RbnReport>>,
    cfg: &GradeConfig,
) -> Vec<SpotGrade> {
    let mut spans: Vec<(i64, i64)> = archives
        .iter()
        .filter_map(|a| {
            let first = a.iter().map(|r| r.time).min()?;
            let last = a.iter().map(|r| r.time).max()?;
            Some((first, last))
        })
        .collect();
    spans.sort_unstable();
    let mut coverage = Coverage::empty();
    let mut joined: Option<(i64, i64)> = None;
    for (first, last) in spans {
        joined = match joined {
            Some((from, to)) if first.saturating_sub(to) <= ARCHIVE_JOIN_SECS => {
                Some((from, to.max(last)))
            }
            Some((from, to)) => {
                coverage.up(from);
                coverage.down(to);
                Some((first, last))
            }
            None => Some((first, last)),
        };
    }
    if let Some((from, to)) = joined {
        coverage.up(from);
        coverage.down(to);
    }
    let mut reports: Vec<RbnReport> = archives.into_iter().flatten().collect();
    reports.sort_by_key(|r| r.time);
    let filter = SpotterFilter::new(cfg);
    // Archives hold every call RBN heard all day; index only spotted ones.
    let wanted: BTreeSet<String> = spots
        .iter()
        .map(|s| s.call.trim().to_ascii_uppercase())
        .collect();
    let mut index = ReportIndex::default();
    for r in reports {
        if !wanted.contains(&r.call.trim().to_ascii_uppercase()) {
            continue;
        }
        if let Some(r) = filter.admit(r) {
            index.insert(r);
        }
    }
    let mut grades: Vec<SpotGrade> = spots
        .iter()
        .map(|s| {
            let (lo, hi) = window_of(s, cfg);
            let call = s.call.trim().to_ascii_uppercase();
            let candidates = index.candidates(&call, lo, hi);
            grade_with(s, &candidates, coverage.covers(lo, hi), cfg, &filter)
        })
        .collect();
    sort_grades(&mut grades);
    grades
}

/// Live, incremental grading: reports are observed as the feed delivers
/// them, spots are submitted as the bus publishes them, and
/// [`close_due`](Self::close_due) grades every spot whose window has
/// closed. Bounded in memory by [`MAX_BUFFERED_REPORTS`] and
/// [`MAX_PENDING_SPOTS`].
pub struct Grader {
    cfg: GradeConfig,
    filter: SpotterFilter,
    index: ReportIndex,
    coverage: Coverage,
    pending: BTreeMap<(i64, String), GradeInput>,
    ungraded_full: u64,
}

impl Grader {
    pub fn new(cfg: GradeConfig) -> Self {
        let filter = SpotterFilter::new(&cfg);
        Self {
            cfg,
            filter,
            index: ReportIndex::default(),
            coverage: Coverage::empty(),
            pending: BTreeMap::new(),
            ungraded_full: 0,
        }
    }

    /// Stores one report; `false` when the spotter filter or exclusion list
    /// rejected it. Past the cap the oldest report is evicted and the
    /// coverage horizon moves past it.
    pub fn observe(&mut self, r: RbnReport) -> bool {
        // Any report shows the feed alive, filtered out or not.
        self.coverage.alive(r.time);
        let Some(r) = self.filter.admit(r) else {
            return false;
        };
        self.index.insert(r);
        while self.index.len() > MAX_BUFFERED_REPORTS {
            if let Some(t) = self.index.pop_oldest() {
                self.coverage.raise_horizon(t.saturating_add(1));
            }
        }
        true
    }

    /// The feed connected (logged in) at `at`. A no-op while already up.
    pub fn feed_up(&mut self, at: i64) {
        self.coverage.up(at);
    }

    /// The feed dropped at `at`. A no-op while already down.
    pub fn feed_down(&mut self, at: i64) {
        self.coverage.down(at);
    }

    /// Queues a spot until its window closes; `Submit::Full` (counted, not
    /// graded) once [`MAX_PENDING_SPOTS`] are pending.
    pub fn submit(&mut self, spot: GradeInput) -> Submit {
        if self.pending.len() >= MAX_PENDING_SPOTS {
            self.ungraded_full += 1;
            return Submit::Full;
        }
        let (_, close) = window_of(&spot, &self.cfg);
        self.pending.insert((close, spot.spot_id.clone()), spot);
        Submit::Queued
    }

    /// Grades every pending spot whose window closed at or before `now`,
    /// sorted by `(window_close, spot_id)`, then prunes reports older than
    /// `now - (2 * window + RETENTION_SLACK_SECS)` and raises the coverage
    /// horizon to that cutoff. A spot whose window only the open feed span
    /// covers waits while no report has arrived at or after its close, up
    /// to [`FEED_SILENCE_GRACE_SECS`] past it: a silent feed is not yet
    /// down, and its silence must not grade `none`/`elsewhere`. The prune
    /// never passes a waiting spot's window start.
    pub fn close_due(&mut self, now: i64) -> Vec<SpotGrade> {
        let mut out = Vec::new();
        while let Some(entry) = self.pending.first_entry() {
            if entry.key().0 > now {
                break;
            }
            let (lo, hi) = window_of(entry.get(), &self.cfg);
            // Later spots close no earlier, so they wait too.
            if self.coverage.unconfirmed(lo, hi) && now < hi.saturating_add(FEED_SILENCE_GRACE_SECS)
            {
                break;
            }
            let spot = entry.remove();
            let call = spot.call.trim().to_ascii_uppercase();
            let candidates = self.index.candidates(&call, lo, hi);
            let covered = self.coverage.covers(lo, hi);
            out.push(grade_with(
                &spot,
                &candidates,
                covered,
                &self.cfg,
                &self.filter,
            ));
        }
        let mut cutoff = now.saturating_sub(2 * self.cfg.window_secs + RETENTION_SLACK_SECS);
        if let Some(spot) = self.pending.values().next() {
            cutoff = cutoff.min(window_of(spot, &self.cfg).0);
        }
        self.index.prune_before(cutoff);
        self.coverage.raise_horizon(cutoff);
        out
    }

    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Reports currently buffered.
    pub fn buffered_reports(&self) -> usize {
        self.index.len()
    }

    /// Spots refused by [`submit`](Self::submit) because the pending queue
    /// was full.
    pub fn ungraded_full(&self) -> u64 {
        self.ungraded_full
    }
}

impl GradeInput {
    /// Live path: the spot exactly as its SpotMessage would carry it.
    pub fn from_bus_spot(
        spot: &manta_spot::Spot,
        bus: &crate::bus::SpotBus,
        station_call: &str,
    ) -> Self {
        Self {
            spot_id: crate::spot_message::spot_id(station_call, bus.session_nonce(), spot),
            call: spot.callsign.clone(),
            time: bus.unix_ts_for(spot.sample_ts),
            freq_hz: crate::spot_message::wire_frequency_hz(spot),
            wpm: crate::spot_message::wire_wpm(spot),
        }
    }
}

/// The SpotMessage keys grading needs, under their wire names.
#[derive(Deserialize)]
struct SpotWire {
    id: String,
    timestamp: i64,
    frequency: i64,
    #[serde(rename = "dxCall")]
    dx_call: String,
    #[serde(default)]
    wpm: Option<i32>,
}

impl From<SpotWire> for GradeInput {
    fn from(w: SpotWire) -> Self {
        Self {
            spot_id: w.id,
            call: w.dx_call,
            time: w.timestamp,
            freq_hz: w.frequency,
            wpm: w.wpm,
        }
    }
}

/// Offline path (`manta grade`): one JSON-stream line as a spot, read from
/// the same `id`/`timestamp`/`frequency`/`dxCall`/`wpm` fields the live
/// path derives through `spot_message`'s helpers. A `spotGrade` (or any
/// other `kind`) record is an error, never a spot.
pub fn parse_spot_message_line(line: &str) -> Result<GradeInput, String> {
    let value: serde_json::Value =
        serde_json::from_str(line.trim()).map_err(|e| format!("not JSON: {e}"))?;
    if let Some(kind) = value.get("kind") {
        return Err(format!("a {kind} record, not a spot"));
    }
    serde_json::from_value::<SpotWire>(value)
        .map(GradeInput::from)
        .map_err(|e| format!("not a spot line: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: i64 = 180;
    /// 2026-10-11 12:00:00Z.
    const T0: i64 = 1_791_720_000;

    fn cfg() -> GradeConfig {
        GradeConfig::default()
    }

    fn rep(spotter: &str, call: &str, khz: f64, wpm: Option<f64>, time: i64) -> RbnReport {
        RbnReport {
            spotter: spotter.to_string(),
            call: call.to_string(),
            freq_hz: khz * 1000.0,
            wpm,
            time,
        }
    }

    fn spot(id: &str, call: &str, freq_hz: i64, wpm: Option<i32>, time: i64) -> GradeInput {
        GradeInput {
            spot_id: id.to_string(),
            call: call.to_string(),
            time,
            freq_hz,
            wpm,
        }
    }

    fn refs(reports: &[RbnReport]) -> Vec<&RbnReport> {
        reports.iter().collect()
    }

    fn scenario1() -> (GradeInput, Vec<RbnReport>) {
        (
            spot(
                "W3XYZ-1:1791719990000000000:7:960000:W1AW",
                "W1AW",
                14_025_100,
                Some(25),
                T0,
            ),
            vec![
                rep("K5TR", "W1AW", 14025.0, Some(22.0), T0 - 30),
                rep("DL8LAS", "W1AW", 14025.0, Some(24.0), T0 + 10),
                rep("W3LPL", "W1AW", 14024.9, Some(25.0), T0 + 60),
            ],
        )
    }

    fn scenario2() -> (GradeInput, Vec<RbnReport>) {
        (
            spot(
                "W3XYZ-1:1791719990000000000:9:58560000:K5XYZ",
                "K5XYZ",
                7_012_300,
                Some(22),
                T0 + 600,
            ),
            Vec::new(),
        )
    }

    fn scenario3() -> (GradeInput, Vec<RbnReport>) {
        let t = T0 + 1800;
        (
            spot(
                "W3XYZ-1:1791719990000000000:12:173760000:W1AW",
                "W1AW",
                14_026_400,
                Some(30),
                t,
            ),
            vec![
                rep("K5TR", "W1AW", 14025.0, Some(20.0), t - 20),
                rep("DL8LAS", "W1AW", 14025.0, Some(20.0), t + 40),
            ],
        )
    }

    fn scenario4() -> (GradeInput, Vec<RbnReport>) {
        let t = T0 + 2700;
        (
            spot(
                "W3XYZ-1:1791719990000000000:15:260160000:DL1ABC",
                "DL1ABC",
                14_030_000,
                Some(28),
                t,
            ),
            vec![rep("DL8LAS", "DL1ABC", 7030.0, Some(28.0), t + 30)],
        )
    }

    fn graded(
        (s, reports): (GradeInput, Vec<RbnReport>),
        covered: bool,
        cfg: &GradeConfig,
    ) -> SpotGrade {
        grade_spot(&s, &refs(&reports), covered, cfg)
    }

    #[test]
    fn same_call_same_band_within_tolerance_is_a_match() {
        let g = graded(scenario1(), true, &cfg());
        assert_eq!(g.grade, Grade::Match);
        assert!(g.disagreements.is_empty());
        assert_eq!(g.rbn_modal_freq_hz, Some(14_025_000));
        assert_eq!(g.rbn_median_wpm, Some(24.0));
        assert_eq!(g.rbn_skimmers, 3);
        assert_eq!(g.rbn_reports, 3);
        assert_eq!(g.freq_offset_hz, Some(100));
        assert_eq!(g.rbn_bands, vec!["20m"]);
        assert_eq!(g.band, "20m");
        assert_eq!(g.window_close, T0 + W);
    }

    #[test]
    fn no_report_anywhere_with_full_coverage_is_none() {
        let g = graded(scenario2(), true, &cfg());
        assert_eq!(g.grade, Grade::None);
        assert_eq!(g.band, "40m");
        assert!(g.rbn_bands.is_empty());
        assert_eq!(
            (g.rbn_modal_freq_hz, g.rbn_median_wpm, g.freq_offset_hz),
            (None, None, None)
        );
        assert_eq!((g.rbn_skimmers, g.rbn_reports), (0, 0));
    }

    #[test]
    fn frequency_and_speed_gaps_are_disagree_not_none() {
        let g = graded(scenario3(), true, &cfg());
        assert_eq!(g.grade, Grade::Disagree);
        assert_eq!(
            g.disagreements,
            vec![Disagreement::Frequency, Disagreement::Speed]
        );
        assert_eq!(g.freq_offset_hz, Some(1400));
        assert_eq!(g.rbn_median_wpm, Some(20.0));
        assert_eq!(g.rbn_skimmers, 2);
    }

    #[test]
    fn same_call_only_on_another_band_is_elsewhere() {
        let g = graded(scenario4(), true, &cfg());
        assert_eq!(g.grade, Grade::Elsewhere);
        assert_eq!(g.rbn_bands, vec!["40m"]);
        assert_eq!(
            (g.rbn_modal_freq_hz, g.rbn_median_wpm, g.freq_offset_hz),
            (None, None, None)
        );
        assert_eq!((g.rbn_skimmers, g.rbn_reports), (0, 0));
    }

    #[test]
    fn same_band_wins_over_other_bands() {
        let (s, mut reports) = scenario1();
        reports.push(rep("K1TTT", "W1AW", 7025.0, Some(25.0), T0 + 5));
        let g = grade_spot(&s, &refs(&reports), true, &cfg());
        assert_eq!(g.grade, Grade::Match);
        // Band-table order (`band::allocations`, ascending frequency).
        assert_eq!(g.rbn_bands, vec!["40m", "20m"]);
        assert_eq!(g.rbn_reports, 3, "same-band stats exclude the 40m report");
    }

    fn speed_flagged(manta: i32, rbn: f64) -> bool {
        let s = spot("id", "W1AW", 14_025_000, Some(manta), T0);
        let reports = vec![rep("K5TR", "W1AW", 14025.0, Some(rbn), T0)];
        let g = grade_spot(&s, &refs(&reports), true, &cfg());
        g.disagreements.contains(&Disagreement::Speed)
    }

    #[test]
    fn speed_within_floor_or_ratio_is_not_a_disagreement() {
        assert!(!speed_flagged(22, 20.0));
        assert!(speed_flagged(26, 20.0), "6 > max(3, 4)");
        assert!(!speed_flagged(12, 9.0), "3 is not > max(3, 1.8)");
    }

    #[test]
    fn missing_wpm_never_flags_speed() {
        let s = spot("id", "W1AW", 14_025_000, None, T0);
        let reports = vec![rep("K5TR", "W1AW", 14025.0, Some(40.0), T0)];
        let g = grade_spot(&s, &refs(&reports), true, &cfg());
        assert_eq!(g.grade, Grade::Match);
        assert_eq!(g.manta_wpm, None);

        let s = spot("id", "W1AW", 14_025_000, Some(40), T0);
        let reports = vec![rep("K5TR", "W1AW", 14025.0, None, T0)];
        let g = grade_spot(&s, &refs(&reports), true, &cfg());
        assert_eq!(g.grade, Grade::Match);
        assert_eq!(g.rbn_median_wpm, None);
    }

    #[test]
    fn freq_exactly_at_tolerance_is_not_flagged() {
        let reports = vec![rep("K5TR", "W1AW", 14025.0, Some(25.0), T0)];
        let at = spot("id", "W1AW", 14_025_500, Some(25), T0);
        assert_eq!(
            grade_spot(&at, &refs(&reports), true, &cfg()).grade,
            Grade::Match
        );
        let past = spot("id", "W1AW", 14_025_501, Some(25), T0);
        assert_eq!(
            grade_spot(&past, &refs(&reports), true, &cfg()).grade,
            Grade::Disagree
        );
    }

    #[test]
    fn modal_frequency_tie_breaks_toward_manta_then_lower() {
        // One report each at 14 025.0 and 14 025.4 kHz: manta at 14 025 350
        // is nearer the upper.
        let reports = vec![
            rep("K5TR", "W1AW", 14025.0, None, T0),
            rep("DL8LAS", "W1AW", 14025.4, None, T0),
        ];
        let near_upper = spot("id", "W1AW", 14_025_350, None, T0);
        let g = grade_spot(&near_upper, &refs(&reports), true, &cfg());
        assert_eq!(g.rbn_modal_freq_hz, Some(14_025_400));
        // Equidistant (14 025 200): the lower wins.
        let middle = spot("id", "W1AW", 14_025_200, None, T0);
        let g = grade_spot(&middle, &refs(&reports), true, &cfg());
        assert_eq!(g.rbn_modal_freq_hz, Some(14_025_000));
        // A strict majority beats proximity.
        let mut more = reports.clone();
        more.push(rep("W3LPL", "W1AW", 14025.04, None, T0));
        let g = grade_spot(&near_upper, &refs(&more), true, &cfg());
        assert_eq!(g.rbn_modal_freq_hz, Some(14_025_000));
    }

    #[test]
    fn reports_outside_the_window_do_not_count() {
        let s = spot("id", "W1AW", 14_025_000, Some(25), T0);
        let late = vec![rep("K5TR", "W1AW", 14025.0, Some(25.0), T0 + W + 1)];
        assert_eq!(
            grade_spot(&s, &refs(&late), true, &cfg()).grade,
            Grade::None
        );
        let early = vec![rep("K5TR", "W1AW", 14025.0, Some(25.0), T0 - W - 1)];
        assert_eq!(
            grade_spot(&s, &refs(&early), true, &cfg()).grade,
            Grade::None
        );
        let edge = vec![
            rep("K5TR", "W1AW", 14025.0, Some(25.0), T0 + W),
            rep("DL8LAS", "W1AW", 14025.0, Some(25.0), T0 - W),
        ];
        let g = grade_spot(&s, &refs(&edge), true, &cfg());
        assert_eq!((g.grade, g.rbn_reports), (Grade::Match, 2));
    }

    fn k5tr_only() -> GradeConfig {
        GradeConfig {
            spotters: BTreeSet::from(["K5TR".to_string()]),
            ..cfg()
        }
    }

    #[test]
    fn spotter_filter_restricts_truth_before_aggregation() {
        let g = graded(scenario1(), true, &k5tr_only());
        assert_eq!(g.grade, Grade::Match);
        assert_eq!((g.rbn_skimmers, g.rbn_reports), (1, 1));
        assert_eq!(g.rbn_median_wpm, Some(22.0));
        assert_eq!(g.spotter_filter, vec!["K5TR".to_string()]);

        let g = graded(scenario4(), true, &k5tr_only());
        assert_eq!(g.grade, Grade::None);
        assert!(g.rbn_bands.is_empty());

        // The live index applies the filter at insert.
        let mut grader = Grader::new(k5tr_only());
        assert!(grader.observe(rep("K5TR", "W1AW", 14025.0, None, T0)));
        assert!(!grader.observe(rep("DL8LAS", "W1AW", 14025.0, None, T0)));
        assert_eq!(grader.buffered_reports(), 1);
    }

    #[test]
    fn own_station_and_excluded_spotters_never_count() {
        let c = GradeConfig {
            exclude_spotters: ["W3XYZ-1", "W3XYZ", "ok1xyz"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            ..cfg()
        };
        let s = spot("id", "W1AW", 14_025_000, Some(25), T0);
        let reports = vec![
            rep("W3XYZ-1", "W1AW", 14025.0, Some(25.0), T0),
            rep("W3XYZ-#", "W1AW", 14025.0, Some(25.0), T0),
            rep("OK1XYZ-#", "W1AW", 14025.0, Some(25.0), T0),
        ];
        assert_eq!(grade_spot(&s, &refs(&reports), true, &c).grade, Grade::None);
        let mut grader = Grader::new(c);
        for r in reports {
            assert!(!grader.observe(r));
        }
    }

    #[test]
    fn spotter_suffix_hash_is_stripped() {
        assert_eq!(normalize_spotter("k5tr-#"), "K5TR");
        assert_eq!(normalize_spotter("W3XYZ-1"), "W3XYZ-1");
        let s = spot("id", "W1AW", 14_025_000, Some(25), T0);
        let reports = vec![rep("K5TR-#", "W1AW", 14025.0, Some(25.0), T0)];
        let g = grade_spot(&s, &refs(&reports), true, &k5tr_only());
        assert_eq!((g.grade, g.rbn_skimmers), (Grade::Match, 1));
    }

    #[test]
    fn incomplete_coverage_turns_none_and_elsewhere_into_unknown_but_keeps_match() {
        assert_eq!(graded(scenario1(), false, &cfg()).grade, Grade::Match);
        assert_eq!(graded(scenario2(), false, &cfg()).grade, Grade::Unknown);
        assert_eq!(graded(scenario3(), false, &cfg()).grade, Grade::Disagree);
        let g = graded(scenario4(), false, &cfg());
        assert_eq!(g.grade, Grade::Unknown);
        assert_eq!(g.rbn_bands, vec!["40m"]);

        // Live coverage: the feed came up 100 s before the spot, so its
        // window start (T0 - 180) is not covered...
        let mut grader = Grader::new(cfg());
        grader.feed_up(T0 - 100);
        let (s, _) = scenario2();
        grader.submit(GradeInput {
            time: T0,
            ..s.clone()
        });
        // ... nor is a window cut by an outage.
        grader.feed_down(T0 + 200);
        grader.feed_up(T0 + 210);
        grader.submit(GradeInput {
            spot_id: "b".into(),
            time: T0 + 300,
            ..s.clone()
        });
        // A window inside one span is covered.
        grader.submit(GradeInput {
            spot_id: "c".into(),
            time: T0 + 600,
            ..s
        });
        let grades: Vec<Grade> = grader
            .close_due(T0 + 1000)
            .iter()
            .map(|g| g.grade)
            .collect();
        assert_eq!(grades, vec![Grade::Unknown, Grade::Unknown, Grade::None]);
    }

    #[test]
    fn offline_gap_between_archives_is_not_covered() {
        // K5XYZ at T0 + 600, heard by nobody: its window is
        // [T0 + 420, T0 + 780].
        let (s, _) = scenario2();
        let edge = |t: i64| rep("K5TR", "N0EDGE", 14010.0, None, t);
        let grade = |archives: Vec<Vec<RbnReport>>| {
            grade_batch(std::slice::from_ref(&s), archives, &cfg())[0].grade
        };
        // One archive spanning the window covers it.
        assert_eq!(grade(vec![vec![edge(T0), edge(T0 + 2000)]]), Grade::None);
        // Two archives with the window in the gap between them do not, as
        // when a day is left out between two daily dumps.
        assert_eq!(
            grade(vec![vec![edge(T0), edge(T0 + 100)], vec![edge(T0 + 2000)]]),
            Grade::Unknown
        );
        // Archives at most ARCHIVE_JOIN_SECS apart join, in any order...
        let to = T0 + 500;
        assert_eq!(
            grade(vec![
                vec![edge(to + ARCHIVE_JOIN_SECS), edge(T0 + 2000)],
                vec![edge(T0), edge(to)],
            ]),
            Grade::None
        );
        // ... and one second further apart they do not.
        assert_eq!(
            grade(vec![
                vec![edge(T0), edge(to)],
                vec![edge(to + ARCHIVE_JOIN_SECS + 1), edge(T0 + 2000)],
            ]),
            Grade::Unknown
        );
        // An empty archive covers nothing.
        assert_eq!(grade(vec![Vec::new()]), Grade::Unknown);
    }

    #[test]
    fn out_of_band_spot_correlates_by_frequency_tolerance() {
        let s = spot("id", "W1AW", 4_500_000, Some(25), T0);
        let near = vec![rep("K5TR", "W1AW", 4500.2, Some(25.0), T0)];
        let g = grade_spot(&s, &refs(&near), true, &cfg());
        assert_eq!(g.band, "unknown");
        assert_eq!(g.grade, Grade::Match);
        assert_eq!(g.rbn_bands, vec!["unknown"]);
        let far = vec![rep("K5TR", "W1AW", 4502.0, Some(25.0), T0)];
        let g = grade_spot(&s, &refs(&far), true, &cfg());
        assert_eq!(g.grade, Grade::Elsewhere);
    }

    const DOCUMENTED: [&str; 4] = [
        r#"{"kind":"spotGrade","spotId":"W3XYZ-1:1791719990000000000:7:960000:W1AW","call":"W1AW","band":"20m","spotTime":1791720000,"windowClose":1791720180,"windowS":180,"grade":"match","disagreements":[],"mantaFreqHz":14025100,"mantaWpm":25,"rbnModalFreqHz":14025000,"rbnMedianWpm":24.0,"rbnSkimmers":3,"rbnReports":3,"freqOffsetHz":100,"rbnBands":["20m"],"spotterFilter":[]}"#,
        r#"{"kind":"spotGrade","spotId":"W3XYZ-1:1791719990000000000:9:58560000:K5XYZ","call":"K5XYZ","band":"40m","spotTime":1791720600,"windowClose":1791720780,"windowS":180,"grade":"none","disagreements":[],"mantaFreqHz":7012300,"mantaWpm":22,"rbnModalFreqHz":null,"rbnMedianWpm":null,"rbnSkimmers":0,"rbnReports":0,"freqOffsetHz":null,"rbnBands":[],"spotterFilter":[]}"#,
        r#"{"kind":"spotGrade","spotId":"W3XYZ-1:1791719990000000000:12:173760000:W1AW","call":"W1AW","band":"20m","spotTime":1791721800,"windowClose":1791721980,"windowS":180,"grade":"disagree","disagreements":["frequency","speed"],"mantaFreqHz":14026400,"mantaWpm":30,"rbnModalFreqHz":14025000,"rbnMedianWpm":20.0,"rbnSkimmers":2,"rbnReports":2,"freqOffsetHz":1400,"rbnBands":["20m"],"spotterFilter":[]}"#,
        r#"{"kind":"spotGrade","spotId":"W3XYZ-1:1791719990000000000:15:260160000:DL1ABC","call":"DL1ABC","band":"20m","spotTime":1791722700,"windowClose":1791722880,"windowS":180,"grade":"elsewhere","disagreements":[],"mantaFreqHz":14030000,"mantaWpm":28,"rbnModalFreqHz":null,"rbnMedianWpm":null,"rbnSkimmers":0,"rbnReports":0,"freqOffsetHz":null,"rbnBands":["40m"],"spotterFilter":[]}"#,
    ];

    #[test]
    fn grade_record_serialises_to_the_documented_shape() {
        let grades =
            [scenario1(), scenario2(), scenario3(), scenario4()].map(|s| graded(s, true, &cfg()));
        for (g, want) in grades.iter().zip(DOCUMENTED) {
            assert_eq!(serde_json::to_string(g).unwrap(), want);
        }
        // The batch path produces the same four records, in close order.
        let mut spots = Vec::new();
        let mut reports = Vec::new();
        for (s, r) in [scenario1(), scenario2(), scenario3(), scenario4()] {
            spots.push(s);
            reports.extend(r);
        }
        // Coverage edges well outside every window.
        reports.push(rep("K5TR", "N0EDGE", 14010.0, None, T0 - 1000));
        reports.push(rep("K5TR", "N0EDGE", 14010.0, None, T0 + 5000));
        let lines: Vec<String> = grade_batch(&spots, vec![reports], &cfg())
            .iter()
            .map(|g| serde_json::to_string(g).unwrap())
            .collect();
        assert_eq!(lines, DOCUMENTED);
    }

    #[test]
    fn grade_record_never_carries_spot_identity_keys() {
        let grades = [
            graded(scenario1(), true, &cfg()),
            graded(scenario2(), true, &cfg()),
            graded(scenario3(), true, &cfg()),
            graded(scenario4(), true, &cfg()),
            graded(scenario2(), false, &cfg()),
        ];
        let kinds: BTreeSet<Grade> = grades.iter().map(|g| g.grade).collect();
        assert_eq!(kinds.len(), 5, "one record per grade");
        for g in &grades {
            let v = serde_json::to_value(g).unwrap();
            assert_eq!(v["kind"], "spotGrade");
            for key in ["dxCall", "frequency", "timestamp"] {
                assert!(v.get(key).is_none(), "{key} in {v}");
            }
            assert!(parse_spot_message_line(&v.to_string()).is_err());
        }
    }

    /// `docs/SPOT-SCHEMA.md` is the stream's field reference, so its
    /// `spotGrade` section must list exactly the keys this record sends.
    #[test]
    fn every_grade_field_is_documented_in_spot_schema() {
        let mut g = graded(scenario3(), true, &cfg());
        g.seq = Some(1);
        let line = serde_json::to_string(&g).unwrap();
        let v = serde_json::to_value(&g).unwrap();
        let mut keys: Vec<&str> = v
            .as_object()
            .expect("an object")
            .keys()
            .map(|k| k.as_str())
            .collect();
        // Wire order, whatever order the Map iterates in.
        keys.sort_by_key(|k| line.find(&format!("\"{k}\":")).expect("key on the wire"));
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/SPOT-SCHEMA.md");
        let schema = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("reading {path}: {e}"))
            .replace("\r\n", "\n");
        let (_, rest) = schema
            .split_once("\n## `spotGrade` records\n")
            .expect("docs/SPOT-SCHEMA.md has no `## `spotGrade` records` section");
        let section = rest.split("\n## ").next().unwrap_or(rest);
        let rows: Vec<&str> = section
            .lines()
            .filter_map(|l| l.strip_prefix("- `"))
            .filter_map(|l| l.split_once("` ("))
            .map(|(field, _)| field)
            .collect();
        assert_eq!(
            rows, keys,
            "docs/SPOT-SCHEMA.md's spotGrade list must name every key, in wire order"
        );
        assert!(section.contains(&format!("all {} keys", keys.len())));
    }

    #[test]
    fn parse_spot_message_line_reads_the_wire_fields() {
        let line = r#"{"id":"W3XYZ-1:12534119743695398054:1:2006272:W1AW","source":"skimmer","timestamp":1760205620,"ingestedAt":1760205620,"frequency":14012362,"band":"20m","mode":"CW","dxCall":"W1AW","dxGrid":null,"dxLat":37.6,"dxLon":-91.87,"dxDxcc":291,"dxContinent":"NA","dxCqZone":5,"deCall":"W3XYZ-1","deGrid":null,"deLat":null,"deLon":null,"deDxcc":291,"deContinent":"NA","snr":21,"snrRefHz":2500,"wpm":20,"decodeConfidence":0.5541629,"decoderVersion":"manta-0.1.0+5346c333efd8","channelizerResolutionHz":null}"#;
        let g = parse_spot_message_line(line).unwrap();
        assert_eq!(
            g,
            spot(
                "W3XYZ-1:12534119743695398054:1:2006272:W1AW",
                "W1AW",
                14_012_362,
                Some(20),
                1_760_205_620
            )
        );
        assert!(parse_spot_message_line("not json").is_err());
        assert!(parse_spot_message_line(r#"{"id":"x"}"#).is_err());
    }

    /// Tiny deterministic LCG (Numerical Recipes constants): no new crate.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    #[test]
    fn incremental_grader_matches_batch() {
        const CALLS: [&str; 5] = ["W1AW", "K5XYZ", "DL1ABC", "JA1ABC", "G4XYZ"];
        const SPOTTERS: [&str; 6] = ["K5TR-#", "DL8LAS", "W3LPL-#", "VE2WU", "W3XYZ-1", "OH6BG"];
        let c = GradeConfig {
            exclude_spotters: BTreeSet::from(["W3XYZ-1".to_string()]),
            ..cfg()
        };
        let mut rng = Lcg(0x004d_414e_2d33_3031);
        // Each call sits near one frequency per band.
        let base = |call: usize, band40: bool| -> f64 {
            if band40 {
                7_010_000.0 + call as f64 * 5_000.0
            } else {
                14_010_000.0 + call as f64 * 5_000.0
            }
        };
        let span = 7200;
        let mut reports = Vec::new();
        for _ in 0..400 {
            let call = rng.below(CALLS.len() as u64) as usize;
            let band40 = rng.below(3) == 0;
            reports.push(RbnReport {
                spotter: SPOTTERS[rng.below(SPOTTERS.len() as u64) as usize].to_string(),
                call: CALLS[call].to_string(),
                freq_hz: base(call, band40) + rng.below(2_000) as f64 - 1_000.0,
                wpm: (rng.below(5) != 0).then(|| 15.0 + rng.below(20) as f64),
                time: T0 + rng.below(span) as i64,
            });
        }
        let mut spots = Vec::new();
        for i in 0..40 {
            let call = rng.below(CALLS.len() as u64) as usize;
            // Some spots fall outside every report so `none` appears too.
            let call_name = if rng.below(8) == 0 {
                "N0SPOT"
            } else {
                CALLS[call]
            };
            let time = T0 - 300 + rng.below(span + 600) as i64;
            spots.push(GradeInput {
                spot_id: format!("N0CALL:1:{i}:{time}:{call_name}"),
                call: call_name.to_string(),
                time,
                freq_hz: (base(call, rng.below(3) == 0) + rng.below(1_400) as f64 - 700.0) as i64,
                wpm: (rng.below(6) != 0).then(|| 15 + rng.below(20) as i32),
            });
        }

        let want = grade_batch(&spots, vec![reports.clone()], &c);
        let grades: BTreeSet<Grade> = want.iter().map(|g| g.grade).collect();
        assert!(
            grades.len() >= 4,
            "generator should exercise most grades: {grades:?}"
        );

        let mut by_time = reports.clone();
        by_time.sort_by_key(|r| r.time);
        let first = by_time.first().unwrap().time;
        let last = by_time.last().unwrap().time;
        let mut by_submit = spots.clone();
        by_submit.sort_by_key(|s| s.time);

        let mut grader = Grader::new(c);
        let mut reports_iter = by_time.into_iter().peekable();
        let mut spots_iter = by_submit.into_iter().peekable();
        let mut got = Vec::new();
        let mut up = false;
        let mut down = false;
        let mut now = T0 - 600;
        while now <= T0 + span as i64 + 1200 {
            if !up && now >= first {
                grader.feed_up(first);
                up = true;
            }
            while let Some(r) = reports_iter.next_if(|r| r.time <= now) {
                grader.observe(r);
            }
            if !down && now >= last {
                grader.feed_down(last);
                down = true;
            }
            while let Some(s) = spots_iter.next_if(|s| s.time + 20 <= now) {
                assert_eq!(grader.submit(s), Submit::Queued);
            }
            got.extend(grader.close_due(now));
            now += 5;
        }
        assert_eq!(grader.pending(), 0);
        sort_grades(&mut got);
        assert_eq!(got, want);
    }

    /// Code review (MAN-301 validate): the feed's last line is at `L` and it
    /// goes silent; its idle timeout fires only at `L + 60`. A window that
    /// closes in between is not graded `none` on the silent stretch: it
    /// waits, then grades `unknown` once the feed is declared down at `L`,
    /// or grades once a later report shows the feed was alive at the close.
    #[test]
    fn silent_feed_before_its_idle_timeout_does_not_grade_none() {
        let (s, _) = scenario2();
        let l = T0 + 150;

        let mut grader = Grader::new(cfg());
        grader.feed_up(T0 - 1000);
        grader.observe(rep("K5TR", "N4RV", 7021.0, None, l));
        grader.submit(GradeInput {
            time: T0,
            ..s.clone()
        });
        // Window [T0 - 180, T0 + 180] closes 30 s into the silence.
        assert!(grader.close_due(T0 + 185).is_empty());
        assert_eq!(grader.pending(), 1);
        grader.feed_down(l);
        let grades = grader.close_due(l + 60);
        assert_eq!(grades.len(), 1);
        assert_eq!(grades[0].grade, Grade::Unknown);

        // A report at or after the close confirms coverage at once.
        let mut grader = Grader::new(cfg());
        grader.feed_up(T0 - 1000);
        grader.submit(GradeInput {
            time: T0,
            ..s.clone()
        });
        assert!(grader.close_due(T0 + 185).is_empty());
        grader.observe(rep("K5TR", "N4RV", 7021.0, None, T0 + 186));
        let grades = grader.close_due(T0 + 186);
        assert_eq!(grades.len(), 1);
        assert_eq!(grades[0].grade, Grade::None);

        // A feed still up after the grace was delivering lines (non-CW,
        // say): the spot grades on the open span, and the report at its
        // window start was kept while it waited (the prune cutoff passes
        // T0 - 178 from now = T0 + 243 on).
        let mut grader = Grader::new(cfg());
        grader.feed_up(T0 - 1000);
        grader.observe(rep("K5TR", "W1AW", 14025.0, None, T0 - 178));
        grader.submit(scenario1().0);
        let grace_end = T0 + 180 + FEED_SILENCE_GRACE_SECS;
        for now in (T0 + 180..grace_end).step_by(5) {
            assert!(grader.close_due(now).is_empty(), "{now}");
        }
        let grades = grader.close_due(grace_end);
        assert_eq!(grades.len(), 1);
        assert_eq!(grades[0].grade, Grade::Match);
        assert_eq!(grades[0].rbn_skimmers, 1);
    }

    #[test]
    fn grader_is_bounded() {
        let mut grader = Grader::new(cfg());
        grader.feed_up(0);
        for i in 0..=MAX_BUFFERED_REPORTS as i64 {
            let call = if i == 0 { "OLDEST" } else { "W1AW" };
            assert!(grader.observe(rep("K5TR", call, 14025.0, None, i)));
        }
        assert_eq!(grader.buffered_reports(), MAX_BUFFERED_REPORTS);
        assert!(
            !grader.index.by_call.contains_key("OLDEST"),
            "oldest evicted first"
        );
        assert_eq!(grader.index.order.front().map(|(t, _)| *t), Some(1));
        assert_eq!(grader.coverage.horizon, 1);

        for i in 0..MAX_PENDING_SPOTS {
            let s = spot(&format!("s{i}"), "W1AW", 14_025_000, None, 1_000_000);
            assert_eq!(grader.submit(s), Submit::Queued);
        }
        let s = spot("overflow", "W1AW", 14_025_000, None, 1_000_000);
        assert_eq!(grader.submit(s), Submit::Full);
        assert_eq!(grader.pending(), MAX_PENDING_SPOTS);
        assert_eq!(grader.ungraded_full(), 1);
    }

    #[test]
    fn pruned_report_horizon_counts_as_a_coverage_gap() {
        let mut grader = Grader::new(cfg());
        grader.feed_up(T0 - 1000);
        grader.observe(rep("K5TR", "W1AW", 14025.0, None, T0 + 100));
        grader.observe(rep("K5TR", "W1AW", 14025.0, None, T0 + 1600));
        // Cutoff: T0 + 2000 - (2 * 180 + 60) = T0 + 1580.
        assert!(grader.close_due(T0 + 2000).is_empty());
        assert_eq!(grader.buffered_reports(), 1);

        // Submitted late: its window [T0 - 80, T0 + 280] starts behind the
        // horizon, and its report was pruned -- unknown, not none.
        grader.submit(spot("late", "W1AW", 14_025_000, None, T0 + 100));
        // Also behind the horizon, but a report still in the buffer matches.
        grader.submit(spot("matched", "W1AW", 14_025_000, None, T0 + 1500));
        let grades: Vec<(String, Grade)> = grader
            .close_due(T0 + 2000)
            .into_iter()
            .map(|g| (g.spot_id, g.grade))
            .collect();
        assert_eq!(
            grades,
            vec![
                ("late".to_string(), Grade::Unknown),
                ("matched".to_string(), Grade::Match)
            ]
        );
    }
    #[test]
    fn a_streamed_grade_carries_seq_right_after_kind() {
        let mut g = graded(scenario3(), true, &cfg());
        g.seq = Some(9);
        assert!(serde_json::to_string(&g)
            .unwrap()
            .starts_with(r#"{"kind":"spotGrade","seq":9,"spotId":"#));
    }
}
