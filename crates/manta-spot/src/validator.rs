//! Ties `grammar`/`context`/`cty`/`scp`/`confidence`/`gate`/`dedupe`
//! together into one `Validator::ingest` entry point. ARCHITECTURE §6.

use crate::blocklist::Blocklist;
use crate::confidence;
use crate::context::{self, SpotType};
use crate::cty;
use crate::dedupe::Dedupe;
use crate::gate::RepetitionGate;
use crate::grammar;
use crate::notch::NotchList;
use crate::scp;
use manta_decode::events::{ClosureKind, DecoderEvent};
use manta_decode::tree::{Glyph, Prosign};
use std::collections::{BTreeMap, VecDeque};

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
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Spot {
    pub callsign: String,
    pub freq_hz: f64,
    pub snr_db: f32,
    pub wpm: f32,
    pub spot_type: SpotType,
    pub confidence: f32,
    pub track_id: u32,
    pub sample_ts: u64,
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

/// Per-reason counts of spots suppressed by an operator override (MAN-31).
/// ARCHITECTURE §8: "Every dropped/evicted/suppressed item is counted. No
/// silent loss anywhere in the pipeline." Exposed via
/// `Validator::suppression_counts` for the future M3 metrics endpoint to
/// read, mirroring `manta_engine::track::CloseCounts` -- nothing wires it
/// externally yet, since the Prometheus text endpoint itself is explicit
/// M3 scope (ROADMAP.md).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
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
}

pub struct Validator {
    cty: cty::Table,
    scp: Option<scp::Set>,
    tracks: BTreeMap<u32, TrackState>,
    gate: RepetitionGate,
    dedupe: Dedupe,
    freq_calibration: f64,
    allowlist: std::collections::BTreeSet<String>,
    blocklist: Blocklist,
    notch: NotchList,
    suppression_counts: SuppressionCounts,
}

