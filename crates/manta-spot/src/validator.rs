//! Ties `grammar`/`context`/`cty`/`scp`/`confidence`/`gate`/`dedupe`
//! together into one `Validator::ingest` entry point. ARCHITECTURE §6.

use crate::blocklist::Blocklist;
use crate::confidence;
use crate::context::{self, SpotType};
use crate::cty;
use crate::decision::{CandidateSupport, Decision, Reason, Rival};
use crate::dedupe::{Dedupe, DedupeOutcome};
use crate::gate::{RepetitionCounts, RepetitionGate, WINDOW_SECONDS as GATE_WINDOW_SECONDS};
use crate::grammar;
use crate::message;
use crate::notch::NotchList;
use crate::scp;
use crate::support::SupportLedger;
use crate::{pattern, ValidationLevel};
use manta_decode::events::{ClosureKind, DecoderEvent};
use manta_decode::tree::{Glyph, Prosign};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

/// How many recently-completed words a track remembers for context
/// parsing. Calls/context keywords always appear within a handful of
/// words of each other in practice; this bound keeps `TrackState` small
/// without needing a time-based window here (the repetition gate and
/// dedupe windows, which *do* need to be time-based, live in `gate.rs`/
/// `dedupe.rs`).
const WORD_WINDOW: usize = 16;

/// A track reporting faster than this is treated as implausible for real
/// hand-sent CW, not evaluated for a spot at all (see the call site in
/// `evaluate_candidate` for the full real-hardware evidence). NOT a
/// SPEC-defined value -- a validator-local heuristic, conservatively
/// above every confirmed-real spot observed so far (42.8 WPM) and well
/// below the speed tracker's own 60 WPM ceiling, chosen to catch the
/// noise-artifact zone without risking a genuinely fast (if rare) real
/// operator.
const MAX_PLAUSIBLE_WPM: f32 = 45.0;

/// Bounds `TrackState::pending_beacons` per track. A regularly-transmitting
/// carrier resets the silent-GC timer on every decoded character, so an
/// unbounded track (in principle alive for the daemon's lifetime) could
/// otherwise accumulate one entry per distinct Beacon-shaped decode
/// forever, all the way until an eventual `TrackClosed` (Codex review on
/// PR #154, round 8). Small: realistically only one or two genuinely
/// distinct beacon identities are ever plausible on a single track: this
/// exists to cap pathological growth, not to hold a meaningful backlog.
const MAX_PENDING_BEACONS: usize = 8;

/// A validated spot, ready for `manta-server` to serialize and emit.
/// No wall-clock timestamp -- that conversion happens at the
/// `manta-server` boundary (SPEC-decode-core.md §5), not here.
///
/// Serialized only by the CLI (`decode --json`'s `spots`, `run --json`'s
/// `{"spot": …}`); field names follow the JSON spot stream (MAN-139,
/// docs/CLI-JSON.md). manta-server builds its own `SpotMessage` instead.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Spot {
    #[serde(rename = "dxCall")]
    pub callsign: String,
    #[serde(rename = "frequency")]
    pub freq_hz: f64,
    /// 2500 Hz-referenced; `snr` in JSON.
    #[serde(rename = "snr")]
    pub snr_db: f32,
    pub wpm: f32,
    pub spot_type: SpotType,
    #[serde(rename = "decodeConfidence")]
    pub confidence: f32,
    pub track_id: u32,
    pub sample_ts: u64,
    /// The most recent RST signal report this spot's track decoded before
    /// the spot was built, as three digits (`5NN` -> `"599"`), or `None`
    /// (MAN-272 decision D4). Read from the emitting track only; a report a
    /// merged-away track decoded is not carried (D5).
    pub rst: Option<String>,
    /// Set when this spot's track decoded `QRL?` before the spot was built.
    /// Sticky for the track's life and never forwarded through a merge
    /// (MAN-272 decisions D4/D5).
    pub qrl_query: bool,
}

#[derive(Default)]
struct Word {
    text: String,
    confidences: Vec<f32>,
    /// Assigned from `TrackState::next_word_seq` when this word is pushed
    /// to `TrackState::words` -- a per-track, strictly-increasing "age"
    /// used to tell a genuinely new supporting word apart from an older
    /// one merely still being present (MAN-28 round 12 review).
    seq: u64,
    /// Set once this word has been offered to the validation pipeline as a
    /// context-match candidate, so a later, unrelated word boundary that
    /// re-scans a growing window doesn't re-process it.
    attempted: bool,
    /// The `SpotType` this word was last processed as, if any. An
    /// allowlisted word can spot immediately with no context yet (type
    /// `Unknown`); if a trailing word later completes a real context
    /// pattern (e.g. "K5ARH" then "UP" completing `<call> UP` -> `De`),
    /// that's a genuinely new type worth a reclassification, not a
    /// re-attempt of the same evaluation -- `attempted` alone must not
    /// permanently block it (MAN-28 round 8 review).
    last_spot_type: Option<SpotType>,
    /// The highest `seq` among the words that produced `last_spot_type`
    /// (this word's own `seq`, for a context-free allowlist match). A
    /// later re-evaluation is only a genuine reclassification -- driven by
    /// a newly-arrived word -- if its own max involved `seq` is strictly
    /// greater than this. Re-deriving a word's type from whatever
    /// currently sits in the window, with no way to tell "gained new
    /// context" from "lost old context" as it ages out, produced two
    /// separate downgrade bugs (rounds 11 and 12) before this was added.
    classified_max_seq: u64,
    /// The repetition count computed the first time this word was
    /// evaluated. A reclassification (see `last_spot_type`) reuses this
    /// rather than calling `RepetitionGate::record` again -- the word was
    /// decoded once, not twice, so re-recording would let an ordinary,
    /// non-exempt callsign spot after a type change alone inflated its
    /// rep count to 2 (MAN-28 round 9 review).
    last_reps: u32,
    /// Set once this word's power-step (`<call> T`, MAN-37) candidacy has
    /// been burned by the coarse CQ/DE framing guard. Guard-private, and
    /// deliberately NOT the general `attempted` flag: `attempted` is also
    /// set by any named-pattern evaluation of the same word, so gating the
    /// `SuppressionCounts::power_step_guard` counter on it silently missed
    /// real suppressions whenever some other pattern had already touched
    /// the word without spotting it -- e.g. "CQ K5ARH T", where
    /// `CQ_CALL_RE` marks `K5ARH` attempted one boundary early as an
    /// unspotted `Cq` candidate (reps = 1 < 2), so the repetition-exempt
    /// Beacon occurrence the guard then discarded was counted as zero
    /// (MAN-48, Codex review on PR #90). Whether the guard suppressed an
    /// occurrence is a property of the guard alone, so it needs its own
    /// per-occurrence bit.
    power_step_suppressed: bool,
}

/// A non-allowlisted Beacon-type candidate that has passed every
/// permanent, timing-independent check (blocklist/notch/grammar/cty) but
/// not yet the WPM-plausibility check -- captured once, at the moment its
/// pattern completes, and judged exactly once at the track's true close
/// (`Validator::resolve_pending_beacons`). Round 7 redesign: a live WPM
/// reading is inherently unreliable as a gate for a decision that must
/// reflect the track's FINAL state (Codex review on PR #154, rounds 2-7
/// each found a new way a reactive, opportunistic check could fire on a
/// transient value in either direction). Deliberately independent of
/// `TrackState::words`/`Word`: a long transmission can evict the
/// originating word from the bounded `WORD_WINDOW` before the track
/// closes (round 7), and later unrelated words must not corrupt this
/// candidate's own decode-time timestamp/frequency/SNR (round 7).
struct PendingBeacon {
    candidate: String,
    sample_ts: u64,
    freq_hz: f64,
    snr_db: f32,
    char_confidences: Vec<f32>,
    /// The track_id that originally captured this candidate -- NOT
    /// necessarily the track_id it's eventually resolved under (Codex
    /// review, PR #152, round 14): `migrate_or_discard_pending_beacons`
    /// moves a `PendingBeacon` into a merge survivor's own list, and
    /// resolving it there under the survivor's track_id would wrongly
    /// grant it `RepetitionGate`'s rapid-same-track exemption even though
    /// it's a genuinely different track's (duplicate-spawn) candidate --
    /// letting one real over-the-air occurrence, captured by two
    /// overlapping tracks, reach `reps >= 2` on its own.
    origin_track_id: u32,
    /// The captured word's own `seq` (MAN-100 Scenario 2), threaded to
    /// `RepetitionGate::record` on eventual resolution so a deferred
    /// Beacon replay is subject to the same message-distinctness rule as
    /// every other candidate.
    word_seq: u64,
}

#[derive(Default)]
struct TrackState {
    words: VecDeque<Word>,
    current: Word,
    freq_hz: f64,
    snr_db: f32,
    wpm: f32,
    /// Set once a real `SpeedUpdate` has been received for this track_id.
    /// `wpm` holds a bogus `0.0` default until then, which is BELOW
    /// `MAX_PLAUSIBLE_WPM` -- without this flag a survivor track that
    /// closes before its own speed tracker ever reports (e.g. a merge
    /// migrates evidence into it moments before it closes) would resolve
    /// its migrated `pending_beacons` as if 0.0 WPM were a confirmed final
    /// speed, admitting a candidate whose actual, never-recorded speed may
    /// have been at the 60 WPM noise ceiling (Codex review on PR #154,
    /// round 10).
    wpm_confirmed: bool,
    /// Set once a real `TrackMeta` event has been received. `freq_hz`/
    /// `snr_db` hold bogus `0.0` defaults until then (decoder.rs emits
    /// `TrackMeta` only every 375 hops -- a fast decode can complete
    /// chars/words before the first one ever arrives), so no spot may be
    /// emitted before this is true (MAN-28 round 8 review).
    has_meta: bool,
    /// The most recent `sample_ts` seen for this track (any event kind).
    /// `try_spot` is normally only invoked by a `WordBoundary`, but a
    /// candidate held back by `has_meta` must be retried the moment
    /// metadata arrives even if no further word ever completes -- this is
    /// the timestamp that retry uses (MAN-28 round 9 review).
    last_sample_ts: u64,
    /// Source of `Word::seq`; incremented each time a word is pushed to
    /// `words` (MAN-28 round 12 review).
    next_word_seq: u64,
    /// Captured non-allowlisted Beacon candidates awaiting the track's
    /// true close -- see `PendingBeacon`'s doc comment.
    pending_beacons: Vec<PendingBeacon>,
    /// The most recent RST any completed word on this track carried
    /// (last value wins), scanned at word push so it outlives the word's
    /// `WORD_WINDOW` slot (MAN-272 decision D4).
    rst: Option<String>,
    /// Set once any completed word on this track was `QRL?`; sticky until
    /// this state is removed on `TrackClosed` (MAN-272 decision D4).
    qrl_query: bool,
}

/// A `freq_correction_ppm` value that doesn't yield a finite, positive
/// calibration factor. Rejected before construction so an invalid config
/// value (NaN, infinity, or a ppm so negative it flips the correction
/// negative or zero) can never poison an emitted spot's frequency or its
/// dedupe bucket (MAN-29).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InvalidCalibration {
    pub ppm: f64,
    pub factor: f64,
}

impl std::fmt::Display for InvalidCalibration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Names the actual reason -- a ppm outside the supported range can
        // have a factor that's perfectly finite and positive, so that
        // message would be actively misleading for it (MAN-29 review
        // round 3).
        if !self.ppm.is_finite() {
            write!(f, "freq_correction_ppm {} is not finite", self.ppm)
        } else if self.ppm.abs() > MAX_ABS_PPM {
            write!(
                f,
                "freq_correction_ppm {} is outside the supported range [-{MAX_ABS_PPM}, {MAX_ABS_PPM}]",
                self.ppm
            )
        } else {
            write!(
                f,
                "freq_correction_ppm {} yields calibration factor {}, which must be finite and positive",
                self.ppm, self.factor
            )
        }
    }
}

impl std::error::Error for InvalidCalibration {}

/// Widest physically-plausible oscillator drift a real source could report
/// (a few hundred ppm covers even a badly-drifted uncalibrated RTL-SDR
/// crystal; SPEC's own worked example is 10 ppm). Bounding `ppm` this way
/// keeps the derived factor comfortably inside `[0.999, 1.001]`, which
/// rules out the overflow-to-infinity a finite-but-absurd ppm (e.g.
/// `f64::MAX`) would otherwise produce once multiplied against a real RF
/// frequency -- a factor merely being finite and positive isn't enough
/// (MAN-29 review round 2).
const MAX_ABS_PPM: f64 = 1_000.0;

/// Converts a ppm frequency-correction setting (config key
/// `input.freq_correction_ppm`, SPEC-decode-core.md §1.4) into the
/// multiplicative factor applied to a spot's reported frequency:
/// `factor = 1.0 + ppm * 1e-6`. Errors if `ppm` is outside
/// `[-MAX_ABS_PPM, MAX_ABS_PPM]`, or the result isn't finite and positive
/// (MAN-29).
pub fn calibration_factor_from_ppm(ppm: f64) -> Result<f64, InvalidCalibration> {
    let factor = 1.0 + ppm * 1e-6;
    if !ppm.is_finite() || ppm.abs() > MAX_ABS_PPM || !factor.is_finite() || factor <= 0.0 {
        return Err(InvalidCalibration { ppm, factor });
    }
    Ok(factor)
}

/// Per-reason counts of spots suppressed by an operator override (MAN-31),
/// and of captured deferred Beacon candidates (`PendingBeacon`) lost
/// before they could spot (PR #154, MAN-174). ARCHITECTURE §8: "Every dropped/evicted/suppressed item is counted. No
/// silent loss anywhere in the pipeline." Exposed via
/// `Validator::suppression_counts` for the future M3 metrics endpoint to
/// read, mirroring `manta_engine::track::CloseCounts` -- nothing wires it
/// externally yet, since the Prometheus text endpoint itself is explicit
/// M3 scope (ROADMAP.md).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SuppressionCounts {
    pub blocklist: u64,
    pub notch: u64,
    /// A captured `PendingBeacon` permanently discarded to keep
    /// `MAX_PENDING_BEACONS` bounded on a track that stays active
    /// indefinitely (Codex review on PR #154, round 9) -- ARCHITECTURE
    /// §8 requires every dropped item be counted, this one included.
    pub pending_beacon_overflow: u64,
    /// A captured `PendingBeacon` discarded because its track closed via
    /// `ClosureKind::Bookkeeping` with no survivor (`Evicted`) -- not
    /// evidence the physical signal ended, but with no successor
    /// track_id to migrate the evidence to either (round 9).
    pub pending_beacon_lost_to_eviction: u64,
    /// A captured `PendingBeacon` discarded at its track's `SignalEnded`
    /// close because the track's final speed was implausible (above
    /// `MAX_PLAUSIBLE_WPM`) or never measured (no `SpeedUpdate`): the WPM
    /// gate (MAN-174, ARCHITECTURE §8). A candidate the operator lists
    /// suppress at that moment is counted under `blocklist`/`notch`
    /// instead, never both.
    pub pending_beacon_rejected_wpm: u64,
    /// A captured `PendingBeacon` discarded at resolution because a spot
    /// for the same (callsign, frequency bucket) with a strictly newer
    /// `sample_ts` was already emitted, so emitting it would rewind
    /// `Dedupe`'s watermark (MAN-174) -- or, since MAN-120, because the
    /// same input's newer spot sits one bucket over within one channel.
    pub pending_beacon_stale: u64,
    /// Power-step beacon occurrences (MAN-37 `<call> T`) discarded by the
    /// coarse whole-window CQ/DE framing guard -- see `context::parse`'s own
    /// docs for why that guard is deliberately coarse. Counted once per
    /// OCCURRENCE, on the first burn of that occurrence: `try_spot`
    /// re-discovers and re-burns the same occurrence on every word boundary
    /// until the triggering token ages out of the 16-word window, so counting
    /// per burn call would report one missed beacon many times over (MAN-48,
    /// deferred from Codex review on PR #65, round 9).
    ///
    /// "Once per occurrence" is tracked by `Word::power_step_suppressed`, a
    /// bit private to this guard, NOT by the general `Word::attempted` flag:
    /// a word can already be `attempted` because some other pattern
    /// evaluated it and produced no spot (e.g. "CQ K5ARH T", where
    /// `CQ_CALL_RE` offers `K5ARH` as a `Cq` candidate that fails the
    /// two-repetition gate one boundary before the guard discards the
    /// repetition-exempt Beacon candidate), and gating on `attempted` made
    /// exactly those real, silent losses read as zero (Codex review on PR
    /// #90). Every occurrence this guard discards is counted whatever else
    /// happened to the same decoded word -- the Beacon classification was
    /// thrown away either way, which is what this metric measures -- with one
    /// exception: an occurrence already evaluated AS a Beacon before the
    /// guard appeared (a clean "K5ARH T" that spotted, then a bare CQ/DE
    /// arriving before it ages out) is re-burned but not counted, because the
    /// guard destroyed no beacon candidacy there (Codex review on PR #90,
    /// round 10). See `burn_suppressed_power_step_candidate`.
    pub power_step_guard: u64,
    /// Spots withheld by MAN-100's cross-candidate variant arbitration: a
    /// confusable, better-supported rival existed on the same track in the
    /// same window. See `support::SupportLedger`.
    pub variant: u64,
    /// MAN-14: candidate spots withheld by MAN-13 D6's cross-input dedupe
    /// -- a different input had already reported the same signal inside
    /// the window -- indexed by the WITHHELD input (`input_of` its track):
    /// its report proves it hears. One element per input
    /// (`with_input_count`); a same-input repeat is not counted here.
    pub cross_input_dedupe_by_input: Vec<u64>,
}

pub struct Validator {
    cty: Arc<cty::Table>,
    scp: Option<Arc<scp::Set>>,
    patterns: Arc<pattern::Table>,
    level: ValidationLevel,
    tracks: BTreeMap<u32, TrackState>,
    gate: RepetitionGate,
    dedupe: Dedupe,
    /// MAN-100 Scenario 1: per-track ledger of every observed,
    /// spottable-shaped decoded word, used to arbitrate between confusable
    /// candidates on the same track (see `support::SupportLedger`).
    ledger: SupportLedger,
    freq_calibration: f64,
    allowlist: std::collections::BTreeSet<String>,
    blocklist: Blocklist,
    notch: NotchList,
    suppression_counts: SuppressionCounts,
    /// The latest `sample_ts` seen across every track (not per-track --
    /// see `TrackState::last_sample_ts` for that), used to `sweep` the
    /// repetition gate on `TrackClosed` (MAN-166, Codex review PR #152).
    /// A closing track's *own* `last_sample_ts` is not a safe stand-in:
    /// it can be stale or still 0 (a track that emitted metadata but no
    /// `WordBoundary`), which would make that sweep a no-op and leave
    /// other, genuinely-expired gate entries growing unbounded forever --
    /// this field is the only thing that's actually monotonic across the
    /// whole `Validator`, matching SPEC-decode-core.md §6 rule 2
    /// (sample_ts-based, never wall clock).
    now_ts: u64,
    /// `now_ts` as of the last `gate.sweep()` call (Codex review, PR
    /// #152). `TrackClosed` alone isn't a sufficient sweep trigger: a
    /// persistently-active track that never closes (e.g. real noise/QRM
    /// continually feeding it new plausible-but-wrong callsigns, each
    /// resetting its own silent-GC timer) would otherwise let the gate
    /// grow unbounded for as long as it stays open, even though
    /// `now_ts` itself keeps advancing correctly. `maybe_sweep` also
    /// runs off every `now_ts`-bearing event, throttled by
    /// `sweep_interval_samples` so a busy pipeline doesn't pay for an
    /// O(gate size) scan on every single character.
    last_swept_ts: u64,
    sweep_interval_samples: u64,
    /// `GATE_WINDOW_SECONDS` in samples, cached at construction -- the
    /// prune cutoff for `merged_into` below.
    gate_window_samples: u64,
    /// track_id -> (its current best-known root survivor, `now_ts` at the
    /// moment it was closed via `ClosureKind::Bookkeeping`) -- a merge
    /// survivor exists, so the physical signal's identity continues, it's
    /// just renamed -- MAN-100 message-distinctness, Codex review PR #133
    /// rounds 3-4. `self.tracks.remove` (MAN-19) fires for a `Bookkeeping`
    /// closure exactly the same as a genuine `SignalEnded` one, so absence
    /// from `self.tracks` alone can't tell `gate::message_distinct_indices`'s
    /// `is_track_active` query "this track genuinely ended" from "this
    /// track's identity just moved to a survivor" -- without this, a
    /// duplicate-spawn track A merging into survivor B mid-transmission
    /// (A catches the first call utterance, B the second) reads as A
    /// having "closed," letting B's second word manufacture a false
    /// second message from what's still one over-the-air transmission.
    ///
    /// The root survivor is tracked (not just presence) so a later
    /// `SignalEnded` on the ROOT can retire every alias that ultimately
    /// points to it immediately (round 4): without this, A's alias
    /// outlives the survivor's own genuine close for up to the sweep
    /// window below, during which a brand-new, unrelated track C
    /// reopening at the same frequency gets every one of its own
    /// occurrences wrongly compared against A's stale entry and
    /// collapsed, suppressing a real repetition-gate confirmation.
    /// `resolve_survivor_root` handles chained merges (A into B, B later
    /// into C) by re-pointing existing aliases when their OWN root
    /// itself merges again, so a chain retires as one unit when its
    /// final root ends.
    ///
    /// Still pruned in lockstep with `gate.sweep()` (same
    /// `GATE_WINDOW_SECONDS` cutoff) as a memory-bound backstop (MAN-19)
    /// for a root that lives past this window without ever closing --
    /// past that window the repetition gate's own `accepted` list has
    /// already dropped any of the alias's occurrences regardless, so it
    /// can no longer affect message-distinctness either way.
    merged_into: BTreeMap<u32, (u32, u64)>,
    /// MAN-324: the opt-in decision log (`record_decisions`). `None`, the
    /// default, records nothing and allocates nothing.
    decisions: Option<Vec<Decision>>,
    /// MAN-13: inputs interleaving their track ids into this validator
    /// (`with_input_count`).
    input_count: u32,
}

