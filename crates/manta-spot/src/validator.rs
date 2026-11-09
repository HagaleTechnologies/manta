//! Ties `grammar`/`context`/`cty`/`scp`/`confidence`/`gate`/`dedupe`
//! together into one `Validator::ingest` entry point. ARCHITECTURE §6.

use crate::blocklist::Blocklist;
use crate::confidence;
use crate::context::{self, SpotType};
use crate::cty;
use crate::decision::{CandidateSupport, Confirmation, Decision, Reason, Rival};
use crate::dedupe::{Dedupe, DedupeOutcome};
use crate::exchange::{self, Basis, ExchangeWord, Frame, Role};
use crate::gate::{
    GateOccurrence, RepetitionCounts, RepetitionGate, WINDOW_SECONDS as GATE_WINDOW_SECONDS,
};
use crate::grammar;
use crate::lattice::{self, EvidenceLattice, LatticeCounts};
use crate::lookup::{CallsignLookup, Existence, LookupStatus};
use crate::message;
use crate::notch::NotchList;
use crate::observation::{
    Cause, Draft, EvidenceLink, Finding, FreqSpan, ObservationCounts, ObservationId,
    ObservationLog, ObservationPolicy, RoleEvidence, SampleSpan, Scores, Source, TextTrace,
};
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

/// Whether a published spot met its pattern tier's repetition requirement.
/// Exempt single-decode spots can publish as tentative. Zero-evidence
/// candidates are withheld as `Reason::NoEvidence` (MAN-106).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EmissionState {
    Confirmed,
    Tentative,
}

/// The mode a spot was decoded in (MAN-322). Every spot the validator
/// builds today is `Cw`; `Rtty` is set by the RTTY decode path the MAN-321
/// follow-up adds. Outputs read it to pick the mode label and the speed
/// unit (WPM for CW, baud for RTTY).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum SpotMode {
    #[default]
    Cw,
    Rtty,
}

impl SpotMode {
    /// The wire label: `"CW"` or `"RTTY"`, as JSON, the telnet mode column
    /// and the RBN uplink line spell it.
    pub fn label(self) -> &'static str {
        match self {
            SpotMode::Cw => "CW",
            SpotMode::Rtty => "RTTY",
        }
    }
}

/// A validated spot, ready for `manta-server` to serialize and emit.
/// No wall-clock timestamp -- that conversion happens at the
/// `manta-server` boundary (SPEC-decode-core.md §5), not here.
///
/// Serialized only by the CLI (`decode --json`'s `spots`, `run --json`'s
/// `{"spot": …}`) through `SpotJson`; field names follow the JSON spot
/// stream (MAN-139, docs/CLI-JSON.md). manta-server builds its own
/// `SpotMessage` instead.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(into = "SpotJson")]
pub struct Spot {
    pub callsign: String,
    pub freq_hz: f64,
    /// 2500 Hz-referenced; `snr` in JSON.
    pub snr_db: f32,
    /// CW keying speed in WPM. Outputs read it only when `mode` is
    /// [`SpotMode::Cw`]; an RTTY spot carries its speed in `baud` (MAN-322).
    pub wpm: f32,
    pub spot_type: SpotType,
    pub confidence: f32,
    /// `confirmed` when the pattern tier's repetition requirement is met,
    /// otherwise `tentative`. CLI JSON only (MAN-106).
    pub emission_state: EmissionState,
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
    /// The mode this spot was decoded in (MAN-322). The validator stamps
    /// `Cw`; the RTTY decode path sets `Rtty`.
    pub mode: SpotMode,
    /// RTTY symbol rate in baud (`45.45` for standard amateur RTTY), or
    /// `None` on a CW spot (MAN-322).
    pub baud: Option<f32>,
}

/// The CLI JSON shape of a [`Spot`] (MAN-139, docs/CLI-JSON.md): `wpm` is
/// `null` on a non-CW spot, and `mode`/`baud` follow `qrlQuery` (MAN-322).
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SpotJson {
    #[serde(rename = "dxCall")]
    callsign: String,
    #[serde(rename = "frequency")]
    freq_hz: f64,
    #[serde(rename = "snr")]
    snr_db: f32,
    wpm: Option<f32>,
    spot_type: SpotType,
    #[serde(rename = "decodeConfidence")]
    confidence: f32,
    emission_state: EmissionState,
    track_id: u32,
    sample_ts: u64,
    rst: Option<String>,
    qrl_query: bool,
    mode: SpotMode,
    baud: Option<f32>,
}

impl From<Spot> for SpotJson {
    fn from(spot: Spot) -> Self {
        // Exhaustive: a new `Spot` field stops this compiling until it is
        // serialized.
        let Spot {
            callsign,
            freq_hz,
            snr_db,
            wpm,
            spot_type,
            confidence,
            emission_state,
            track_id,
            sample_ts,
            rst,
            qrl_query,
            mode,
            baud,
        } = spot;
        Self {
            callsign,
            freq_hz,
            snr_db,
            wpm: (mode == SpotMode::Cw).then_some(wpm),
            spot_type,
            confidence,
            emission_state,
            track_id,
            sample_ts,
            rst,
            qrl_query,
            mode,
            baud,
        }
    }
}

#[derive(Default)]
struct Word {
    refine_hypothesis: Option<&'static str>,
    decoded_wpm: f32,
    text: String,
    confidences: Vec<f32>,
    /// MAN-200: each character's alternatives, parallel to `confidences`.
    /// Kept at every observation detail level, unlike `alternatives`.
    char_alternatives: Vec<Vec<(char, f32)>>,
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
    /// MAN-201: this word's `<call> T` occurrence has recorded its
    /// `BeaconNoEvidence` decision; each occurrence counts once.
    beacon_no_evidence_recorded: bool,
    /// MAN-207: the first character's `sample_ts`; with `end_ts`, the
    /// word's sample span (MAN-200 D2).
    start_ts: u64,
    /// MAN-201: the `WordBoundary` `sample_ts` this word completed at. A
    /// settled occurrence is evaluated -- repetition gate, dedupe, decision
    /// and spot -- at this time, not at the event that settled it (D5).
    /// MAN-207 also records it as the observation's sample-span end.
    end_ts: u64,
    /// MAN-207: the lowest and highest raw track frequency read while
    /// this word was decoded (`None` before the track's first `TrackMeta`).
    freq_lo_hz: Option<f64>,
    freq_hi_hz: Option<f64>,
    /// MAN-207: the decoder's competing glyphs per character, kept only at
    /// `Detail::Text` (empty, unallocated, otherwise).
    alternatives: Vec<Vec<(Glyph, f32)>>,
    /// MAN-201: a turn ends after this word -- an `AR`/`SK` prosign glyph,
    /// or `TURN_SILENCE_SECONDS` with no decoded character after it.
    ends_turn: bool,
    /// MAN-201: an `<ERR>` prosign arrived right after this word.
    corrected: bool,
    /// MAN-201 D6a: the role and frame `exchange::interpret` gave this word
    /// the first time its occurrence settled, never overwritten. Without it
    /// a framing word scrolling out of the window could turn a settled
    /// `relayed_mention` into `uncertain`.
    role: Option<(Role, Option<Frame>)>,
    /// MAN-207: the rule that decided `role`, latched with it.
    basis: Option<Basis>,
    /// MAN-207: the first and last `seq` of the words that decided `role`,
    /// latched with it.
    context_seqs: Option<(u64, u64)>,
    /// MAN-207 D6: the MAN-201 turn index on the track, the number of turn
    /// ends before this word. Set when the word is pushed.
    episode: u32,
    /// MAN-207 D10: this word's role-skip has been counted and recorded;
    /// each occurrence counts once.
    not_candidate_recorded: bool,
    /// MAN-208: this occurrence was withheld on a genuinely ambiguous
    /// candidate -- below its repetition requirement, or beaten by a
    /// better-supported rival -- so `refine_candidates` offers it to the
    /// refinement scheduler. Cleared by a refinement of the word.
    refine_hint: bool,
    /// MAN-208: the rival that withheld this occurrence (`Reason::Variant`),
    /// latched with `refine_hint` so `refine_candidates` can report it.
    rival: Option<String>,
}

impl Word {
    /// MAN-207: widens the frequency span with one reading.
    fn note_freq(&mut self, freq_hz: f64) {
        self.freq_lo_hz = Some(self.freq_lo_hz.map_or(freq_hz, |lo| lo.min(freq_hz)));
        self.freq_hi_hz = Some(self.freq_hi_hz.map_or(freq_hz, |hi| hi.max(freq_hz)));
    }
}

/// MAN-201: the silence, in seconds with no decoded character, that ends a
/// turn. The longest character-free stretch inside one transmission at the
/// slowest supported speed, 8 WPM (dit 150 ms), is a 7-dit word gap plus the
/// 19-dit `0` plus its 3-dit letter gap, about 4.35 s; rounded out. A turn
/// end bounds every frame lookup in `exchange` and settles the call before
/// it.
const TURN_SILENCE_SECONDS: f64 = 5.0;

/// One candidate from `Validator::candidates`: the call, its spot type, the
/// highest `Word::seq` involved, the exact word's seq (`None` never comes
/// from `candidates` since MAN-201; kept for `evaluate_candidate`'s
/// contract) and that word's own `end_ts`.
type Candidate = (String, SpotType, u64, Option<u64>, u64);

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
    /// MAN-200 D2: the captured word's first character's `sample_ts`.
    start_ts: u64,
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
    /// MAN-208: `next_word_seq` when the latest `SpeedUpdate` arrived --
    /// how many words the current speed has held across
    /// (`RefineCandidate::wpm_stable`).
    wpm_since_word_seq: u64,
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
    /// `<call> T` occurrences of a plausible, allocated call that is not on
    /// the allowlist (MAN-201): a trailing `T` alone is not beacon evidence.
    /// Counted once per occurrence. The two power-step counts partition the
    /// shape: `beacon_no_evidence` for an unguarded, unlisted call,
    /// `power_step_guard` for an allowlisted, guarded one.
    pub beacon_no_evidence: u64,
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
    /// `TURN_SILENCE_SECONDS` in samples, cached at construction (MAN-201).
    turn_silence_samples: u64,
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
    /// MAN-318: validation's last step (`set_callsign_lookup`). `None`, the
    /// default, never asks.
    lookup: Option<Arc<dyn CallsignLookup>>,
    /// MAN-318: spots held until their base call's lookup settles, keyed by
    /// that base call, in hold order.
    held: BTreeMap<String, Vec<HeldSpot>>,
    /// MAN-200: the opt-in evidence lattice (`with_repetition_fusion`).
    /// `None`, the default, allocates nothing.
    lattice: Option<EvidenceLattice>,
    /// MAN-207: the bounded observation record. Counts every decision at
    /// every detail level; keeps records only when `with_observations`
    /// asks for them.
    observations: ObservationLog,
    /// The sample rate, for `with_observations`.
    fs: f64,
}

/// MAN-318: a spot waiting for its callsign lookup.
struct HeldSpot {
    spot: Spot,
    reps: u32,
    /// MAN-200 D12: how the spot met its repetition requirement.
    confirmation: Confirmation,
    /// MAN-207: the occurrence the held spot was decoded from.
    id: ObservationId,
}

/// MAN-200 D12: the marker a spotted decision carries -- only a confirmed
/// spot met its repetition requirement.
fn confirmation_of(spot: &Spot, how: Confirmation) -> Option<Confirmation> {
    (spot.emission_state == EmissionState::Confirmed).then_some(how)
}

/// MAN-207: which occurrence a decision is about, why it is recorded, and
/// what the site knows about it beyond the `Decision`.
#[derive(Clone)]
struct At {
    id: ObservationId,
    cause: Cause,
    scores: Scores,
    evidence: Vec<EvidenceLink>,
}

impl At {
    fn new(track_id: u32, word_seq: u64, cause: Cause) -> Self {
        Self {
            id: ObservationId { track_id, word_seq },
            cause,
            scores: Scores::default(),
            evidence: Vec::new(),
        }
    }

    fn with_call_confidence(mut self, confidence: f32) -> Self {
        self.scores.call_confidence = Some(confidence);
        self
    }
}

fn evidence_links(counted: &[GateOccurrence]) -> Vec<EvidenceLink> {
    counted
        .iter()
        .map(|o| EvidenceLink {
            observation: ObservationId {
                track_id: o.track_id,
                word_seq: o.word_seq,
            },
            sample_ts: o.sample_ts,
            message_distinct: o.message_distinct,
        })
        .collect()
}

/// How often (in seconds of `sample_ts`) the repetition gate is swept
/// independent of any `TrackClosed` event (MAN-166, Codex review PR
/// #152). Small relative to the gate's own 90s window (`gate::
/// WINDOW_SECONDS`) so a persistently-open, never-closing track can't
/// let it grow far past its steady-state size before the next sweep --
/// a reasoned choice, not yet measured against real long-run data.
const SWEEP_INTERVAL_SECONDS: f64 = 10.0;

/// MAN-208: one externally computed refinement of a retained occurrence --
/// the result of re-decoding that occurrence's preserved samples under an
/// alternate frontend/timing configuration. Same occurrence, same span;
/// only the reading may change.
#[derive(Debug, Clone)]
pub struct RefinedWord {
    /// The call the re-decode read. May equal the causal decode's.
    pub call: String,
    /// Per-character confidences of the re-decoded reading.
    pub confidences: Vec<f32>,
    /// The occurrence's original sample span `(start, end)` -- the span the
    /// refinement must attach to, never a new one.
    pub span: (u64, u64),
    /// The re-decode's whole-word confidence (geometric mean).
    pub word_confidence: f32,
    /// Which alternate configuration produced this reading (e.g.
    /// `"engine:legacy"`, `"wpm-seed"`), for the scheduler's own
    /// bookkeeping.
    pub hypothesis: &'static str,
}

/// MAN-515: what [`Validator::apply_refinement_with_outcome`] did with one
/// refined reading, for the scheduler's outcome counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefinementOutcome {
    /// The occurrence's word had left the window (or its track was gone):
    /// the record was revised, nothing was re-evaluated.
    Orphaned,
    /// The reading equals the word's decoded text.
    Unchanged,
    /// The reading replaced the word's decoded text, and the occurrence's
    /// repetition support moved to it.
    Changed,
    /// The reading would have turned a spottable callsign into a string
    /// that can never be spotted, so it was refused: the decoded word,
    /// its support and its ledger entry are untouched.
    Rejected,
}

/// MAN-525: the nearest other track's frequency distance from `freq_hz`,
/// read from `sorted` (finite frequencies, ascending by `(frequency,
/// track_id)`) by binary search: the closest entry below `freq_hz`, and the
/// first entry at or above it that is not `track_id` itself. Exact against
/// the fold over every track because rounding is monotone (for
/// `a <= b <= f`, `fl(f - b) <= fl(f - a)`). `None` for a non-finite
/// `freq_hz`, which the caller answers with the fold.
fn nearest_gap_hz(sorted: &[(f64, u32)], track_id: u32, freq_hz: f64) -> Option<f64> {
    if !freq_hz.is_finite() {
        return None;
    }
    let p = sorted.partition_point(|&(f, _)| f < freq_hz);
    let mut gap = f64::INFINITY;
    if p > 0 {
        gap = gap.min((sorted[p - 1].0 - freq_hz).abs());
    }
    if let Some(&(f, _)) = sorted[p..].iter().find(|&&(_, id)| id != track_id) {
        gap = gap.min((f - freq_hz).abs());
    }
    Some(gap)
}

/// MAN-208: one retained occurrence the refinement scheduler may re-decode
/// (`Validator::refine_candidates`). A read-only view borrowing the
/// validator; a caller that keeps a candidate across a mutation copies the
/// plain fields it needs (MAN-525). Deciding anything stays with the
/// validator.
#[derive(Debug, Clone, Copy)]
pub struct RefineCandidate<'a> {
    pub episode_wpm: f32,
    pub neighbor_gap_hz: f64,
    pub latest_word_seq: u64,
    pub track_id: u32,
    pub word_seq: u64,
    /// The occurrence's currently decoded call.
    pub call: &'a str,
    /// `(start_ts, end_ts)` of the occurrence, for replay range selection
    /// and span-matching the refined words back to occurrences.
    pub span: (u64, u64),
    /// The repetition count the occurrence's evaluation last saw.
    pub reps: u32,
    /// The track's latest speed, used by a seeded replay.
    pub wpm: f32,
    /// The better-supported rival that withheld it, if any.
    pub rival: Option<&'a str>,
    /// How many of its characters carried unresolved decoder alternatives.
    pub alternatives: usize,
    /// Whether `wpm` is a reliable estimate: the track has decoded at
    /// least [`STABLE_SPEED_WORDS`] words since its last `SpeedUpdate`, so
    /// the speed held rather than just jumped. The speed trigger requires
    /// it.
    pub wpm_stable: bool,
}