impl Validator {
    pub fn new(fs: f64, cty_dat: &str, master_scp: Option<&str>) -> Self {
        Self {
            cty: cty::Table::parse(cty_dat),
            scp: master_scp.map(scp::Set::parse),
            tracks: BTreeMap::new(),
            gate: RepetitionGate::new(fs),
            dedupe: Dedupe::new(fs),
            freq_calibration: 1.0,
            allowlist: std::collections::BTreeSet::new(),
            blocklist: Blocklist::default(),
            notch: NotchList::default(),
            suppression_counts: SuppressionCounts::default(),
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

    /// Per-reason counts of operator-suppressed spots so far (MAN-31,
    /// ARCHITECTURE §8).
    pub fn suppression_counts(&self) -> SuppressionCounts {
        self.suppression_counts
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
                ..
            } => {
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
            } => {
                let track = self.tracks.entry(*track_id).or_default();
                if !track.current.text.is_empty() {
                    let mut word = std::mem::take(&mut track.current);
                    word.seq = track.next_word_seq;
                    track.next_word_seq += 1;
                    track.words.push_back(word);
                    if track.words.len() > WORD_WINDOW {
                        track.words.pop_front();
                    }
                }
                track.last_sample_ts = *sample_ts;
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
            DecoderEvent::TrackPromoted { .. } => Vec::new(),
            DecoderEvent::TrackClosed { track_id, closure } => {
                // Round 7 redesign: `SignalEnded` is the ONE point where
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
                    ClosureKind::SignalEnded => self.resolve_pending_beacons(*track_id),
                    ClosureKind::Bookkeeping { survivor_track_id } => {
                        self.migrate_or_discard_pending_beacons(*track_id, *survivor_track_id);
                        Vec::new()
                    }
                };
                // MAN-19: without this, `self.tracks` and `self.gate`'s
                // per-track_id state both grow forever -- `TrackManager`
                // never reuses a `track_id`, and until `TrackClosed`
                // existed neither structure had any signal that one would
                // never be seen again. Confirmed as the soak's actual
                // unbounded-RSS-growth root cause under sustained track
                // churn.
                self.tracks.remove(track_id);
                self.gate.forget_track(*track_id);
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
        exact_seq: u64,
        involved_max_seq: u64,
    ) {
        let first_suppression = {
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
            first_suppression
        };
        if first_suppression {
            self.suppression_counts.power_step_guard += 1;
        }
    }

    fn try_spot(&mut self, track_id: u32, sample_ts: u64) -> Vec<Spot> {
        // No real TrackMeta yet -- freq_hz/snr_db still hold bogus 0.0
        // defaults. Bail without marking anything attempted, so pending
        // candidates are simply re-evaluated once metadata does arrive.
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
        for (_call, involved_max_seq, exact_seq) in suppressed {
            // Identity resolved via exact_seq, not text -- see burn's own docs.
            self.burn_suppressed_power_step_candidate(track_id, exact_seq, involved_max_seq);
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

        let (char_confidences, reclassifying) = {
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
            (word.confidences.clone(), reclassifying)
        };

        // Operator suppression overrides (MAN-31) -- orthogonal to, and
        // checked ahead of, both the automatic validation pipeline and the
        // MAN-28 allowlist below: an explicit blocklist/notch entry is the
        // operator's more specific, deliberate override and must not be
        // silently defeated by a broader allowlist entry. Each hit is
        // counted (ARCHITECTURE §8) so it reads as a deliberate
        // suppression, not silent coverage loss.
        if self.blocklist.contains(&candidate) {
            self.suppression_counts.blocklist += 1;
            return None;
        }
        if self.notch.contains(freq_hz) {
            self.suppression_counts.notch += 1;
            return None;
        }

        // MAN-28 Watch List: an allowlisted callsign bypasses grammar/cty
        // validation and the repetition gate below entirely.
        let is_allowlisted = self.allowlist.contains(&candidate);

        if !is_allowlisted {
            if !grammar::is_plausible(&candidate) {
                return None;
            }
            if !self.cty.is_allocated(&candidate) {
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
            self.gate.record(track_id, &candidate, sample_ts) as u32
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
        if !is_allowlisted && spot_type != SpotType::Beacon && reps < 2 {
            return None;
        }
        if !self
            .dedupe
            .should_emit(&candidate, freq_hz, snr_db, spot_type, sample_ts)
        {
            return None;
        }

        Some(Spot {
            callsign: candidate,
            freq_hz,
            snr_db,
            wpm,
            spot_type,
            confidence,
            track_id,
            sample_ts,
        })
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
        let char_confidences = {
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
            word.confidences.clone()
        };

        // Operator suppression overrides (MAN-31), same boundary as the
        // non-Beacon path (ARCHITECTURE §8).
        if self.blocklist.contains(&candidate) {
            self.suppression_counts.blocklist += 1;
            return None;
        }
        if self.notch.contains(freq_hz) {
            self.suppression_counts.notch += 1;
            return None;
        }
        if !grammar::is_plausible(&candidate) {
            return None;
        }
        if !self.cty.is_allocated(&candidate) {
            return None;
        }

        let pending = &mut self.tracks.get_mut(&track_id)?.pending_beacons;
        if pending.len() >= MAX_PENDING_BEACONS {
            // Drop the oldest to bound growth on a track that stays alive
            // indefinitely (round 8) -- see MAX_PENDING_BEACONS's doc.
            // Counted (ARCHITECTURE §8 -- round 9): this candidate's own
            // source word was already marked attempted and won't be
            // recaptured, so this is a real, permanent loss.
            pending.remove(0);
            self.suppression_counts.pending_beacon_overflow += 1;
        }
        pending.push(PendingBeacon {
            candidate,
            sample_ts,
            freq_hz,
            snr_db,
            char_confidences,
        });
        None
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
                for pb in pending {
                    if survivor_track.pending_beacons.len() >= MAX_PENDING_BEACONS {
                        survivor_track.pending_beacons.remove(0);
                        self.suppression_counts.pending_beacon_overflow += 1;
                    }
                    survivor_track.pending_beacons.push(pb);
                }
            }
            None => {
                self.suppression_counts.pending_beacon_lost_to_eviction += pending.len() as u64;
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
        // An unconfirmed `wpm` (still its `0.0` default) is not evidence of
        // a genuine slow speed -- refuse to resolve rather than let a
        // never-measured survivor pass the plausibility check on a
        // placeholder value (round 10).
        if pending.is_empty() || !wpm_confirmed || wpm > MAX_PLAUSIBLE_WPM {
            return Vec::new();
        }
        pending
            .into_iter()
            .filter_map(|pb| {
                let reps = self.gate.record(track_id, &pb.candidate, pb.sample_ts) as u32;
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
                // round 8). Simplest safe rule: an already-superseded
                // observation is not worth emitting at all.
                if let Some(last_ts) = self.dedupe.last_sample_ts(&pb.candidate, pb.freq_hz) {
                    if pb.sample_ts <= last_ts {
                        return None;
                    }
                }
                if !self.dedupe.should_emit(
                    &pb.candidate,
                    pb.freq_hz,
                    pb.snr_db,
                    SpotType::Beacon,
                    pb.sample_ts,
                ) {
                    return None;
                }
                Some(Spot {
                    callsign: pb.candidate,
                    freq_hz: pb.freq_hz,
                    snr_db: pb.snr_db,
                    wpm,
                    spot_type: SpotType::Beacon,
                    confidence,
                    track_id,
                    sample_ts: pb.sample_ts,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f64 = 96_000.0;
    const CTY_FIXTURE: &str = "\
United States:    5:  8: NA:  40.0:  75.0:  5.0:  K:
    K,W,N,AA,AB,AC;
";

    fn word_events(track_id: u32, text: &str, start_ts: u64) -> (Vec<DecoderEvent>, u64) {
        let mut events = Vec::new();
        let mut ts = start_ts;
        for c in text.chars() {
            events.push(DecoderEvent::CharDecoded {
                track_id,
                sample_ts: ts,
                glyph: Glyph::Char(c),
                confidence: 0.95,
            });
            ts += 100;
        }
        events.push(DecoderEvent::WordBoundary {
            track_id,
            sample_ts: ts,
        });
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
            snr_2500_db: 20.0,
            freq_hz: 14_000_000.0,
        });
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
            v.dedupe.last_sample_ts("K5ARH", 14_000_000.0),
            Some(newer_ts),
            "dedupe must have recorded the newer, ordinary spot's timestamp"
        );

        // Track 1 finally closes -- its captured (OLDER) Beacon candidate
        // must not emit now, and must not roll dedupe's watermark back to
        // its own stale timestamp.
        let spots3 = v.ingest(&DecoderEvent::TrackClosed {
            track_id: 1,
            closure: ClosureKind::SignalEnded,
        });
        assert!(
            spots3.is_empty(),
            "an already-superseded deferred candidate must not emit, got {spots3:?}"
        );
        assert_eq!(
            v.dedupe.last_sample_ts("K5ARH", 14_000_000.0),
            Some(newer_ts),
            "resolving the stale deferred candidate must not roll the \
             dedupe watermark backward"
        );
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
            DecoderEvent::CharDecoded {
                track_id: 1,
                sample_ts: 0,
                glyph: Glyph::Char('D'),
                confidence: 0.9,
            },
            DecoderEvent::CharDecoded {
                track_id: 1,
                sample_ts: 100,
                glyph: Glyph::Char('E'),
                confidence: 0.9,
            },
            DecoderEvent::WordBoundary {
                track_id: 1,
                sample_ts: 200,
            },
            DecoderEvent::CharDecoded {
                track_id: 1,
                sample_ts: 300,
                glyph: Glyph::Char('K'),
                confidence: 0.9,
            },
            DecoderEvent::CharDecoded {
                track_id: 1,
                sample_ts: 400,
                glyph: Glyph::Prosign(Prosign::Err),
                confidence: 0.0,
            },
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
    /// through to `self.tracks.remove`/`self.gate.forget_track`, `tracks`
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
}