/// How often (in seconds of `sample_ts`) the repetition gate is swept
/// independent of any `TrackClosed` event (MAN-166, Codex review PR
/// #152). Small relative to the gate's own 90s window (`gate::
/// WINDOW_SECONDS`) so a persistently-open, never-closing track can't
/// let it grow far past its steady-state size before the next sweep --
/// a reasoned choice, not yet measured against real long-run data.
const SWEEP_INTERVAL_SECONDS: f64 = 10.0;

impl Validator {
    /// Parses the supplied country and known-callsign tables.
    pub fn new(fs: f64, cty_dat: &str, master_scp: Option<&str>) -> Self {
        Self::from_tables(
            fs,
            Arc::new(cty::Table::parse(cty_dat)),
            master_scp.map(|s| Arc::new(scp::Set::parse(s))),
        )
    }

    /// Shares already parsed tables with other consumers, such as JSON geography lookup.
    pub fn from_tables(fs: f64, cty: Arc<cty::Table>, scp: Option<Arc<scp::Set>>) -> Self {
        Self {
            cty,
            scp,
            patterns: Arc::new(pattern::Table::bundled()),
            level: ValidationLevel::Normal,
            tracks: BTreeMap::new(),
            gate: RepetitionGate::new(fs),
            dedupe: Dedupe::new(fs),
            ledger: SupportLedger::new(fs),
            freq_calibration: 1.0,
            allowlist: std::collections::BTreeSet::new(),
            blocklist: Blocklist::default(),
            notch: NotchList::default(),
            suppression_counts: SuppressionCounts {
                cross_input_dedupe_by_input: vec![0],
                ..SuppressionCounts::default()
            },
            now_ts: 0,
            last_swept_ts: 0,
            sweep_interval_samples: (SWEEP_INTERVAL_SECONDS * fs) as u64,
            gate_window_samples: (GATE_WINDOW_SECONDS * fs) as u64,
            merged_into: BTreeMap::new(),
            decisions: None,
            input_count: 1,
        }
    }

    /// MAN-13: the number of inputs whose interleaved track ids feed this
    /// validator. Input `i` of `n` numbers its tracks `1+i, 1+i+n, ...`
    /// (`TrackIdSpace` in `manta-engine`), so a track's input is
    /// `(track_id - 1) % n`, and dedupe treats a different input's report
    /// of a stored spot as a duplicate for the whole window (D6). `1` (the
    /// default) is the single-input validator, unchanged. `0` is taken as 1.
    pub fn with_input_count(mut self, n: u32) -> Self {
        self.input_count = n.max(1);
        self.suppression_counts
            .cross_input_dedupe_by_input
            .resize(self.input_count as usize, 0);
        self
    }

    /// MAN-14: counts a cross-input dedupe withhold against the withheld
    /// input; a same-input repeat is not counted.
    fn count_cross_input_dedupe(&mut self, outcome: DedupeOutcome, input: u32) {
        if outcome == DedupeOutcome::WithheldCrossInput {
            if let Some(n) = self
                .suppression_counts
                .cross_input_dedupe_by_input
                .get_mut(input as usize)
            {
                *n += 1;
            }
        }
    }

    /// MAN-13: the input that spawned `track_id` (see `with_input_count`).
    pub fn input_of(&self, track_id: u32) -> u32 {
        track_id.saturating_sub(1) % self.input_count
    }

    /// Replace the independent callsign-pattern reference table.
    pub fn with_patterns(mut self, patterns: Arc<pattern::Table>) -> Self {
        self.patterns = patterns;
        self
    }

    /// Select the rarity-tiered or flat repetition requirement.
    pub fn with_validation_level(mut self, level: ValidationLevel) -> Self {
        self.level = level;
        self
    }

    /// MAN-324: turn the decision log on or off. On, every validation
    /// decision (spotted, withheld with its reason, or a deferred beacon)
    /// is kept until `take_decisions`. Off discards anything unread.
    /// Recording never changes which spots are emitted.
    pub fn record_decisions(&mut self, on: bool) {
        self.decisions = if on { Some(Vec::new()) } else { None };
    }

    /// Whether `record_decisions` is on.
    pub fn records_decisions(&self) -> bool {
        self.decisions.is_some()
    }

    /// The decisions recorded since the last call, oldest first. Empty
    /// when recording is off.
    pub fn take_decisions(&mut self) -> Vec<Decision> {
        self.decisions
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    }

    /// MAN-324: the grammar- and cty-plausible words decoded on
    /// `track_id` within the support window, as the variant arbitration
    /// weighs them, strongest first (ties by call ascending). Empty once
    /// the track has closed.
    pub fn candidates_for(&self, track_id: u32, now: u64) -> Vec<CandidateSupport> {
        self.ledger
            .candidates(track_id, now)
            .into_iter()
            .map(|(call, support)| CandidateSupport { call, support })
            .collect()
    }

    /// Records one decision when the log is on; `build` runs only then.
    fn decide(&mut self, build: impl FnOnce() -> Decision) {
        if let Some(log) = self.decisions.as_mut() {
            let mut decision = build();
            // One boundary covers early refusals, reclassification and every
            // deferred-beacon path, without doing pattern lookups when logging is off.
            if decision.spot_type != SpotType::Beacon && !self.allowlist.contains(&decision.call) {
                let tier = self.patterns.tier(&decision.call);
                decision.required_reps = self.level.required_reps(tier);
                decision.pattern_tier = Some(tier);
            }
            log.push(decision);
        }
    }

    /// Advances `now_ts` to `sample_ts` (if later), then `maybe_sweep`s.
    /// Called from every event carrying a real `sample_ts` -- see
    /// `now_ts`'s doc for why `TrackClosed` alone isn't a sufficient
    /// sweep trigger on its own.
    fn advance_clock(&mut self, sample_ts: u64) {
        self.now_ts = self.now_ts.max(sample_ts);
        self.maybe_sweep();
    }

    /// Sweeps the repetition gate if `sweep_interval_samples` has elapsed
    /// since the last sweep -- throttled the same way regardless of
    /// caller, `TrackClosed` included: under real sustained track churn
    /// (tens of thousands of closes in a 15-minute recording, MAN-166) an
    /// *unthrottled* per-close sweep would mean an O(gate size) scan on
    /// every single close, reintroducing the cost problem periodic
    /// sweeping exists to bound.
    fn maybe_sweep(&mut self) {
        if self.now_ts.saturating_sub(self.last_swept_ts) >= self.sweep_interval_samples {
            self.gate.sweep(self.now_ts);
            // `merged_into` bounded the same way, on the same cadence, as
            // a memory-bound backstop for a root that lives past this
            // window without ever closing (its own `SignalEnded` retires
            // its aliases immediately -- see the field's own doc): past
            // the gate's own window a merged-away track's lineage can no
            // longer affect message-distinctness (its occurrences are
            // already gone from `accepted`), so it's safe -- and
            // necessary, for MAN-19's unbounded-growth concern -- to drop.
            let cutoff = self.now_ts.saturating_sub(self.gate_window_samples);
            self.merged_into
                .retain(|_, &mut (_, merged_ts)| merged_ts >= cutoff);
            self.last_swept_ts = self.now_ts;
        }
    }

    /// A production `Validator` backed by this crate's bundled `cty.dat`/
    /// `MASTER.SCP` snapshot (`crate::CTY_DAT`/`crate::MASTER_SCP`).
    pub fn bundled(fs: f64) -> Self {
        Self::new(fs, crate::CTY_DAT, Some(crate::MASTER_SCP))
    }

    /// Sets the per-source frequency-calibration correction (config key
    /// `input.freq_correction_ppm`, SPEC-decode-core.md §1.4 -- the
    /// oscillator-accuracy setting that section names as out of scope for
    /// the ±10 Hz decode-accuracy figure it defines, ARCHITECTURE §6 step
    /// 5). Corrects a systematically drifted source clock/LO (legacy
    /// precedent: CW Skimmer/SkimSrv's `FreqCalibration=` .ini key, though
    /// that key is a raw multiplier -- this crate's contract is ppm, per
    /// the spec). Applied to a spot's reported frequency before emission
    /// (MAN-29). Errors if `ppm` is outside the supported range
    /// `[-1000, 1000]` (any physically-plausible oscillator drift; see
    /// `MAX_ABS_PPM`), or doesn't yield a finite, positive correction
    /// factor within that range.
    pub fn with_freq_correction_ppm(mut self, ppm: f64) -> Result<Self, InvalidCalibration> {
        self.freq_calibration = calibration_factor_from_ppm(ppm)?;
        Ok(self)
    }

    /// Adds `call` to the operator's Watch List (MAN-28): an explicitly
    /// allowlisted callsign bypasses grammar/cty validation and the
    /// repetition gate entirely, matching CW Skimmer's Watch List
    /// behavior (Aggregator manual Appendix A2). Checked after the MAN-31
    /// suppression overrides below -- an explicit blocklist/notch entry is
    /// the more specific, deliberate override and is never silently
    /// defeated by a broader allowlist entry.
    pub fn allowlist(&mut self, call: &str) {
        self.allowlist.insert(call.to_ascii_uppercase());
    }

    /// Cumulative `RepetitionGate::record` calls for life -- see that
    /// method's doc. MAN-19 round 3: direct evidence the gate (and so
    /// `forget_track`'s teardown) was ever exercised at all.
    pub fn gate_records_total(&self) -> u64 {
        self.gate.records_total()
    }

    /// MAN-118: the distribution of every `RepetitionGate::record` result
    /// so far, cumulative for life, for the daemon's
    /// `manta_candidate_repetitions` histogram. Read after `ingest`, as
    /// `suppression_counts` is.
    pub fn repetition_counts(&self) -> RepetitionCounts {
        self.gate.repetition_counts()
    }

    /// Sets the operator's bad-callsign blocklist (MAN-31). Empty by
    /// default -- no suppression until the operator supplies one.
    pub fn with_blocklist(mut self, blocklist: Blocklist) -> Self {
        self.blocklist = blocklist;
        self
    }

    /// Sets the operator's notched-frequency list (MAN-31). Empty by
    /// default -- no suppression until the operator supplies one.
    pub fn with_notch(mut self, notch: NotchList) -> Self {
        self.notch = notch;
        self
    }

    /// MAN-78: replaces the operator's three lists as one unit, for a live
    /// config reload. Tracks, dedupe, the repetition gate, the ledger and
    /// `suppression_counts` are untouched; a word already attempted under the
    /// old lists is not re-evaluated, so the new lists apply from the next
    /// decoder event.
    pub fn replace_operator_lists(
        &mut self,
        allowlist: &[String],
        blocklist: Blocklist,
        notch: NotchList,
    ) {
        self.allowlist = allowlist.iter().map(|c| c.to_ascii_uppercase()).collect();
        self.blocklist = blocklist;
        self.notch = notch;
    }

    /// MAN-316: replaces the country table and known-call set, for a
    /// scheduled list refresh. Like `replace_operator_lists`, tracks, dedupe,
    /// the repetition gate, the ledger and `suppression_counts` are
    /// untouched; the new tables apply from the next decoder event.
    pub fn replace_reference_tables(&mut self, cty: Arc<cty::Table>, scp: Option<Arc<scp::Set>>) {
        self.cty = cty;
        self.scp = scp;
    }

    /// Per-reason counts of operator-suppressed spots so far (MAN-31,
    /// ARCHITECTURE §8).
    pub fn suppression_counts(&self) -> SuppressionCounts {
        self.suppression_counts.clone()
    }

    /// Feeds one decoder event in. Returns zero or more validated spots
    /// (almost always zero -- a spot only comes out on the event that
    /// completes a passing candidate's word).
    pub fn ingest(&mut self, event: &DecoderEvent) -> Vec<Spot> {
        match event {
            DecoderEvent::CharDecoded {
                track_id,
                glyph,
                confidence,
                sample_ts,
                ..
            } => {
                self.advance_clock(*sample_ts);
                let track = self.tracks.entry(*track_id).or_default();
                match glyph {
                    Glyph::Char(c) => {
                        track.current.text.push(c.to_ascii_uppercase());
                        track.current.confidences.push(*confidence);
                    }
                    Glyph::Prosign(Prosign::Err) => {
                        // SPEC §4.4: operator-error prosign discards the
                        // current word buffer back to the previous
                        // boundary.
                        track.current = Word::default();
                    }
                    Glyph::Prosign(_) => {}
                }
                Vec::new()
            }
            DecoderEvent::WordBoundary {
                track_id,
                sample_ts,
                ..
            } => {
                self.advance_clock(*sample_ts);
                let track = self.tracks.entry(*track_id).or_default();
                let mut newly_completed: Option<(String, u64, f32)> = None;
                if !track.current.text.is_empty() {
                    let mut word = std::mem::take(&mut track.current);
                    // MAN-272: scan the word that just completed, so "most
                    // recent RST" is a per-track fact that survives
                    // WORD_WINDOW aging (D4).
                    if let Some(rst) = message::parse_rst(&word.text) {
                        track.rst = Some(rst);
                    }
                    track.qrl_query |= message::is_qrl_query(&word.text);
                    word.seq = track.next_word_seq;
                    track.next_word_seq += 1;
                    newly_completed = Some((
                        word.text.clone(),
                        word.seq,
                        confidence::geo_mean(&word.confidences),
                    ));
                    track.words.push_back(word);
                    if track.words.len() > WORD_WINDOW {
                        track.words.pop_front();
                    }
                }
                track.last_sample_ts = *sample_ts;
                // MAN-100 Scenario 1: the ledger records every observed
                // word that is ITSELF spottable-shaped (grammar + cty) --
                // a form that could never be spotted must not be able to
                // veto one that could. Gated on the same two checks
                // `evaluate_candidate` runs for a non-allowlisted
                // candidate below, so the ledger's population is never
                // wider than what could plausibly spot.
                if let Some((text, seq, geo)) = newly_completed {
                    if grammar::is_plausible(&text) && self.cty.is_allocated(&text) {
                        self.ledger.observe(*track_id, &text, seq, *sample_ts, geo);
                    }
                }
                self.try_spot(*track_id, *sample_ts)
            }
            DecoderEvent::SpeedUpdate { track_id, wpm } => {
                let track = self.tracks.entry(*track_id).or_default();
                track.wpm = *wpm;
                track.wpm_confirmed = true;
                // No retry here, and no Beacon-WPM decision here either
                // (round 7 redesign) -- a non-allowlisted Beacon
                // candidate is only ever judged once, at `TrackClosed`,
                // against whatever `pending_beacons` has accumulated.
                // Rounds 2-7 each found a new way a reactive check tied
                // to a live, evolving WPM reading could fire on a
                // transient value in either direction; a live SpeedUpdate
                // now only ever records the value, nothing more.
                Vec::new()
            }
            DecoderEvent::TrackMeta {
                track_id,
                snr_2500_db,
                freq_hz,
                ..
            } => {
                let track = self.tracks.entry(*track_id).or_default();
                let had_meta = track.has_meta;
                track.snr_db = *snr_2500_db;
                track.freq_hz = *freq_hz;
                track.has_meta = true;
                let sample_ts = track.last_sample_ts;
                // Retry: a candidate held back by the has_meta gate is
                // otherwise only ever re-evaluated by a later
                // WordBoundary, which a short transmission may never
                // produce again -- silently losing the pending exemption
                // (MAN-28 round 9 review). No-op once already true.
                if had_meta {
                    Vec::new()
                } else {
                    self.try_spot(*track_id, sample_ts)
                }
            }
            // Ground-truth "detector found a candidate" signal for
            // `manta_engine::doctor()` (its NoSignal check), with nothing
            // for the Validator itself to do -- deliberately never touches
            // `self.tracks`, so a track promoted and merged/evicted away
            // before producing any other event (the exact case this event
            // exists to surface) creates no per-track_id state here to
            // leak.
            DecoderEvent::TrackPromoted { sample_ts, .. } => {
                self.advance_clock(*sample_ts);
                Vec::new()
            }
            // Sender-timing diagnostics (MAN-315): nothing to validate.
            DecoderEvent::ReadingsHeld { .. } | DecoderEvent::TimingUpdate { .. } => Vec::new(),
            DecoderEvent::TrackClosed { track_id, closure } => {
                // Round 7 redesign (PR #154): `SignalEnded` is the ONE point where
                // every captured non-allowlisted Beacon candidate for
                // this track is judged, using its TRUE FINAL speed --
                // manta-engine guarantees a final speed flush
                // (TrackDecoder::finish()/finish_speed_only()) is ordered
                // immediately before this TrackClosed for the same track.
                // See `resolve_pending_beacons` and `PendingBeacon`'s doc
                // comments for why this replaced rounds 2-6's reactive,
                // opportunistic-evaluation-plus-retry design.
                //
                // `Bookkeeping` (round 9) is NOT proof the signal ended --
                // the identity may continue on a merge survivor, or (an
                // eviction) simply drop out of tracking while still
                // transmitting. Resolving now against a not-yet-settled
                // WPM would reopen exactly the oscillation risk the
                // deferred design exists to close, so this migrates
                // pending evidence to the survivor instead (to be judged
                // by ITS OWN eventual true close), or discards it if there
                // is none.
                let spots = match closure {
                    ClosureKind::SignalEnded => {
                        // MAN-100 message-distinctness (Codex review, PR
                        // #133 round 4): if OTHER track_ids are known
                        // aliases whose lineage ultimately points to this
                        // one, this track's genuine end retires them too,
                        // immediately -- not just whenever the time-based
                        // sweep below gets around to it. Without this, an
                        // alias can outlive its own root's real close for
                        // up to the sweep window, wrongly suppressing a
                        // brand-new, unrelated track's repetition-gate
                        // confirmation at the same frequency in the
                        // meantime.
                        self.merged_into.retain(|_, (root, _)| *root != *track_id);
                        self.resolve_pending_beacons(*track_id)
                    }
                    ClosureKind::Bookkeeping { survivor_track_id } => {
                        // MAN-174: resolve the named survivor through
                        // `merged_into` BEFORE migrating. manta-engine
                        // orders a merge loser's close ahead of its
                        // survivor's, so this is normally the survivor
                        // itself; if closures ever arrive out of order
                        // (the survivor already merged further), the
                        // evidence lands on the live root instead of
                        // recreating a dead track_id's state that no
                        // close would ever free.
                        let root = survivor_track_id.map(|s| self.resolve_survivor_root(s));
                        // RST/QRL? annotations are deliberately NOT
                        // migrated: they end with this track's `TrackState`
                        // like its words do (MAN-272 D5, see
                        // docs/DECISIONS/2026-10-16-man272-rst-and-qrl-on-spots.md).
                        self.migrate_or_discard_pending_beacons(*track_id, root);
                        // MAN-100 message-distinctness (Codex review, PR
                        // #133 rounds 3-5): a survivor exists, so this
                        // track_id's identity continues rather than
                        // genuinely ending -- record that so
                        // `gate::message_distinct_indices`'s
                        // `is_track_active` query doesn't read this as a
                        // real close and let the survivor's next word
                        // manufacture a false second message from the
                        // SAME transmission.
                        if let Some(root) = root {
                            // A chained merge (A already an alias into
                            // THIS track_id, which is now itself merging
                            // further into `root`): re-point A straight
                            // to the new root so its own eventual
                            // `SignalEnded` retirement above still finds
                            // it, instead of leaving A pointed at this
                            // now-closing intermediate track_id forever.
                            for (existing_root, _) in self.merged_into.values_mut() {
                                if *existing_root == *track_id {
                                    *existing_root = root;
                                }
                            }
                            self.merged_into.insert(*track_id, (root, self.now_ts));
                        } else {
                            // An eviction/Silent closure (`survivor_track_id:
                            // None`) has no lineage to correlate to a future
                            // reopen, but per this event's own doc comment
                            // it is STILL not evidence the RF signal ended --
                            // it just dropped out of tracking, possibly
                            // still transmitting (round 5). Mark the
                            // track_id conservatively active under its OWN
                            // identity (self-referencing root) rather than
                            // reading it as closed the instant it's evicted:
                            // without this, a duplicate/replacement track
                            // decoding the transmission's later utterance
                            // shortly after this eviction could manufacture
                            // a false second message. No `SignalEnded` will
                            // ever arrive for an evicted track_id to retire
                            // this early (track_ids are never reused), so it
                            // decays only via the time-based sweep below --
                            // the same conservative, bounded lifetime a
                            // no-lineage closure already gets nowhere else.
                            self.merged_into.insert(*track_id, (*track_id, self.now_ts));
                        }
                        Vec::new()
                    }
                };
                // MAN-19: without removing `self.tracks`' entry,
                // per-track_id word/grammar-context state grows forever --
                // `TrackManager` never reuses a `track_id`, and until
                // `TrackClosed` existed nothing had any signal that one
                // would never be seen again. Confirmed as the soak's
                // actual unbounded-RSS-growth root cause under sustained
                // track churn.
                //
                // `self.gate` is swept, not forgotten by `track_id`
                // (MAN-166): the repetition gate is keyed by frequency
                // bucket, which a closing-and-reopening real signal keeps
                // across the churn this event represents -- forgetting it
                // here, the way `self.tracks` correctly is, would defeat
                // the gate's whole 90s window the instant a track closed.
                // `maybe_sweep` still bounds `gate`'s memory the way
                // MAN-19 needed, just on elapsed time instead of track
                // lifetime -- and unlike this event alone (Codex review,
                // PR #152), it also runs off every other `sample_ts`-
                // bearing event via `advance_clock`, so a persistently-
                // active track that never closes at all still gets swept.
                // See `now_ts`'s and `maybe_sweep`'s own docs for why a
                // closing track's own (possibly stale/zero)
                // `last_sample_ts` was never a safe sweep reference on its
                // own.
                self.tracks.remove(track_id);
                // MAN-100 Scenario 1: unlike `self.gate` above, the
                // variant-arbitration ledger is still keyed by `track_id`
                // (mirrors the pre-MAN-166 `RepetitionGate::forget_track`
                // pattern), so it's still forgotten on close rather than
                // swept. A real signal's track closing and reopening
                // under a new `track_id` (MAN-166) resets its observed-
                // word history here, the same class of bug MAN-166 fixed
                // for the gate. MAN-108 measured that case (a Watterson
                // fade outlasting the 5 s hang) and removed it at the
                // source: a track that has decoded goes DORMANT for up to
                // `hang_ms + reacquire_ms` and keeps its `track_id` when
                // the signal returns, so no `TrackClosed` arrives and this
                // state, `self.tracks`' and the MAN-272 annotations all
                // survive such a fade. A signal gone longer than that
                // still starts over here.
                self.ledger.forget_track(*track_id);
                self.maybe_sweep();
                spots
            }
        }
    }