/// MAN-208: words a track must decode after its latest `SpeedUpdate`
/// before that speed counts as a stable new estimate for the refinement
/// speed trigger. `SpeedUpdate` fires only on a >= 1 WPM change (SPEC
/// §5), so "no update across two whole words" means the estimate held.
pub const STABLE_SPEED_WORDS: u64 = 2;

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
            turn_silence_samples: (TURN_SILENCE_SECONDS * fs) as u64,
            merged_into: BTreeMap::new(),
            decisions: None,
            input_count: 1,
            lookup: None,
            held: BTreeMap::new(),
            lattice: None,
            observations: ObservationLog::new(fs, ObservationPolicy::summary()),
            fs,
        }
    }

    /// MAN-200: turn repetition-evidence fusion on (`[spot]
    /// repetition_fusion`). On, damaged copies of one station's CQ/DE
    /// self-ID that the exact path did not confirm are combined by
    /// `lattice::EvidenceLattice`; a confirmed winner passes the usual
    /// checks and spots with `Confirmation::Fused`. Off (the default)
    /// changes nothing and allocates nothing.
    pub fn with_repetition_fusion(mut self, on: bool) -> Self {
        self.lattice = on.then(|| EvidenceLattice::new(self.fs));
        self
    }

    /// MAN-200: the lattice's counters, `None` when fusion is off.
    pub fn lattice_counts(&self) -> Option<LatticeCounts> {
        self.lattice.as_ref().map(EvidenceLattice::counts)
    }

    /// MAN-318: asks `lookup` whether each would-be spot's call exists, as
    /// validation's last step. See `admit`.
    pub fn set_callsign_lookup(&mut self, lookup: Arc<dyn CallsignLookup>) {
        self.lookup = Some(lookup);
    }

    /// MAN-318: validation's last step. With a lookup set and the call not
    /// allowlisted, a call the lookup does not list is withheld, and one it
    /// has not answered yet is held until `poll_lookups`/`finish_lookups`.
    /// Dedupe has already stored the spot either way.
    fn admit(
        &mut self,
        spot: Spot,
        reps: u32,
        allowlisted: bool,
        confirmation: Confirmation,
        at: At,
    ) -> Option<Spot> {
        let at = at.with_call_confidence(spot.confidence);
        let lookup = match &self.lookup {
            Some(l) if !allowlisted => l.clone(),
            _ => {
                self.decide(at, None, || {
                    Decision::spotted(&spot, reps, confirmation_of(&spot, confirmation))
                });
                return Some(spot);
            }
        };
        let key = lookup_key(&spot.callsign).to_string();
        match lookup.check(&key) {
            LookupStatus::Known(Existence::NotFound) => {
                let reason = Reason::LookupNotFound;
                self.decide(at, Some(reason), || lookup_decision(&spot, reason, reps));
                None
            }
            LookupStatus::Known(Existence::Exists | Existence::Unknown) => {
                self.decide(at, None, || {
                    Decision::spotted(&spot, reps, confirmation_of(&spot, confirmation))
                });
                Some(spot)
            }
            LookupStatus::Pending => {
                let id = at.id;
                let reason = Reason::LookupPending;
                self.decide(at, Some(reason), || lookup_decision(&spot, reason, reps));
                self.held.entry(key).or_default().push(HeldSpot {
                    spot,
                    reps,
                    confirmation,
                    id,
                });
                None
            }
        }
    }

    /// MAN-318: held spots whose lookups settled since the last call, in
    /// settle order (one call's spots in hold order). Empty, without
    /// allocating, when no lookup is set. Call once per batch.
    pub fn poll_lookups(&mut self) -> Vec<Spot> {
        match self.lookup.clone() {
            Some(lookup) => self.release(lookup.settled()),
            None => Vec::new(),
        }
    }

    /// MAN-318: settles every held spot, waiting at most each call's own
    /// deadline. Anything the lookup failed to report is spotted (as
    /// `Unknown`), so no held spot is ever lost.
    pub fn finish_lookups(&mut self) -> Vec<Spot> {
        let Some(lookup) = self.lookup.clone() else {
            return Vec::new();
        };
        let mut spots = self.release(lookup.settle_all());
        let rest: Vec<(String, Existence)> = self
            .held
            .keys()
            .map(|k| (k.clone(), Existence::Unknown))
            .collect();
        spots.extend(self.release(rest));
        spots
    }

    /// MAN-318: decides the held spots of each settled call. The operator's
    /// lists can be replaced (MAN-78, SIGHUP) while a spot is held, so each
    /// one is checked against the current lists first: a newly blocklisted
    /// call or notched frequency is withheld and counted as such, and a
    /// newly allowlisted call spots whatever the lookup said, as it would
    /// have had it been allowlisted at capture.
    fn release(&mut self, settled: Vec<(String, Existence)>) -> Vec<Spot> {
        let mut spots = Vec::new();
        for (key, existence) in settled {
            let Some(held) = self.held.remove(&key) else {
                continue;
            };
            for HeldSpot {
                spot,
                reps,
                confirmation,
                id,
            } in held
            {
                let at = At::new(id.track_id, id.word_seq, Cause::Settled)
                    .with_call_confidence(spot.confidence);
                if let Some(reason) = self.held_operator_suppressed(&spot) {
                    self.decide(at, Some(reason), || lookup_decision(&spot, reason, reps));
                } else if existence == Existence::NotFound
                    && !self.allowlist.contains(&spot.callsign)
                {
                    let reason = Reason::LookupNotFound;
                    self.decide(at, Some(reason), || lookup_decision(&spot, reason, reps));
                } else {
                    self.decide(at, None, || {
                        Decision::spotted(&spot, reps, confirmation_of(&spot, confirmation))
                    });
                    spots.push(spot);
                }
            }
        }
        spots
    }

    /// MAN-318: like `operator_suppressed`, for a spot held on its lookup:
    /// counts and reports one the current lists suppress. A no-op when the
    /// lists were not replaced, since capture already passed the same checks.
    fn held_operator_suppressed(&mut self, spot: &Spot) -> Option<Reason> {
        if self.blocklist.contains(&spot.callsign) {
            self.suppression_counts.blocklist += 1;
            return Some(Reason::Blocklist);
        }
        if self.notch.contains(spot.freq_hz) {
            self.suppression_counts.notch += 1;
            return Some(Reason::Notch);
        }
        None
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

    /// MAN-207: keep observation records at `policy`'s detail level. The
    /// default keeps counters only. Never changes which spots are emitted.
    pub fn with_observations(mut self, policy: ObservationPolicy) -> Self {
        self.observations = ObservationLog::new(self.fs, policy);
        self
    }

    /// MAN-207: the observation record.
    pub fn observations(&self) -> &ObservationLog {
        &self.observations
    }

    /// MAN-208: marks one retained word as a refinement candidate (see
    /// `Word::refine_hint`). Private: only the validator's own ambiguity
    /// decisions set it, and only `apply_refinement` clears it.
    fn note_refine_hint(&mut self, track_id: u32, word_seq: u64, rival: Option<String>) {
        if let Some(track) = self.tracks.get_mut(&track_id) {
            if let Some(word) = track.words.iter_mut().find(|w| w.seq == word_seq) {
                word.refine_hint = true;
                word.rival = word.rival.take().or(rival);
            }
        }
    }

    /// MAN-208: retained occurrences worth re-decoding under an alternate
    /// frontend/timing configuration, read-only. An occurrence qualifies
    /// when its word was withheld below its repetition requirement or
    /// beaten by a better-supported rival (`refine_hint`), or when the
    /// decoder offered per-character alternatives it never resolved, and
    /// its span ends inside the last `lookback_samples` before `now_ts`.
    /// Ordered by `(track_id, word_seq)` so the caller's admission order is
    /// stable. Decides nothing and records nothing.
    ///
    /// MAN-525: neighbour gaps come from one frequency-sorted pass per
    /// call, built on the first candidate, and each track's gap is computed
    /// once, on its first candidate; the text is borrowed, not cloned.
    pub fn refine_candidates(
        &self,
        now_ts: u64,
        lookback_samples: u64,
    ) -> Vec<RefineCandidate<'_>> {
        let mut out = Vec::new();
        let cutoff = now_ts.saturating_sub(lookback_samples);
        let mut sorted: Option<Vec<(f64, u32)>> = None;
        for (&track_id, track) in &self.tracks {
            let mut gap: Option<f64> = None;
            for word in &track.words {
                if !word.attempted {
                    continue;
                }
                if word.end_ts < cutoff {
                    continue;
                }
                let alternatives = word
                    .char_alternatives
                    .iter()
                    .filter(|a| !a.is_empty())
                    .count();
                if !word.refine_hint && alternatives == 0 {
                    continue;
                }
                let neighbor_gap_hz = *gap.get_or_insert_with(|| {
                    let s = sorted.get_or_insert_with(|| self.sorted_track_freqs());
                    self.neighbor_gap_hz(s, track_id, track.freq_hz)
                });
                out.push(RefineCandidate {
                    episode_wpm: word.decoded_wpm,
                    latest_word_seq: track.next_word_seq,
                    neighbor_gap_hz,
                    track_id,
                    word_seq: word.seq,
                    call: &word.text,
                    span: (word.start_ts, word.end_ts),
                    reps: word.last_reps,
                    wpm: track.wpm,
                    rival: word.rival.as_deref(),
                    alternatives,
                    wpm_stable: track.wpm_confirmed
                        && track.next_word_seq - track.wpm_since_word_seq >= STABLE_SPEED_WORDS,
                });
            }
        }
        out
    }

    /// MAN-525: every track's finite frequency with its id, sorted by
    /// `(frequency, track_id)`: the one pass `refine_candidates` reads
    /// neighbour gaps from. Non-finite frequencies are left out; against a
    /// finite own frequency they only ever contribute NaN or infinity,
    /// which the gap's `f64::min` from `INFINITY` ignores.
    fn sorted_track_freqs(&self) -> Vec<(f64, u32)> {
        let mut sorted: Vec<(f64, u32)> = self
            .tracks
            .iter()
            .filter(|(_, t)| t.freq_hz.is_finite())
            .map(|(&id, t)| (t.freq_hz, id))
            .collect();
        sorted.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        sorted
    }

    /// MAN-208's neighbour gap: the distance to the nearest OTHER track's
    /// frequency, `INFINITY` with no other track. Reads `sorted`
    /// ([`Self::sorted_track_freqs`]) by binary search (MAN-525); a
    /// non-finite own frequency falls back to the original fold over every
    /// track, so the value is bit-identical either way.
    fn neighbor_gap_hz(&self, sorted: &[(f64, u32)], track_id: u32, freq_hz: f64) -> f64 {
        nearest_gap_hz(sorted, track_id, freq_hz).unwrap_or_else(|| {
            self.tracks
                .iter()
                .filter(|(id, _)| **id != track_id)
                .map(|(_, other)| (other.freq_hz - freq_hz).abs())
                .fold(f64::INFINITY, f64::min)
        })
    }

    /// MAN-208: applies one externally computed refinement of a retained
    /// occurrence -- the result of re-decoding the occurrence's preserved
    /// samples under an alternate configuration. The refinement is recorded
    /// as a `Refined` observation about the SAME occurrence (never a new
    /// repetition); if the call changed, the occurrence's repetition
    /// support MOVES to the refined call (`RepetitionGate::move_occurrence`)
    /// rather than being re-recorded; the refined candidate is then
    /// re-evaluated through the normal candidate checks at the newest
    /// supporting occurrence's RF time -- which is what makes a rescued
    /// spot's timestamp the later repetition's, never the refined span's
    /// older one, and what keeps a refinement alone (no later repetition)
    /// from emitting anything.
    ///
    /// `decision_sample_ts` is the stream position at the re-decode: the
    /// earliest time the refined identity could have been known. It stamps
    /// the observation record, never the spot.
    ///
    /// MAN-515: a reading that would turn a spottable callsign (allowlisted,
    /// or grammar-plausible with an allocated prefix) into a string that can
    /// never be spotted is refused before anything moves: the word keeps its
    /// text and support, and a `Refined` withheld decision records why
    /// (`Grammar` or `Cty`). [`Validator::apply_refinement_with_outcome`]
    /// reports which case applied.
    pub fn apply_refinement(
        &mut self,
        track_id: u32,
        word_seq: u64,
        refined: RefinedWord,
        decision_sample_ts: u64,
    ) -> Vec<Spot> {
        self.apply_refinement_with_outcome(track_id, word_seq, refined, decision_sample_ts)
            .0
    }

    /// MAN-515: whether `call` could ever be spotted by the permanent
    /// checks: allowlisted, or grammar-plausible with an allocated prefix.
    fn spottable(&self, call: &str) -> bool {
        self.allowlist.contains(call)
            || (grammar::is_plausible(call) && self.cty.is_allocated(call))
    }

    /// MAN-515: [`Validator::apply_refinement`], also reporting what it did
    /// with the reading ([`RefinementOutcome`]) so the scheduler can count
    /// the replays that changed a word and the ones the guard refused.
    pub fn apply_refinement_with_outcome(
        &mut self,
        track_id: u32,
        word_seq: u64,
        refined: RefinedWord,
        decision_sample_ts: u64,
    ) -> (Vec<Spot>, RefinementOutcome) {
        let current = self.tracks.get(&track_id).and_then(|t| {
            t.words.iter().find(|w| w.seq == word_seq).map(|w| {
                (
                    w.text.clone(),
                    w.last_spot_type.unwrap_or(SpotType::Unknown),
                )
            })
        });
        let outcome = match &current {
            None => RefinementOutcome::Orphaned,
            Some((old_call, _)) if *old_call == refined.call => RefinementOutcome::Unchanged,
            Some((old_call, _)) if self.spottable(old_call) && !self.spottable(&refined.call) => {
                RefinementOutcome::Rejected
            }
            Some(_) => RefinementOutcome::Changed,
        };
        if let (RefinementOutcome::Rejected, Some((_, spot_type))) = (outcome, current) {
            // The guard returns before `move_occurrence`, the ledger
            // revision and any write to the word: the causal reading keeps
            // everything it had.
            self.advance_clock(decision_sample_ts);
            let reason = if grammar::is_plausible(&refined.call) {
                Reason::Cty
            } else {
                Reason::Grammar
            };
            let freq_hz = self.tracks[&track_id].freq_hz * self.freq_calibration;
            let at = At::new(track_id, word_seq, Cause::Refined);
            self.decide(at, Some(reason), || {
                Decision::withheld(
                    track_id,
                    decision_sample_ts,
                    &refined.call,
                    freq_hz,
                    spot_type,
                    reason,
                )
            });
            return (Vec::new(), outcome);
        }
        (
            self.apply_refinement_inner(track_id, word_seq, refined, decision_sample_ts),
            outcome,
        )
    }

    /// The MAN-208 refinement path behind [`Validator::apply_refinement`],
    /// after the MAN-515 guard has let the reading through.
    fn apply_refinement_inner(
        &mut self,
        track_id: u32,
        word_seq: u64,
        refined: RefinedWord,
        decision_sample_ts: u64,
    ) -> Vec<Spot> {
        self.advance_clock(decision_sample_ts);
        let id = ObservationId { track_id, word_seq };
        // An occurrence whose word has left the bounded window (or whose
        // track is gone) can be revised in the record but never
        // re-evaluated or re-counted: an orphaned revision, not a spot.
        let Some((old_call, span, end_ts, spot_type)) = self.tracks.get(&track_id).and_then(|t| {
            t.words.iter().find(|w| w.seq == word_seq).map(|w| {
                (
                    w.text.clone(),
                    (w.start_ts.min(w.end_ts), w.end_ts),
                    w.end_ts,
                    w.last_spot_type.unwrap_or(SpotType::Unknown),
                )
            })
        }) else {
            let finding = Finding::Decided(Decision::withheld(
                track_id,
                decision_sample_ts,
                &refined.call,
                0.0,
                SpotType::Unknown,
                Reason::Repetition,
            ));
            let draft =
                self.observation_draft(At::new(track_id, word_seq, Cause::Refined), finding);
            self.observations.revise(id, draft, decision_sample_ts).ok();
            return Vec::new();
        };
        if refined.call != old_call {
            // A non-allowlisted Beacon word's occurrence was never
            // recorded in the gate (`capture_pending_beacon` defers to
            // close-time), so there is no repetition support to move --
            // see the Beacon branch below.
            let deferred_beacon =
                spot_type == SpotType::Beacon && !self.allowlist.contains(&old_call);
            if !deferred_beacon {
                let is_active =
                    |tid| self.tracks.contains_key(&tid) || self.merged_into.contains_key(&tid);
                self.gate
                    .move_occurrence(&old_call, &refined.call, track_id, span, is_active);
            }
            // The variant-arbitration ledger holds the same occurrence:
            // revise it there too, or the rival check below compares the
            // refined call against the reading it just superseded
            // (validate round 2, PR #520). Moved, never added.
            let plausible =
                grammar::is_plausible(&refined.call) && self.cty.is_allocated(&refined.call);
            self.ledger.revise_occurrence(
                track_id,
                word_seq,
                &old_call,
                span,
                plausible.then(|| {
                    (
                        refined.call.as_str(),
                        confidence::geo_mean(&refined.confidences),
                    )
                }),
            );
        }
        // The word now carries the refined reading: its text, confidences
        // and ambiguity signals are the re-decode's, so the window's later
        // context rescans see the resolved call.
        {
            let track = self.tracks.get_mut(&track_id).expect("track present above");
            if let Some(word) = track.words.iter_mut().find(|w| w.seq == word_seq) {
                word.refine_hypothesis = Some(refined.hypothesis);
                word.text = refined.call.clone();
                word.confidences = refined.confidences.clone();
                word.char_alternatives = Vec::new();
                word.refine_hint = false;
            }
        }
        let call = refined.call.clone();
        let candidate = call.clone();
        let (freq_hz, snr_db, wpm) = {
            let track = &self.tracks[&track_id];
            (
                track.freq_hz * self.freq_calibration,
                track.snr_db,
                track.wpm,
            )
        };
        let snapshot = self
            .gate
            .support_snapshot(freq_hz, &candidate, self.now_ts)
            .unwrap_or_default();
        let reps = snapshot.counted.len() as u32;
        // The emission timestamp is the newest supporting occurrence's RF
        // time -- the later repetition's, never the refined span's older
        // one (dedupe rejects older timestamps, and backdating would be
        // wrong even where it passed).
        let sample_ts = snapshot
            .accepted
            .iter()
            .map(|o| o.sample_ts)
            .max()
            .unwrap_or(end_ts);
        let withheld = |reason: Reason| {
            Decision::withheld(
                track_id,
                decision_sample_ts,
                &candidate,
                freq_hz,
                spot_type,
                reason,
            )
            .with_reps(reps)
        };
        let mut at = At::new(track_id, word_seq, Cause::Refined);
        at.evidence = evidence_links(&snapshot.accepted);
        if self.blocklist.contains(&candidate) {
            self.suppression_counts.blocklist += 1;
            self.decide(at, Some(Reason::Blocklist), || withheld(Reason::Blocklist));
            return Vec::new();
        }
        if self.notch.contains(freq_hz) {
            self.suppression_counts.notch += 1;
            self.decide(at, Some(Reason::Notch), || withheld(Reason::Notch));
            return Vec::new();
        }
        let is_allowlisted = self.allowlist.contains(&candidate);
        if !is_allowlisted {
            if !grammar::is_plausible(&candidate) {
                self.decide(at, Some(Reason::Grammar), || withheld(Reason::Grammar));
                return Vec::new();
            }
            if !self.cty.is_allocated(&candidate) {
                self.decide(at, Some(Reason::Cty), || withheld(Reason::Cty));
                return Vec::new();
            }
        }
        // A non-allowlisted Beacon candidate is never evaluated on a
        // live track (`evaluate_candidate`'s Beacon branch): it is
        // captured once and judged exactly once at its track's true
        // close, when a final WPM reading exists. A refinement re-reads
        // the word; it does not change that contract (Codex review, PR
        // #520). The permanent checks above have re-judged the refined
        // call, so the captured reading is updated and the normal
        // close-time validation (`resolve_pending_beacons`) still
        // decides -- including its final-speed check.
        if spot_type == SpotType::Beacon && !is_allowlisted {
            let mut updated = false;
            for pb in self
                .tracks
                .values_mut()
                .flat_map(|t| t.pending_beacons.iter_mut())
                .filter(|pb| pb.origin_track_id == track_id && pb.word_seq == word_seq)
            {
                pb.candidate = candidate.clone();
                pb.char_confidences = refined.confidences.clone();
                updated = true;
            }
            let reason = if updated {
                Reason::BeaconPending
            } else {
                // Nothing is pending anymore (the capture was
                // permanently rejected, or the pending beacon was lost
                // to eviction): the refined reading has nothing to
                // update and nowhere to be published.
                Reason::BeaconDiscarded
            };
            self.decide(at, Some(reason), || withheld(reason));
            return Vec::new();
        }
        if !confidence::has_acoustic_evidence(&refined.confidences) {
            self.decide(at, Some(Reason::NoEvidence), || {
                withheld(Reason::NoEvidence)
            });
            return Vec::new();
        }
        let tier = self.patterns.tier(&candidate);
        let mut confidence = confidence::c_call(&refined.confidences, reps);
        if let Some(scp) = &self.scp {
            confidence = confidence::apply_scp_boost(confidence, scp.contains(&candidate));
        }
        let at = at.with_call_confidence(confidence);
        let emission_state = if reps >= self.level.required_reps(tier) {
            EmissionState::Confirmed
        } else {
            EmissionState::Tentative
        };
        if !is_allowlisted && spot_type != SpotType::Beacon && reps < self.level.required_reps(tier)
        {
            self.decide(at, Some(Reason::Repetition), || {
                withheld(Reason::Repetition)
            });
            // MAN-208: the refined reading is still below its repetition
            // requirement, so the episode stays a refinement candidate.
            self.note_refine_hint(track_id, word_seq, None);
            return Vec::new();
        }
        let scp_exempt = self.scp.as_ref().is_some_and(|s| s.contains(&candidate));
        let rival = if !is_allowlisted && !scp_exempt && spot_type != SpotType::Beacon {
            self.ledger
                .better_supported_rival(track_id, &candidate, sample_ts)
        } else {
            None
        };
        if let Some((rival_call, rival_support)) = rival {
            let rival_for_hint = rival_call.clone();
            self.suppression_counts.variant += 1;
            self.decide(at, Some(Reason::Variant), || {
                let mut d = withheld(Reason::Variant);
                d.rival = Some(Rival {
                    call: rival_call,
                    support: rival_support,
                });
                d
            });
            self.note_refine_hint(track_id, word_seq, Some(rival_for_hint));
            return Vec::new();
        }
        let input = self.input_of(track_id);
        if self
            .dedupe
            .last_sample_ts(&candidate, freq_hz, input)
            .is_some_and(|last| sample_ts < last)
        {
            self.decide(at, Some(Reason::Dedupe), || withheld(Reason::Dedupe));
            return Vec::new();
        }
        let outcome = self
            .dedupe
            .assess(&candidate, freq_hz, snr_db, spot_type, sample_ts, input);
        if outcome != DedupeOutcome::Emit {
            self.count_cross_input_dedupe(outcome, input);
            self.decide(at, Some(Reason::Dedupe), || withheld(Reason::Dedupe));
            return Vec::new();
        }
        let (rst, qrl_query) = self.track_annotations(track_id);
        let spot = Spot {
            callsign: candidate,
            freq_hz,
            snr_db,
            wpm,
            spot_type,
            confidence,
            emission_state,
            track_id,
            sample_ts,
            rst,
            qrl_query,
            mode: SpotMode::Cw,
            baud: None,
        };
        match self.admit(spot, reps, is_allowlisted, Confirmation::ExactRepeat, at) {
            Some(spot) => vec![spot],
            None => Vec::new(),
        }
    }

    /// MAN-207: the observation record, for `ObservationLog::revise`,
    /// `take_appended` and `note_discontinuity`.
    pub fn observations_mut(&mut self) -> &mut ObservationLog {
        &mut self.observations
    }

    /// MAN-207: decisions so far by verdict and reason, at every detail
    /// level.
    pub fn observation_counts(&self) -> ObservationCounts {
        *self.observations.counts()
    }

    /// The one boundary every decision passes (MAN-324, MAN-207). Always
    /// counts it under `reason` (`None` is a spot). Runs `build` only when
    /// the decision log is on or the observation record keeps records.
    fn decide(&mut self, at: At, reason: Option<Reason>, build: impl FnOnce() -> Decision) {
        self.observations.count_decision(reason);
        let keep = self.observations.keeps_records();
        if self.decisions.is_none() && !keep {
            return;
        }
        let mut decision = build();
        debug_assert_eq!(decision.reason, reason, "decide's counted reason drifted");
        // One boundary covers early refusals, reclassification and every
        // deferred-beacon path, without doing pattern lookups when logging is off.
        if decision.spot_type != SpotType::Beacon && !self.allowlist.contains(&decision.call) {
            let tier = self.patterns.tier(&decision.call);
            decision.required_reps = self.level.required_reps(tier);
            decision.pattern_tier = Some(tier);
        }
        let for_log = if keep {
            let for_log = self.decisions.is_some().then(|| decision.clone());
            let draft = self.observation_draft(at, Finding::Decided(decision));
            self.observations.append(draft, self.now_ts);
            for_log
        } else {
            Some(decision)
        };
        if let (Some(log), Some(decision)) = (self.decisions.as_mut(), for_log) {
            log.push(decision);
        }
    }

    /// MAN-207: a record of `decision` about `at`, with the occurrence's
    /// spans, confidence and (at `Detail::Text`) trace while its word is
    /// still in the track's window.
    fn observation_draft(&self, at: At, finding: Finding) -> Draft {
        let source = Source {
            input: self.input_of(at.id.track_id),
            epoch: self.observations.epoch(),
        };
        let mut draft = Draft::of(at.id, at.cause, source, finding);
        draft.scores = at.scores;
        draft.evidence = at.evidence;
        let track = self.tracks.get(&at.id.track_id);
        let word = track.and_then(|t| t.words.iter().find(|w| w.seq == at.id.word_seq));
        if let (Some(track), Some(word)) = (track, word) {
            draft.span = Some(SampleSpan {
                start_ts: word.start_ts,
                end_ts: word.end_ts,
            });
            draft.episode = Some(word.episode);
            draft.role = word.role.map(|(role, frame)| RoleEvidence {
                role,
                frame,
                basis: word.basis,
            });
            // MAN-207 D3: a role-skipped word that became a candidate later
            // is reclassified, not decided for the first time.
            if draft.cause == Cause::Decided
                && word.not_candidate_recorded
                && matches!(draft.finding, Finding::Decided(_))
            {
                draft.cause = Cause::Reclassified;
            }
            if let (Some(lo), Some(hi)) = (word.freq_lo_hz, word.freq_hi_hz) {
                draft.freq = Some(FreqSpan {
                    lo_hz: lo * self.freq_calibration,
                    hi_hz: hi * self.freq_calibration,
                });
            }
            if !word.confidences.is_empty() {
                draft.scores.word_confidence = Some(confidence::geo_mean(&word.confidences));
            }
            if self.observations.detail() == crate::observation::Detail::Text {
                let mut context: Vec<&str> = word.context_seqs.map_or_else(Vec::new, |(lo, hi)| {
                    track
                        .words
                        .iter()
                        .filter(|w| (lo..=hi).contains(&w.seq))
                        .map(|w| w.text.as_str())
                        .collect()
                });
                if draft.cause == Cause::Refined {
                    if let Some(hypothesis) = word.refine_hypothesis {
                        context.push(hypothesis);
                    }
                }
                draft.trace = Some(TextTrace::new(
                    &word.text,
                    &word.confidences,
                    &word.alternatives,
                    &context,
                ));
            }
        }
        draft
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
            if let Some(lattice) = self.lattice.as_mut() {
                lattice.sweep(self.now_ts);
            }
            self.observations.sweep(self.now_ts);
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
                alternatives,
            } => {
                self.advance_clock(*sample_ts);
                let keep_text = self.observations.detail() == crate::observation::Detail::Text;
                let turn_silence_samples = self.turn_silence_samples;
                let track = self.tracks.entry(*track_id).or_default();
                let buffer_empty = track.current.text.is_empty();
                match glyph {
                    Glyph::Char(c) => {
                        if buffer_empty {
                            // MAN-207: the word's sample span and frequency
                            // span start at its first character.
                            track.current.start_ts = *sample_ts;
                            if track.has_meta {
                                track.current.note_freq(track.freq_hz);
                            }
                            // MAN-201 D3: a first character arriving after
                            // `TURN_SILENCE_SECONDS` of silence ends the
                            // previous word's turn.
                            if let Some(back) = track.words.back_mut() {
                                if sample_ts.saturating_sub(back.end_ts) >= turn_silence_samples {
                                    back.ends_turn = true;
                                }
                            }
                        }
                        track.current.text.push(c.to_ascii_uppercase());
                        track.current.confidences.push(*confidence);
                        track.current.char_alternatives.push(
                            alternatives
                                .iter()
                                .filter_map(|(g, a)| match g {
                                    Glyph::Char(c) => Some((c.to_ascii_uppercase(), *a)),
                                    Glyph::Prosign(_) => None,
                                })
                                .collect(),
                        );
                        if keep_text {
                            track.current.alternatives.push(alternatives.clone());
                        }
                    }
                    Glyph::Prosign(Prosign::Err) => {
                        // SPEC §4.4: operator-error prosign discards the
                        // current word buffer back to the previous
                        // boundary. With nothing buffered it retracts the
                        // word just completed (MAN-201).
                        if buffer_empty {
                            if let Some(back) = track.words.back_mut() {
                                back.corrected = true;
                            }
                        }
                        track.current = Word::default();
                    }
                    // MAN-201 D3: `AR`/`SK` end the turn.
                    Glyph::Prosign(Prosign::Ar | Prosign::Sk) => {
                        if !buffer_empty {
                            track.current.ends_turn = true;
                        } else if let Some(back) = track.words.back_mut() {
                            back.ends_turn = true;
                        }
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
                let mut newly_completed: Option<(String, u64, u64, f32)> = None;
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
                    word.end_ts = *sample_ts;
                    word.decoded_wpm = track.wpm;
                    // MAN-207 D6: the previous word's `ends_turn` is final
                    // once a later word is pushed.
                    word.episode = track
                        .words
                        .back()
                        .map_or(0, |prev| prev.episode + u32::from(prev.ends_turn));
                    track.next_word_seq += 1;
                    newly_completed = Some((
                        word.text.clone(),
                        word.seq,
                        word.start_ts.min(*sample_ts),
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
                if let Some((text, seq, start_ts, geo)) = newly_completed {
                    if grammar::is_plausible(&text) && self.cty.is_allocated(&text) {
                        self.ledger.observe_span(
                            *track_id,
                            &text,
                            seq,
                            (start_ts, *sample_ts),
                            geo,
                        );
                    }
                }
                self.try_spot(*track_id, *sample_ts, false)
            }
            DecoderEvent::SpeedUpdate { track_id, wpm } => {
                let track = self.tracks.entry(*track_id).or_default();
                track.wpm = *wpm;
                track.wpm_confirmed = true;
                track.wpm_since_word_seq = track.next_word_seq;
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
                sample_ts: meta_ts,
                last_char_ts,
            } => {
                let turn_silence_samples = self.turn_silence_samples;
                let track = self.tracks.entry(*track_id).or_default();
                let had_meta = track.has_meta;
                track.snr_db = *snr_2500_db;
                track.freq_hz = *freq_hz;
                track.has_meta = true;
                if !track.current.text.is_empty() {
                    track.current.note_freq(*freq_hz);
                }
                let sample_ts = track.last_sample_ts;
                // MAN-201 D3: `TURN_SILENCE_SECONDS` with no decoded
                // character after the newest word ends its turn, which
                // settles that word's role; decide it now. Measured from
                // the decoder's own newest character, not the newest word
                // released here: `meta_ts` runs ahead of a word the MAN-317
                // prior still holds, which is no silence (V8 with
                // `prior = "on"`).
                let silent_tail = track.current.text.is_empty()
                    && track.words.back_mut().is_some_and(|back| {
                        let quiet_from = back.end_ts.max(*last_char_ts);
                        let silent = !back.ends_turn
                            && meta_ts.saturating_sub(quiet_from) >= turn_silence_samples;
                        back.ends_turn |= silent;
                        silent
                    });
                if silent_tail {
                    return self.try_spot(*track_id, sample_ts, false);
                }
                // Retry: a candidate held back by the has_meta gate is
                // otherwise only ever re-evaluated by a later
                // WordBoundary, which a short transmission may never
                // produce again -- silently losing the pending exemption
                // (MAN-28 round 9 review). No-op once already true.
                if had_meta {
                    Vec::new()
                } else {
                    self.try_spot(*track_id, sample_ts, false)
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
                // MAN-201 D5: the track's end settles its last call, so
                // decide every still-pending occurrence first -- a
                // deferred beacon it captures is then judged (or migrated)
                // with the rest below.
                let close_ts = self.tracks.get(track_id).map_or(0, |t| t.last_sample_ts);
                let mut spots = self.try_spot(*track_id, close_ts, true);
                let closure_spots = match closure {
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
                spots.extend(closure_spots);
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
    fn candidates(&mut self, track_id: u32, complete: bool) -> Vec<Candidate> {
        let Some(track) = self.tracks.get_mut(&track_id) else {
            return Vec::new();
        };
        let mut candidates = Vec::new();

        // MAN-201: every callsign occurrence's role, from the exchange
        // grammar. Only a settled occurrence is decided (D5); its role is
        // latched on the word at its first settle (D6a).
        let occurrences = {
            let words: Vec<ExchangeWord<'_>> = track
                .words
                .iter()
                .map(|w| ExchangeWord {
                    text: &w.text,
                    ends_turn: w.ends_turn,
                    corrected: w.corrected,
                })
                .collect();
            exchange::interpret(&words, complete)
        };
        let seqs: Vec<u64> = track.words.iter().map(|w| w.seq).collect();
        // (role, frame, involved_max_seq) per word index, settled only.
        let mut roles: Vec<Option<(Role, Option<Frame>, u64)>> = vec![None; seqs.len()];
        for o in occurrences.iter().filter(|o| o.settled) {
            let word = &mut track.words[o.index];
            if word.role.is_none() {
                // MAN-207: the basis and the deciding words latch with the role.
                word.basis = Some(o.basis);
                word.context_seqs =
                    match (seqs.get(o.span.start), seqs.get(o.span.end.wrapping_sub(1))) {
                        (Some(&lo), Some(&hi)) if !o.span.is_empty() => Some((lo, hi)),
                        _ => None,
                    };
            }
            let (role, frame) = *word.role.get_or_insert((o.role, o.frame));
            let involved = o
                .span
                .clone()
                .filter_map(|i| seqs.get(i).copied())
                .max()
                .unwrap_or(seqs[o.index]);
            roles[o.index] = Some((role, frame, involved));
        }

        // Byte range of each word within `joined`, in the same order as
        // `track.words`, to map a power-step match's span back to the
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
        let mut power_step_seqs = Vec::new();
        if !context::power_step_framing_is_unresolved(&joined) {
            for (candidate, range, r) in context::power_step_candidates(&joined) {
                let involved_max_seq = word_spans
                    .iter()
                    .filter(|(start, end, _)| *start < range.end && range.start < *end)
                    .map(|(_, _, seq)| *seq)
                    .max()
                    .unwrap_or(0);
                // Power-step origin MUST bind to the exact word or be
                // discarded entirely. A capture landing on only PART of a
                // decoded word (e.g. the regex's `\b` matching mid-word
                // after a punctuation character glued onto a callsign,
                // "-K5ARH") produces a range that doesn't equal any word's
                // own span; resolving it by text could then bind this
                // match to a wholly unrelated, unsuppressed word that
                // merely happens to share the callsign's text (Codex
                // review on PR #65, round 10).
                let Some(index) = word_spans
                    .iter()
                    .position(|(start, end, _)| *start == r.start && *end == r.end)
                else {
                    continue;
                };
                // MAN-201 D8: a trailing `T` alone is not beacon evidence;
                // `record_beacon_no_evidence` explains the unlisted call.
                if !self.allowlist.contains(&candidate) {
                    continue;
                }
                // MAN-201: a call the exchange shows was only called or
                // mentioned is never that station's beacon ID.
                if matches!(
                    roles[index],
                    Some((Role::Addressee | Role::RelayedMention, _, _))
                ) {
                    continue;
                }
                let seq = seqs[index];
                power_step_seqs.push(seq);
                candidates.push((
                    candidate,
                    SpotType::Beacon,
                    involved_max_seq,
                    Some(seq),
                    track.words[index].end_ts,
                ));
            }
        }

        // MAN-201: a settled `self_id` occurrence with a spot form is a
        // candidate of that type, bound to its exact word (D6). D7: the
        // allowlist skips plausibility and repetition, not role -- an
        // allowlisted `uncertain` occurrence, or a `self_id` with no spot
        // form (`TU <call>`), is offered as `Unknown` (the MAN-28 Watch
        // List case: no framing words at all), unless a power-step
        // candidate already names that word. An `addressee` or
        // `relayed_mention` occurrence is never a candidate.
        for (index, entry) in roles.iter().enumerate() {
            let Some((role, frame, involved)) = *entry else {
                continue;
            };
            let word = &track.words[index];
            let spot_type = match (role, frame.and_then(Frame::spot_type)) {
                (Role::SelfId, Some(ty)) => ty,
                (Role::SelfId | Role::Uncertain, _)
                    if self.allowlist.contains(&word.text)
                        && !power_step_seqs.contains(&word.seq) =>
                {
                    SpotType::Unknown
                }
                _ => continue,
            };
            candidates.push((
                word.text.clone(),
                spot_type,
                involved,
                Some(word.seq),
                word.end_ts,
            ));
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
                // MAN-201 D8: only an allowlisted call has a beacon
                // candidacy for the guard to burn.
                self.allowlist.contains(&call).then_some(())?;
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
        let (first_suppression, freq_hz, cause) = {
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
            // MAN-207: a word already decided as another spot type is
            // reclassified by the burn, not decided for the first time.
            let cause = if word.attempted {
                Cause::Reclassified
            } else {
                Cause::Decided
            };
            word.power_step_suppressed = true;
            word.attempted = true;
            word.classified_max_seq = word.classified_max_seq.max(involved_max_seq);
            (
                first_suppression,
                track.freq_hz * self.freq_calibration,
                cause,
            )
        };
        if first_suppression {
            self.suppression_counts.power_step_guard += 1;
            let at = At::new(track_id, exact_seq, cause);
            self.decide(at, Some(Reason::PowerStepGuard), || {
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

    /// MAN-201 D8: records one `BeaconNoEvidence` decision per unguarded
    /// `<call> T` occurrence of a plausible, allocated, unlisted call. A
    /// call the exchange shows was only called or mentioned has no beacon
    /// question to explain.
    fn record_beacon_no_evidence(&mut self, track_id: u32) {
        let Some(track) = self.tracks.get(&track_id) else {
            return;
        };
        let mut joined = String::new();
        let mut word_spans = Vec::with_capacity(track.words.len());
        for word in &track.words {
            if !joined.is_empty() {
                joined.push(' ');
            }
            let start = joined.len();
            joined.push_str(&word.text);
            word_spans.push((start, joined.len()));
        }
        let guarded = context::power_step_framing_is_unresolved(&joined);
        let mut found = Vec::new();
        for (call, _, r) in context::power_step_candidates(&joined) {
            let Some(index) = word_spans
                .iter()
                .position(|&(start, end)| start == r.start && end == r.end)
            else {
                continue;
            };
            let word = &track.words[index];
            if guarded && !self.allowlist.contains(&call) {
                found.push((call, word.seq, word.end_ts));
                continue;
            }
            if word.beacon_no_evidence_recorded
                || self.allowlist.contains(&call)
                || matches!(word.role, Some((Role::Addressee | Role::RelayedMention, _)))
                || !grammar::is_plausible(&call)
                || !self.cty.is_allocated(&call)
            {
                continue;
            }
            found.push((call, word.seq, word.end_ts));
        }
        for (call, seq, end_ts) in found {
            let Some(track) = self.tracks.get_mut(&track_id) else {
                return;
            };
            let freq_hz = track.freq_hz * self.freq_calibration;
            // MAN-207: a word already decided as another spot type is
            // reclassified by this decision, not decided for the first time.
            let mut cause = Cause::Decided;
            if let Some(word) = track.words.iter_mut().find(|w| w.seq == seq) {
                word.beacon_no_evidence_recorded = true;
                if word.attempted {
                    cause = Cause::Reclassified;
                }
            }
            // A guarded occurrence stays explained by its CQ/DE framing,
            // also once that framing ages out of the window.
            if guarded {
                continue;
            }
            self.suppression_counts.beacon_no_evidence += 1;
            let at = At::new(track_id, seq, cause);
            self.decide(at, Some(Reason::BeaconNoEvidence), || {
                Decision::withheld(
                    track_id,
                    end_ts,
                    &call,
                    freq_hz,
                    SpotType::Beacon,
                    Reason::BeaconNoEvidence,
                )
            });
        }
    }

    /// Decides every settled candidate on `track_id`, each at its own
    /// word's `end_ts` (MAN-201 D5); `complete` settles the window's last
    /// call (the track is closing). `sample_ts` is the triggering event's
    /// time, used only for guard burns.
    fn try_spot(&mut self, track_id: u32, sample_ts: u64, complete: bool) -> Vec<Spot> {
        // No real TrackMeta yet -- freq_hz/snr_db still hold bogus 0.0
        // defaults. Bail without marking anything attempted, so pending
        // candidates are simply re-evaluated once metadata does arrive.
        // Not a decision, so nothing is recorded (MAN-324).
        if !self.tracks.get(&track_id).is_some_and(|t| t.has_meta) {
            return Vec::new();
        }
        let candidates = self.candidates(track_id, complete);
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
            .filter_map(
                |(candidate, spot_type, involved_max_seq, exact_seq, end_ts)| {
                    self.evaluate_candidate(
                        track_id,
                        end_ts,
                        candidate,
                        spot_type,
                        involved_max_seq,
                        exact_seq,
                    )
                },
            )
            .collect();
        self.record_beacon_no_evidence(track_id);
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
        self.record_not_candidates(track_id);
        spots
    }

    /// MAN-207 D10: counts, and at `Detail::Records` and above records,
    /// each settled call occurrence on `track_id` that its role kept from
    /// becoming a candidate. Each occurrence once. Only a word with a
    /// plausible callsign structure counts: the exchange grammar also gives
    /// roles to ordinary call-shaped words (`TNX`, `NAME`).
    fn record_not_candidates(&mut self, track_id: u32) {
        let Some(track) = self.tracks.get_mut(&track_id) else {
            return;
        };
        let freq_hz = track.freq_hz * self.freq_calibration;
        let mut skipped = Vec::new();
        for word in track.words.iter_mut() {
            let Some((role, _)) = word.role else {
                continue;
            };
            if word.attempted
                || word.power_step_suppressed
                || word.beacon_no_evidence_recorded
                || word.not_candidate_recorded
                || !grammar::is_plausible(&word.text)
            {
                continue;
            }
            word.not_candidate_recorded = true;
            skipped.push((role, word.seq, word.text.clone(), word.end_ts));
        }
        for (role, seq, call, end_ts) in skipped {
            self.observations.count_not_candidate(role);
            if self.observations.keeps_records() {
                let finding = Finding::NotCandidate {
                    call,
                    sample_ts: end_ts,
                    freq_hz,
                };
                let draft = self.observation_draft(At::new(track_id, seq, Cause::Decided), finding);
                self.observations.append(draft, self.now_ts);
            }
        }
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

        let fused_ctx = {
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
            let fusion_copy = (self.lattice.is_some() && !reclassifying).then(|| lattice::Copy {
                text: word.text.clone(),
                chars: word
                    .text
                    .chars()
                    .zip(&word.confidences)
                    .zip(
                        word.char_alternatives
                            .iter()
                            .chain(std::iter::repeat(&Vec::new())),
                    )
                    .map(|((glyph, &confidence), alts)| lattice::CharEvidence {
                        glyph,
                        confidence,
                        alternatives: alts.clone(),
                    })
                    .collect(),
                span: (word.start_ts.min(sample_ts), sample_ts),
                track_id,
                word_seq: word.seq,
                freq_hz,
            });
            (
                word.confidences.clone(),
                reclassifying,
                word.seq,
                word.start_ts.min(sample_ts),
                fusion_copy,
            )
        };
        let (char_confidences, reclassifying, resolved_word_seq, start_ts, fusion_copy) = fused_ctx;
        // MAN-200 D10: a first evaluation of a non-allowlisted CQ/DE self-ID
        // enters the lattice before the operator and plausibility checks,
        // because a damaged copy is often implausible.
        let episode = match (fusion_copy, self.lattice.is_some()) {
            (Some(copy), true)
                if matches!(spot_type, SpotType::Cq | SpotType::De)
                    && !self.allowlist.contains(&candidate)
                    && copy.chars.len() >= lattice::MIN_COPY_CHARS =>
            {
                let input = self.input_of(track_id);
                let tracks = &self.tracks;
                let merged = &self.merged_into;
                let outcome = self.lattice.as_mut().map(|l| {
                    l.add(input, copy, |tid| {
                        tracks.contains_key(&tid) || merged.contains_key(&tid)
                    })
                });
                match outcome {
                    Some(lattice::AddOutcome::Added(k) | lattice::AddOutcome::Duplicate(k)) => {
                        Some(k)
                    }
                    None => None,
                }
            }
            _ => None,
        };
        let fused_site = FusedSite {
            track_id,
            word_seq: resolved_word_seq,
            sample_ts,
            spot_type,
            freq_hz,
            snr_db,
            wpm,
            episode,
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
        let cause = if reclassifying {
            Cause::Reclassified
        } else {
            Cause::Decided
        };
        let mut at = At::new(track_id, resolved_word_seq, cause);
        if self.blocklist.contains(&candidate) {
            self.suppression_counts.blocklist += 1;
            let reason = Reason::Blocklist;
            self.decide(at, Some(reason), || withheld(reason));
            return None;
        }
        if self.notch.contains(freq_hz) {
            self.suppression_counts.notch += 1;
            let reason = Reason::Notch;
            self.decide(at, Some(reason), || withheld(reason));
            return None;
        }

        // MAN-28 Watch List: an allowlisted callsign bypasses grammar/cty
        // validation and the repetition gate below entirely.
        let is_allowlisted = self.allowlist.contains(&candidate);

        if !is_allowlisted {
            if !grammar::is_plausible(&candidate) {
                let reason = Reason::Grammar;
                self.decide(at, Some(reason), || withheld(reason));
                return self.try_fused_other(&fused_site, &candidate);
            }
            if !self.cty.is_allocated(&candidate) {
                let reason = Reason::Cty;
                self.decide(at, Some(reason), || withheld(reason));
                return self.try_fused_other(&fused_site, &candidate);
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
                .find(|w| w.seq == resolved_word_seq)
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
            let is_active =
                |tid| self.tracks.contains_key(&tid) || self.merged_into.contains_key(&tid);
            if self.observations.keeps_records() {
                // MAN-207: the same record, plus the occurrences it counted.
                let (reps, counted) = self.gate.record_span_traced(
                    track_id,
                    freq_hz,
                    &candidate,
                    (start_ts, sample_ts),
                    resolved_word_seq,
                    is_active,
                );
                at.evidence = evidence_links(&counted);
                reps as u32
            } else {
                self.gate.record_span(
                    track_id,
                    freq_hz,
                    &candidate,
                    (start_ts, sample_ts),
                    resolved_word_seq,
                    is_active,
                ) as u32
            }
        };
        {
            let track = self.tracks.get_mut(&track_id)?;
            if let Some(word) = track.words.iter_mut().find(|w| w.seq == resolved_word_seq) {
                word.last_reps = reps;
            }
        }
        // MAN-106 zero-evidence abstention gate: a candidate whose every
        // contract) has no acoustic evidence, so no public emission --
        // regardless of database membership. Investigation 11: "Do not
        // let a callsign database overwhelm contradictory sound" -- here,
        // absent sound. Deliberately an evidence gate, NOT a confidence
        // threshold: there is no numeric cutoff to argue with, and
        // calibration never gets a chance to rescue pure silence.
        // Applied before `c_call`'s non-empty assertion and before the
        // repetition gate; allowlist, SCP and Beacon exemptions do not
        // bypass it.
        if !confidence::has_acoustic_evidence(&char_confidences) {
            let reason = Reason::NoEvidence;
            self.decide(at, Some(reason), || withheld(reason).with_reps(reps));
            return None;
        }
        let tier = self.patterns.tier(&candidate);
        let mut confidence = confidence::c_call(&char_confidences, reps);
        if let Some(scp) = &self.scp {
            confidence = confidence::apply_scp_boost(confidence, scp.contains(&candidate));
        }
        let at = at.with_call_confidence(confidence);
        let emission_state = if reps >= self.level.required_reps(tier) {
            EmissionState::Confirmed
        } else {
            EmissionState::Tentative
        };
        // ARCHITECTURE §6.4 exempts BEACON-tagged messages from the
        // repetition requirement: NCDXF-style beacons ID once per cycle,
        // so a single decode must still spot (MAN-28). Only reachable
        // here for an ALLOWLISTED Beacon candidate -- non-allowlisted
        // ones never reach this function body at all (see the early
        // dispatch above).
        if !is_allowlisted && spot_type != SpotType::Beacon && reps < self.level.required_reps(tier)
        {
            // MAN-200 D10: a fused winner that is this candidate's own text
            // replaces its `Repetition` decision; any other winner is
            // decided after it.
            let fused = self.fused_winner(&fused_site);
            if fused.as_ref().is_none_or(|f| f.call != candidate) {
                let reason = Reason::Repetition;
                self.decide(at, Some(reason), || withheld(reason).with_reps(reps));
                // MAN-208: a candidate below its repetition requirement is
                // the refinement scheduler's ambiguity signal.
                self.note_refine_hint(track_id, resolved_word_seq, None);
            }
            return self.try_fused(&fused_site, fused);
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
            let rival_for_hint = rival_call.clone();
            self.suppression_counts.variant += 1;
            self.decide(at, Some(Reason::Variant), || {
                let mut d = withheld(Reason::Variant).with_reps(reps);
                d.rival = Some(Rival {
                    call: rival_call,
                    support: rival_support,
                });
                d
            });
            // MAN-208: a better-supported rival withholding this candidate
            // is the refinement scheduler's ambiguity signal.
            self.note_refine_hint(track_id, resolved_word_seq, Some(rival_for_hint));
            return None;
        }

        let input = self.input_of(track_id);
        // MAN-201 D5: a settled occurrence is decided at its own, possibly
        // older, timestamp. One older than this station's last spot here
        // must not rewind dedupe's watermark: withhold it, as MAN-174 does
        // for a deferred beacon.
        if self
            .dedupe
            .last_sample_ts(&candidate, freq_hz, input)
            .is_some_and(|last| sample_ts < last)
        {
            let reason = Reason::Dedupe;
            self.decide(at, Some(reason), || withheld(reason).with_reps(reps));
            return None;
        }
        let outcome = self
            .dedupe
            .assess(&candidate, freq_hz, snr_db, spot_type, sample_ts, input);
        if outcome != DedupeOutcome::Emit {
            self.count_cross_input_dedupe(outcome, input);
            let reason = Reason::Dedupe;
            self.decide(at, Some(reason), || withheld(reason).with_reps(reps));
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
            emission_state,
            track_id,
            sample_ts,
            rst,
            qrl_query,
            mode: SpotMode::Cw,
            baud: None,
        };
        self.admit(spot, reps, is_allowlisted, Confirmation::ExactRepeat, at)
    }

    /// MAN-200 D8/D9: the episode's confirmed winner, if every position is
    /// decided, the winner's pattern tier has its separate messages, and no
    /// disagreeing copy already spotted on this channel.
    fn fused_winner(&self, site: &FusedSite) -> Option<lattice::Fused> {
        let lattice = self.lattice.as_ref()?;
        let key = site.episode?;
        let lattice::Verdict::Confirmed(fused) = lattice.evaluate(key, site.sample_ts, 1) else {
            return None;
        };
        let required = self.level.required_reps(self.patterns.tier(&fused.call));
        if fused.reps < required {
            return None;
        }
        let input = self.input_of(site.track_id);
        if fused.disagreeing.iter().any(|text| {
            self.dedupe
                .last_sample_ts(text, site.freq_hz, input)
                .is_some()
        }) {
            return None;
        }
        Some(fused)
    }

    /// MAN-200 D10: the fused path after a `Grammar` or `Cty` withhold. A
    /// winner that is the candidate's own text would fail that same check
    /// and decide this word a second time, so only a different winner is
    /// tried.
    fn try_fused_other(&mut self, site: &FusedSite, candidate: &str) -> Option<Spot> {
        let fused = self.fused_winner(site).filter(|f| f.call != candidate)?;
        self.try_fused(site, Some(fused))
    }

    /// MAN-200 D8/D11: decides a fused winner through the same checks as
    /// any candidate, then admits it as `Confirmation::Fused`. `fused` is
    /// the already-evaluated winner, or `None` to evaluate it here. A
    /// refused fusion records nothing.
    fn try_fused(&mut self, site: &FusedSite, fused: Option<lattice::Fused>) -> Option<Spot> {
        let fused = match fused {
            Some(f) => f,
            None => self.fused_winner(site)?,
        };
        let call = fused.call.clone();
        let reps = fused.reps;
        let (track_id, sample_ts, freq_hz, spot_type) =
            (site.track_id, site.sample_ts, site.freq_hz, site.spot_type);
        let withheld = |reason: Reason| {
            Decision::withheld(track_id, sample_ts, &call, freq_hz, spot_type, reason)
                .with_reps(reps)
        };
        // MAN-207: a fused decision is recorded about the copy that
        // triggered it, with no gate evidence links: the exact gate never
        // counted the fused call.
        let at = At::new(track_id, site.word_seq, Cause::Decided);
        if self.blocklist.contains(&call) {
            self.suppression_counts.blocklist += 1;
            self.decide(at, Some(Reason::Blocklist), || withheld(Reason::Blocklist));
            return None;
        }
        if self.notch.contains(freq_hz) {
            self.suppression_counts.notch += 1;
            self.decide(at, Some(Reason::Notch), || withheld(Reason::Notch));
            return None;
        }
        let is_allowlisted = self.allowlist.contains(&call);
        if !is_allowlisted {
            if !grammar::is_plausible(&call) {
                self.decide(at, Some(Reason::Grammar), || withheld(Reason::Grammar));
                return None;
            }
            if !self.cty.is_allocated(&call) {
                self.decide(at, Some(Reason::Cty), || withheld(Reason::Cty));
                return None;
            }
        }
        if !confidence::has_acoustic_evidence(&fused.char_confidences) {
            self.decide(at, Some(Reason::NoEvidence), || {
                withheld(Reason::NoEvidence)
            });
            return None;
        }
        let mut confidence = confidence::c_call(&fused.char_confidences, reps);
        if let Some(scp) = &self.scp {
            confidence = confidence::apply_scp_boost(confidence, scp.contains(&call));
        }
        // Variant arbitration, ignoring rivals that are this episode's own
        // copies: they are the damage the fusion already outvoted.
        let scp_exempt = self.scp.as_ref().is_some_and(|s| s.contains(&call));
        let rival = if !is_allowlisted && !scp_exempt {
            self.ledger.better_supported_rival_except(
                track_id,
                &call,
                sample_ts,
                &fused.disagreeing,
            )
        } else {
            None
        };
        if let Some((rival_call, rival_support)) = rival {
            self.suppression_counts.variant += 1;
            self.decide(at, Some(Reason::Variant), || {
                let mut d = withheld(Reason::Variant);
                d.rival = Some(Rival {
                    call: rival_call,
                    support: rival_support,
                });
                d
            });
            return None;
        }
        let input = self.input_of(track_id);
        if self
            .dedupe
            .last_sample_ts(&call, freq_hz, input)
            .is_some_and(|last| sample_ts < last)
        {
            self.decide(at, Some(Reason::Dedupe), || withheld(Reason::Dedupe));
            return None;
        }
        let outcome = self
            .dedupe
            .assess(&call, freq_hz, site.snr_db, spot_type, sample_ts, input);
        if outcome != DedupeOutcome::Emit {
            self.count_cross_input_dedupe(outcome, input);
            self.decide(at, Some(Reason::Dedupe), || withheld(Reason::Dedupe));
            return None;
        }
        let (rst, qrl_query) = self.track_annotations(track_id);
        let spot = Spot {
            callsign: call,
            freq_hz,
            snr_db: site.snr_db,
            wpm: site.wpm,
            spot_type,
            confidence,
            emission_state: EmissionState::Confirmed,
            track_id,
            sample_ts,
            rst,
            qrl_query,
            mode: SpotMode::Cw,
            baud: None,
        };
        self.admit(spot, reps, is_allowlisted, Confirmation::Fused, at)
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
        let (char_confidences, word_seq, start_ts, reclassifying) = {
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
            let reclassifying = word.attempted;
            word.attempted = true;
            word.last_spot_type = Some(SpotType::Beacon);
            word.classified_max_seq = involved_max_seq;
            (
                word.confidences.clone(),
                word.seq,
                word.start_ts.min(sample_ts),
                reclassifying,
            )
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
        let cause = if reclassifying {
            Cause::Reclassified
        } else {
            Cause::Decided
        };
        let at = At::new(track_id, word_seq, cause);
        if let Some(reason) = rejected {
            self.decide(at, Some(reason), || withheld(reason));
            return None;
        }
        let reason = Reason::BeaconPending;
        self.decide(at, Some(reason), || withheld(reason));

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
                start_ts,
                freq_hz,
                snr_db,
                char_confidences,
                origin_track_id: track_id,
                word_seq,
            },
        );
        if let Some(evicted) = evicted {
            self.suppression_counts.pending_beacon_overflow += 1;
            let reason = Reason::BeaconOverflow;
            self.decide(settled_at(&evicted), Some(reason), || {
                beacon_decision(track_id, &evicted, reason)
            });
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
                let mut overflowed = Vec::new();
                for pb in pending {
                    if let Some(evicted) = insert_by_age(&mut survivor_track.pending_beacons, pb) {
                        overflowed.push(evicted);
                    }
                }
                for evicted in overflowed {
                    self.suppression_counts.pending_beacon_overflow += 1;
                    let reason = Reason::BeaconOverflow;
                    self.decide(settled_at(&evicted), Some(reason), || {
                        beacon_decision(survivor, &evicted, reason)
                    });
                }
            }
            None => {
                self.suppression_counts.pending_beacon_lost_to_eviction += pending.len() as u64;
                for pb in &pending {
                    let reason = Reason::BeaconDiscarded;
                    self.decide(settled_at(pb), Some(reason), || {
                        beacon_decision(track_id, pb, reason)
                    });
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
                self.decide(settled_at(pb), Some(reason), || {
                    beacon_decision(track_id, pb, reason)
                });
            }
            return Vec::new();
        }
        pending
            .into_iter()
            .filter_map(|pb| {
                // Before `gate.record`, so a suppressed beacon never feeds
                // the repetition gate.
                let mut at = settled_at(&pb);
                if let Some(reason) = self.operator_suppressed(&pb) {
                    self.decide(at, Some(reason), || beacon_decision(track_id, &pb, reason));
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
                let is_active =
                    |tid| self.tracks.contains_key(&tid) || self.merged_into.contains_key(&tid);
                let reps = if self.observations.keeps_records() {
                    // MAN-207: the same record, plus the occurrences it counted.
                    let (reps, counted) = self.gate.record_span_traced(
                        pb.origin_track_id,
                        pb.freq_hz,
                        &pb.candidate,
                        (pb.start_ts, pb.sample_ts),
                        pb.word_seq,
                        is_active,
                    );
                    at.evidence = evidence_links(&counted);
                    reps as u32
                } else {
                    self.gate.record_span(
                        pb.origin_track_id,
                        pb.freq_hz,
                        &pb.candidate,
                        (pb.start_ts, pb.sample_ts),
                        pb.word_seq,
                        is_active,
                    ) as u32
                };
                // MAN-106: the zero-evidence abstention gate applies to
                // beacons exactly as to every other candidate -- absent
                // sound is absent sound (see `try_spot`'s gate).
                if !confidence::has_acoustic_evidence(&pb.char_confidences) {
                    let reason = Reason::NoEvidence;
                    self.decide(at, Some(reason), || {
                        beacon_decision(track_id, &pb, reason).with_reps(reps)
                    });
                    return None;
                }
                let tier = self.patterns.tier(&pb.candidate);
                let mut confidence = confidence::c_call(&pb.char_confidences, reps);
                if let Some(scp) = &self.scp {
                    confidence =
                        confidence::apply_scp_boost(confidence, scp.contains(&pb.candidate));
                }
                let emission_state = if reps >= self.level.required_reps(tier) {
                    EmissionState::Confirmed
                } else {
                    EmissionState::Tentative
                };
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
                        let reason = Reason::Dedupe;
                        self.decide(at, Some(reason), || {
                            beacon_decision(track_id, &pb, reason).with_reps(reps)
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
                    let reason = Reason::Dedupe;
                    self.decide(at, Some(reason), || {
                        beacon_decision(track_id, &pb, reason).with_reps(reps)
                    });
                    return None;
                }
                let spot = Spot {
                    callsign: pb.candidate,
                    freq_hz: pb.freq_hz,
                    snr_db: pb.snr_db,
                    wpm,
                    spot_type: SpotType::Beacon,
                    confidence,
                    emission_state,
                    track_id,
                    sample_ts: pb.sample_ts,
                    rst: rst.clone(),
                    qrl_query,
                    mode: SpotMode::Cw,
                    baud: None,
                };
                self.admit(spot, reps, false, Confirmation::ExactRepeat, at)
            })
            .collect()
    }
}

/// MAN-200: where a fused spot would be emitted -- the triggering copy's
/// track, time, frequency, SNR, speed and spot type (D11) -- and its episode.
struct FusedSite {
    track_id: u32,
    /// MAN-207: the triggering copy's word, which a fused decision is
    /// recorded about.
    word_seq: u64,
    sample_ts: u64,
    spot_type: SpotType,
    freq_hz: f64,
    snr_db: f32,
    wpm: f32,
    episode: Option<lattice::EpisodeKey>,
}

/// MAN-207: a pending beacon is decided later about the occurrence that
/// captured it -- its origin track's word, even after a merge migrated it.
fn settled_at(pb: &PendingBeacon) -> At {
    At::new(pb.origin_track_id, pb.word_seq, Cause::Settled)
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

/// MAN-318: the call a lookup is asked about: `call` without a valid
/// portable designator (`W1AW/P` -> `W1AW`), the rule
/// `cty::exact_match_base_len` uses.
fn lookup_key(call: &str) -> &str {
    match call.split_once('/') {
        Some((base, portable)) if grammar::is_valid_portable(portable) => base,
        _ => call,
    }
}

/// MAN-318: a lookup decision (`LookupPending`/`LookupNotFound`) about `spot`.
fn lookup_decision(spot: &Spot, reason: Reason, reps: u32) -> Decision {
    Decision::withheld(
        spot.track_id,
        spot.sample_ts,
        &spot.callsign,
        spot.freq_hz,
        spot.spot_type,
        reason,
    )
    .with_reps(reps)
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
            last_char_ts: 0,
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

    // ---- MAN-318: the callsign lookup step ----

    use std::sync::Mutex;

    /// MAN-318: answers `check` from a script and records every question.
    #[derive(Default)]
    struct ScriptedLookup {
        known: Mutex<BTreeMap<String, LookupStatus>>,
        asked: Mutex<Vec<String>>,
        settle_next: Mutex<Vec<(String, Existence)>>,
    }

    impl ScriptedLookup {
        fn answering(call: &str, status: LookupStatus) -> Arc<Self> {
            let l = Self::default();
            l.known.lock().unwrap().insert(call.to_string(), status);
            Arc::new(l)
        }
        fn settle(&self, call: &str, existence: Existence) {
            self.settle_next
                .lock()
                .unwrap()
                .push((call.to_string(), existence));
        }
        fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }
    }

    impl CallsignLookup for ScriptedLookup {
        fn check(&self, call: &str) -> LookupStatus {
            self.asked.lock().unwrap().push(call.to_string());
            self.known
                .lock()
                .unwrap()
                .get(call)
                .copied()
                .unwrap_or(LookupStatus::Pending)
        }
        fn settled(&self) -> Vec<(String, Existence)> {
            std::mem::take(&mut *self.settle_next.lock().unwrap())
        }
        fn settle_all(&self) -> Vec<(String, Existence)> {
            let mut out = self.settled();
            let asked = self.asked();
            let known = self.known.lock().unwrap();
            for call in asked {
                let pending = !matches!(known.get(&call), Some(LookupStatus::Known(_)));
                if pending && !out.iter().any(|(c, _)| *c == call) {
                    out.push((call, Existence::Unknown));
                }
            }
            out
        }
    }

    /// The K5ARH spot two `DE K5ARH K` transmissions yield, with `lookup` set.
    fn k5arh_with(lookup: Option<Arc<ScriptedLookup>>) -> (Validator, Vec<Spot>) {
        let mut v = recording(CTY_FIXTURE);
        if let Some(l) = lookup {
            v.set_callsign_lookup(l);
        }
        let words = ["DE", "K5ARH", "K"];
        let mut spots = run(&transmission_events(1, &words, 0), &mut v);
        spots.extend(run(&transmission_events(1, &words, 100_000), &mut v));
        (v, spots)
    }

    fn last_k5arh_decision(v: &mut Validator) -> Decision {
        let log = v.take_decisions();
        decisions_for(&log, "K5ARH")
            .last()
            .copied()
            .cloned()
            .expect("a K5ARH decision")
    }

    #[test]
    fn a_call_the_lookup_reports_not_found_is_not_spotted() {
        let lookup = ScriptedLookup::answering("K5ARH", LookupStatus::Known(Existence::NotFound));
        let (mut v, spots) = k5arh_with(Some(lookup.clone()));
        assert!(spots.is_empty(), "{spots:?}");
        let d = last_k5arh_decision(&mut v);
        assert_eq!(d.verdict, Verdict::Withheld);
        assert_eq!(d.reason, Some(Reason::LookupNotFound));
        assert!(d.reps.is_some());
        assert!(d.spot.is_none());
        assert_eq!(lookup.asked(), ["K5ARH"]);
    }

    /// MAN-200: V60's two damaged copies, `W1AV` and `W1AQ`, whose fourth
    /// character lists `W` as an alternative.
    fn complementary_copies() -> Vec<DecoderEvent> {
        let mut events = Vec::new();
        for (call, start) in [("W1AV", 0u64), ("W1AQ", 100_000)] {
            let mut ts = start;
            for word in ["CQ", call, "K"] {
                for (i, c) in word.chars().enumerate() {
                    let damaged = word == call && i == 3;
                    events.push(DecoderEvent::CharDecoded {
                        track_id: 1,
                        sample_ts: ts,
                        glyph: Glyph::Char(c),
                        confidence: if damaged { 0.45 } else { 0.95 },
                        alternatives: if damaged {
                            vec![(Glyph::Char('W'), 0.40)]
                        } else {
                            Vec::new()
                        },
                    });
                    ts += 100;
                }
                events.push(DecoderEvent::word_boundary(1, ts));
                ts += 100;
            }
        }
        events
    }

    #[test]
    fn fusion_is_off_by_default_and_allocates_nothing() {
        let mut v = recording(CTY_FIXTURE);
        assert!(run(&complementary_copies(), &mut v).is_empty());
        assert!(v.lattice.is_none());
        assert_eq!(v.lattice_counts(), None);
    }

    #[test]
    fn a_reclassification_never_adds_a_copy() {
        let mut v = recording(CTY_FIXTURE).with_repetition_fusion(true);
        let words = ["DE", "K5ARH", "K"];
        run(&transmission_events(1, &words, 0), &mut v);
        let added = v.lattice_counts().unwrap().added;
        assert_eq!(added, 1);
        // A later word re-scans the window; the settled K5ARH word is not
        // offered again.
        run(&transmission_events(1, &["QSL"], 50_000), &mut v);
        assert_eq!(v.lattice_counts().unwrap().added, added);
    }

    /// MAN-200 D10: repeated exact copies of a call the exact path withholds
    /// for `Grammar` or `Cty` make the lattice confirm the candidate's own
    /// text, which fails the same check again. The fused path must not
    /// decide that call a second time for the same word.
    #[test]
    fn a_grammar_or_cty_withhold_is_decided_once_per_word() {
        for (call, reason) in [("Q1AW", Reason::Cty), ("W1AW/X", Reason::Grammar)] {
            let mut v = recording(CTY_FIXTURE).with_repetition_fusion(true);
            for i in 0..5u64 {
                run(
                    &transmission_events(1, &["CQ", call, "K"], i * 100_000),
                    &mut v,
                );
            }
            let log = v.take_decisions();
            let decided = decisions_for(&log, call);
            assert_eq!(decided.len(), 5, "{call}: {decided:?}");
            let mut words: Vec<_> = decided.iter().map(|d| (d.track_id, d.sample_ts)).collect();
            words.dedup();
            assert_eq!(words.len(), 5, "{call}: a word decided twice: {decided:?}");
            assert!(
                decided.iter().all(|d| d.reason == Some(reason)),
                "{decided:?}"
            );
        }
    }

    #[test]
    fn a_fused_spot_held_for_lookup_keeps_its_marker() {
        let mut v = recording(CTY_FIXTURE).with_repetition_fusion(true);
        let lookup = Arc::new(ScriptedLookup::default());
        v.set_callsign_lookup(lookup.clone());
        assert!(run(&complementary_copies(), &mut v).is_empty());
        let held = v.take_decisions();
        assert_eq!(
            held.last().map(|d| (d.call.as_str(), d.reason)),
            Some(("W1AW", Some(Reason::LookupPending)))
        );
        lookup.settle("W1AW", Existence::Exists);
        let released = v.poll_lookups();
        assert_eq!(released.len(), 1);
        assert_eq!(released[0].callsign, "W1AW");
        let d = v.take_decisions().pop().expect("released decision");
        assert_eq!(d.verdict, Verdict::Spotted);
        assert_eq!(d.confirmation, Some(Confirmation::Fused));
    }

    /// Codex review, PR #500: the episode's own copies are ignored inside
    /// the rival search, so a copy that is the strongest rival cannot mask
    /// a weaker rival outside the episode.
    #[test]
    fn an_episode_copy_does_not_mask_another_rival() {
        let fused = |with_rival: bool| {
            let mut v = recording(CTY_FIXTURE).with_repetition_fusion(true);
            let mut events = complementary_copies();
            if with_rival {
                // A bare W1AX, never a self-ID copy, weaker than both copies.
                let at = events
                    .iter()
                    .position(|e| {
                        matches!(
                            e,
                            DecoderEvent::CharDecoded {
                                sample_ts: 100_000,
                                ..
                            }
                        )
                    })
                    .unwrap();
                let rival = "W1AX".chars().enumerate().map(|(i, c)| {
                    DecoderEvent::char_decoded(1, 50_000 + i as u64 * 100, Glyph::Char(c), 0.5)
                });
                let mut words: Vec<_> = rival.collect();
                words.push(DecoderEvent::word_boundary(1, 50_400));
                events.splice(at..at, words);
            }
            let spots = run(&events, &mut v);
            let log = v.take_decisions();
            let last = decisions_for(&log, "W1AW").last().map(|d| (*d).clone());
            (spots, last)
        };
        let (spots, d) = fused(false);
        assert_eq!(spots.len(), 1, "{spots:?}");
        assert_eq!(d.unwrap().confirmation, Some(Confirmation::Fused));
        let (spots, d) = fused(true);
        assert!(spots.is_empty(), "{spots:?}");
        let d = d.expect("a W1AW decision");
        assert_eq!(d.reason, Some(Reason::Variant));
        assert_eq!(d.rival.map(|r| r.call), Some("W1AX".to_string()));
    }

    #[test]
    fn alternatives_follow_their_characters() {
        let mut v = recording(CTY_FIXTURE);
        let ch = |ts: u64, c: char, alts: Vec<(Glyph, f32)>| DecoderEvent::CharDecoded {
            track_id: 1,
            sample_ts: ts,
            glyph: Glyph::Char(c),
            confidence: 0.9,
            alternatives: alts,
        };
        v.ingest(&ch(10, 'x', vec![(Glyph::Char('y'), 0.3)]));
        v.ingest(&ch(20, 'Q', vec![(Glyph::Prosign(Prosign::Ar), 0.3)]));
        v.ingest(&DecoderEvent::char_decoded(
            1,
            30,
            Glyph::Prosign(Prosign::Err),
            1.0,
        ));
        let current = &v.tracks[&1].current;
        assert!(current.text.is_empty() && current.char_alternatives.is_empty());
        v.ingest(&ch(
            40,
            'w',
            vec![(Glyph::Char('v'), 0.3), (Glyph::Prosign(Prosign::Ar), 0.2)],
        ));
        let current = &v.tracks[&1].current;
        assert_eq!(current.char_alternatives, vec![vec![('V', 0.3)]]);
        assert_eq!(current.start_ts, 40);
    }

    #[test]
    fn a_pending_call_is_held_and_spotted_once_it_exists() {
        let (_, plain) = k5arh_with(None);
        assert_eq!(plain.len(), 1);
        let lookup = Arc::new(ScriptedLookup::default());
        let (mut v, spots) = k5arh_with(Some(lookup.clone()));
        assert!(spots.is_empty());
        let d = last_k5arh_decision(&mut v);
        assert_eq!(d.verdict, Verdict::Deferred);
        assert_eq!(d.reason, Some(Reason::LookupPending));
        assert!(v.poll_lookups().is_empty(), "nothing settled yet");
        lookup.settle("K5ARH", Existence::Exists);
        let released = v.poll_lookups();
        assert_eq!(released, plain);
        let d = last_k5arh_decision(&mut v);
        assert_eq!(d.verdict, Verdict::Spotted);
        assert_eq!(d.spot.as_ref(), Some(&released[0]));
        assert!(v.poll_lookups().is_empty(), "released once");
    }

    #[test]
    fn a_pending_call_settled_not_found_is_withheld() {
        let lookup = Arc::new(ScriptedLookup::default());
        let (mut v, _) = k5arh_with(Some(lookup.clone()));
        v.take_decisions();
        lookup.settle("K5ARH", Existence::NotFound);
        assert!(v.poll_lookups().is_empty());
        let d = last_k5arh_decision(&mut v);
        assert_eq!(d.verdict, Verdict::Withheld);
        assert_eq!(d.reason, Some(Reason::LookupNotFound));
        assert!(d.spot.is_none());
    }

    #[test]
    fn a_pending_call_settled_unknown_is_spotted() {
        let lookup = Arc::new(ScriptedLookup::default());
        let (mut v, _) = k5arh_with(Some(lookup.clone()));
        lookup.settle("K5ARH", Existence::Unknown);
        let released = v.poll_lookups();
        assert_eq!(released.len(), 1);
        assert_eq!(released[0].callsign, "K5ARH");
    }

    #[test]
    fn allowlisted_calls_are_never_looked_up() {
        let lookup = Arc::new(ScriptedLookup::default());
        let mut v = recording(CTY_FIXTURE);
        v.set_callsign_lookup(lookup.clone());
        v.allowlist("W4KTNL");
        // MAN-201 (D5): the trailing `K` settles the call's role.
        let spots = run(&transmission_events(1, &["W4KTNL", "K"], 0), &mut v);
        assert_eq!(spots.len(), 1);
        assert!(lookup.asked().is_empty());
    }

    #[test]
    fn a_portable_call_is_looked_up_by_its_base_call() {
        let lookup = Arc::new(ScriptedLookup::default());
        let mut v2 = recording(CTY_FIXTURE);
        v2.set_callsign_lookup(lookup.clone());
        let words = ["DE", "W1AW/P", "K"];
        let mut spots = run(&transmission_events(1, &words, 0), &mut v2);
        spots.extend(run(&transmission_events(1, &words, 100_000), &mut v2));
        assert!(spots.is_empty());
        assert_eq!(lookup.asked(), ["W1AW"]);
        lookup.settle("W1AW", Existence::Exists);
        let released = v2.poll_lookups();
        assert_eq!(released.len(), 1);
        assert_eq!(released[0].callsign, "W1AW/P");
    }

    #[test]
    fn spots_held_for_one_call_are_released_together_in_order() {
        let lookup = Arc::new(ScriptedLookup::default());
        let mut v = recording(CTY_FIXTURE);
        v.set_callsign_lookup(lookup.clone());
        seed_meta(&mut v, 2);
        v.ingest(&DecoderEvent::TrackMeta {
            track_id: 2,
            sample_ts: 0,
            snr_2500_db: 20.0,
            freq_hz: 14_010_000.0,
            last_char_ts: 0,
        });
        let words = ["DE", "K5ARH", "K"];
        let mut spots = Vec::new();
        for track in [1, 2] {
            spots.extend(run(&transmission_events(track, &words, 0), &mut v));
            spots.extend(run(&transmission_events(track, &words, 100_000), &mut v));
        }
        assert!(spots.is_empty());
        lookup.settle("K5ARH", Existence::Exists);
        let released = v.poll_lookups();
        assert_eq!(released.len(), 2, "{released:?}");
        assert_eq!(released[0].track_id, 1);
        assert_eq!(released[1].track_id, 2);
    }

    #[test]
    fn a_deferred_beacon_is_looked_up_when_its_track_ends() {
        let lookup = ScriptedLookup::answering("K5ARH", LookupStatus::Known(Existence::NotFound));
        let mut v = recording(CTY_FIXTURE);
        v.set_callsign_lookup(lookup.clone());
        run(
            &transmission_events(1, &["V", "V", "V", "K5ARH", "K"], 0),
            &mut v,
        );
        v.take_decisions();
        v.ingest(&DecoderEvent::SpeedUpdate {
            track_id: 1,
            wpm: 22.0,
        });
        let spots = v.ingest(&DecoderEvent::TrackClosed {
            track_id: 1,
            closure: ClosureKind::SignalEnded,
        });
        assert!(spots.is_empty(), "{spots:?}");
        let log = v.take_decisions();
        assert_eq!(log.len(), 1, "{log:?}");
        assert_eq!(log[0].verdict, Verdict::Withheld);
        assert_eq!(log[0].reason, Some(Reason::LookupNotFound));
        assert_eq!(lookup.asked(), ["K5ARH"]);
    }

    /// Codex review, PR #442: a reload while a spot is held must not let it
    /// leak past a blocklist or notch it now falls under.
    #[test]
    fn a_held_spot_newly_blocklisted_or_notched_is_withheld_on_release() {
        for (blocklist, notch, reason) in [
            (
                Blocklist::parse("K5ARH\n"),
                NotchList::default(),
                Reason::Blocklist,
            ),
            (
                Blocklist::default(),
                NotchList::parse("13999900-14000100\n"),
                Reason::Notch,
            ),
        ] {
            for existence in [Existence::Exists, Existence::Unknown] {
                let lookup = Arc::new(ScriptedLookup::default());
                let (mut v, spots) = k5arh_with(Some(lookup.clone()));
                assert!(spots.is_empty());
                v.take_decisions();
                v.replace_operator_lists(&[], blocklist.clone(), notch.clone());
                lookup.settle("K5ARH", existence);
                assert!(v.poll_lookups().is_empty(), "{reason:?} {existence:?}");
                let d = last_k5arh_decision(&mut v);
                assert_eq!(d.verdict, Verdict::Withheld);
                assert_eq!(d.reason, Some(reason));
                let counts = v.suppression_counts();
                assert_eq!(
                    (counts.blocklist, counts.notch),
                    if reason == Reason::Blocklist {
                        (1, 0)
                    } else {
                        (0, 1)
                    }
                );
            }
        }
        // finish_lookups (shutdown) applies the same check.
        let lookup = Arc::new(ScriptedLookup::default());
        let (mut v, _) = k5arh_with(Some(lookup.clone()));
        v.replace_operator_lists(&[], Blocklist::parse("K5ARH\n"), NotchList::default());
        assert!(v.finish_lookups().is_empty());
        assert_eq!(v.suppression_counts().blocklist, 1);
    }

    #[test]
    fn a_held_spot_newly_allowlisted_spots_even_if_not_found() {
        let lookup = Arc::new(ScriptedLookup::default());
        let (mut v, _) = k5arh_with(Some(lookup.clone()));
        v.replace_operator_lists(
            &["k5arh".to_string()],
            Blocklist::default(),
            NotchList::default(),
        );
        lookup.settle("K5ARH", Existence::NotFound);
        let released = v.poll_lookups();
        assert_eq!(released.len(), 1);
        assert_eq!(released[0].callsign, "K5ARH");
    }

    #[test]
    fn finish_lookups_settles_every_held_spot() {
        let lookup = Arc::new(ScriptedLookup::default());
        let (mut v, _) = k5arh_with(Some(lookup.clone()));
        assert!(v.poll_lookups().is_empty());
        let released = v.finish_lookups();
        assert_eq!(released.len(), 1);
        assert_eq!(released[0].callsign, "K5ARH");
        assert!(v.held.is_empty());
        assert!(v.finish_lookups().is_empty());
    }

    #[test]
    fn without_a_lookup_poll_and_finish_return_nothing() {
        let (mut v, spots) = k5arh_with(None);
        assert_eq!(spots.len(), 1);
        assert!(v.poll_lookups().is_empty());
        assert!(v.finish_lookups().is_empty());
    }

    #[test]
    fn unlisted_decisions_and_reclassification_keep_the_five_rep_requirement() {
        let mut v = recording(CTY_FIXTURE);
        let events = transmission_events(1, &["DE", "W4KTNL", "CQ"], 0);
        assert!(run(&events, &mut v).is_empty());
        let log = v.take_decisions();
        let decisions = decisions_for(&log, "W4KTNL");
        // MAN-201 (D5): the call is decided once, when the trailing `CQ`
        // settles it -- as `Cq` -- not as `De` first and again as `Cq`.
        assert_eq!(decisions.len(), 1, "decided once, at settle: {log:?}");
        assert_eq!(decisions[0].spot_type, SpotType::Cq);
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
            run(&transmission_events(1, &["W4KTNL", "K"], 0), &mut v).len(),
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

    fn meta_with_chars(track_id: u32, sample_ts: u64, last_char_ts: u64) -> DecoderEvent {
        DecoderEvent::TrackMeta {
            track_id,
            sample_ts,
            snr_2500_db: 20.0,
            freq_hz: 14_020_000.0,
            last_char_ts,
        }
    }

    /// MAN-201 D3: silence after the tail word ends its turn at a `TrackMeta`,
    /// which settles the call without waiting for the track to close.
    #[test]
    fn a_silent_tail_settles_at_track_meta() {
        let mut v = Validator::bundled(FS);
        v.allowlist("K5ARH");
        seed_meta(&mut v, 1);
        let events = transmission_events(1, &["DE", "K5ARH"], 1_000);
        assert!(run(&events, &mut v).is_empty(), "the tail is unsettled");
        let end = 1_000 + 2_000;
        let spots = v.ingest(&meta_with_chars(1, end + 480_000, end));
        assert_eq!(spots.len(), 1, "{spots:?}");
        assert_eq!(spots[0].callsign, "K5ARH");
    }

    /// MAN-201 D3: the MAN-317 prior holds a word's characters until it
    /// ends. A newer character in the decoder's hands is not silence, even
    /// though nothing newer than the tail has reached the validator.
    #[test]
    fn a_word_the_decoder_still_holds_is_not_silence() {
        let mut v = Validator::bundled(FS);
        v.allowlist("K5ARH");
        seed_meta(&mut v, 1);
        let events = transmission_events(1, &["DE", "K5ARH"], 1_000);
        assert!(run(&events, &mut v).is_empty());
        let now = 1_000 + 2_000 + 480_000;
        let spots = v.ingest(&meta_with_chars(1, now, now - 96_000));
        assert!(spots.is_empty(), "{spots:?}");
    }

    /// MAN-201 D6: a candidate is bound to its exact word, not the newest
    /// word sharing its text.
    #[test]
    fn exact_binding_evaluates_the_occurrence_not_the_newest_same_text_word() {
        let mut v = recording(CTY_FIXTURE);
        let words = ["DE", "K1ABC", "K", "K1ABC", "DE", "W1AW", "K"];
        run(&transmission_events(1, &words, 0), &mut v);
        let log = v.take_decisions();
        let d = decisions_for(&log, "K1ABC");
        assert_eq!(d.len(), 1, "{log:?}");
        assert_eq!(d[0].sample_ts, 800, "the first occurrence's boundary");
    }

    /// MAN-201 D5: the V41 shape. A held occurrence reaches the gate at its
    /// own time, so two calls 80 s apart still confirm.
    #[test]
    fn held_occurrences_are_recorded_in_the_gate_at_their_own_time() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        seed_meta(&mut v, 1);
        let mut spots = run(&transmission_events(1, &["DE", "K5ARH"], 0), &mut v);
        spots.extend(run(
            &transmission_events(1, &["DE", "K5ARH"], (80.0 * FS) as u64),
            &mut v,
        ));
        spots.extend(v.ingest(&DecoderEvent::TrackClosed {
            track_id: 1,
            closure: ClosureKind::SignalEnded,
        }));
        assert_eq!(spots.len(), 1, "{spots:?}");
        assert_eq!(spots[0].callsign, "K5ARH");
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
            run(
                &transmission_events(1, &["V", "V", "V", call.as_str(), "K"], ts),
                &mut v,
            );
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
        v.allowlist("K5ARH");
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
            last_char_ts: 0,
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
        let words = ["V", "V", "V", "K5ARH"];
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
        let words = ["V", "V", "V", "K5ARH"];
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
        // MAN-201 (D5): each trailing `K` settles the call's role.
        let spots = run(&transmission_events(1, &["W1AW", "K"], 0), &mut v);
        assert_eq!(spots.len(), 1, "got {spots:?}");
        assert_eq!(spots[0].callsign, "W1AW");
        let spots = run(&transmission_events(2, &["K5ARH", "K"], 0), &mut v);
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
        let spots = run(
            &transmission_events(1, &["V", "V", "V", "K5ARH"], 0),
            &mut v,
        );
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
        let spots = run(
            &transmission_events(1, &["V", "V", "V", "K5ARH"], 0),
            &mut v,
        );
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
        let spots = run(
            &transmission_events(1, &["V", "V", "V", "K5ARH"], 0),
            &mut v,
        );
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
        let spots = run(
            &transmission_events(1, &["V", "V", "V", "K5ARH"], 0),
            &mut v,
        );
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
        // MAN-322: the validator stamps every spot it builds as CW.
        assert_eq!(spots[0].mode, SpotMode::Cw);
        assert!(spots[0].baud.is_none());
    }

    #[test]
    fn spot_mode_labels_and_serializes_in_capitals() {
        assert_eq!(SpotMode::Cw.label(), "CW");
        assert_eq!(SpotMode::Rtty.label(), "RTTY");
        assert_eq!(serde_json::to_value(SpotMode::Rtty).unwrap(), "RTTY");
        assert_eq!(serde_json::to_value(SpotMode::Cw).unwrap(), "CW");
        assert_eq!(SpotMode::default(), SpotMode::Cw);
    }

    /// MAN-322 D4: CLI JSON appends `mode` and `baud` after `qrlQuery`, and
    /// sends `wpm` as `null` on an RTTY spot.
    #[test]
    fn cli_json_appends_mode_and_baud_and_nulls_wpm_on_rtty() {
        let cw = Spot {
            callsign: "W1AW".into(),
            freq_hz: 14_000_759.2,
            snr_db: 29.2,
            wpm: 19.9,
            spot_type: SpotType::Cq,
            confidence: 0.794,
            emission_state: EmissionState::Confirmed,
            track_id: 1,
            sample_ts: 0,
            rst: None,
            qrl_query: false,
            mode: SpotMode::Cw,
            baud: None,
        };
        assert_eq!(
            serde_json::to_string(&cw).unwrap(),
            r#"{"dxCall":"W1AW","frequency":14000759.2,"snr":29.2,"wpm":19.9,"spotType":"cq","decodeConfidence":0.794,"emissionState":"confirmed","trackId":1,"sampleTs":0,"rst":null,"qrlQuery":false,"mode":"CW","baud":null}"#
        );
        let rtty = Spot {
            wpm: 0.0,
            mode: SpotMode::Rtty,
            baud: Some(45.45),
            ..cw
        };
        assert_eq!(
            serde_json::to_string(&rtty).unwrap(),
            r#"{"dxCall":"W1AW","frequency":14000759.2,"snr":29.2,"wpm":null,"spotType":"cq","decodeConfidence":0.794,"emissionState":"confirmed","trackId":1,"sampleTs":0,"rst":null,"qrlQuery":false,"mode":"RTTY","baud":45.45}"#
        );
    }

    #[test]
    fn pending_beacon_in_a_newly_notched_range_never_spots() {
        let mut v = Validator::new(FS, CTY_FIXTURE, None);
        seed_meta(&mut v, 1);
        v.ingest(&DecoderEvent::SpeedUpdate {
            track_id: 1,
            wpm: 22.0,
        });
        let spots = run(
            &transmission_events(1, &["V", "V", "V", "K5ARH"], 0),
            &mut v,
        );
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
        let spots = run(&transmission_events(2, &["W1AW", "K"], 200_000), &mut v);
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
        let words = ["V", "V", "V", "K5ARH", "K"];
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
        let spots = run(
            &transmission_events(1, &["V", "V", "V", "K5ARH"], 0),
            &mut v,
        );
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
        let spots = run(
            &transmission_events(1, &["V", "V", "V", "K5ARH"], 0),
            &mut v,
        );
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
        let spots = run(
            &transmission_events(2, &["V", "V", "V", "K5ARH"], 0),
            &mut v,
        );
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
        let spots = run(
            &transmission_events(1, &["V", "V", "V", "K5AAZ", "K"], 0),
            &mut v,
        );
        assert!(spots.is_empty());
        let mut expected = Vec::new();
        for i in 0..MAX_PENDING_BEACONS {
            let call = format!("K5AA{}", (b'A' + i as u8) as char);
            let start = 10_000_000 + i as u64 * 1_000_000;
            let spots = run(
                &transmission_events(2, &["V", "V", "V", call.as_str(), "K"], start),
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
        // MAN-201 (D5): the capture is stamped at "K5ARH"'s own word
        // boundary, 1 100 samples after the first `V`.
        let spots = run(
            &transmission_events(1, &["V", "V", "V", "K5ARH", "K"], t - 1_100),
            &mut v,
        );
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
            let words = ["V", "V", "V", call.as_str()];
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
        let words = ["V", "V", "V", "K5ARH"];
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
                last_char_ts: 0,
            });
        };

        // bucket(14_012_549) == 46_708, bucket(14_012_551) == 46_709.
        meta(&mut v, 1, 14_012_549.0);
        let spots = run(
            &transmission_events(1, &["V", "V", "V", "K5ARH"], 1_000),
            &mut v,
        );
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
        let words = ["V", "V", "V", "K5ARH"];
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
        // MAN-201 (D5): the trailing `K` settles the call's role.
        let spots = run(
            &transmission_events(3, &["DE", "K5ARH", "K"], ten_seconds),
            &mut v,
        );
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
        let words = ["V", "V", "V", "K5ARH"];

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
        let words = ["V", "V", "V", "K5ARH"];
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
        let words = ["V", "V", "V", "K5ARH"];
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
        let words = ["V", "V", "V", "K5ARH", "K"];
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
            freq_hz: 14_000_030.0, // 30 Hz away -- same signal, same bucket,
            last_char_ts: 0,
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
            freq_hz: 14_001_000.0, // 1 kHz away -- a different bucket,
            last_char_ts: 0,
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
            last_char_ts: 0,
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
            last_char_ts: 0,
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
                last_char_ts: 0,
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

    // ---- MAN-208: bounded delayed refinement of ambiguous decodes ----

    /// A refinement result for a five-character call, as the engine-side
    /// scheduler would produce one from a scratch replay.
    fn refined(call: &str, span: (u64, u64), hypothesis: &'static str) -> RefinedWord {
        RefinedWord {
            call: call.to_string(),
            confidences: vec![0.95; call.chars().count()],
            span,
            word_confidence: 0.95,
            hypothesis,
        }
    }

    /// The `(seq, span)` of the track's only word with this text.
    fn word_named(v: &Validator, track_id: u32, text: &str) -> (u64, (u64, u64)) {
        let track = &v.tracks[&track_id];
        let word = track
            .words
            .iter()
            .find(|w| w.text == text)
            .unwrap_or_else(|| panic!("no {text} word"));
        (word.seq, (word.start_ts, word.end_ts))
    }

    /// Gherkin 1's shape: an ambiguous `K3ABC` decode at t=1_000_000, a
    /// clean `K3ABD` repetition 8 s later (96 kS/s audio rate) on the same
    /// track. Returns the validator and every spot so far (none expected).
    fn ambiguous_then_clean_repeat(v: &mut Validator) {
        let events = transmission_events(1, &["DE", "K3ABC", "K"], 1_000_000);
        assert!(
            run(&events, v).is_empty(),
            "ambiguous first decode alone never spots"
        );
        let later = transmission_events(1, &["DE", "K3ABD", "K"], 1_768_000);
        assert!(
            run(&later, v).is_empty(),
            "a single clean repetition alone never spots"
        );
    }

    #[test]
    fn a_refined_episode_confirmed_by_a_later_repetition_spots_at_the_later_ts() {
        let mut v = recording(CTY_FIXTURE);
        ambiguous_then_clean_repeat(&mut v);
        let (seq, span) = word_named(&v, 1, "K3ABC");
        // The scheduler re-decodes 32 ms of stream later; the repetition
        // completed at its own boundary, start+800.
        let spots = v.apply_refinement(1, seq, refined("K3ABD", span, "wpm:25"), 1_800_000);
        assert_eq!(spots.len(), 1, "{spots:?}");
        assert_eq!(spots[0].callsign, "K3ABD");
        assert_eq!(
            spots[0].sample_ts, 1_768_800,
            "the spot carries the later repetition's RF time, not the first decode's"
        );
    }

    /// Validate round 2 (MAN-208): a call-changing refinement must revise
    /// the SAME occurrence in the variant-arbitration ledger too, not only
    /// in the repetition gate. Before the fix the ledger kept the stale
    /// `K3ABC` (0.95) reading next to the clean `K3ABD` (0.90) repetition:
    /// one rep each, the stale reading winning the confidence tie-break,
    /// so the rescue was withheld as a Variant by the very evidence it
    /// had just superseded.
    #[test]
    fn a_call_changing_refinement_revises_the_rival_ledger_occurrence() {
        let mut v = recording(CTY_FIXTURE);
        let word_at = |text: &str, conf: f32, start: u64| -> Vec<DecoderEvent> {
            let mut events = Vec::new();
            let mut ts = start;
            for w in ["DE", text, "K"] {
                let c = if w == text { conf } else { 0.95 };
                for ch in w.chars() {
                    events.push(DecoderEvent::char_decoded(1, ts, Glyph::Char(ch), c));
                    ts += 100;
                }
                events.push(DecoderEvent::word_boundary(1, ts));
                ts += 100;
            }
            events
        };
        assert!(run(&word_at("K3ABC", 0.95, 1_000_000), &mut v).is_empty());
        assert!(run(&word_at("K3ABD", 0.90, 1_768_000), &mut v).is_empty());
        let (seq, span) = word_named(&v, 1, "K3ABC");
        let mut fix = refined("K3ABD", span, "engine:hsmm");
        fix.confidences = vec![0.90; 5];
        fix.word_confidence = 0.90;
        let spots = v.apply_refinement(1, seq, fix, 1_800_000);
        assert_eq!(
            spots
                .iter()
                .map(|s| s.callsign.as_str())
                .collect::<Vec<_>>(),
            ["K3ABD"],
            "the stale K3ABC reading must not veto its own correction"
        );
        assert_eq!(
            v.ledger.support(1, "K3ABC", v.now_ts).reps,
            0,
            "the occurrence's K3ABC reading left the ledger"
        );
        assert_eq!(
            v.ledger.support(1, "K3ABD", v.now_ts).reps,
            2,
            "moved, not added: the occurrence plus its repetition"
        );
    }

    /// Validate round 2 (MAN-208): a candidate's speed is reported stable
    /// only once the track has decoded `STABLE_SPEED_WORDS` words since
    /// its last `SpeedUpdate` -- the estimate held, it did not just jump.
    #[test]
    fn refine_candidates_report_speed_stability_by_words_held() {
        let mut v = recording(CTY_FIXTURE);
        ambiguous_then_clean_repeat(&mut v);
        let stable = |v: &Validator| {
            v.refine_candidates(v.now_ts, 10 * FS as u64)
                .iter()
                .find(|c| c.call == "K3ABC")
                .expect("candidate")
                .wpm_stable
        };
        v.ingest(&DecoderEvent::SpeedUpdate {
            track_id: 1,
            wpm: 28.0,
        });
        assert!(!stable(&v), "a speed that just changed is not stable");
        let (one, next) = word_events(1, "DE", 1_900_000);
        run(&one, &mut v);
        assert!(!stable(&v), "one word is not yet a held estimate");
        let (two, _) = word_events(1, "K", next);
        run(&two, &mut v);
        assert!(stable(&v), "held across two words");
    }

    #[test]
    fn refine_candidates_are_read_only_and_expire_at_the_lookback() {
        let mut v = recording(CTY_FIXTURE);
        ambiguous_then_clean_repeat(&mut v);
        let before = v.gate.counted_total();
        let observations = v.observations().iter().count();
        let candidates = v.refine_candidates(v.now_ts, 10 * FS as u64);
        assert!(!candidates.is_empty());
        assert!(candidates
            .windows(2)
            .all(|c| (c[0].track_id, c[0].word_seq) < (c[1].track_id, c[1].word_seq)));
        assert_eq!(v.gate.counted_total(), before);
        assert_eq!(v.observations().iter().count(), observations);
        assert!(v
            .refine_candidates(v.now_ts + 11 * FS as u64, 10 * FS as u64)
            .is_empty());
    }

    #[test]
    fn a_refinement_that_reassigns_the_call_moves_the_repetition_not_adds_one() {
        let mut v = recording(CTY_FIXTURE);
        ambiguous_then_clean_repeat(&mut v);
        let (seq, span) = word_named(&v, 1, "K3ABC");
        let before = v.gate.counted_total();
        let spots = v.apply_refinement(1, seq, refined("K3ABD", span, "engine:hsmm"), 1_800_000);
        assert_eq!(spots.len(), 1);
        assert_eq!(v.gate.counted_total(), before, "support moved, not added");
        // The losing callsign lost the occurrence's support: a second clean
        // K3ABC repetition still needs two of its own.
        let again = transmission_events(1, &["DE", "K3ABC", "K"], 2_500_000);
        let spots = run(&again, &mut v);
        assert!(spots.is_empty(), "K3ABC reopened from 1 rep: {spots:?}");
        let snapshot = v
            .gate
            .support_snapshot(14_000_000.0, "K3ABC", v.now_ts)
            .expect("K3ABC entry");
        assert_eq!(snapshot.counted.len(), 1, "only its own new repetition");
        let snapshot = v
            .gate
            .support_snapshot(14_000_000.0, "K3ABD", v.now_ts)
            .expect("K3ABD entry");
        assert_eq!(snapshot.counted.len(), 2, "moved occurrence + repetition");
    }

    #[test]
    fn two_agreeing_refinements_leave_the_repetition_count_unchanged() {
        let mut v = recording(CTY_FIXTURE);
        ambiguous_then_clean_repeat(&mut v);
        let (seq, span) = word_named(&v, 1, "K3ABC");
        let first = v.apply_refinement(1, seq, refined("K3ABD", span, "engine:hsmm"), 1_800_000);
        assert_eq!(first.len(), 1);
        let between = v.gate.counted_total();
        // A second refinement of the same evidence under a different
        // timing hypothesis agrees: nothing may move or be added.
        let second = v.apply_refinement(1, seq, refined("K3ABD", span, "wpm:25"), 1_832_000);
        assert_eq!(v.gate.counted_total(), between);
        assert!(
            second.is_empty(),
            "an agreeing refinement re-emits nothing: {second:?}"
        );
        let snapshot = v
            .gate
            .support_snapshot(14_000_000.0, "K3ABD", v.now_ts)
            .expect("K3ABD entry");
        assert_eq!(snapshot.counted.len(), 2);
    }

    #[test]
    fn a_rescued_spot_is_never_emitted_at_the_refined_spans_older_ts() {
        let mut v = recording(CTY_FIXTURE);
        // No later repetition exists: the refinement alone has one moved
        // occurrence of support and must not emit at the old span's time.
        let events = transmission_events(1, &["DE", "K3ABC", "K"], 1_000_000);
        assert!(run(&events, &mut v).is_empty());
        let (seq, span) = word_named(&v, 1, "K3ABC");
        let spots = v.apply_refinement(1, seq, refined("K3ABD", span, "engine:hsmm"), 1_100_000);
        assert!(spots.is_empty(), "no second repetition, no spot: {spots:?}");
    }

    #[test]
    fn a_refinement_of_an_evicted_word_is_an_orphaned_revision_not_a_spot() {
        let mut v = recording(CTY_FIXTURE);
        let events = transmission_events(1, &["DE", "K3ABC", "K"], 1_000_000);
        assert!(run(&events, &mut v).is_empty());
        let (seq, span) = word_named(&v, 1, "K3ABC");
        // Evict the word from the 16-word window.
        for i in 0..17u64 {
            run(
                &transmission_events(1, &["QSL"], 1_100_000 + i * 100_000),
                &mut v,
            );
        }
        let before = v.gate.counted_total();
        let orphaned = v.observations().losses().orphaned_revisions;
        let spots = v.apply_refinement(1, seq, refined("K3ABD", span, "engine:hsmm"), 3_000_000);
        assert!(spots.is_empty());
        assert_eq!(
            v.observations().losses().orphaned_revisions,
            orphaned + 1,
            "the orphaned revision is counted, not silent"
        );
        assert_eq!(v.gate.counted_total(), before, "no repetition change");
    }

    #[test]
    fn a_refinement_does_not_reopen_the_losing_callsign() {
        let mut v = recording(CTY_FIXTURE);
        ambiguous_then_clean_repeat(&mut v);
        let (seq, span) = word_named(&v, 1, "K3ABC");
        let spots = v.apply_refinement(1, seq, refined("K3ABD", span, "engine:hsmm"), 1_800_000);
        assert_eq!(spots.len(), 1);
        assert_eq!(spots[0].callsign, "K3ABD");
        // The losing callsign has no live candidate left: nothing about it
        // is re-evaluated, and no later window rescan resurrects it.
        let tail = transmission_events(1, &["K"], 1_900_000);
        assert!(run(&tail, &mut v).is_empty());
        let candidates = v.refine_candidates(v.now_ts, 10 * FS as u64);
        assert!(
            candidates.iter().all(|c| c.call != "K3ABC"),
            "the refined word no longer reads as ambiguous: {candidates:?}"
        );
    }

    #[test]
    fn a_refinement_never_increases_any_repetition_count() {
        let mut v = recording(CTY_FIXTURE);
        ambiguous_then_clean_repeat(&mut v);
        let (seq, span) = word_named(&v, 1, "K3ABC");
        let total = v.gate.counted_total();
        // A disagreeing refinement moves the support back; conservation
        // must hold through every reassignment, not just the first.
        v.apply_refinement(1, seq, refined("K3ABD", span, "engine:hsmm"), 1_800_000);
        assert_eq!(v.gate.counted_total(), total);
        v.apply_refinement(1, seq, refined("K3ABC", span, "wpm:25"), 1_832_000);
        assert_eq!(
            v.gate.counted_total(),
            total,
            "repeated refinements conserve counted occurrences"
        );
    }

    // ---- MAN-515: the narrow refinement guard and its outcomes ----

    /// The `N3NXI` -> `EEETT` loss in miniature: a refinement that would
    /// turn a spottable call into garbage is refused, the word keeps its
    /// text and support, and a later clean repetition still spots it.
    #[test]
    fn refinement_never_turns_a_spottable_call_into_garbage() {
        let mut v = recording(CTY_FIXTURE);
        let events = transmission_events(1, &["DE", "K3ABD", "K"], 1_000_000);
        assert!(
            run(&events, &mut v).is_empty(),
            "one repetition never spots"
        );
        let (seq, span) = word_named(&v, 1, "K3ABD");
        let counted = v.gate.counted_total();
        let (spots, outcome) = v.apply_refinement_with_outcome(
            1,
            seq,
            refined("EEETT", span, "engine:legacy"),
            1_100_000,
        );
        assert!(spots.is_empty());
        assert_eq!(outcome, RefinementOutcome::Rejected);
        assert_eq!(word_named(&v, 1, "K3ABD").0, seq, "the word keeps its text");
        assert_eq!(v.gate.counted_total(), counted, "nothing moved");
        let snapshot = v
            .gate
            .support_snapshot(14_000_000.0, "K3ABD", v.now_ts)
            .expect("K3ABD entry");
        assert_eq!(snapshot.counted.len(), 1, "the occurrence still counts");
        assert!(
            v.gate
                .support_snapshot(14_000_000.0, "EEETT", v.now_ts)
                .is_none_or(|s| s.counted.is_empty()),
            "no support moved to the garbage reading"
        );
        let later = transmission_events(1, &["DE", "K3ABD", "K"], 1_768_000);
        let spots = run(&later, &mut v);
        assert_eq!(
            spots
                .iter()
                .map(|s| s.callsign.as_str())
                .collect::<Vec<_>>(),
            ["K3ABD"],
            "the kept occurrence plus a clean repetition spots"
        );
    }

    /// The HSMM `AC3GYH` context case: a non-call word is not spottable,
    /// so a refinement of it still applies (`NEE` -> `DE`).
    #[test]
    fn refinement_of_a_non_call_word_still_applies() {
        let mut v = recording(CTY_FIXTURE);
        let events = transmission_events(1, &["NEE", "K3ABD", "K"], 1_000_000);
        run(&events, &mut v);
        let (seq, span) = word_named(&v, 1, "NEE");
        let mut fix = refined("DE", span, "engine:legacy");
        fix.confidences = vec![0.95; 2];
        let (_, outcome) = v.apply_refinement_with_outcome(1, seq, fix, 1_100_000);
        assert_eq!(outcome, RefinementOutcome::Changed);
        assert_eq!(word_named(&v, 1, "DE").0, seq, "the word now reads DE");
    }

    #[test]
    fn plausible_to_plausible_refinement_still_moves_support() {
        let mut v = recording(CTY_FIXTURE);
        ambiguous_then_clean_repeat(&mut v);
        let (seq, span) = word_named(&v, 1, "K3ABC");
        let (spots, outcome) = v.apply_refinement_with_outcome(
            1,
            seq,
            refined("K3ABD", span, "engine:legacy"),
            1_800_000,
        );
        assert_eq!(outcome, RefinementOutcome::Changed);
        assert_eq!(spots.len(), 1, "{spots:?}");
        assert_eq!(spots[0].callsign, "K3ABD");
        assert_eq!(spots[0].sample_ts, 1_768_800);
    }

    #[test]
    fn unchanged_and_orphaned_outcomes() {
        let mut v = recording(CTY_FIXTURE);
        let events = transmission_events(1, &["DE", "K3ABC", "K"], 1_000_000);
        run(&events, &mut v);
        let (seq, span) = word_named(&v, 1, "K3ABC");
        let (_, outcome) =
            v.apply_refinement_with_outcome(1, seq, refined("K3ABC", span, "wpm-seed"), 1_050_000);
        assert_eq!(outcome, RefinementOutcome::Unchanged);
        for i in 0..17u64 {
            run(
                &transmission_events(1, &["QSL"], 1_100_000 + i * 100_000),
                &mut v,
            );
        }
        let (spots, outcome) =
            v.apply_refinement_with_outcome(1, seq, refined("EEETT", span, "wpm-seed"), 3_000_000);
        assert!(spots.is_empty());
        assert_eq!(
            outcome,
            RefinementOutcome::Orphaned,
            "an evicted word is orphaned, never rejected"
        );
    }

    /// The rejection is recorded once, as a withheld decision about the
    /// refined reading, under the permanent check it failed.
    #[test]
    fn a_rejected_refinement_records_one_grammar_or_cty_decision() {
        let mut v = recording(CTY_FIXTURE).with_observations(ObservationPolicy::records());
        run(
            &transmission_events(1, &["DE", "K3ABD", "K"], 1_000_000),
            &mut v,
        );
        v.take_decisions();
        let (seq, span) = word_named(&v, 1, "K3ABD");
        let (_, outcome) = v.apply_refinement_with_outcome(
            1,
            seq,
            refined("EEETT", span, "engine:legacy"),
            1_100_000,
        );
        assert_eq!(outcome, RefinementOutcome::Rejected);
        let log = v.take_decisions();
        assert_eq!(log.len(), 1, "{log:?}");
        assert_eq!(log[0].call, "EEETT");
        assert_eq!(log[0].reason, Some(Reason::Grammar));
        let records: Vec<_> = v
            .observations()
            .iter()
            .filter(|o| o.draft.cause == Cause::Refined)
            .collect();
        assert_eq!(records.len(), 1, "one Refined record");
        assert_eq!(
            records[0].draft.decision().and_then(|d| d.reason),
            Some(Reason::Grammar)
        );
        // A plausible reading whose prefix the table does not allocate
        // fails cty instead.
        let (_, outcome) =
            v.apply_refinement_with_outcome(1, seq, refined("Z3ABD", span, "wpm-seed"), 1_150_000);
        assert_eq!(outcome, RefinementOutcome::Rejected);
        let log = v.take_decisions();
        assert_eq!(log.len(), 1, "{log:?}");
        assert_eq!(log[0].reason, Some(Reason::Cty));
        assert_eq!(word_named(&v, 1, "K3ABD").0, seq);
    }

    /// A beacon-shaped transmission (`V V V` + call, twice) whose first
    /// callsign word carries an unresolved per-character alternative --
    /// the shape `refine_candidates` admits (the word is attempted, from
    /// its `capture_pending_beacon`, and keeps alternatives) and the
    /// scheduler re-decodes. The second cycle's clean repetition reads
    /// the true call, which is also what makes the frame parse capture
    /// the first, ambiguous reading.
    fn beacon_events_with_an_alternative(
        ambiguous: &str,
        true_call: &str,
        alt: char,
    ) -> Vec<DecoderEvent> {
        let mut events = Vec::new();
        let mut ts = 0u64;
        for (cycle, call) in [ambiguous, true_call].into_iter().enumerate() {
            for word in ["V", "V", "V"] {
                let (mut w, next) = word_events(1, word, ts);
                events.append(&mut w);
                ts = next;
            }
            let damaged_at = call.chars().count() - 1;
            for (i, c) in call.chars().enumerate() {
                let damaged = cycle == 0 && i == damaged_at;
                events.push(DecoderEvent::CharDecoded {
                    track_id: 1,
                    sample_ts: ts,
                    glyph: Glyph::Char(c),
                    confidence: if damaged { 0.45 } else { 0.95 },
                    alternatives: if damaged {
                        vec![(Glyph::Char(alt), 0.40)]
                    } else {
                        Vec::new()
                    },
                });
                ts += 100;
            }
            events.push(DecoderEvent::word_boundary(1, ts));
            ts += 100;
        }
        events
    }

    /// Codex review, PR #520: a non-allowlisted beacon word with decoder
    /// alternatives is refinement-admitted (`capture_pending_beacon` marks
    /// it attempted), but a refinement may not publish it: a beacon is
    /// judged exactly once at its track's true close. The refinement
    /// updates the captured pending beacon's reading; the normal
    /// close-time validation still decides.
    #[test]
    fn a_refined_beacon_stays_pending_and_resolves_at_its_tracks_close() {
        let mut v = recording(CTY_FIXTURE);
        v.ingest(&DecoderEvent::SpeedUpdate {
            track_id: 1,
            wpm: 22.0,
        });
        assert!(
            run(
                &beacon_events_with_an_alternative("K5ARH", "K5ARW", 'W'),
                &mut v
            )
            .is_empty(),
            "captured pending, never emitted on a live track"
        );
        let (seq, span) = word_named(&v, 1, "K5ARH");
        assert!(
            v.refine_candidates(v.now_ts, 10 * FS as u64)
                .iter()
                .any(|c| c.word_seq == seq),
            "the ambiguous beacon word is a refinement candidate"
        );
        let spots = v.apply_refinement(1, seq, refined("K5ARW", span, "engine:hsmm"), 500_000);
        assert!(
            spots.is_empty(),
            "a refined beacon never emits while its track is active: {spots:?}"
        );
        let pending: Vec<&str> = v.tracks[&1]
            .pending_beacons
            .iter()
            .map(|pb| pb.candidate.as_str())
            .collect();
        assert!(
            pending.iter().all(|c| *c == "K5ARW"),
            "the captured reading carries the refinement: {pending:?}"
        );
        let spots = v.ingest(&DecoderEvent::TrackClosed {
            track_id: 1,
            closure: ClosureKind::SignalEnded,
        });
        assert_eq!(spots.len(), 1, "got {spots:?}");
        assert_eq!(spots[0].callsign, "K5ARW");
        assert_eq!(spots[0].spot_type, SpotType::Beacon);
    }

    /// Codex review, PR #520: the close-time validation a refinement must
    /// not bypass includes the final-speed check -- a track that closes
    /// with an implausibly fast WPM reading (> MAX_PLAUSIBLE_WPM) rejects
    /// its pending beacons, refined or not.
    #[test]
    fn a_refined_beacon_on_a_track_that_closes_too_fast_never_spots() {
        let mut v = recording(CTY_FIXTURE);
        v.ingest(&DecoderEvent::SpeedUpdate {
            track_id: 1,
            wpm: 55.0,
        });
        assert!(run(
            &beacon_events_with_an_alternative("K5ARH", "K5ARW", 'W'),
            &mut v
        )
        .is_empty());
        let (seq, span) = word_named(&v, 1, "K5ARH");
        let spots = v.apply_refinement(1, seq, refined("K5ARW", span, "engine:hsmm"), 500_000);
        assert!(spots.is_empty(), "got {spots:?}");
        let spots = v.ingest(&DecoderEvent::TrackClosed {
            track_id: 1,
            closure: ClosureKind::SignalEnded,
        });
        assert!(spots.is_empty(), "got {spots:?}");
        assert!(v.suppression_counts().pending_beacon_rejected_wpm >= 1);
    }
}

#[cfg(test)]
mod man525_tests {
    use super::nearest_gap_hz;

    fn splitmix64(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    #[test]
    fn nearest_gap_equals_the_fold_bit_for_bit() {
        // MAN-525: the binary-searched gap equals main's fold over every
        // other track, bit for bit, including NaN, +inf, -0.0 and
        // duplicate frequencies (a non-finite own frequency falls back to
        // the fold itself).
        let mut compared = 0usize;
        for seed in 0..200u64 {
            let mut rng = seed ^ 0x5EED_0525;
            let n = 1 + (splitmix64(&mut rng) % 80) as u32;
            let mut tracks: Vec<(u32, f64)> = Vec::new();
            for i in 0..n {
                let id = i * 3 + 1;
                let f = match splitmix64(&mut rng) % 23 {
                    0 => f64::NAN,
                    1 => f64::INFINITY,
                    2 => -0.0,
                    3 if !tracks.is_empty() => {
                        tracks[(splitmix64(&mut rng) % tracks.len() as u64) as usize].1
                    }
                    _ => 14e6 + (splitmix64(&mut rng) % 200_000) as f64 * 0.37,
                };
                tracks.push((id, f));
            }
            let mut sorted: Vec<(f64, u32)> = tracks
                .iter()
                .filter(|(_, f)| f.is_finite())
                .map(|&(id, f)| (f, id))
                .collect();
            sorted.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            for &(id, f) in &tracks {
                let fold = tracks
                    .iter()
                    .filter(|(other, _)| *other != id)
                    .map(|(_, other)| (other - f).abs())
                    .fold(f64::INFINITY, f64::min);
                let gap = nearest_gap_hz(&sorted, id, f).unwrap_or(fold);
                assert_eq!(
                    gap.to_bits(),
                    fold.to_bits(),
                    "seed {seed} track {id} freq {f}: {gap} vs {fold}"
                );
                compared += 1;
            }
        }
        println!("MAN-525 gap differential: {compared} gaps compared");
        assert!(compared > 5_000, "only {compared} gaps compared");
    }
}