    /// Gathers every candidate word worth evaluating this event: every
    /// match `context::parse` finds, plus every allowlisted word in the
    /// window not yet attempted. Independent sources, not one-or-the-
    /// other by priority (MAN-28 round 7 review): a stale, already-
    /// attempted context match elsewhere in the 16-word window must never
    /// block discovery of a different, freshly-allowlisted word -- the
    /// whole window is scanned (not just the newest word) so a qualifying
    /// word is found the moment it's allowlisted, `word.attempted`
    /// (checked in `evaluate_candidate`) prevents re-processing one this
    /// already spotted or rejected.
    ///
    /// `context::parse` can return more than one match (e.g. a named
    /// pattern naming one callsign and the power-step fallback naming a
    /// different, newer one, or even the same callsign from both) --
    /// `parse` itself no longer picks a winner between them (Codex review
    /// on PR #65, rounds 2-3), so every match becomes its own candidate
    /// here. Each candidate carries the highest `Word::seq` among the
    /// words that produced it, so `evaluate_candidate` can tell a genuine
    /// reclassification (a newer word contributed) from a type merely
    /// changing because an older one aged out (MAN-28 round 12 review) --
    /// this is also what reconciles two candidates that both map to the
    /// SAME decoded word (its own seq-based provenance guard decides
    /// whether the second is a genuine reclassification), rather than a
    /// text-position heuristic inside `context::parse`.
    ///
    /// The 4th element is `Some(exact seq)` for a power-step-origin
    /// candidate -- the specific `Word` the regex actually captured, found
    /// by matching `context::parse`'s exact-call-range against a word's
    /// own span -- or `None` for a named-pattern-origin one, which
    /// `evaluate_candidate` resolves by text instead (see `context::parse`'s
    /// own docs for why the two pattern families need different
    /// resolution strategies; Codex review on PR #65, round 9).
    fn candidates(&self, track_id: u32) -> Vec<(String, SpotType, u64, Option<u64>)> {
        let Some(track) = self.tracks.get(&track_id) else {
            return Vec::new();
        };
        let mut candidates = Vec::new();

        // Byte range of each word within `joined`, in the same order as
        // `track.words`, to map a context match's span back to the
        // word(s) that produced it.
        let mut joined = String::new();
        let mut word_spans = Vec::with_capacity(track.words.len());
        for word in &track.words {
            if !joined.is_empty() {
                joined.push(' ');
            }
            let start = joined.len();
            joined.push_str(&word.text);
            word_spans.push((start, joined.len(), word.seq));
        }
        for (candidate, spot_type, range, exact_range) in context::parse(&joined) {
            let involved_max_seq = word_spans
                .iter()
                .filter(|(start, end, _)| *start < range.end && range.start < *end)
                .map(|(_, _, seq)| *seq)
                .max()
                .unwrap_or(0);
            match exact_range {
                // Named-pattern origin: no exact-word binding by design,
                // resolved by text in evaluate_candidate (see context::
                // parse's own docs on why -- V29 needs it).
                None => candidates.push((candidate, spot_type, involved_max_seq, None)),
                // Power-step origin MUST bind to the exact word or be
                // discarded entirely -- falling back to a text search here
                // is exactly the bug this whole mechanism exists to close.
                // A capture landing on only PART of a decoded word (e.g.
                // the regex's `\b` matching mid-word after a punctuation
                // character glued onto a callsign, "-K5ARH") produces a
                // range that doesn't equal any word's own span; resolving
                // it by text could then bind this match to a wholly
                // unrelated, unsuppressed word that merely happens to
                // share the callsign's text (Codex review on PR #65,
                // round 10).
                Some(r) => {
                    if let Some(seq) = word_spans
                        .iter()
                        .find(|(start, end, _)| *start == r.start && *end == r.end)
                        .map(|(_, _, seq)| *seq)
                    {
                        candidates.push((candidate, spot_type, involved_max_seq, Some(seq)));
                    }
                }
            }
        }

        // MAN-28 Watch List: an allowlisted word is found independently of
        // context parsing -- including with no recognized CQ/DE/UP/beacon
        // pattern at all, the primary real-world case (an NCDXF beacon
        // transmits its callsign followed by power-step dashes, no
        // framing words). `SpotType::Unknown` is the context-parse-
        // documented fallback for exactly this case.
        for word in &track.words {
            if self.allowlist.contains(&word.text)
                && !candidates.iter().any(|(c, _, _, _)| *c == word.text)
            {
                candidates.push((word.text.clone(), SpotType::Unknown, word.seq, None));
            }
        }

        candidates
    }

    /// Every power-step match `context::parse` currently withholds because
    /// of its CQ/DE guard (MAN-37), paired with the highest `Word::seq`
    /// involved (same computation `candidates` does for accepted matches)
    /// and the exact seq of the specific `Word` the regex captured (see
    /// `candidates`'s own docs on why power-step candidates need exact,
    /// not text-based, word identity). Not filtered against `accepted`: a
    /// word can be BOTH an accepted candidate through a different,
    /// narrower-ranged match (e.g. "CQ K5ARH T" -- CQ_CALL_RE resolves "CQ
    /// K5ARH" with no filler at all, but that match's own range doesn't
    /// cover the trailing "T") AND have its power-step candidacy on the
    /// SAME word suppressed by the whole-window CQ/DE guard; burning must
    /// still record that the trailing word was already considered, or the
    /// accepted match's own, narrower `classified_max_seq` won't account
    /// for it (MAN-37 review).
    fn suppressed_power_step_candidates(&self, track_id: u32) -> Vec<(String, u64, u64)> {
        let Some(track) = self.tracks.get(&track_id) else {
            return Vec::new();
        };
        let mut joined = String::new();
        let mut word_spans = Vec::with_capacity(track.words.len());
        for word in &track.words {
            if !joined.is_empty() {
                joined.push(' ');
            }
            let start = joined.len();
            joined.push_str(&word.text);
            word_spans.push((start, joined.len(), word.seq));
        }
        if !context::power_step_framing_is_unresolved(&joined) {
            return Vec::new();
        }
        context::power_step_candidates(&joined)
            .into_iter()
            .filter_map(|(call, range, call_range)| {
                let involved_max_seq = word_spans
                    .iter()
                    .filter(|(start, end, _)| *start < range.end && range.start < *end)
                    .map(|(_, _, seq)| *seq)
                    .max()
                    .unwrap_or(0);
                // The exact word the regex captured must be identifiable,
                // or there's nothing to bind this candidate to -- skip
                // rather than fall back to a text search, which is
                // precisely the ambiguity this whole mechanism exists to
                // avoid (Codex review on PR #65, round 9).
                let exact_seq = word_spans
                    .iter()
                    .find(|(start, end, _)| *start == call_range.start && *end == call_range.end)
                    .map(|(_, _, seq)| *seq)?;
                Some((call, involved_max_seq, exact_seq))
            })
            .collect()
    }

    /// Marks a suppressed power-step candidate's decoded word as
    /// `attempted`, with `classified_max_seq` raised to cover it -- with
    /// no spot -- so the CQ/DE guard's suppression survives the word aging
    /// out of the 16-word window. Without this, a withheld candidate never
    /// reaches `evaluate_candidate` at all, so its word's `attempted`/
    /// `classified_max_seq` never account for it; once the CQ/DE token
    /// that triggered the guard ages out, the SAME occurrence -- no newer
    /// evidence, nothing new decoded -- looks like fresh evidence to the
    /// aging-out guard and passes it, spotting as if freshly seen (Codex
    /// review on PR #65, round 7). `classified_max_seq` is only ever
    /// raised (`max`), never lowered, so an accepted classification from
    /// `evaluate_candidate` -- evaluated separately, in either order --
    /// is never weakened, only ever given a fuller picture of what's
    /// already been considered. A genuinely newer word arriving later
    /// still gets a fair, real reclassification, exactly as before.
    ///
    /// Resolved by `exact_seq`, not by text -- the exact `Word` the regex
    /// captured, not whichever word currently shares its text. Otherwise a
    /// stale, already-suppressed match's callsign could get bound to a
    /// brand-new, unrelated word decoded later that merely shares the same
    /// callsign string (Codex review on PR #65, round 9).
    ///
    /// The first burn of an occurrence also counts it against
    /// `SuppressionCounts::power_step_guard` (ARCHITECTURE §8: every
    /// suppressed item is counted). The gate for that is the guard's own
    /// `Word::power_step_suppressed` bit, not the general `attempted` flag:
    /// `attempted` is shared with named-pattern evaluation, so a word some
    /// other pattern had already touched without spotting it ("CQ K5ARH T")
    /// had its very real guard suppression counted as zero (MAN-48, Codex
    /// review on PR #90). `attempted` is still SET here -- that's what makes
    /// the suppression survive the triggering token aging out, as described
    /// above -- it just no longer decides whether to count.
    ///
    /// A word whose Beacon candidacy was ALREADY evaluated before the guard
    /// appeared (`last_spot_type == Some(Beacon)`) is burned but NOT counted:
    /// a clean "K5ARH T" resolves and emits, and only then does a bare CQ/DE
    /// enter the rolling window and make this guard re-discover the same,
    /// already-processed occurrence. Burning it is still right -- it stops
    /// the occurrence re-spotting once the triggering token ages out -- but
    /// the guard cost the operator no beacon there, so counting it would
    /// report a miss that never happened and inflate the metric on exactly
    /// the windows the guard handled well (MAN-48, Codex review on PR #90,
    /// round 10). Only the power-step family and `BEACON_RE` ever produce
    /// `Beacon`, and both mean the same thing here: this word's beacon
    /// classification already got its evaluation.
    fn burn_suppressed_power_step_candidate(
        &mut self,
        track_id: u32,
        call: &str,
        exact_seq: u64,
        involved_max_seq: u64,
        sample_ts: u64,
    ) {
        let (first_suppression, freq_hz) = {
            let Some(track) = self.tracks.get_mut(&track_id) else {
                return;
            };
            let Some(word) = track.words.iter_mut().find(|w| w.seq == exact_seq) else {
                return;
            };
            let involved_max_seq = involved_max_seq.max(word.seq);
            // An occurrence whose Beacon candidacy was already evaluated
            // before the guard appeared lost nothing to the guard -- see
            // this function's own docs.
            let already_processed = word.last_spot_type == Some(SpotType::Beacon);
            let first_suppression = !word.power_step_suppressed && !already_processed;
            word.power_step_suppressed = true;
            word.attempted = true;
            word.classified_max_seq = word.classified_max_seq.max(involved_max_seq);
            (first_suppression, track.freq_hz * self.freq_calibration)
        };
        if first_suppression {
            self.suppression_counts.power_step_guard += 1;
            self.decide(|| {
                Decision::withheld(
                    track_id,
                    sample_ts,
                    call,
                    freq_hz,
                    SpotType::Beacon,
                    Reason::PowerStepGuard,
                )
            });
        }
    }

    fn try_spot(&mut self, track_id: u32, sample_ts: u64) -> Vec<Spot> {
        // No real TrackMeta yet -- freq_hz/snr_db still hold bogus 0.0
        // defaults. Bail without marking anything attempted, so pending
        // candidates are simply re-evaluated once metadata does arrive.
        // Not a decision, so nothing is recorded (MAN-324).
        if !self.tracks.get(&track_id).is_some_and(|t| t.has_meta) {
            return Vec::new();
        }
        let candidates = self.candidates(track_id);
        // Suppressed candidates must be computed before evaluating the
        // accepted list below (which can mutate track.words), but burned
        // only after: evaluate_candidate assigns classified_max_seq
        // directly (not via max), so burning first would let an accepted
        // evaluation of the SAME word silently overwrite it back down.
        // Burning's own max()-merge afterward is what makes the order
        // safe -- it only ever raises the bar, never lowers it.
        let suppressed = self.suppressed_power_step_candidates(track_id);
        let spots = candidates
            .into_iter()
            .filter_map(|(candidate, spot_type, involved_max_seq, exact_seq)| {
                self.evaluate_candidate(
                    track_id,
                    sample_ts,
                    candidate,
                    spot_type,
                    involved_max_seq,
                    exact_seq,
                )
            })
            .collect();
        for (call, involved_max_seq, exact_seq) in suppressed {
            // Identity resolved via exact_seq, not text -- see burn's own docs.
            self.burn_suppressed_power_step_candidate(
                track_id,
                &call,
                exact_seq,
                involved_max_seq,
                sample_ts,
            );
        }
        spots
    }

    fn evaluate_candidate(
        &mut self,
        track_id: u32,
        sample_ts: u64,
        candidate: String,
        spot_type: SpotType,
        involved_max_seq: u64,
        exact_seq: Option<u64>,
    ) -> Option<Spot> {
        // Round 7 redesign: a non-allowlisted Beacon candidate is NEVER
        // evaluated/emitted opportunistically here -- only captured (once
        // blocklist/notch/grammar/cty confirm it isn't a permanent
        // reject), then judged exactly once at the track's true close.
        // See `capture_pending_beacon`/`resolve_pending_beacons` and
        // `PendingBeacon`'s doc comment for why: rounds 2-6 each found a
        // new way a reactive check tied to a live, evolving WPM reading
        // could misfire on a transient value, in either direction.
        // Allowlisted Beacon candidates are unaffected -- they fall
        // through to the unchanged logic below, exactly as before.
        if spot_type == SpotType::Beacon && !self.allowlist.contains(&candidate) {
            return self.capture_pending_beacon(
                track_id,
                sample_ts,
                candidate,
                involved_max_seq,
                exact_seq,
            );
        }

        let (freq_hz, snr_db, wpm) = {
            let track = self.tracks.get(&track_id)?;
            (
                track.freq_hz * self.freq_calibration,
                track.snr_db,
                track.wpm,
            )
        };

        let (char_confidences, reclassifying, resolved_word_seq) = {
            let track = self.tracks.get_mut(&track_id)?;
            // Named patterns resolve by text, always to the NEWEST word
            // sharing it (MAN-28 round 13, V29 -- a repeated "DE K5ARH ...
            // DE K5ARH" must credit the newest occurrence, not whichever
            // one DE_RE's own match happens to describe). Power-step
            // candidates instead carry `exact_seq`, the specific `Word`
            // context::parse's regex actually captured -- a text search
            // here could otherwise bind a stale, already-suppressed
            // match's callsign to an unrelated, brand-new same-text word
            // (Codex review on PR #65, round 9). The two pattern families
            // need opposite resolution strategies; see context::parse's
            // own docs for why both are genuine, already-tested
            // requirements.
            let word = if let Some(seq) = exact_seq {
                track.words.iter_mut().find(|w| w.seq == seq)?
            } else {
                track.words.iter_mut().rev().find(|w| w.text == candidate)?
            };
            // Clamping to the selected word's own seq guarantees
            // involved_max_seq is never understated relative to the
            // occurrence actually being evaluated (a word's own seq is
            // always a valid lower bound on its true provenance), closing
            // the mismatch that let a stale, first-occurrence-derived seq
            // pass the aging-out guard above as if it were genuinely new
            // context (MAN-28 round 13 review).
            let involved_max_seq = involved_max_seq.max(word.seq);
            if word.attempted {
                // A prior attempt is only a genuine reclassification --
                // not a re-attempt of stale information -- if a word
                // strictly younger than any that produced the previous
                // classification is involved this time. Re-deriving a
                // word's type from whatever currently sits in the window,
                // with no way to tell "gained new context" from "lost old
                // context" as it ages out, produced two separate downgrade
                // bugs before this check existed: a type reverting to
                // Unknown (round 11) and a type changing between two real
                // context types (round 12), both merely because an older
                // framing word (DE, CQ) fell out of the 16-word window,
                // not because anything new arrived.
                //
                // A re-attempt that changes nothing is not a decision, so
                // it records nothing (MAN-324).
                if word.last_spot_type == Some(spot_type)
                    || involved_max_seq <= word.classified_max_seq
                {
                    return None;
                }
            }
            let reclassifying = word.attempted;
            word.attempted = true;
            word.last_spot_type = Some(spot_type);
            word.classified_max_seq = involved_max_seq;
            (word.confidences.clone(), reclassifying, word.seq)
        };

        // Operator suppression overrides (MAN-31) -- orthogonal to, and
        // checked ahead of, both the automatic validation pipeline and the
        // MAN-28 allowlist below: an explicit blocklist/notch entry is the
        // operator's more specific, deliberate override and must not be
        // silently defeated by a broader allowlist entry. Each hit is
        // counted (ARCHITECTURE §8) so it reads as a deliberate
        // suppression, not silent coverage loss.
        let withheld = |reason: Reason| {
            Decision::withheld(track_id, sample_ts, &candidate, freq_hz, spot_type, reason)
        };
        if self.blocklist.contains(&candidate) {
            self.suppression_counts.blocklist += 1;
            self.decide(|| withheld(Reason::Blocklist));
            return None;
        }
        if self.notch.contains(freq_hz) {
            self.suppression_counts.notch += 1;
            self.decide(|| withheld(Reason::Notch));
            return None;
        }

        // MAN-28 Watch List: an allowlisted callsign bypasses grammar/cty
        // validation and the repetition gate below entirely.
        let is_allowlisted = self.allowlist.contains(&candidate);

        if !is_allowlisted {
            if !grammar::is_plausible(&candidate) {
                self.decide(|| withheld(Reason::Grammar));
                return None;
            }
            if !self.cty.is_allocated(&candidate) {
                self.decide(|| withheld(Reason::Cty));
                return None;
            }
        }

        // A reclassification is the same decode re-typed, not a new one --
        // reuse its already-recorded repetition count instead of calling
        // `gate.record` again, which would otherwise let a type change
        // alone inflate an ordinary, non-exempt callsign's rep count past
        // the repetition gate after only one real decode (MAN-28 round 9
        // review).
        let reps = if reclassifying {
            let track = self.tracks.get(&track_id)?;
            track
                .words
                .iter()
                .rev()
                .find(|w| w.text == candidate)
                .map(|w| w.last_reps)
                .unwrap_or(0)
        } else {
            // MAN-100 Scenario 2 / Codex review PR #133 (round 2):
            // message-distinctness across a track_id change needs to know
            // whether the PRIOR track has actually closed, not just how
            // much sample_ts has elapsed -- `self.tracks` (MAN-19 keeps it
            // authoritative, entries removed on `TrackClosed`) is exactly
            // that signal, EXCEPT a `Bookkeeping`-merged track_id: its
            // identity continues on a survivor, so `self.merged_into`
            // (round 3) still counts it as active. See
            // `gate::message_distinct_indices`'s doc.
            self.gate.record(
                track_id,
                freq_hz,
                &candidate,
                sample_ts,
                resolved_word_seq,
                |tid| self.tracks.contains_key(&tid) || self.merged_into.contains_key(&tid),
            ) as u32
        };
        {
            let track = self.tracks.get_mut(&track_id)?;
            if let Some(word) = track.words.iter_mut().rev().find(|w| w.text == candidate) {
                word.last_reps = reps;
            }
        }
        let mut confidence = confidence::c_call(&char_confidences, reps);
        if let Some(scp) = &self.scp {
            confidence = confidence::apply_scp_boost(confidence, scp.contains(&candidate));
        }
        // ARCHITECTURE §6.4 exempts BEACON-tagged messages from the
        // repetition requirement: NCDXF-style beacons ID once per cycle,
        // so a single decode must still spot (MAN-28). Only reachable
        // here for an ALLOWLISTED Beacon candidate -- non-allowlisted
        // ones never reach this function body at all (see the early
        // dispatch above).
        if !is_allowlisted
            && spot_type != SpotType::Beacon
            && reps < self.level.required_reps(self.patterns.tier(&candidate))
        {
            self.decide(|| withheld(Reason::Repetition).with_reps(reps));
            return None;
        }

        // MAN-100 step 4b: cross-candidate arbitration. Allowlisted calls
        // are exempt (the Watch List already bypasses grammar/cty and the
        // repetition gate, MAN-28), so is any call in master.scp -- a
        // curated list of real, active callsigns, where a false
        // suppression would cost recall on exactly the population RBN
        // cares most about -- and so is any `SpotType::Beacon` candidate
        // (MAN-100 remediation C3), for the same reason step 4's own
        // repetition gate exempts beacons two checks above: an NCDXF-style
        // beacon legitimately IDs once per cycle, so its "support" is
        // structurally capped at a low rep count that a confusable
        // rival's fading-corrupted repeat can easily outrun on rep count
        // alone -- measured: "V V V W6DPH K" x2 then "V V V W6DPG K" x1
        // spotted only the corrupted W6DPH and permanently suppressed the
        // genuine, once-per-cycle W6DPG. Not exempting would let this
        // rep-count-based heuristic (tuned for calls that must clear >= 2
        // reps to spot at all) systematically displace a true beacon spot
        // with a false one -- worse than the small, bounded risk of a
        // corrupted beacon variant occasionally spotting unarbitrated,
        // the same tradeoff already accepted for the allowlist/SCP
        // exemptions above. All three exemptions only ever *add* spots
        // relative to the bare rule. Purely subtractive otherwise: this
        // check can only withhold a spot the rest of the pipeline would
        // have emitted, never create one.
        let scp_exempt = self.scp.as_ref().is_some_and(|s| s.contains(&candidate));
        let rival = if !is_allowlisted && !scp_exempt && spot_type != SpotType::Beacon {
            self.ledger
                .better_supported_rival(track_id, &candidate, sample_ts)
        } else {
            None
        };
        if let Some((rival_call, rival_support)) = rival {
            self.suppression_counts.variant += 1;
            self.decide(|| {
                let mut d = withheld(Reason::Variant).with_reps(reps);
                d.rival = Some(Rival {
                    call: rival_call,
                    support: rival_support,
                });
                d
            });
            return None;
        }

        let input = self.input_of(track_id);
        let outcome = self
            .dedupe
            .assess(&candidate, freq_hz, snr_db, spot_type, sample_ts, input);
        if outcome != DedupeOutcome::Emit {
            self.count_cross_input_dedupe(outcome, input);
            self.decide(|| withheld(Reason::Dedupe).with_reps(reps));
            return None;
        }

        let (rst, qrl_query) = self.track_annotations(track_id);
        let spot = Spot {
            callsign: candidate,
            freq_hz,
            snr_db,
            wpm,
            spot_type,
            confidence,
            track_id,
            sample_ts,
            rst,
            qrl_query,
        };
        self.decide(|| Decision::spotted(&spot, reps));
        Some(spot)
    }

    /// Captures a non-allowlisted Beacon candidate once every permanent,
    /// timing-independent check passes -- never evaluates WPM or emits a
    /// spot here. See `PendingBeacon`'s doc comment for why this is
    /// deferred, and `resolve_pending_beacons` for where it's finally
    /// judged.
    fn capture_pending_beacon(
        &mut self,
        track_id: u32,
        sample_ts: u64,
        candidate: String,
        involved_max_seq: u64,
        exact_seq: Option<u64>,
    ) -> Option<Spot> {
        let (freq_hz, snr_db) = {
            let track = self.tracks.get(&track_id)?;
            (track.freq_hz * self.freq_calibration, track.snr_db)
        };

        // Word-attempted bookkeeping FIRST, same ordering as the
        // non-Beacon path and for the same reason (Codex review on
        // PR #154, round 3): checking blocklist/notch before this guard
        // would re-count a permanently-suppressed candidate every time an
        // unrelated later word re-triggers a scan that finds it again.
        let (char_confidences, word_seq) = {
            let track = self.tracks.get_mut(&track_id)?;
            let word = if let Some(seq) = exact_seq {
                track.words.iter_mut().find(|w| w.seq == seq)?
            } else {
                track.words.iter_mut().rev().find(|w| w.text == candidate)?
            };
            let involved_max_seq = involved_max_seq.max(word.seq);
            // Same reclassification guard as the non-Beacon path (MAN-28
            // round 12/13) -- a word already captured as this exact
            // candidate, with no genuinely new supporting evidence since,
            // is not captured (or re-suppressed) again.
            if word.attempted
                && (word.last_spot_type == Some(SpotType::Beacon)
                    || involved_max_seq <= word.classified_max_seq)
            {
                return None;
            }
            word.attempted = true;
            word.last_spot_type = Some(SpotType::Beacon);
            word.classified_max_seq = involved_max_seq;
            (word.confidences.clone(), word.seq)
        };

        // Operator suppression overrides (MAN-31), same boundary as the
        // non-Beacon path (ARCHITECTURE §8).
        let withheld = |reason: Reason| {
            Decision::withheld(
                track_id,
                sample_ts,
                &candidate,
                freq_hz,
                SpotType::Beacon,
                reason,
            )
        };
        let rejected = if self.blocklist.contains(&candidate) {
            self.suppression_counts.blocklist += 1;
            Some(Reason::Blocklist)
        } else if self.notch.contains(freq_hz) {
            self.suppression_counts.notch += 1;
            Some(Reason::Notch)
        } else if !grammar::is_plausible(&candidate) {
            Some(Reason::Grammar)
        } else if !self.cty.is_allocated(&candidate) {
            Some(Reason::Cty)
        } else {
            None
        };
        if let Some(reason) = rejected {
            self.decide(|| withheld(reason));
            return None;
        }
        self.decide(|| withheld(Reason::BeaconPending));

        let pending = &mut self.tracks.get_mut(&track_id)?.pending_beacons;
        // Past the cap, drop the oldest to bound growth on a track that
        // stays alive indefinitely (round 8) -- see MAX_PENDING_BEACONS's
        // doc. Counted (ARCHITECTURE §8 -- round 9): this candidate's own
        // source word was already marked attempted and won't be
        // recaptured, so this is a real, permanent loss.
        let evicted = insert_by_age(
            pending,
            PendingBeacon {
                candidate,
                sample_ts,
                freq_hz,
                snr_db,
                char_confidences,
                origin_track_id: track_id,
                word_seq,
            },
        );
        if let Some(evicted) = evicted {
            self.suppression_counts.pending_beacon_overflow += 1;
            if let Some(log) = self.decisions.as_mut() {
                log.push(beacon_decision(track_id, &evicted, Reason::BeaconOverflow));
            }
        }
        None
    }

    /// Follows `self.merged_into`'s alias chain from `tid` to its current
    /// best-known root survivor (MAN-100 message-distinctness, Codex
    /// review PR #133 round 4) -- `tid` itself if it isn't a known alias.
    /// A track_id passed in as a fresh `survivor_track_id` should always
    /// already be a root in practice (a currently-live track), but this
    /// is defensive against a chain forming in an order this code doesn't
    /// otherwise anticipate. Bounded to `self.merged_into.len() + 1` hops
    /// so a (should-be-impossible) cycle can never loop forever.
    fn resolve_survivor_root(&self, tid: u32) -> u32 {
        let mut current = tid;
        for _ in 0..=self.merged_into.len() {
            match self.merged_into.get(&current) {
                Some(&(root, _)) if root != current => current = root,
                _ => return current,
            }
        }
        current
    }

    /// MAN-272: the RST/QRL? annotations a spot emitted under `track_id`
    /// carries, read when the spot is built (D6).
    fn track_annotations(&self, track_id: u32) -> (Option<String>, bool) {
        self.tracks
            .get(&track_id)
            .map_or((None, false), |t| (t.rst.clone(), t.qrl_query))
    }

    /// A `ClosureKind::Bookkeeping` closure is not evidence this
    /// identity's signal ended (round 9) -- move its `PendingBeacon`s to
    /// the surviving track (a merge) so they're judged by ITS eventual
    /// true close instead, or discard them (an eviction, no successor),
    /// counted per ARCHITECTURE §8. No-op if there was nothing pending.
    fn migrate_or_discard_pending_beacons(
        &mut self,
        track_id: u32,
        survivor_track_id: Option<u32>,
    ) {
        let Some(track) = self.tracks.get_mut(&track_id) else {
            return;
        };
        let pending = std::mem::take(&mut track.pending_beacons);
        if pending.is_empty() {
            return;
        }
        match survivor_track_id {
            Some(survivor) => {
                let survivor_track = self.tracks.entry(survivor).or_default();
                // Merged by `sample_ts`, not appended (MAN-174): the cap
                // must drop the true oldest across both queues, so an old
                // loser candidate never displaces the survivor's newer ones.
                for pb in pending {
                    if let Some(evicted) = insert_by_age(&mut survivor_track.pending_beacons, pb) {
                        self.suppression_counts.pending_beacon_overflow += 1;
                        if let Some(log) = self.decisions.as_mut() {
                            log.push(beacon_decision(survivor, &evicted, Reason::BeaconOverflow));
                        }
                    }
                }
            }
            None => {
                self.suppression_counts.pending_beacon_lost_to_eviction += pending.len() as u64;
                for pb in &pending {
                    self.decide(|| beacon_decision(track_id, pb, Reason::BeaconDiscarded));
                }
            }
        }
    }

    /// The one point where every `PendingBeacon` captured for `track_id`
    /// is judged, using the track's speed at the moment of its true close
    /// (`TrackClosed`'s handler calls this before removing track state).
    /// Real-hardware finding (2026-09-09, docs/DECISIONS): every overnight
    /// noise-floor false positive from a real RSP1B/40m session read
    /// implausibly fast -- avg 51.5 WPM, several pinned at the tracker's
    /// own 60 WPM ceiling (SPEC-decode-core.md's tracked range is
    /// 8..60 WPM) -- while every confirmed-real spot from the same
    /// session topped out at 42.8 WPM. Real NCDXF/IARU beacons ID at a
    /// fixed ~20-22 WPM, well under `MAX_PLAUSIBLE_WPM`. A single WPM
    /// value covers every pending candidate on this track -- it's a
    /// track-level property, not a per-candidate one.
    /// MAN-78: the lists can be replaced between a beacon's capture and its
    /// track's close. Counts and reports a pending beacon the current lists
    /// suppress; a no-op when they were not replaced, since capture already
    /// passed the same checks.
    fn operator_suppressed(&mut self, pb: &PendingBeacon) -> Option<Reason> {
        if self.blocklist.contains(&pb.candidate) {
            self.suppression_counts.blocklist += 1;
            return Some(Reason::Blocklist);
        }
        if self.notch.contains(pb.freq_hz) {
            self.suppression_counts.notch += 1;
            return Some(Reason::Notch);
        }
        None
    }

    fn resolve_pending_beacons(&mut self, track_id: u32) -> Vec<Spot> {
        let (wpm, wpm_confirmed, pending) = {
            let Some(track) = self.tracks.get_mut(&track_id) else {
                return Vec::new();
            };
            (
                track.wpm,
                track.wpm_confirmed,
                std::mem::take(&mut track.pending_beacons),
            )
        };
        let (rst, qrl_query) = self.track_annotations(track_id);
        // An unconfirmed `wpm` (still its `0.0` default) is not evidence of
        // a genuine slow speed -- refuse to resolve rather than let a
        // never-measured survivor pass the plausibility check on a
        // placeholder value (round 10).
        if pending.is_empty() {
            return Vec::new();
        }
        if !wpm_confirmed || wpm > MAX_PLAUSIBLE_WPM {
            // Operator suppression takes precedence over the automatic
            // speed rejection (`blocklisted_beacon_above_max_wpm_still_counts_as_suppressed`),
            // so lists reloaded since capture are still counted here (MAN-78).
            // Every other candidate is counted as a WPM-gate rejection,
            // unconfirmed and implausible speed alike (MAN-174,
            // ARCHITECTURE §8) -- exactly one counter per candidate.
            for pb in &pending {
                let reason = match self.operator_suppressed(pb) {
                    Some(reason) => reason,
                    None => {
                        self.suppression_counts.pending_beacon_rejected_wpm += 1;
                        Reason::BeaconSpeed
                    }
                };
                self.decide(|| beacon_decision(track_id, pb, reason));
            }
            return Vec::new();
        }
        pending
            .into_iter()
            .filter_map(|pb| {
                // Before `gate.record`, so a suppressed beacon never feeds
                // the repetition gate.
                if let Some(reason) = self.operator_suppressed(&pb) {
                    self.decide(|| beacon_decision(track_id, &pb, reason));
                    return None;
                }
                // pb.origin_track_id, not the resolving `track_id`
                // (Codex review, PR #152, round 14): a migrated
                // PendingBeacon must keep the identity that ACTUALLY
                // captured it, or a merge survivor resolving a different
                // track's candidate here would wrongly grant it the
                // rapid-same-track exemption -- letting one real
                // over-the-air occurrence, captured by two overlapping
                // duplicate-spawn tracks, reach reps >= 2 on its own.
                let reps = self.gate.record(
                    pb.origin_track_id,
                    pb.freq_hz,
                    &pb.candidate,
                    pb.sample_ts,
                    pb.word_seq,
                    |tid| self.tracks.contains_key(&tid) || self.merged_into.contains_key(&tid),
                ) as u32;
                let mut confidence = confidence::c_call(&pb.char_confidences, reps);
                if let Some(scp) = &self.scp {
                    confidence =
                        confidence::apply_scp_boost(confidence, scp.contains(&pb.candidate));
                }
                // ARCHITECTURE §6.4 exempts BEACON-tagged messages from
                // the repetition requirement -- no `reps < 2` gate here,
                // by design.
                //
                // A deferred candidate can resolve well after capture --
                // if a NEWER spot for the same (callsign, freq) identity
                // already recorded dedupe state in the meantime (via the
                // ordinary, non-deferred path), this older sample_ts must
                // never be handed to `should_emit`: a spot_type/SNR jump
                // there would still record it, silently rewinding the
                // watermark backward and letting a later real spot escape
                // the suppression window early (Codex review on PR #154,
                // round 8). So a strictly older observation is dropped,
                // and counted (MAN-174, ARCHITECTURE §8). An EQUAL
                // timestamp cannot rewind the watermark -- e.g. two tracks
                // completing a word on the same input hop -- so it goes
                // through `should_emit` exactly like a non-deferred
                // candidate: suppressed as a duplicate, or emitted on a
                // spot-type change or SNR jump (MAN-174). MAN-120: the
                // identity includes this input's own spot one bucket over
                // within one channel (Codex review on PR #387).
                let input = self.input_of(pb.origin_track_id);
                if let Some(last_ts) = self.dedupe.last_sample_ts(&pb.candidate, pb.freq_hz, input)
                {
                    if pb.sample_ts < last_ts {
                        self.suppression_counts.pending_beacon_stale += 1;
                        self.decide(|| {
                            beacon_decision(track_id, &pb, Reason::Dedupe).with_reps(reps)
                        });
                        return None;
                    }
                }
                let outcome = self.dedupe.assess(
                    &pb.candidate,
                    pb.freq_hz,
                    pb.snr_db,
                    SpotType::Beacon,
                    pb.sample_ts,
                    input,
                );
                if outcome != DedupeOutcome::Emit {
                    self.count_cross_input_dedupe(outcome, input);
                    self.decide(|| beacon_decision(track_id, &pb, Reason::Dedupe).with_reps(reps));
                    return None;
                }
                let spot = Spot {
                    callsign: pb.candidate,
                    freq_hz: pb.freq_hz,
                    snr_db: pb.snr_db,
                    wpm,
                    spot_type: SpotType::Beacon,
                    confidence,
                    track_id,
                    sample_ts: pb.sample_ts,
                    rst: rst.clone(),
                    qrl_query,
                };
                self.decide(|| Decision::spotted(&spot, reps));
                Some(spot)
            })
            .collect()
    }
}

/// Inserts `pb` at its `sample_ts` position (after any equal ones, so
/// capture order breaks ties) and, past `MAX_PENDING_BEACONS`, removes and
/// returns the oldest entry, which may be `pb` itself. Keeps `pending`
/// sorted by `sample_ts`, so the cap always drops the true oldest
/// (MAN-174).
fn insert_by_age(pending: &mut Vec<PendingBeacon>, pb: PendingBeacon) -> Option<PendingBeacon> {
    let at = pending.partition_point(|p| p.sample_ts <= pb.sample_ts);
    pending.insert(at, pb);
    (pending.len() > MAX_PENDING_BEACONS).then(|| pending.remove(0))
}

/// A decision about a deferred beacon candidate, decided under
/// `track_id` (the track it is resolved or discarded on).
fn beacon_decision(track_id: u32, pb: &PendingBeacon, reason: Reason) -> Decision {
    Decision::withheld(
        track_id,
        pb.sample_ts,
        &pb.candidate,
        pb.freq_hz,
        SpotType::Beacon,
        reason,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    const QQ9_ENTITY: &str =
        "Test DXpedition:  14:  27:  EU:  50.0:  -5.0:  0.0:  QQ9:\n    QQ9;\n";

    #[test]
    fn from_tables_spots_a_call_only_the_supplied_table_allocates() {
        let words = ["DE", "QQ9ZZZ", "K"];
        let spots_with = |cty: &str| {
            let mut v = Validator::from_tables(FS, Arc::new(cty::Table::parse(cty)), None)
                .with_patterns(Arc::new(pattern::Table::parse("+QQ9@@@\n").unwrap()));
            seed_meta(&mut v, 1);
            let mut spots = run(&transmission_events(1, &words, 0), &mut v);
            spots.extend(run(&transmission_events(1, &words, 100_000), &mut v));
            spots
        };
        assert!(
            spots_with(CTY_FIXTURE).is_empty(),
            "QQ9 is unallocated in the fixture"
        );
        let spots = spots_with(&format!("{CTY_FIXTURE}{QQ9_ENTITY}"));
        assert_eq!(spots.len(), 1);
        assert_eq!(spots[0].callsign, "QQ9ZZZ");
    }

    #[test]
    fn from_tables_applies_the_supplied_scp_set() {
        // `apply_scp_boost` (validator.rs:1173-1175) still sees an Arc'd set:
        // the same QQ9ZZZ spot scores higher when the supplied SCP lists it.
        let cty = Arc::new(cty::Table::parse(&format!("{CTY_FIXTURE}{QQ9_ENTITY}")));
        let confidence_with = |scp: &str| {
            let mut v =
                Validator::from_tables(FS, cty.clone(), Some(Arc::new(scp::Set::parse(scp))))
                    .with_patterns(Arc::new(pattern::Table::parse("+QQ9@@@\n").unwrap()));
            seed_meta(&mut v, 1);
            let words = ["DE", "QQ9ZZZ", "K"];
            let mut spots = run(&transmission_events(1, &words, 0), &mut v);
            spots.extend(run(&transmission_events(1, &words, 100_000), &mut v));
            spots[0].confidence
        };
        assert!(confidence_with("QQ9ZZZ\n") > confidence_with("K1ABC\n"));
    }

    const FS: f64 = 96_000.0;
    const CTY_FIXTURE: &str = "\
United States:    5:  8: NA:  40.0:  75.0:  5.0:  K:
    K,W,N,AA,AB,AC;
";

    fn word_events(track_id: u32, text: &str, start_ts: u64) -> (Vec<DecoderEvent>, u64) {
        let mut events = Vec::new();
        let mut ts = start_ts;
        for c in text.chars() {
            events.push(DecoderEvent::char_decoded(
                track_id,
                ts,
                Glyph::Char(c),
                0.95,
            ));
            ts += 100;
        }
        events.push(DecoderEvent::word_boundary(track_id, ts));
        ts += 100;
        (events, ts)
    }

    fn transmission_events(track_id: u32, words: &[&str], start_ts: u64) -> Vec<DecoderEvent> {
        let mut events = Vec::new();
        let mut ts = start_ts;
        for word in words {
            let (mut w_events, next_ts) = word_events(track_id, word, ts);
            events.append(&mut w_events);
            ts = next_ts;
        }
        events
    }

    fn run(events: &[DecoderEvent], v: &mut Validator) -> Vec<Spot> {
        events.iter().flat_map(|e| v.ingest(e)).collect()
    }

    /// Real telemetry, so `try_spot`'s `has_meta` gate (MAN-28 round 8)
    /// doesn't hold back every spot in tests that don't otherwise care
    /// about metadata timing.
    fn seed_meta(v: &mut Validator, track_id: u32) {
        v.ingest(&DecoderEvent::TrackMeta {
            track_id,
            sample_ts: 0,
            snr_2500_db: 20.0,
            freq_hz: 14_000_000.0,
        });
    }

    // ---- MAN-324: the decision record ----

    use crate::decision::Verdict;

    fn recording(cty: &str) -> Validator {
        let mut v = Validator::new(FS, cty, None);
        v.record_decisions(true);
        seed_meta(&mut v, 1);
        v
    }

    fn decisions_for<'a>(log: &'a [Decision], call: &str) -> Vec<&'a Decision> {
        log.iter().filter(|d| d.call == call).collect()
    }

    #[test]
    fn unlisted_decisions_and_reclassification_keep_the_five_rep_requirement() {
        let mut v = recording(CTY_FIXTURE);
        let events = transmission_events(1, &["DE", "W4KTNL", "CQ"], 0);
        assert!(run(&events, &mut v).is_empty());
        let log = v.take_decisions();
        let decisions = decisions_for(&log, "W4KTNL");
        assert_eq!(decisions.len(), 2, "DE then CQ reclassification: {log:?}");
        for d in decisions {
            assert_eq!(d.reason, Some(Reason::Repetition));
            assert_eq!(d.required_reps, 5);
            assert_eq!(d.pattern_tier, Some(crate::PatternTier::Unlisted));
            assert_eq!(d.reps, Some(1), "reclassification must not record again");
        }
    }

    #[test]
    fn allowlisted_decision_reports_one_rep_and_no_pattern_tier() {
        let mut v = recording(CTY_FIXTURE);
        v.allowlist("W4KTNL");
        assert_eq!(
            run(&transmission_events(1, &["W4KTNL"], 0), &mut v).len(),
            1
        );
        let log = v.take_decisions();
        let d = decisions_for(&log, "W4KTNL")[0];
        assert_eq!(d.required_reps, 1);
        assert_eq!(d.pattern_tier, None);
    }

    #[test]
    fn decision_log_is_empty_unless_enabled() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        seed_meta(&mut v, 1);
        let words = ["DE", "K5ARH", "K"];
        let mut spots = run(&transmission_events(1, &words, 0), &mut v);
        spots.extend(run(&transmission_events(1, &words, 100_000), &mut v));
        assert_eq!(spots.len(), 1);
        assert!(v.take_decisions().is_empty());
    }

    #[test]
    fn a_first_decode_is_withheld_for_repetition_then_spotted_on_the_second() {
        let mut v = recording(CTY_FIXTURE);
        let words = ["DE", "K5ARH", "K"];
        assert!(run(&transmission_events(1, &words, 0), &mut v).is_empty());
        let first = v.take_decisions();
        let first = decisions_for(&first, "K5ARH");
        assert_eq!(first.len(), 1, "{first:?}");
        assert_eq!(first[0].verdict, Verdict::Withheld);
        assert_eq!(first[0].reason, Some(Reason::Repetition));
        assert_eq!(first[0].reps, Some(1));
        assert_eq!(first[0].spot_type, SpotType::De);
        assert_eq!(first[0].freq_hz, 14_000_000.0);

        let spots = run(&transmission_events(1, &words, 100_000), &mut v);
        assert_eq!(spots.len(), 1);
        let second = v.take_decisions();
        let second = decisions_for(&second, "K5ARH");
        assert_eq!(second.len(), 1, "{second:?}");
        assert_eq!(second[0].verdict, Verdict::Spotted);
        assert_eq!(second[0].reason, None);
        assert_eq!(second[0].reps, Some(2));
        assert_eq!(second[0].spot.as_ref(), Some(&spots[0]));
        assert!(v.take_decisions().is_empty(), "take drains the log");
    }

    #[test]
    fn variant_withhold_names_the_better_supported_rival() {
        let mut v = recording(CTY_FIXTURE);
        for i in 0..2u64 {
            run(
                &transmission_events(1, &["CQ", "DE", "K5ARH", "K"], i * 300_000),
                &mut v,
            );
        }
        let filler: Vec<String> = (1..=16).map(|i| format!("QQQ{i}")).collect();
        let filler_refs: Vec<&str> = filler.iter().map(String::as_str).collect();
        run(&transmission_events(1, &filler_refs, 600_000), &mut v);
        for i in 0..2u64 {
            run(
                &transmission_events(1, &["CQ", "DE", "K5AR", "K"], 900_000 + i * 300_000),
                &mut v,
            );
        }
        let log = v.take_decisions();
        let variant: Vec<&Decision> = decisions_for(&log, "K5AR")
            .into_iter()
            .filter(|d| d.reason == Some(Reason::Variant))
            .collect();
        assert!(!variant.is_empty(), "{log:?}");
        let rival = variant[0]
            .rival
            .as_ref()
            .expect("a variant withhold names its rival");
        assert_eq!(rival.call, "K5ARH");
        assert_eq!(rival.support.reps, 2);
        assert_eq!(variant[0].verdict, Verdict::Withheld);
        assert!(variant[0].reps.unwrap() >= 2);
    }

    #[test]
    fn blocklist_notch_grammar_and_cty_withholds_carry_their_reason() {
        let reason_of = |mut v: Validator, words: &[&str], call: &str| {
            v.record_decisions(true);
            seed_meta(&mut v, 1);
            run(&transmission_events(1, words, 0), &mut v);
            let log = v.take_decisions();
            let d = decisions_for(&log, call);
            assert_eq!(d.len(), 1, "{log:?}");
            assert_eq!(d[0].verdict, Verdict::Withheld);
            assert_eq!(d[0].reps, None, "decided before the repetition gate");
            d[0].reason
        };
        let base = || Validator::new(FS, CTY_FIXTURE, None);
        assert_eq!(
            reason_of(
                base().with_blocklist(Blocklist::parse("K5ARH\n")),
                &["DE", "K5ARH", "K"],
                "K5ARH"
            ),
            Some(Reason::Blocklist)
        );
        assert_eq!(
            reason_of(
                base().with_notch(NotchList::parse("13999000-14001000\n")),
                &["DE", "K5ARH", "K"],
                "K5ARH"
            ),
            Some(Reason::Notch)
        );
        assert_eq!(
            reason_of(base(), &["DE", "ABC", "K"], "ABC"),
            Some(Reason::Grammar)
        );
        assert_eq!(
            reason_of(base(), &["DE", "QQ9ZZZ", "K"], "QQ9ZZZ"),
            Some(Reason::Cty)
        );
    }

    #[test]
    fn dedupe_withhold_is_recorded() {
        let mut v = recording(CTY_FIXTURE);
        let words = ["DE", "K5ARH", "K"];
        run(&transmission_events(1, &words, 0), &mut v);
        run(&transmission_events(1, &words, 100_000), &mut v);
        v.take_decisions();
        assert!(run(&transmission_events(1, &words, 200_000), &mut v).is_empty());
        let log = v.take_decisions();
        let d = decisions_for(&log, "K5ARH");
        assert_eq!(d.len(), 1, "{log:?}");
        assert_eq!(d[0].reason, Some(Reason::Dedupe));
        assert!(d[0].reps.is_some());
    }

    #[test]
    fn a_beacon_is_deferred_then_decided_when_its_track_ends() {
        // Deferred, then spotted at a plausible final speed.
        let mut v = recording(CTY_FIXTURE);
        run(
            &transmission_events(1, &["V", "V", "V", "K5ARH", "K"], 0),
            &mut v,
        );
        let log = v.take_decisions();
        let d = decisions_for(&log, "K5ARH");
        assert_eq!(d.len(), 1, "{log:?}");
        assert_eq!(d[0].verdict, Verdict::Deferred);
        assert_eq!(d[0].reason, Some(Reason::BeaconPending));
        v.ingest(&DecoderEvent::SpeedUpdate {
            track_id: 1,
            wpm: 22.0,
        });
        let spots = v.ingest(&DecoderEvent::TrackClosed {
            track_id: 1,
            closure: ClosureKind::SignalEnded,
        });
        assert_eq!(spots.len(), 1);
        let log = v.take_decisions();
        assert_eq!(log.len(), 1, "{log:?}");
        assert_eq!(log[0].verdict, Verdict::Spotted);
        assert_eq!(log[0].spot.as_ref(), Some(&spots[0]));

        // Rejected for speed.
        let mut v = recording(CTY_FIXTURE);
        run(
            &transmission_events(1, &["V", "V", "V", "K5ARH", "K"], 0),
            &mut v,
        );
        v.take_decisions();
        v.ingest(&DecoderEvent::SpeedUpdate {
            track_id: 1,
            wpm: 60.0,
        });
        v.ingest(&DecoderEvent::TrackClosed {
            track_id: 1,
            closure: ClosureKind::SignalEnded,
        });
        let log = v.take_decisions();
        assert_eq!(log.len(), 1, "{log:?}");
        assert_eq!(log[0].reason, Some(Reason::BeaconSpeed));

        // Discarded on a bookkeeping close with no survivor.
        let mut v = recording(CTY_FIXTURE);
        run(
            &transmission_events(1, &["V", "V", "V", "K5ARH", "K"], 0),
            &mut v,
        );
        v.take_decisions();
        v.ingest(&DecoderEvent::TrackClosed {
            track_id: 1,
            closure: ClosureKind::Bookkeeping {
                survivor_track_id: None,
            },
        });
        let log = v.take_decisions();
        assert_eq!(log.len(), 1, "{log:?}");
        assert_eq!(log[0].reason, Some(Reason::BeaconDiscarded));

        // Overflow past MAX_PENDING_BEACONS.
        let mut v = recording(CTY_FIXTURE);
        let mut ts = 0u64;
        for i in 0..(MAX_PENDING_BEACONS + 1) {
            let call = format!("K5AA{}", (b'A' + i as u8) as char);
            run(&transmission_events(1, &[call.as_str(), "T"], ts), &mut v);
            ts += 1_000_000;
        }
        let log = v.take_decisions();
        let overflow: Vec<&Decision> = log
            .iter()
            .filter(|d| d.reason == Some(Reason::BeaconOverflow))
            .collect();
        assert_eq!(overflow.len(), 1, "{log:?}");
        assert_eq!(overflow[0].call, "K5AAA", "the oldest is evicted");
    }

    #[test]
    fn a_power_step_guard_withhold_is_recorded() {
        let mut v = recording(CTY_FIXTURE);
        let words = ["CQ", "DX", "FILLER1", "FILLER2", "FILLER3", "K5ARH", "T"];
        run(&transmission_events(1, &words, 0), &mut v);
        assert_eq!(v.suppression_counts().power_step_guard, 1);
        let log = v.take_decisions();
        let d: Vec<&Decision> = log
            .iter()
            .filter(|d| d.reason == Some(Reason::PowerStepGuard))
            .collect();
        assert_eq!(d.len(), 1, "{log:?}");
        assert_eq!(d[0].call, "K5ARH");
        assert_eq!(d[0].spot_type, SpotType::Beacon);
    }

    #[test]
    fn a_reclassification_reattempt_records_no_new_decision() {
        let mut v = recording(CTY_FIXTURE);
        let words = ["DE", "K5ARH", "K"];
        run(&transmission_events(1, &words, 0), &mut v);
        assert_eq!(decisions_for(&v.take_decisions(), "K5ARH").len(), 1);
        // A later unrelated word re-scans the window; the same K5ARH word
        // with the same type is not a new decision.
        run(&transmission_events(1, &["QSL"], 50_000), &mut v);
        assert!(decisions_for(&v.take_decisions(), "K5ARH").is_empty());
    }

    #[test]
    fn no_decision_before_the_first_track_meta() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        v.record_decisions(true);
        run(&transmission_events(1, &["DE", "K5ARH", "K"], 0), &mut v);
        assert!(v.take_decisions().is_empty());
        seed_meta(&mut v, 1);
        let log = v.take_decisions();
        assert_eq!(decisions_for(&log, "K5ARH").len(), 1, "{log:?}");
    }

    #[test]
    fn candidates_for_lists_a_tracks_plausible_words_strongest_first() {
        let mut v = recording(CTY_FIXTURE);
        seed_meta(&mut v, 2);
        run(&transmission_events(1, &["DE", "K5ARH", "K"], 0), &mut v);
        run(
            &transmission_events(1, &["DE", "K5ARX", "K"], 100_000),
            &mut v,
        );
        run(
            &transmission_events(1, &["DE", "K5ARH", "K"], 400_000),
            &mut v,
        );
        run(&transmission_events(2, &["DE", "W1AW", "K"], 0), &mut v);
        let got = v.candidates_for(1, 500_000);
        let calls: Vec<&str> = got.iter().map(|c| c.call.as_str()).collect();
        assert_eq!(calls, ["K5ARH", "K5ARX"], "{got:?}");
        assert_eq!(got[0].support.reps, 2);
        assert!(got[0].support.strictly_better_than(&got[1].support));
        v.ingest(&DecoderEvent::TrackClosed {
            track_id: 1,
            closure: ClosureKind::SignalEnded,
        });
        assert!(v.candidates_for(1, 500_000).is_empty());
        assert_eq!(v.candidates_for(2, 500_000).len(), 1);
    }

    // ---- MAN-13: several inputs, one validator ----

    fn meta_at(v: &mut Validator, track_id: u32, freq_hz: f64) {
        v.ingest(&DecoderEvent::TrackMeta {
            track_id,
            sample_ts: 0,
            snr_2500_db: 20.0,
            freq_hz,
        });
    }

    /// Two tracks' transmissions, interleaved by `sample_ts` the way the
    /// multi-input merge delivers them.
    fn interleaved(a: (u32, &str), b: (u32, &str), start_ts: u64) -> Vec<DecoderEvent> {
        let mut events = transmission_events(a.0, &["DE", a.1, "K"], start_ts);
        events.extend(transmission_events(b.0, &["DE", b.1, "K"], start_ts + 50));
        events.sort_by_key(|e| match e {
            DecoderEvent::CharDecoded { sample_ts, .. }
            | DecoderEvent::WordBoundary { sample_ts, .. } => *sample_ts,
            _ => 0,
        });
        events
    }

    #[test]
    fn input_count_maps_interleaved_track_ids_to_inputs() {
        let v = Validator::new(FS, CTY_FIXTURE, None).with_input_count(3);
        let inputs: Vec<u32> = [1, 4, 2, 5, 3].iter().map(|&id| v.input_of(id)).collect();
        assert_eq!(inputs, vec![0, 0, 1, 1, 2]);
        let single = Validator::new(FS, CTY_FIXTURE, None);
        assert_eq!(single.input_of(7), 0);
    }

    #[test]
    fn two_inputs_hearing_one_station_spot_it_once() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None).with_input_count(2);
        // The measured 3 Hz disagreement, straddling the gate's 100 Hz
        // bucket edge at 14_012_350 Hz.
        meta_at(&mut v, 1, 14_012_349.9);
        meta_at(&mut v, 2, 14_012_352.7);
        let mut spots = Vec::new();
        for start in [0, 100_000, 200_000] {
            spots.extend(run(&interleaved((1, "K5ARH"), (2, "K5ARH"), start), &mut v));
        }
        assert_eq!(spots.len(), 1, "{spots:?}");
        assert_eq!(spots[0].callsign, "K5ARH");
    }

    /// MAN-14: the D6 withhold counts against the withheld input only.
    #[test]
    fn a_cross_input_duplicate_counts_against_the_withheld_input() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None).with_input_count(2);
        assert_eq!(
            v.suppression_counts().cross_input_dedupe_by_input,
            vec![0, 0]
        );
        meta_at(&mut v, 1, 14_012_349.9);
        meta_at(&mut v, 2, 14_012_352.7);
        let mut spots = Vec::new();
        for start in [0, 100_000, 200_000] {
            spots.extend(run(&interleaved((1, "K5ARH"), (2, "K5ARH"), start), &mut v));
        }
        assert_eq!(spots.len(), 1, "{spots:?}");
        let winner = v.input_of(spots[0].track_id) as usize;
        let counts = v.suppression_counts().cross_input_dedupe_by_input;
        assert_eq!(counts[winner], 0, "{counts:?}");
        assert!(counts[1 - winner] >= 1, "{counts:?}");
    }

    /// MAN-14: two stations, a same-input repeat and one input count none.
    #[test]
    fn no_cross_input_duplicate_counts_nothing() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None).with_input_count(2);
        meta_at(&mut v, 1, 14_012_340.0);
        meta_at(&mut v, 2, 14_085_000.0);
        for start in [0, 100_000, 200_000] {
            run(&interleaved((1, "K5ARH"), (2, "W1AW"), start), &mut v);
        }
        assert_eq!(
            v.suppression_counts().cross_input_dedupe_by_input,
            vec![0, 0]
        );
        let mut one = Validator::new(FS, CTY_FIXTURE, None);
        seed_meta(&mut one, 1);
        let words = ["DE", "K5ARH", "K"];
        let mut spots = Vec::new();
        for start in [0, 100_000, 200_000] {
            spots.extend(run(&transmission_events(1, &words, start), &mut one));
        }
        assert_eq!(spots.len(), 1, "{spots:?}");
        assert_eq!(
            one.suppression_counts().cross_input_dedupe_by_input,
            vec![0]
        );
    }

    #[test]
    fn two_inputs_hearing_two_stations_spot_both() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None).with_input_count(2);
        meta_at(&mut v, 1, 14_012_340.0);
        meta_at(&mut v, 2, 14_085_000.0);
        let mut spots = Vec::new();
        for start in [0, 100_000] {
            spots.extend(run(&interleaved((1, "K5ARH"), (2, "W1AW"), start), &mut v));
        }
        let mut calls: Vec<&str> = spots.iter().map(|s| s.callsign.as_str()).collect();
        calls.sort_unstable();
        assert_eq!(calls, vec!["K5ARH", "W1AW"]);
    }

    #[test]
    fn one_input_validator_is_unchanged() {
        let scene = |v: &mut Validator| {
            seed_meta(v, 1);
            let words = ["DE", "K5ARH", "K"];
            let mut spots = run(&transmission_events(1, &words, 0), v);
            spots.extend(run(&transmission_events(1, &words, 100_000), v));
            spots.extend(run(
                &transmission_events(1, &["DE", "K5ARH", "K"], 900_000),
                v,
            ));
            format!("{spots:?}")
        };
        let mut default = Validator::new(FS, CTY_FIXTURE, None);
        let mut one = Validator::new(FS, CTY_FIXTURE, None).with_input_count(1);
        assert_eq!(scene(&mut default), scene(&mut one));
    }

    #[test]
    fn full_pipeline_spots_a_repeated_valid_callsign() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        seed_meta(&mut v, 1);
        let words = ["DE", "K5ARH", "K"];
        let mut spots = run(&transmission_events(1, &words, 0), &mut v);
        spots.extend(run(&transmission_events(1, &words, 100_000), &mut v));
        assert_eq!(spots.len(), 1);
        assert_eq!(spots[0].callsign, "K5ARH");
        assert_eq!(spots[0].spot_type, SpotType::De);
        assert_eq!(spots[0].track_id, 1);
    }

    /// Real-hardware finding (2026-09-09, docs/DECISIONS): every overnight
    /// noise-floor false positive from a real RSP1B/40m session read
    /// implausibly fast (avg 51.5 WPM) and reached a spot through the
    /// `SpotType::Beacon` repetition-gate exemption -- the only path
    /// where one low-evidence decode can reach public output. Round 7
    /// redesign: a non-allowlisted Beacon candidate is captured
    /// regardless of the WPM seen while it's decoding, and rejected at
    /// `TrackClosed` if the track's TRUE FINAL speed is still implausible.
    #[test]
    fn beacon_never_spots_when_the_true_final_wpm_is_implausible() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        seed_meta(&mut v, 1);
        v.ingest(&DecoderEvent::SpeedUpdate {
            track_id: 1,
            wpm: 60.0,
        });
        // "K5ARH T" parses as SpotType::Beacon (context::parse) -- exempt
        // from the repetition gate, so one occurrence would otherwise spot.
        let words = ["K5ARH", "T"];
        let spots = run(&transmission_events(1, &words, 0), &mut v);
        assert!(
            spots.is_empty(),
            "a non-allowlisted Beacon candidate must never spot before TrackClosed, got {spots:?}"
        );

        let spots = v.ingest(&DecoderEvent::TrackClosed {
            track_id: 1,
            closure: ClosureKind::SignalEnded,
        });
        assert!(
            spots.is_empty(),
            "a track whose true final speed is implausible must never spot, got {spots:?}"
        );
    }

    /// Codex review on PR #154, rounds 2-6: a reactive check tied to a
    /// live, evolving WPM reading kept finding new ways to misfire on a
    /// transient value, in either direction (round 2: an early-inflated
    /// reading permanently lost a real beacon; round 5: a noise track
    /// oscillating across the cutoff, e.g. 46 -> 44 -> 47 WPM, could spot
    /// on the dip; round 6: an unrelated later word could reopen that
    /// same dip-driven spot). The round 7 redesign closes all three
    /// structurally: nothing is EVER evaluated against WPM until
    /// `TrackClosed`, so no interim reading -- high, low, or from an
    /// unrelated word -- can matter at all. Only the track's true final
    /// speed decides.
    #[test]
    fn only_the_true_final_wpm_at_track_close_decides() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        seed_meta(&mut v, 1);
        v.ingest(&DecoderEvent::SpeedUpdate {
            track_id: 1,
            wpm: 46.0,
        });
        let words = ["K5ARH", "T"];
        let spots = run(&transmission_events(1, &words, 0), &mut v);
        assert!(spots.is_empty());

        v.ingest(&DecoderEvent::SpeedUpdate {
            track_id: 1,
            wpm: 44.0,
        });
        // An unrelated later word on the SAME track produces its own
        // WordBoundary -- it must not cause an early spot or a duplicate
        // capture. ("5NN" specifically, not e.g. "DE": a bare CQ/DE
        // anywhere in the window suppresses the power-step beacon
        // fallback entirely -- see context.rs -- which would make this
        // candidate vanish for an unrelated reason instead of exercising
        // the property this test targets.)
        let more = run(&transmission_events(1, &["5NN"], 200_000), &mut v);
        assert!(
            more.is_empty(),
            "an unrelated WordBoundary must never itself spot, got {more:?}"
        );

        v.ingest(&DecoderEvent::SpeedUpdate {
            track_id: 1,
            wpm: 47.0,
        });
        // Settles to a plausible value before the track actually closes.
        v.ingest(&DecoderEvent::SpeedUpdate {
            track_id: 1,
            wpm: 22.0,
        });

        let spots = v.ingest(&DecoderEvent::TrackClosed {
            track_id: 1,
            closure: ClosureKind::SignalEnded,
        });
        assert_eq!(
            spots.len(),
            1,
            "the track's true final (plausible) speed must decide, got {spots:?}"
        );
        assert_eq!(spots[0].callsign, "K5ARH");
        assert_eq!(spots[0].spot_type, SpotType::Beacon);
    }

    /// MAN-78: a K5ARH transmission, then a reload that blocklists
    /// K5ARH, then the same transmission again -- the second one would
    /// spot (see `full_pipeline_spots_a_repeated_valid_callsign`) and must
    /// not.
    #[test]
    fn replaced_blocklist_suppresses_the_next_occurrence() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        seed_meta(&mut v, 1);
        let words = ["DE", "K5ARH", "K"];
        let mut spots = run(&transmission_events(1, &words, 0), &mut v);
        v.replace_operator_lists(&[], Blocklist::parse("K5ARH\n"), NotchList::default());
        spots.extend(run(&transmission_events(1, &words, 100_000), &mut v));
        assert!(spots.is_empty(), "got {spots:?}");
        assert_eq!(v.suppression_counts().blocklist, 1);
    }

    #[test]
    fn replaced_empty_blocklist_lets_later_words_spot() {
        let mut v =
            Validator::new(FS, CTY_FIXTURE, None).with_blocklist(Blocklist::parse("K5ARH\n"));
        seed_meta(&mut v, 1);
        let words = ["DE", "K5ARH", "K"];
        let mut spots = run(&transmission_events(1, &words, 0), &mut v);
        assert!(spots.is_empty(), "got {spots:?}");
        assert_eq!(v.suppression_counts().blocklist, 1);
        v.replace_operator_lists(&[], Blocklist::default(), NotchList::default());
        spots.extend(run(&transmission_events(1, &words, 100_000), &mut v));
        spots.extend(run(&transmission_events(1, &words, 200_000), &mut v));
        assert_eq!(spots.len(), 1, "got {spots:?}");
        assert_eq!(spots[0].callsign, "K5ARH");
    }

    #[test]
    fn replaced_allowlist_is_uppercased_and_replaces_the_old_list() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        v.allowlist("k5arh");
        v.replace_operator_lists(
            &["w1aw".to_string()],
            Blocklist::default(),
            NotchList::default(),
        );
        seed_meta(&mut v, 1);
        seed_meta(&mut v, 2);
        let spots = run(&transmission_events(1, &["W1AW"], 0), &mut v);
        assert_eq!(spots.len(), 1, "got {spots:?}");
        assert_eq!(spots[0].callsign, "W1AW");
        let spots = run(&transmission_events(2, &["K5ARH"], 0), &mut v);
        assert!(
            spots.is_empty(),
            "the old allowlist must be gone, got {spots:?}"
        );
    }

    /// A beacon captured before a reload, resolved at `TrackClosed` after
    /// it: the new blocklist must still apply (control:
    /// `only_the_true_final_wpm_at_track_close_decides` spots it).
    #[test]
    fn pending_beacon_captured_before_a_blocklist_reload_never_spots() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        seed_meta(&mut v, 1);
        v.ingest(&DecoderEvent::SpeedUpdate {
            track_id: 1,
            wpm: 22.0,
        });
        let spots = run(&transmission_events(1, &["K5ARH", "T"], 0), &mut v);
        assert!(spots.is_empty());
        v.replace_operator_lists(&[], Blocklist::parse("K5ARH\n"), NotchList::default());
        let spots = v.ingest(&DecoderEvent::TrackClosed {
            track_id: 1,
            closure: ClosureKind::SignalEnded,
        });
        assert!(spots.is_empty(), "got {spots:?}");
        assert_eq!(v.suppression_counts().blocklist, 1);
    }

    /// Codex review on PR #232: a beacon blocklisted by a reload and then
    /// closed above `MAX_PLAUSIBLE_WPM` still counts as operator
    /// suppression, matching `blocklisted_beacon_above_max_wpm_still_counts_as_suppressed`.
    #[test]
    fn pending_beacon_blocklisted_by_a_reload_then_too_fast_counts_as_suppressed() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        seed_meta(&mut v, 1);
        v.ingest(&DecoderEvent::SpeedUpdate {
            track_id: 1,
            wpm: 22.0,
        });
        let spots = run(&transmission_events(1, &["K5ARH", "T"], 0), &mut v);
        assert!(spots.is_empty());
        v.replace_operator_lists(&[], Blocklist::parse("K5ARH\n"), NotchList::default());
        v.ingest(&DecoderEvent::SpeedUpdate {
            track_id: 1,
            wpm: 60.0,
        });
        let spots = v.ingest(&DecoderEvent::TrackClosed {
            track_id: 1,
            closure: ClosureKind::SignalEnded,
        });
        assert!(spots.is_empty(), "got {spots:?}");
        assert_eq!(v.suppression_counts().blocklist, 1);
    }

    #[test]
    fn pending_beacon_without_a_reload_still_spots() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        seed_meta(&mut v, 1);
        v.ingest(&DecoderEvent::SpeedUpdate {
            track_id: 1,
            wpm: 22.0,
        });
        let spots = run(&transmission_events(1, &["K5ARH", "T"], 0), &mut v);
        assert!(spots.is_empty());
        let spots = v.ingest(&DecoderEvent::TrackClosed {
            track_id: 1,
            closure: ClosureKind::SignalEnded,
        });
        assert_eq!(spots.len(), 1, "got {spots:?}");
        assert_eq!(spots[0].spot_type, SpotType::Beacon);
    }

    /// MAN-272 D6: the deferred-beacon `Spot` construction site reads its
    /// track's annotations when the beacon resolves, so a report and a
    /// `QRL?` decoded after capture but before the close are carried.
    #[test]
    fn a_deferred_beacon_spot_carries_its_tracks_annotations() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        seed_meta(&mut v, 1);
        v.ingest(&DecoderEvent::SpeedUpdate {
            track_id: 1,
            wpm: 22.0,
        });
        let spots = run(&transmission_events(1, &["K5ARH", "T"], 0), &mut v);
        assert!(spots.is_empty(), "should be captured, not yet resolved");
        let spots = run(&transmission_events(1, &["QRL?", "5NN"], 10_000), &mut v);
        assert!(spots.is_empty(), "got {spots:?}");
        let spots = v.ingest(&DecoderEvent::TrackClosed {
            track_id: 1,
            closure: ClosureKind::SignalEnded,
        });
        assert_eq!(spots.len(), 1, "got {spots:?}");
        assert_eq!(spots[0].callsign, "K5ARH");
        assert_eq!(spots[0].spot_type, SpotType::Beacon);
        assert_eq!(spots[0].rst.as_deref(), Some("599"));
        assert!(spots[0].qrl_query);
    }

    #[test]
    fn pending_beacon_in_a_newly_notched_range_never_spots() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        seed_meta(&mut v, 1);
        v.ingest(&DecoderEvent::SpeedUpdate {
            track_id: 1,
            wpm: 22.0,
        });
        let spots = run(&transmission_events(1, &["K5ARH", "T"], 0), &mut v);
        assert!(spots.is_empty());
        v.replace_operator_lists(
            &[],
            Blocklist::default(),
            NotchList::parse("13999900-14000100\n"),
        );
        let spots = v.ingest(&DecoderEvent::TrackClosed {
            track_id: 1,
            closure: ClosureKind::SignalEnded,
        });
        assert!(spots.is_empty(), "got {spots:?}");
        assert_eq!(v.suppression_counts().notch, 1);
    }

    /// MAN-316: QQ9ZZZ is unallocated in the bundled table, so the first
    /// track's two messages never spot it; after a refresh adds QQ9, a
    /// second track's do.
    #[test]
    fn replaced_cty_table_lets_a_new_prefix_spot() {
        let scp = Arc::new(scp::Set::bundled());
        // MAN-95: a common pattern, so two messages are enough to spot.
        let mut v = Validator::from_tables(FS, Arc::new(cty::Table::bundled()), Some(scp.clone()))
            .with_patterns(Arc::new(pattern::Table::parse("+QQ9@@@\n").unwrap()));
        seed_meta(&mut v, 1);
        seed_meta(&mut v, 2);
        let words = ["CQ", "DE", "QQ9ZZZ", "QQ9ZZZ", "K"];
        let mut spots = run(&transmission_events(1, &words, 0), &mut v);
        spots.extend(run(&transmission_events(1, &words, 100_000), &mut v));
        assert!(spots.is_empty(), "got {spots:?}");
        let with_qq9 = cty::Table::parse(&format!("{}{QQ9_ENTITY}", crate::CTY_DAT));
        v.replace_reference_tables(Arc::new(with_qq9), Some(scp));
        let mut spots = run(&transmission_events(2, &words, 200_000), &mut v);
        spots.extend(run(&transmission_events(2, &words, 300_000), &mut v));
        assert_eq!(spots.len(), 1, "got {spots:?}");
        assert_eq!(spots[0].callsign, "QQ9ZZZ");
        assert_eq!(spots[0].track_id, 2);
    }

    #[test]
    fn replaced_reference_tables_leave_operator_lists_and_counts_alone() {
        let mut v =
            Validator::new(FS, CTY_FIXTURE, None).with_blocklist(Blocklist::parse("K5ARH\n"));
        v.allowlist("W1AW");
        seed_meta(&mut v, 1);
        seed_meta(&mut v, 2);
        let words = ["DE", "K5ARH", "K"];
        let spots = run(&transmission_events(1, &words, 0), &mut v);
        assert!(spots.is_empty(), "got {spots:?}");
        let before = v.suppression_counts();
        assert_eq!(before.blocklist, 1);
        v.replace_reference_tables(
            Arc::new(cty::Table::parse(&format!("{CTY_FIXTURE}{QQ9_ENTITY}"))),
            Some(Arc::new(scp::Set::parse("K5ARH\n"))),
        );
        assert_eq!(v.suppression_counts(), before);
        let spots = run(&transmission_events(1, &words, 100_000), &mut v);
        assert!(
            spots.is_empty(),
            "the blocklist must still hold, got {spots:?}"
        );
        assert_eq!(v.suppression_counts().blocklist, before.blocklist + 1);
        let spots = run(&transmission_events(2, &["W1AW"], 200_000), &mut v);
        assert_eq!(
            spots.len(),
            1,
            "the allowlist must still hold, got {spots:?}"
        );
        assert_eq!(spots[0].callsign, "W1AW");
    }

    #[test]
    fn replacing_lists_keeps_suppression_counts() {
        let mut v =
            Validator::new(FS, CTY_FIXTURE, None).with_blocklist(Blocklist::parse("K5ARH\n"));
        seed_meta(&mut v, 1);
        let spots = run(&transmission_events(1, &["DE", "K5ARH", "K"], 0), &mut v);
        assert!(spots.is_empty());
        let before = v.suppression_counts();
        assert_eq!(before.blocklist, 1);
        v.replace_operator_lists(&[], Blocklist::default(), NotchList::default());
        assert_eq!(v.suppression_counts(), before);
    }

    /// Codex review on PR #154, round 3: a blocklisted callsign that
    /// would ALSO fail the Beacon-WPM gate must still count as an
    /// explicit operator suppression (ARCHITECTURE §8), not be silently
    /// swallowed by the automatic WPM rejection first.
    #[test]
    fn blocklisted_beacon_above_max_wpm_still_counts_as_suppressed() {
        let blocklist = Blocklist::parse("K5ARH\n");
        let mut v = Validator::new(FS, CTY_FIXTURE, None).with_blocklist(blocklist);
        seed_meta(&mut v, 1);
        v.ingest(&DecoderEvent::SpeedUpdate {
            track_id: 1,
            wpm: 60.0,
        });
        let words = ["K5ARH", "T"];
        let spots = run(&transmission_events(1, &words, 0), &mut v);
        assert!(spots.is_empty());
        assert_eq!(v.suppression_counts().blocklist, 1);
        // MAN-174: counted once, as an operator suppression, never also
        // as a WPM-gate rejection.
        let spots = v.ingest(&DecoderEvent::TrackClosed {
            track_id: 1,
            closure: ClosureKind::SignalEnded,
        });
        assert!(spots.is_empty(), "got {spots:?}");
        assert_eq!(v.suppression_counts().blocklist, 1);
        assert_eq!(v.suppression_counts().pending_beacon_rejected_wpm, 0);
    }

    /// MAN-174 finding 2: a captured Beacon dropped by the WPM gate at its
    /// track's close is counted, not silently lost (ARCHITECTURE §8).
    #[test]
    fn a_beacon_rejected_for_implausible_wpm_is_counted() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        seed_meta(&mut v, 1);
        v.ingest(&DecoderEvent::SpeedUpdate {
            track_id: 1,
            wpm: 60.0,
        });
        let spots = run(&transmission_events(1, &["K5ARH", "T"], 0), &mut v);
        assert!(spots.is_empty());
        let spots = v.ingest(&DecoderEvent::TrackClosed {
            track_id: 1,
            closure: ClosureKind::SignalEnded,
        });
        assert!(spots.is_empty(), "got {spots:?}");
        assert_eq!(v.suppression_counts().pending_beacon_rejected_wpm, 1);
    }

    /// MAN-174 finding 2 (round-10 note): the unconfirmed-speed refusal is
    /// the same gate and is counted the same way.
    #[test]
    fn a_beacon_rejected_for_unconfirmed_wpm_is_counted() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        seed_meta(&mut v, 1);
        let spots = run(&transmission_events(1, &["K5ARH", "T"], 0), &mut v);
        assert!(spots.is_empty());
        let spots = v.ingest(&DecoderEvent::TrackClosed {
            track_id: 1,
            closure: ClosureKind::SignalEnded,
        });
        assert!(spots.is_empty(), "got {spots:?}");
        assert_eq!(v.suppression_counts().pending_beacon_rejected_wpm, 1);
    }

    /// MAN-174 finding 1 backstop: closures arriving survivor-first (track
    /// 1 merges into 3, then track 2 into 1) still land track 2's evidence
    /// on the live root, track 3, and leave no state behind for track 1.
    #[test]
    fn a_chained_merge_out_of_order_still_resolves_at_the_final_survivor() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        seed_meta(&mut v, 2);
        v.ingest(&DecoderEvent::SpeedUpdate {
            track_id: 2,
            wpm: 22.0,
        });
        let spots = run(&transmission_events(2, &["K5ARH", "T"], 0), &mut v);
        assert!(spots.is_empty());
        for (track_id, survivor) in [(1, 3), (2, 1)] {
            let spots = v.ingest(&DecoderEvent::TrackClosed {
                track_id,
                closure: ClosureKind::Bookkeeping {
                    survivor_track_id: Some(survivor),
                },
            });
            assert!(spots.is_empty(), "got {spots:?}");
        }
        seed_meta(&mut v, 3);
        v.ingest(&DecoderEvent::SpeedUpdate {
            track_id: 3,
            wpm: 25.0,
        });
        let spots = v.ingest(&DecoderEvent::TrackClosed {
            track_id: 3,
            closure: ClosureKind::SignalEnded,
        });
        assert_eq!(spots.len(), 1, "got {spots:?}");
        assert_eq!(spots[0].callsign, "K5ARH");
        assert_eq!(spots[0].spot_type, SpotType::Beacon);
        assert!(
            !v.tracks.contains_key(&1),
            "a dead survivor's track state must not be recreated"
        );
    }

    /// MAN-174 finding 4: migration merges both queues by `sample_ts`, so
    /// the cap evicts the true oldest. The loser's stale candidate goes,
    /// not the survivor's oldest newer one.
    #[test]
    fn migration_keeps_the_newest_pending_beacons_by_sample_ts() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        seed_meta(&mut v, 1);
        seed_meta(&mut v, 2);
        let spots = run(&transmission_events(1, &["K5AAZ", "T"], 0), &mut v);
        assert!(spots.is_empty());
        let mut expected = Vec::new();
        for i in 0..MAX_PENDING_BEACONS {
            let call = format!("K5AA{}", (b'A' + i as u8) as char);
            let start = 10_000_000 + i as u64 * 1_000_000;
            let spots = run(
                &transmission_events(2, &[call.as_str(), "T"], start),
                &mut v,
            );
            assert!(spots.is_empty());
            expected.push(call);
        }
        v.ingest(&DecoderEvent::TrackClosed {
            track_id: 1,
            closure: ClosureKind::Bookkeeping {
                survivor_track_id: Some(2),
            },
        });
        let pending = &v.tracks[&2].pending_beacons;
        let calls: Vec<&str> = pending.iter().map(|pb| pb.candidate.as_str()).collect();
        assert_eq!(calls, expected);
        assert!(
            pending.windows(2).all(|w| w[0].sample_ts <= w[1].sample_ts),
            "pending_beacons must stay sorted by sample_ts"
        );
        assert_eq!(v.suppression_counts().pending_beacon_overflow, 1);
    }

    /// MAN-174 finding 5: a deferred Beacon with the SAME `sample_ts` as
    /// an emitted spot cannot rewind the dedupe watermark. It goes through
    /// `Dedupe::should_emit`, where a De -> Beacon type change emits it.
    #[test]
    fn an_equal_timestamp_deferred_beacon_goes_through_dedupe() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        seed_meta(&mut v, 2);
        let words = ["DE", "K5ARH", "K"];
        let mut spots = run(&transmission_events(2, &words, 5_000_000), &mut v);
        spots.extend(run(&transmission_events(2, &words, 5_100_000), &mut v));
        assert_eq!(spots.len(), 1, "the ordinary path must spot normally");
        assert_eq!(spots[0].spot_type, SpotType::De);
        let t = spots[0].sample_ts;

        seed_meta(&mut v, 1);
        v.ingest(&DecoderEvent::SpeedUpdate {
            track_id: 1,
            wpm: 22.0,
        });
        // The capture is stamped at "T"'s word boundary, 700 samples after
        // "K5ARH"'s first character.
        let spots = run(&transmission_events(1, &["K5ARH", "T"], t - 700), &mut v);
        assert!(spots.is_empty());
        assert_eq!(
            v.tracks[&1]
                .pending_beacons
                .iter()
                .map(|pb| pb.sample_ts)
                .collect::<Vec<_>>(),
            vec![t],
            "fixture: the Beacon must be captured at the spot's own timestamp"
        );
        let spots = v.ingest(&DecoderEvent::TrackClosed {
            track_id: 1,
            closure: ClosureKind::SignalEnded,
        });
        assert_eq!(spots.len(), 1, "got {spots:?}");
        assert_eq!(spots[0].spot_type, SpotType::Beacon);
        assert_eq!(spots[0].sample_ts, t);
        assert_eq!(v.suppression_counts().pending_beacon_stale, 0);
    }

    /// Codex review on PR #154, round 8: a regularly-transmitting carrier
    /// resets the silent-GC timer on every decoded character and can stay
    /// ACTIVE (never closing) for the daemon's lifetime, so
    /// `pending_beacons` must not grow without bound while it waits.
    #[test]
    fn pending_beacons_bounded_on_a_track_that_stays_active() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        seed_meta(&mut v, 1);
        v.ingest(&DecoderEvent::SpeedUpdate {
            track_id: 1,
            wpm: 22.0,
        });

        let n = MAX_PENDING_BEACONS + 5;
        let mut ts = 0u64;
        for i in 0..n {
            // 26 distinct, grammar/cty-valid callsigns (K-prefixed, ending
            // in a letter): K5AAA, K5AAB, K5AAC, ...
            let call = format!("K5AA{}", (b'A' + (i % 26) as u8) as char);
            let words = [call.as_str(), "T"];
            let spots = run(&transmission_events(1, &words, ts), &mut v);
            assert!(spots.is_empty(), "must only ever capture, not spot yet");
            ts += 1_000_000;
        }

        let spots = v.ingest(&DecoderEvent::TrackClosed {
            track_id: 1,
            closure: ClosureKind::SignalEnded,
        });
        assert!(
            spots.len() <= MAX_PENDING_BEACONS,
            "pending_beacons must stay bounded at {MAX_PENDING_BEACONS}, got {} spots",
            spots.len()
        );
        assert_eq!(
            v.suppression_counts().pending_beacon_overflow,
            (n - MAX_PENDING_BEACONS) as u64,
            "every permanently-dropped overflow candidate must be counted (ARCHITECTURE §8)"
        );
    }

    /// Codex review on PR #154, round 8: a deferred Beacon candidate can
    /// resolve well after capture. If a NEWER spot for the same
    /// (callsign, freq) identity already recorded fresher dedupe state in
    /// the meantime, resolving the older candidate must never call
    /// `Dedupe::should_emit` with its own older timestamp -- doing so
    /// (given a spot_type change, which alone satisfies `should_emit`'s
    /// own criteria) would silently roll the recorded watermark backward.
    #[test]
    fn resolved_beacon_never_rewinds_dedupe_state_backward() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);

        // Track 1 captures a Beacon candidate for K5ARH at an early
        // timestamp, on 14_000_000.0 Hz (seed_meta's fixture value), but
        // does not close yet.
        seed_meta(&mut v, 1);
        let words = ["K5ARH", "T"];
        let spots = run(&transmission_events(1, &words, 1_000), &mut v);
        assert!(spots.is_empty(), "should be captured, not yet resolved");

        // A newer, ordinary (repetition-confirmed) spot for the SAME
        // callsign/frequency arrives on a different track, well after.
        seed_meta(&mut v, 2);
        let words2 = ["DE", "K5ARH", "K"];
        let mut spots2 = run(&transmission_events(2, &words2, 5_000_000), &mut v);
        spots2.extend(run(&transmission_events(2, &words2, 5_100_000), &mut v));
        assert_eq!(spots2.len(), 1, "the ordinary path must spot normally");
        let newer_ts = spots2[0].sample_ts;

        assert_eq!(
            v.dedupe.last_sample_ts("K5ARH", 14_000_000.0, 0),
            Some(newer_ts),
            "dedupe must have recorded the newer, ordinary spot's timestamp"
        );

        // Track 1 finally closes -- its captured (OLDER) Beacon candidate
        // must not emit now, and must not roll dedupe's watermark back to
        // its own stale timestamp. A plausible, confirmed final speed
        // (MAN-174), so the candidate reaches the stale check rather than
        // the round-10 unconfirmed-WPM refusal.
        v.ingest(&DecoderEvent::SpeedUpdate {
            track_id: 1,
            wpm: 22.0,
        });
        let spots3 = v.ingest(&DecoderEvent::TrackClosed {
            track_id: 1,
            closure: ClosureKind::SignalEnded,
        });
        assert!(
            spots3.is_empty(),
            "an already-superseded deferred candidate must not emit, got {spots3:?}"
        );
        assert_eq!(
            v.dedupe.last_sample_ts("K5ARH", 14_000_000.0, 0),
            Some(newer_ts),
            "resolving the stale deferred candidate must not roll the \
             dedupe watermark backward"
        );
        // MAN-174 finding 6: the stale drop is counted.
        assert_eq!(v.suppression_counts().pending_beacon_stale, 1);
    }

    /// MAN-120 (Codex review on PR #387): the same shape across a 300 Hz
    /// bucket edge. The newer ordinary spot is stored one bucket over,
    /// 2 Hz away -- the same station under MAN-120's same-input edge
    /// join -- so the older deferred Beacon is stale against it too, and
    /// must not emit on its spot-type change.
    #[test]
    fn resolved_beacon_never_emits_against_a_newer_spot_across_a_bucket_edge() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        let meta = |v: &mut Validator, track_id: u32, freq_hz: f64| {
            v.ingest(&DecoderEvent::TrackMeta {
                track_id,
                sample_ts: 0,
                snr_2500_db: 20.0,
                freq_hz,
            });
        };

        // bucket(14_012_549) == 46_708, bucket(14_012_551) == 46_709.
        meta(&mut v, 1, 14_012_549.0);
        let spots = run(&transmission_events(1, &["K5ARH", "T"], 1_000), &mut v);
        assert!(spots.is_empty(), "should be captured, not yet resolved");

        meta(&mut v, 2, 14_012_551.0);
        let words2 = ["DE", "K5ARH", "K"];
        let mut spots2 = run(&transmission_events(2, &words2, 5_000_000), &mut v);
        spots2.extend(run(&transmission_events(2, &words2, 5_100_000), &mut v));
        assert_eq!(spots2.len(), 1, "the ordinary path must spot normally");
        let newer_ts = spots2[0].sample_ts;
        assert_eq!(
            v.dedupe.last_sample_ts("K5ARH", 14_012_549.0, 0),
            Some(newer_ts),
            "the peek must see the same input's spot across the edge"
        );

        v.ingest(&DecoderEvent::SpeedUpdate {
            track_id: 1,
            wpm: 22.0,
        });
        let spots3 = v.ingest(&DecoderEvent::TrackClosed {
            track_id: 1,
            closure: ClosureKind::SignalEnded,
        });
        assert!(
            spots3.is_empty(),
            "an older deferred Beacon across the edge from a newer spot of \
             the same station must not emit, got {spots3:?}"
        );
        assert_eq!(
            v.dedupe.last_sample_ts("K5ARH", 14_012_551.0, 0),
            Some(newer_ts),
            "the newer spot's watermark must stand"
        );
        assert_eq!(v.suppression_counts().pending_beacon_stale, 1);
    }

    /// Codex review on PR #154, round 9: a Merged closure is not evidence
    /// this identity's signal ended -- it may continue on the surviving
    /// track. A captured Beacon candidate must migrate there instead of
    /// being judged (or lost) at the loser's own close, then resolve once
    /// the SURVIVOR truly closes, using the SURVIVOR's own final speed.
    #[test]
    fn merged_track_migrates_pending_beacon_to_the_survivor() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        seed_meta(&mut v, 1);
        v.ingest(&DecoderEvent::SpeedUpdate {
            track_id: 1,
            wpm: 22.0,
        });
        let words = ["K5ARH", "T"];
        let spots = run(&transmission_events(1, &words, 0), &mut v);
        assert!(spots.is_empty(), "should be captured, not yet resolved");

        // Track 1 merges into track 2 -- bookkeeping only, not proof the
        // signal ended.
        let spots = v.ingest(&DecoderEvent::TrackClosed {
            track_id: 1,
            closure: ClosureKind::Bookkeeping {
                survivor_track_id: Some(2),
            },
        });
        assert!(
            spots.is_empty(),
            "a merge must never itself resolve the migrated candidate, got {spots:?}"
        );

        // The survivor eventually closes for real, at a plausible speed.
        seed_meta(&mut v, 2);
        v.ingest(&DecoderEvent::SpeedUpdate {
            track_id: 2,
            wpm: 25.0,
        });
        let spots = v.ingest(&DecoderEvent::TrackClosed {
            track_id: 2,
            closure: ClosureKind::SignalEnded,
        });
        assert_eq!(
            spots.len(),
            1,
            "the migrated candidate must resolve at the survivor's true \
             close, using the survivor's own final speed, got {spots:?}"
        );
        assert_eq!(spots[0].callsign, "K5ARH");
        assert_eq!(spots[0].spot_type, SpotType::Beacon);
    }

    /// Codex review, PR #133 (round 3): when duplicate-spawn track A
    /// merges into survivor B mid-transmission (A catches the first call
    /// utterance, B the second, a few real seconds later), A's identity
    /// continues on B -- it did not genuinely close. Before this fix,
    /// `self.tracks.remove` on ANY `TrackClosed` (MAN-19, both closure
    /// kinds) made `is_track_active(1)` read false the instant A merged,
    /// letting B's later decode manufacture a false second message from
    /// what both times was really one over-the-air transmission.
    #[test]
    fn a_track_merging_into_a_survivor_mid_transmission_does_not_manufacture_a_second_message() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        seed_meta(&mut v, 1); // freq_hz 14_000_000.0
        let words = ["DE", "K5ARH"];
        let spots = run(&transmission_events(1, &words, 0), &mut v);
        assert!(
            spots.is_empty(),
            "one decode alone must not spot (reps < 2)"
        );

        // Track 1 merges into track 2 -- bookkeeping only, identity
        // continues, not proof the signal ended.
        v.ingest(&DecoderEvent::TrackClosed {
            track_id: 1,
            closure: ClosureKind::Bookkeeping {
                survivor_track_id: Some(2),
            },
        });
        seed_meta(&mut v, 2);

        // Track 2 decodes the SAME call again a few real seconds later --
        // comfortably clearing the near-duplicate-track gap, but nowhere
        // near MIN_MESSAGE_TIME_GAP_SECONDS -- still plausibly the same
        // transmission continuing under its new track_id.
        let three_seconds = (3.0 * FS) as u64;
        let spots = run(&transmission_events(2, &words, three_seconds), &mut v);
        assert!(
            spots.is_empty(),
            "track 1's identity continues on survivor track 2 (Bookkeeping \
             merge) -- this must still read as ONE message, not a second \
             confirmation, got {spots:?}"
        );
    }

    /// Codex review, PR #133 (round 4): once the SURVIVOR of a merge
    /// genuinely ends (`SignalEnded`), the whole lineage is retired
    /// immediately, not just whenever the time-based sweep gets to it.
    /// Track 1 decodes once, merges into survivor track 2, and track 2
    /// then genuinely ends. A brand-new track 3 reopening at the same
    /// frequency afterward is exactly the MAN-166 "track closes for real,
    /// reopens later" scenario this gate exists to handle -- its first
    /// decode must complete the repetition (comparing against track 1's
    /// now-correctly-inactive stale entry), not get stuck comparing
    /// against a lineage that (before this fix) never actually retired.
    #[test]
    fn a_survivors_genuine_close_retires_its_merged_aliases_immediately() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        seed_meta(&mut v, 1); // freq_hz 14_000_000.0
        let words = ["DE", "K5ARH"];
        let spots = run(&transmission_events(1, &words, 0), &mut v);
        assert!(
            spots.is_empty(),
            "one decode alone must not spot (reps < 2)"
        );

        // Track 1 merges into track 2.
        v.ingest(&DecoderEvent::TrackClosed {
            track_id: 1,
            closure: ClosureKind::Bookkeeping {
                survivor_track_id: Some(2),
            },
        });

        // Track 2 (the survivor) now genuinely ends -- the whole
        // over-the-air transmission is truly over.
        v.ingest(&DecoderEvent::TrackClosed {
            track_id: 2,
            closure: ClosureKind::SignalEnded,
        });

        // A brand-new track reopens at the same frequency 10s later and
        // decodes the same call once -- MAN-166's own "close and reopen"
        // scenario, which must still complete the repetition gate.
        seed_meta(&mut v, 3);
        let ten_seconds = (10.0 * FS) as u64;
        let spots = run(&transmission_events(3, &words, ten_seconds), &mut v);
        assert_eq!(
            spots.len(),
            1,
            "track 2's genuine close must retire track 1's alias \
             immediately, so track 3's decode reads as a real second \
             confirmation (MAN-166 reopen), not get stuck comparing \
             against a lineage that never retired, got {spots:?}"
        );
        assert_eq!(spots[0].callsign, "K5ARH");
    }

    /// Codex review, PR #133 (round 5): a `Bookkeeping` closure with no
    /// survivor (`survivor_track_id: None` -- an eviction/Silent drop) is
    /// explicitly NOT evidence the RF signal ended, per that event's own
    /// doc comment, yet has no lineage to correlate to a future track the
    /// way a merge does. Track 1 decodes once and is evicted; track 2 (a
    /// duplicate or replacement, not a genuine later reopen) decodes the
    /// same call a few seconds later -- this must still read as ONE
    /// message, not a second confirmation, since track 1 might still be
    /// the exact same over-the-air transmission continuing untracked.
    #[test]
    fn an_evicted_tracks_conservative_liveness_prevents_a_false_second_message() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        seed_meta(&mut v, 1); // freq_hz 14_000_000.0
        let words = ["DE", "K5ARH"];
        let spots = run(&transmission_events(1, &words, 0), &mut v);
        assert!(
            spots.is_empty(),
            "one decode alone must not spot (reps < 2)"
        );

        // Track 1 is evicted -- Bookkeeping with no survivor, not proof
        // the signal ended.
        v.ingest(&DecoderEvent::TrackClosed {
            track_id: 1,
            closure: ClosureKind::Bookkeeping {
                survivor_track_id: None,
            },
        });

        // A different track decodes the same call a few real seconds
        // later -- plausibly the SAME untracked transmission continuing,
        // not a genuine new confirmation.
        seed_meta(&mut v, 2);
        let three_seconds = (3.0 * FS) as u64;
        let spots = run(&transmission_events(2, &words, three_seconds), &mut v);
        assert!(
            spots.is_empty(),
            "an evicted track's conservative liveness marker must prevent \
             a duplicate/replacement track's decode from manufacturing a \
             false second message, got {spots:?}"
        );
    }

    /// Codex review, PR #152, round 14: two duplicate-spawn tracks (a
    /// known real phenomenon -- spectral splatter spawning more than one
    /// candidate for one signal) each independently capture the SAME real
    /// transmission as a Beacon candidate, then both merge into the same
    /// survivor. Resolving both under the survivor's own track_id would
    /// wrongly grant the second one the rapid-same-track exemption (it
    /// looks like the survivor's own back-to-back repeat), letting one
    /// real over-the-air occurrence self-confirm to `reps == 2` --
    /// contaminating the SHARED RepetitionGate entry enough that an
    /// unrelated, later ORDINARY (non-Beacon) decode of the same callsign
    /// would see the repetition gate already satisfied despite only one
    /// real transmission ever having happened. Preserving each
    /// candidate's true origin_track_id keeps the cross-track minimum-gap
    /// rule effective across the merge.
    #[test]
    fn duplicate_tracks_capturing_the_same_beacon_do_not_self_confirm_through_a_shared_survivor() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        let words = ["K5ARH", "T"];

        seed_meta(&mut v, 1);
        let spots = run(&transmission_events(1, &words, 0), &mut v);
        assert!(spots.is_empty(), "should be captured, not yet resolved");

        seed_meta(&mut v, 3);
        let spots = run(&transmission_events(3, &words, 0), &mut v);
        assert!(spots.is_empty(), "should be captured, not yet resolved");

        // Both duplicate-spawn tracks merge into the same survivor, as
        // pure bookkeeping -- neither closure is proof either signal
        // ended.
        v.ingest(&DecoderEvent::TrackClosed {
            track_id: 1,
            closure: ClosureKind::Bookkeeping {
                survivor_track_id: Some(2),
            },
        });
        v.ingest(&DecoderEvent::TrackClosed {
            track_id: 3,
            closure: ClosureKind::Bookkeeping {
                survivor_track_id: Some(2),
            },
        });

        // The survivor closes for real, at a plausible speed -- resolving
        // BOTH migrated candidates. Beacon candidates always spot
        // regardless of reps (ARCHITECTURE §6.4), so both resolve
        // normally either way; the bug is only observable downstream.
        seed_meta(&mut v, 2);
        v.ingest(&DecoderEvent::SpeedUpdate {
            track_id: 2,
            wpm: 25.0,
        });
        let spots = v.ingest(&DecoderEvent::TrackClosed {
            track_id: 2,
            closure: ClosureKind::SignalEnded,
        });
        // Both migrated candidates are identical in every dedupe-relevant
        // way (same callsign/freq/sample_ts, since both duplicate tracks
        // decoded the exact same synthetic transmission) -- spot-level
        // dedupe correctly collapses that to a single emitted spot. The
        // real point of this test is the ordinary decode below, not the
        // count here.
        assert!(
            !spots.is_empty(),
            "at least one candidate resolves, got {spots:?}"
        );

        // A near-simultaneous ORDINARY (non-Beacon) decode of the same
        // callsign, on a genuinely different track. Only one real
        // over-the-air occurrence has ever happened here -- the two
        // Beacon candidates were duplicate spawns of that SAME
        // transmission, and (with origin_track_id preserved) correctly
        // rejected as near-duplicates of each other. The ordinary
        // repetition gate must NOT already see reps >= 2 from that alone.
        seed_meta(&mut v, 6);
        let spots = run(&transmission_events(6, &["DE", "K5ARH", "K"], 700), &mut v);
        assert!(
            spots.is_empty(),
            "the merged duplicate Beacon candidates must not have already satisfied the \
             ordinary repetition gate -- only one real occurrence ever happened, got {spots:?}"
        );
    }

    /// Codex review on PR #154, round 9: an Evicted closure has no
    /// survivor to migrate a captured Beacon candidate to -- it must be
    /// discarded (never resolved), and counted per ARCHITECTURE §8.
    #[test]
    fn evicted_track_discards_pending_beacon_and_counts_it() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        seed_meta(&mut v, 1);
        v.ingest(&DecoderEvent::SpeedUpdate {
            track_id: 1,
            wpm: 22.0,
        });
        let words = ["K5ARH", "T"];
        let spots = run(&transmission_events(1, &words, 0), &mut v);
        assert!(spots.is_empty(), "should be captured, not yet resolved");

        let spots = v.ingest(&DecoderEvent::TrackClosed {
            track_id: 1,
            closure: ClosureKind::Bookkeeping {
                survivor_track_id: None,
            },
        });
        assert!(
            spots.is_empty(),
            "an eviction with no survivor must never resolve the candidate, got {spots:?}"
        );
        assert_eq!(
            v.suppression_counts().pending_beacon_lost_to_eviction,
            1,
            "the discarded candidate must be counted (ARCHITECTURE §8)"
        );
    }

    /// A non-Beacon (De-type) candidate at an implausible 60 WPM still
    /// spots once repetition-confirmed -- the WPM gate must not reach
    /// this path, matching real 45+ WPM contest/computer-keyed CW that
    /// the decoder's own 8..60 WPM tracked range explicitly supports.
    #[test]
    fn implausibly_fast_non_beacon_track_still_spots() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        seed_meta(&mut v, 1);
        v.ingest(&DecoderEvent::SpeedUpdate {
            track_id: 1,
            wpm: 60.0,
        });
        let words = ["DE", "K5ARH", "K"];
        let mut spots = run(&transmission_events(1, &words, 0), &mut v);
        spots.extend(run(&transmission_events(1, &words, 100_000), &mut v));
        assert_eq!(spots.len(), 1);
        assert_eq!(spots[0].callsign, "K5ARH");
        assert_eq!(spots[0].spot_type, SpotType::De);
    }

    /// Sanity check: a plausible-speed Beacon-type candidate still spots
    /// once its track truly closes -- proves the WPM gate isn't rejecting
    /// Beacon spots indiscriminately. Round 7 redesign: a non-allowlisted
    /// Beacon candidate is never emitted before `TrackClosed`, by design
    /// (see `PendingBeacon`'s doc comment), so unlike the pre-redesign
    /// version of this test, a spot only appears after that event.
    #[test]
    fn plausibly_fast_beacon_track_still_spots() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        seed_meta(&mut v, 1);
        v.ingest(&DecoderEvent::SpeedUpdate {
            track_id: 1,
            wpm: 22.0,
        });
        let words = ["K5ARH", "T"];
        let spots = run(&transmission_events(1, &words, 0), &mut v);
        assert!(
            spots.is_empty(),
            "a non-allowlisted Beacon candidate must never spot before TrackClosed, got {spots:?}"
        );

        let spots = v.ingest(&DecoderEvent::TrackClosed {
            track_id: 1,
            closure: ClosureKind::SignalEnded,
        });
        assert_eq!(spots.len(), 1);
        assert_eq!(spots[0].callsign, "K5ARH");
        assert_eq!(spots[0].spot_type, SpotType::Beacon);
    }

    /// MAN-28: allowlisted calls bypass grammar/cty/repetition entirely --
    /// the WPM plausibility gate follows the same exemption boundary, not
    /// a stricter one. "K5ARH" alone (before "T" arrives) spots once via
    /// the allowlist's own no-context fallback (SpotType::Unknown), then
    /// reclassifies to Beacon once "T" completes the power-step pattern --
    /// both are expected to spot regardless of the 60 WPM track speed.
    #[test]
    fn allowlisted_call_bypasses_the_wpm_gate_too() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        v.allowlist("K5ARH");
        seed_meta(&mut v, 1);
        v.ingest(&DecoderEvent::SpeedUpdate {
            track_id: 1,
            wpm: 60.0,
        });
        let words = ["K5ARH", "T"];
        let spots = run(&transmission_events(1, &words, 0), &mut v);
        assert!(
            spots
                .iter()
                .any(|s| s.callsign == "K5ARH" && s.spot_type == SpotType::Beacon),
            "an allowlisted callsign must spot as Beacon regardless of WPM, got {spots:?}"
        );
    }

    #[test]
    fn ungrammatical_text_never_spots() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        let words = ["DE", "12345", "K"];
        let mut spots = run(&transmission_events(1, &words, 0), &mut v);
        spots.extend(run(&transmission_events(1, &words, 100_000), &mut v));
        assert!(spots.is_empty());
    }

    #[test]
    fn error_prosign_discards_current_word() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        let events = vec![
            DecoderEvent::char_decoded(1, 0, Glyph::Char('D'), 0.9),
            DecoderEvent::char_decoded(1, 100, Glyph::Char('E'), 0.9),
            DecoderEvent::word_boundary(1, 200),
            DecoderEvent::char_decoded(1, 300, Glyph::Char('K'), 0.9),
            DecoderEvent::char_decoded(1, 400, Glyph::Prosign(Prosign::Err), 0.0),
        ];
        // after the <ERR> prosign, the partial "K" must be gone.
        for e in &events {
            v.ingest(e);
        }
        let track = v.tracks.get(&1).unwrap();
        assert!(track.current.text.is_empty());
        assert_eq!(track.words.len(), 1);
        assert_eq!(track.words[0].text, "DE");
    }

    #[test]
    fn bundled_validator_spots_a_real_repeated_callsign() {
        let mut v = Validator::bundled(FS);
        seed_meta(&mut v, 1);
        let words = ["DE", "K5ARH", "K"];
        let mut spots = run(&transmission_events(1, &words, 0), &mut v);
        spots.extend(run(&transmission_events(1, &words, 100_000), &mut v));
        assert_eq!(spots.len(), 1);
        assert_eq!(spots[0].callsign, "K5ARH");
        assert_eq!(spots[0].spot_type, SpotType::De);
    }

    /// MAN-166: a real signal's track closing and reopening under a new
    /// `track_id` (e.g. `CloseReason::HangExpired`'s 5s silence timer, or
    /// `Merged`/`Evicted`) must not reset its repetition confirmation --
    /// the callsign is still genuinely repeating within the gate's 90s
    /// window, just under a different `track_id` at roughly the same
    /// frequency.
    #[test]
    fn repetition_survives_a_track_closing_and_reopening_at_the_same_frequency() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        seed_meta(&mut v, 1); // freq_hz 14_000_000.0
        let words = ["DE", "K5ARH", "K"];
        let spots = run(&transmission_events(1, &words, 0), &mut v);
        assert!(
            spots.is_empty(),
            "one decode alone must not spot (reps < 2)"
        );

        run(
            &[DecoderEvent::TrackClosed {
                track_id: 1,
                closure: ClosureKind::SignalEnded,
            }],
            &mut v,
        );
        v.ingest(&DecoderEvent::TrackMeta {
            sample_ts: 0,
            track_id: 2,
            snr_2500_db: 20.0,
            freq_hz: 14_000_030.0, // 30 Hz away -- same signal, same bucket
        });
        let spots = run(&transmission_events(2, &words, 100_000), &mut v);

        assert_eq!(
            spots.len(),
            1,
            "second confirmation, on a new track_id but the same frequency, must spot"
        );
        assert_eq!(spots[0].callsign, "K5ARH");
        assert_eq!(spots[0].track_id, 2);
    }

    /// Two distinct real signals, far enough apart to land in different
    /// frequency buckets, must never share repetition credit even if they
    /// happen to decode the same text.
    #[test]
    fn different_frequency_buckets_never_share_repetition_credit() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        seed_meta(&mut v, 1); // freq_hz 14_000_000.0
        v.ingest(&DecoderEvent::TrackMeta {
            sample_ts: 0,
            track_id: 2,
            snr_2500_db: 20.0,
            freq_hz: 14_001_000.0, // 1 kHz away -- a different bucket
        });
        let words = ["DE", "K5ARH", "K"];
        let mut spots = run(&transmission_events(1, &words, 0), &mut v);
        spots.extend(run(&transmission_events(2, &words, 100_000), &mut v));
        assert!(
            spots.is_empty(),
            "one decode each, at different frequencies, must not share repetition credit"
        );
    }

    /// MAN-29: a configured per-source frequency-calibration correction
    /// (config key `input.freq_correction_ppm`, SPEC-decode-core.md §1.4)
    /// corrects a spot's reported frequency before emission -- distinct
    /// from the ~10 Hz decode-accuracy figure (ARCHITECTURE §6 step 5),
    /// which is decode precision, not a drifted source clock/LO.
    #[test]
    fn calibration_ppm_corrects_emitted_spot_frequency() {
        const RAW_FREQ_HZ: f64 = 14_027_000.0;
        const PPM: f64 = 10.0; // SPEC's own worked example (§1.4).

        let mut v = Validator::new(FS, CTY_FIXTURE, None)
            .with_freq_correction_ppm(PPM)
            .unwrap();
        v.ingest(&DecoderEvent::TrackMeta {
            track_id: 1,
            sample_ts: 0,
            snr_2500_db: 20.0,
            freq_hz: RAW_FREQ_HZ,
        });
        let words = ["DE", "K5ARH", "K"];
        let mut spots = run(&transmission_events(1, &words, 0), &mut v);
        spots.extend(run(&transmission_events(1, &words, 100_000), &mut v));

        assert_eq!(spots.len(), 1);
        let expected = RAW_FREQ_HZ * (1.0 + PPM * 1e-6);
        assert!(
            (spots[0].freq_hz - expected).abs() < 1e-6,
            "spot freq_hz {} should equal raw {RAW_FREQ_HZ} * (1 + {PPM}ppm) = {expected}",
            spots[0].freq_hz
        );
    }

    #[test]
    fn default_calibration_is_identity() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        v.ingest(&DecoderEvent::TrackMeta {
            track_id: 1,
            sample_ts: 0,
            snr_2500_db: 20.0,
            freq_hz: 14_027_000.0,
        });
        let words = ["DE", "K5ARH", "K"];
        let mut spots = run(&transmission_events(1, &words, 0), &mut v);
        spots.extend(run(&transmission_events(1, &words, 100_000), &mut v));

        assert_eq!(spots.len(), 1);
        assert_eq!(spots[0].freq_hz, 14_027_000.0);
    }

    #[test]
    fn calibration_factor_from_ppm_zero_is_identity() {
        assert_eq!(calibration_factor_from_ppm(0.0), Ok(1.0));
    }

    #[test]
    fn calibration_factor_from_ppm_rejects_nan() {
        assert!(calibration_factor_from_ppm(f64::NAN).is_err());
    }

    #[test]
    fn calibration_factor_from_ppm_rejects_infinity() {
        assert!(calibration_factor_from_ppm(f64::INFINITY).is_err());
        assert!(calibration_factor_from_ppm(f64::NEG_INFINITY).is_err());
    }

    #[test]
    fn calibration_factor_from_ppm_rejects_a_factor_that_hits_zero_or_goes_negative() {
        // ppm = -1_000_000 drives factor to exactly 0.0; anything more
        // negative flips it negative.
        assert!(calibration_factor_from_ppm(-1_000_000.0).is_err());
        assert!(calibration_factor_from_ppm(-2_000_000.0).is_err());
    }

    /// MAN-29 review round 2: a finite ppm whose derived factor is itself
    /// finite and positive can still overflow to infinity once multiplied
    /// against a real RF frequency (e.g. `f64::MAX` -> factor ~1.8e302).
    /// Reject ppm outside any physically-plausible oscillator drift, not
    /// just outside "finite and positive".
    #[test]
    fn calibration_factor_from_ppm_rejects_absurdly_large_finite_ppm() {
        assert!(calibration_factor_from_ppm(f64::MAX).is_err());
        assert!(calibration_factor_from_ppm(1e300).is_err());
    }

    #[test]
    fn calibration_factor_from_ppm_accepts_realistic_oscillator_drift() {
        // SPEC's own worked example (10 ppm) and a generously bad cheap-SDR
        // crystal (a few hundred ppm) both stay accepted.
        assert!(calibration_factor_from_ppm(10.0).is_ok());
        assert!(calibration_factor_from_ppm(-500.0).is_ok());
    }

    #[test]
    fn with_freq_correction_ppm_rejects_an_invalid_factor_before_use() {
        match Validator::new(FS, CTY_FIXTURE, None).with_freq_correction_ppm(f64::NAN) {
            Ok(_) => panic!("expected NaN ppm to be rejected"),
            Err(err) => assert!(err.ppm.is_nan()),
        }
    }

    /// MAN-29 review round 3: a ppm just outside the supported range
    /// (whose derived factor is otherwise perfectly finite and positive)
    /// must not be reported with the "must be finite and positive"
    /// message -- that's not why it failed, and the message must name the
    /// actual supported range so a caller can fix their input.
    #[test]
    fn invalid_calibration_display_names_the_range_when_ppm_is_out_of_range() {
        let err = calibration_factor_from_ppm(1_001.0).unwrap_err();
        let msg = err.to_string();
        assert!(
            !msg.contains("must be finite and positive"),
            "1001 ppm's factor (1.001001) IS finite and positive -- the real problem is the \
             ppm range, message was: {msg}"
        );
        assert!(
            msg.contains("1000"),
            "expected the message to name the supported range, got: {msg}"
        );
    }

    #[test]
    fn invalid_calibration_display_reports_non_finite_ppm() {
        let err = calibration_factor_from_ppm(f64::NAN).unwrap_err();
        assert!(err.to_string().contains("finite"));
    }

    /// MAN-19: `TrackClosed` must remove the track's `TrackState` from
    /// `self.tracks` (and its `RepetitionGate` state) -- before this event
    /// existed, nothing ever did, and `tracks` grew one entry per
    /// historical track_id for the life of the process.
    #[test]
    fn track_closed_removes_the_track_from_state() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        seed_meta(&mut v, 1);
        assert!(
            v.tracks.contains_key(&1),
            "TrackMeta should have created an entry"
        );

        v.ingest(&DecoderEvent::TrackClosed {
            track_id: 1,
            closure: ClosureKind::SignalEnded,
        });
        assert!(
            !v.tracks.contains_key(&1),
            "TrackClosed must remove the track's state"
        );
    }

    /// A `TrackClosed` for a track_id `Validator` never saw (e.g. a
    /// CANDIDATE that closed Unconfirmed without ever being promoted, so
    /// it never emitted any other event) must be a harmless no-op, not a
    /// panic or a spurious insertion.
    #[test]
    fn track_closed_for_an_unknown_track_id_is_a_harmless_noop() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        assert_eq!(
            v.ingest(&DecoderEvent::TrackClosed {
                track_id: 42,
                closure: ClosureKind::SignalEnded
            }),
            vec![]
        );
        assert!(!v.tracks.contains_key(&42));
    }

    /// MAN-19: reproduces the soak's actual failure mode at unit-test
    /// scale -- many distinct, never-reused track_ids, each getting real
    /// activity (TrackMeta) then closing. Without `TrackClosed` wired
    /// through to `self.tracks.remove`/`self.gate.sweep`, `tracks`
    /// would have 10,000 entries here instead of 0.
    #[test]
    fn sustained_track_churn_stays_bounded() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        for track_id in 0..10_000u32 {
            seed_meta(&mut v, track_id);
            v.ingest(&DecoderEvent::TrackClosed {
                track_id,
                closure: ClosureKind::SignalEnded,
            });
        }
        assert_eq!(
            v.tracks.len(),
            0,
            "Validator.tracks must not accumulate one entry per historical track_id"
        );
    }

    /// Codex review, PR #152: `TrackClosed`'s sweep must use a
    /// validator-wide monotonic clock, not the closing track's own
    /// `last_sample_ts` -- a track that closes having emitted metadata
    /// but no `WordBoundary` still has `last_sample_ts == 0`, so sweeping
    /// with that stale value is a no-op (`sweep`'s own cutoff saturates to
    /// 0) and leaves the gate's `seen` map growing without bound under
    /// exactly this kind of churn, reintroducing the MAN-19 leak this
    /// mechanism exists to prevent.
    #[test]
    fn gate_stays_bounded_under_churn_even_when_closing_tracks_have_a_stale_own_timestamp() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        let window_samples = (90.0 * FS) as u64;
        let words = ["DE", "K5ARH", "K"];

        // Many distinct real signals (different frequencies, one gate
        // entry each), each confirmed once early (small sample_ts) then
        // closed -- every one of these tracks' own `last_sample_ts` stays
        // small/stale relative to the far-future check below.
        for track_id in 0..2_000u32 {
            v.ingest(&DecoderEvent::TrackMeta {
                sample_ts: 0,
                track_id,
                snr_2500_db: 20.0,
                freq_hz: 14_000_000.0 + (track_id as f64) * 1000.0,
            });
            run(&transmission_events(track_id, &words, 0), &mut v);
            v.ingest(&DecoderEvent::TrackClosed {
                track_id,
                closure: ClosureKind::SignalEnded,
            });
        }
        assert!(
            !v.gate.is_empty(),
            "sanity check: the loop above must actually have populated the gate"
        );

        // A real WordBoundary, far past the 90s window, advances the
        // validator's own clock -- this is the only source of "now" a
        // correct implementation has, since every closing track above was
        // stuck at an early, stale `last_sample_ts`.
        // Comfortably past window_samples relative to the small
        // (hundreds-of-samples) timestamps every track above used, so the
        // 2,000 old entries are genuinely expired, not just past a cutoff
        // that's still behind their own real timestamps.
        seed_meta(&mut v, 99_999);
        v.ingest(&DecoderEvent::word_boundary(99_999, window_samples * 2));
        v.ingest(&DecoderEvent::TrackClosed {
            track_id: 99_999,
            closure: ClosureKind::SignalEnded,
        });

        assert_eq!(
            v.gate.len(),
            0,
            "gate must not accumulate one entry per historical frequency once genuinely swept past the window"
        );
    }

    /// Codex review, PR #152: a persistently-active track that never
    /// closes (e.g. real noise/QRM continually feeding it new plausible-
    /// but-wrong callsigns, each resetting its own silent-GC timer so it
    /// never emits `TrackClosed`) must not let the gate grow unbounded
    /// just because sweep was only ever wired to that one event -- it
    /// must also run periodically off the same monotonic clock.
    #[test]
    fn gate_sweeps_periodically_even_when_no_track_ever_closes() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        seed_meta(&mut v, 1); // one persistent track_id, never closed below

        // 500 distinct plausible callsigns on the same never-closing
        // track, spaced ~1.04s apart -- comfortably past both the
        // repetition gate's own window and any reasonable sweep interval
        // by the end of the loop, all without a single TrackClosed.
        for i in 0..500u32 {
            let call = format!("K{i}Y");
            let words = ["DE", &call, "K"];
            run(&transmission_events(1, &words, i as u64 * 100_000), &mut v);
        }

        assert!(
            v.gate.len() < 100,
            "gate must be swept periodically even with no TrackClosed at all, not just accumulate one entry per historical callsign (got {})",
            v.gate.len()
        );
    }
}
