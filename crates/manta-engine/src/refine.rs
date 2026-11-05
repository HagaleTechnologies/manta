//! MAN-208: bounded delayed refinement of ambiguous decodes.
//!
//! A deterministic, budget-bounded scheduler that re-runs a track's
//! PRESERVED decoder inputs for a genuinely ambiguous, promising episode
//! under an alternate frontend/timing configuration, and hands the result
//! to `manta_spot::Validator::apply_refinement`, which attaches it to the
//! SAME evidence spans -- never a new repetition. A rescued spot is
//! timestamped with its true, later, observed RF time (the separate clean
//! repetition), never backdated to look available earlier.
//!
//! Determinism: admission order, budgets and caps are sample/operation
//! counts, never wall clock, so identical input explores an identical
//! hypothesis set on any hardware (docs/DECISIONS/
//! 2026-09-09-decoder-recall-research.md investigation 15).

use manta_decode::decoder::{DecodeConfig, Engine as DecodeEngine, RefineConfig, TrackDecoder};
use manta_decode::events::DecoderEvent;
use manta_spot::{RefineCandidate, Spot, Validator};
use std::collections::{BTreeMap, VecDeque};

/// One preserved decoder input for replay: the `DecoderInputEntry` tuple
/// trimmed to the fields every alternate engine consumes (the MAN-312 hold
/// flag is a live-decode concern, not part of the observation).
pub type ReplayEntry = (f32, f32, Option<f32>, u64, f32, f64);

/// Deterministic per-entry size estimate for the global byte bound: the
/// tuple's own bytes plus a `VecDeque` slot margin. An estimate, not a
/// measurement, so the bound is identical on every platform.
pub const REPLAY_ENTRY_BYTES: usize = 48;

/// The global retention ceiling: 8 MiB of estimated replay entries across
/// every track (~175k entries). The hard ceiling above the per-track
/// lookback FIFOs; exceeded only by pathological track counts, and then
/// evicted oldest-first, deterministically.
pub const REFINE_GLOBAL_BYTES: usize = 8 << 20;

/// Hops of run-up before the candidate's span a replay also receives:
/// the alternate decoder bootstraps its speed tracker from real marks, so
/// a replay starting exactly at the span would lose its first characters
/// to warm-up exactly as a fresh track does. One second (375 hops) is a
/// full speed bootstrap with margin. A fixed hop count, so the hypothesis
/// set stays hardware- and batch-independent.
pub const REPLAY_PREROLL_HOPS: usize = 375;

/// Trims a `DecoderInputEntry` (with the MAN-312 hold flag) to its
/// replay form.
pub(crate) fn replay_entry(e: (f32, f32, Option<f32>, u64, f32, f64, bool)) -> ReplayEntry {
    (e.0, e.1, e.2, e.3, e.4, e.5)
}

/// Per-track rolling retention of decoder inputs, deep enough for the
/// configured lookback window (the 4-10 s experiment range; 8 s at 375
/// hops/s is ~3000 hops -- far deeper than `HopHistory`'s 171 ms lead-in).
/// Broken at input gaps (no hop across a loss is contiguous) and dropped
/// with its track; bounded globally in estimated bytes with deterministic
/// oldest-first eviction.
#[derive(Debug, Default)]
pub struct RefineBuffers {
    per_track: BTreeMap<u32, VecDeque<ReplayEntry>>,
    capacity_hops: usize,
    max_entries: usize,
    /// Entries evicted by the global bound; ARCHITECTURE §8: counted, not
    /// silent.
    evicted_entries: u64,
}

impl RefineBuffers {
    /// Retention for `cfg.lookback_secs` per track, under the global byte
    /// bound. `lookback_secs == 0` (or `enabled == false`) retains
    /// nothing: the caller simply never constructs a populated buffer.
    pub fn new(cfg: &RefineConfig) -> Self {
        RefineBuffers {
            per_track: BTreeMap::new(),
            capacity_hops: cfg.lookback_hops(),
            max_entries: REFINE_GLOBAL_BYTES / REPLAY_ENTRY_BYTES,
            evicted_entries: 0,
        }
    }

    /// Records one decoder input for `track_id`. FIFO at capacity: the
    /// oldest hop of THIS track leaves first.
    pub fn record(&mut self, track_id: u32, entry: ReplayEntry) {
        if self.capacity_hops == 0 {
            return;
        }
        let ring = self.per_track.entry(track_id).or_default();
        ring.push_back(entry);
        while ring.len() > self.capacity_hops {
            ring.pop_front();
        }
        self.enforce_budget();
    }

    /// Enforces the global entry bound: evicts the oldest `(track, hop)`
    /// -- the smallest front `sample_ts`, ties broken by smallest
    /// `track_id` -- never by wall clock or iteration accident.
    fn enforce_budget(&mut self) {
        let mut total: usize = self.per_track.values().map(|r| r.len()).sum();
        while total > self.max_entries {
            let evict = self
                .per_track
                .iter()
                .filter_map(|(id, ring)| ring.front().map(|e| (e.3, *id)))
                .min();
            let Some((_, id)) = evict else { break };
            if let Some(ring) = self.per_track.get_mut(&id) {
                ring.pop_front();
                self.evicted_entries += 1;
            }
            total -= 1;
        }
    }

    /// An unknown-length input gap: nothing recorded before it may be
    /// replayed as contiguous with what follows, for ANY track.
    pub fn break_at_input_gap(&mut self) {
        for ring in self.per_track.values_mut() {
            ring.clear();
        }
    }

    /// Drops a closed/evicted track's retention.
    pub fn drop_track(&mut self, track_id: u32) {
        self.per_track.remove(&track_id);
    }

    /// The preserved inputs covering `span` -- the span's own entries plus
    /// [`REPLAY_PREROLL_HOPS`] of run-up before it, oldest first -- or
    /// `None` when the span itself is not fully retained (evicted,
    /// gap-broken, or never recorded): a buffer miss, never a partial
    /// replay.
    pub fn replay_slice(&self, track_id: u32, span: (u64, u64)) -> Option<Vec<ReplayEntry>> {
        let ring = self.per_track.get(&track_id)?;
        let (start, end) = (span.0.min(span.1), span.1);
        if ring.back().is_none_or(|e| e.3 < end) || ring.front().is_some_and(|e| e.3 > start) {
            return None;
        }
        let first_in_span = ring.partition_point(|e| e.3 < start);
        let from = first_in_span.saturating_sub(REPLAY_PREROLL_HOPS);
        Some(
            ring.iter()
                .skip(from)
                .take_while(|e| e.3 <= end)
                .cloned()
                .collect(),
        )
    }

    /// Retention with a smaller global entry bound than production's
    /// byte-derived one -- the bounded-overload tests' deterministic
    /// eviction fixture.
    pub fn with_entry_budget(mut self, max_entries: usize) -> Self {
        self.max_entries = max_entries;
        self
    }

    /// Total retained entries across tracks.
    pub fn len(&self) -> usize {
        self.per_track.values().map(|r| r.len()).sum()
    }

    /// See `len`.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Entries evicted by the global bound so far.
    pub fn evicted_entries(&self) -> u64 {
        self.evicted_entries
    }
}

/// MAN-208 admission triggers. Each is a pure predicate over sample
/// counts; none reads a clock.
///
/// Ambiguity: the occurrence was withheld with a better-supported rival,
/// or the decoder left per-character alternatives it never resolved.
pub fn ambiguity_trigger(c: &RefineCandidate) -> bool {
    c.rival.is_some() || c.alternatives > 0
}

/// Speed: a new reliable estimate at least 3 WPM from the speed the
/// episode was decoded at (the decision record's materiality bar).
pub fn speed_trigger(new_wpm: f32, episode_wpm: f32) -> bool {
    (new_wpm - episode_wpm).abs() >= 3.0
}

/// Separability: a close pair on the channel separated -- the gap moved
/// from inside one channel (`SAME_SIGNAL_MAX_HZ`, mirroring
/// `manta_spot::gate`) to at least one channel apart while an ambiguous
/// episode is still retained.
pub fn separability_trigger(prev_gap_hz: f64, now_gap_hz: f64) -> bool {
    const SAME_SIGNAL_MAX_HZ: f64 = 93.75;
    prev_gap_hz < SAME_SIGNAL_MAX_HZ && now_gap_hz >= SAME_SIGNAL_MAX_HZ
}

/// Cumulative scheduler activity, for `/metrics` and the decision record
/// (five families: admitted, replayed hops, rescues, budget exhaustion,
/// losses). Static labels only, matching the MAN-207 counter style.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RefineCounters {
    pub admitted_total: u64,
    pub replayed_hops_total: u64,
    pub rescues_total: u64,
    pub budget_exhausted_total: u64,
    pub backlog_overflow_total: u64,
    /// Admitted episodes whose preserved inputs were no longer retained.
    pub buffer_miss_total: u64,
    /// Replays that produced no word over the candidate's span.
    pub no_reading_total: u64,
    /// Refinements whose occurrence was no longer retained (the
    /// validator's orphaned-revision counter, mirrored here).
    pub orphaned_revisions_total: u64,
}

/// The deterministic, budget-bounded refinement scheduler.
pub struct RefineScheduler {
    cfg: RefineConfig,
    fs: f64,
    /// `(track_id, word_seq)` -> refinements run so far; capped at
    /// `cfg.max_refinements_per_episode`.
    per_episode_counts: BTreeMap<(u32, u64), u32>,
    /// Admitted, unrefined episodes in stable `(track_id, word_seq)`
    /// order. Overflow drops the oldest first and counts.
    queue: VecDeque<RefineCandidate>,
    counters: RefineCounters,
}

impl RefineScheduler {
    pub fn new(decode_cfg: &DecodeConfig, fs: f64) -> Self {
        RefineScheduler {
            cfg: decode_cfg.refine.clone(),
            fs,
            per_episode_counts: BTreeMap::new(),
            queue: VecDeque::new(),
            counters: RefineCounters::default(),
        }
    }

    /// The scheduler's counters.
    pub fn counters(&self) -> &RefineCounters {
        &self.counters
    }

    /// One scheduler pass, after a batch's decoder events reached the
    /// validator: pulls refinement candidates, admits triggered episodes
    /// under the backlog cap, then replays the head of the queue through a
    /// scratch `TrackDecoder` under the alternate configurations, inside
    /// the per-batch hop budget. Every rescue is returned exactly as the
    /// normal spot path produces it; `now_ts` (the stream position at the
    /// replay) is the decision's availability time, so nothing is
    /// backdated.
    pub fn step(
        &mut self,
        buffers: &mut RefineBuffers,
        validator: &mut Validator,
        decode_cfg: &DecodeConfig,
        now_ts: u64,
    ) -> Vec<Spot> {
        if !self.cfg.enabled {
            return Vec::new();
        }
        let lookback_samples = (self.cfg.lookback_secs * self.fs) as u64;
        // Stable order: `refine_candidates` iterates tracks and words in
        // key order, so admission order is `(track_id, word_seq)`.
        for candidate in validator.refine_candidates(now_ts, lookback_samples) {
            let key = (candidate.track_id, candidate.word_seq);
            // A fresh episode needs a genuine ambiguity signal; an episode
            // already refined once stays on the hypothesis ladder until
            // the per-episode cap, because a refinement that left it
            // unresolved is itself the reason to try the next
            // configuration.
            let already_refined = self.per_episode_counts.contains_key(&key);
            if !already_refined && !ambiguity_trigger(&candidate) {
                continue;
            }
            if self
                .per_episode_counts
                .get(&key)
                .is_some_and(|n| *n >= self.cfg.max_refinements_per_episode)
            {
                continue;
            }
            if self.queue.iter().any(|c| (c.track_id, c.word_seq) == key) {
                continue;
            }
            self.queue.push_back(candidate);
            self.counters.admitted_total += 1;
            while self.queue.len() > self.cfg.backlog {
                // Deterministic overflow: the OLDEST admission drops.
                self.queue.pop_front();
                self.counters.backlog_overflow_total += 1;
            }
        }
        let mut spots = Vec::new();
        let mut budget = self.cfg.replay_hops_per_batch;
        while let Some(candidate) = self.queue.front().cloned() {
            let entries = match buffers.replay_slice(candidate.track_id, candidate.span) {
                Some(entries) => entries,
                None => {
                    self.queue.pop_front();
                    self.counters.buffer_miss_total += 1;
                    self.per_episode_counts.insert(
                        (candidate.track_id, candidate.word_seq),
                        self.cfg.max_refinements_per_episode,
                    );
                    continue;
                }
            };
            let hops = entries.len() as u32;
            if hops > budget {
                // Deferred to a later batch, not raced against the clock.
                self.counters.budget_exhausted_total += 1;
                break;
            }
            budget -= hops;
            self.counters.replayed_hops_total += hops as u64;
            let refined = self.replay(decode_cfg, &candidate, &entries);
            self.queue.pop_front();
            let key = (candidate.track_id, candidate.word_seq);
            let count = self.per_episode_counts.entry(key).or_insert(0);
            *count += 1;
            let Some(refined) = refined else {
                self.counters.no_reading_total += 1;
                continue;
            };
            let rescued =
                validator.apply_refinement(candidate.track_id, candidate.word_seq, refined, now_ts);
            self.counters.rescues_total += rescued.len() as u64;
            self.counters.orphaned_revisions_total +=
                validator.observations().losses().orphaned_revisions;
            spots.extend(rescued);
        }
        spots
    }

    /// Replays one episode's preserved inputs under the next alternate
    /// configuration for it: first the engine flip (`legacy <-> hsmm`),
    /// then the same engine with the triggering speed estimate seeded
    /// (`TrackDecoder::set_initial_wpm`), alternating for further
    /// refinements. Deterministic: the configuration follows from the
    /// episode's refinement count and the base config, nothing else.
    fn replay(
        &self,
        decode_cfg: &DecodeConfig,
        candidate: &RefineCandidate,
        entries: &[ReplayEntry],
    ) -> Option<manta_spot::RefinedWord> {
        let n = self
            .per_episode_counts
            .get(&(candidate.track_id, candidate.word_seq))
            .copied()
            .unwrap_or(0);
        let mut alt_cfg = decode_cfg.clone();
        let mut seed_wpm = None;
        let hypothesis: &'static str;
        if n % 2 == 0 {
            alt_cfg.engine = match decode_cfg.engine {
                DecodeEngine::Legacy | DecodeEngine::EdgeLegacy => DecodeEngine::Hsmm,
                DecodeEngine::Hsmm => DecodeEngine::Legacy,
            };
            hypothesis = match alt_cfg.engine {
                DecodeEngine::Hsmm => "engine:hsmm",
                DecodeEngine::EdgeLegacy => "engine:edge-legacy",
                DecodeEngine::Legacy => "engine:legacy",
            };
        } else {
            // The timing variant: the episode's own decoded speed seeded
            // before the first hop, so the replay's timing hypotheses
            // start from a different (deterministic) scale than the
            // bootstrap the causal run used.
            seed_wpm = Some(candidate.wpm);
            hypothesis = "wpm-seed";
        }
        let mut decoder = TrackDecoder::new(candidate.track_id, alt_cfg);
        if let Some(wpm) = seed_wpm {
            decoder.set_initial_wpm(wpm);
        }
        let mut events = Vec::new();
        for (amp, power, spectral_ref, ts, _, _) in entries {
            events.extend(decoder.push_hop(*amp, *power, *spectral_ref, *ts));
        }
        events.extend(decoder.finish());
        // Map the replay's words back to occurrences by span intersection:
        // a word whose first-character..boundary ts range touches the
        // candidate's span re-reads that occurrence.
        let mut text = String::new();
        let mut confidences = Vec::new();
        let mut first_ts: Option<u64> = None;
        for event in events {
            match event {
                DecoderEvent::CharDecoded {
                    sample_ts,
                    glyph: manta_decode::tree::Glyph::Char(c),
                    confidence,
                    ..
                } => {
                    if first_ts.is_none() {
                        first_ts = Some(sample_ts);
                    }
                    text.push(c.to_ascii_uppercase());
                    confidences.push(confidence);
                }
                DecoderEvent::WordBoundary { sample_ts, .. } => {
                    if let Some(start) = first_ts.take() {
                        if overlaps((start, sample_ts), candidate.span) && !text.is_empty() {
                            let word_confidence = if confidences.is_empty() {
                                0.0
                            } else {
                                confidences
                                    .iter()
                                    .product::<f32>()
                                    .powf(1.0 / confidences.len() as f32)
                            };
                            return Some(manta_spot::RefinedWord {
                                call: text,
                                confidences,
                                span: candidate.span,
                                word_confidence,
                                hypothesis,
                            });
                        }
                        text.clear();
                        confidences.clear();
                    }
                }
                _ => {}
            }
        }
        None
    }
}

/// Two spans share at least one sample position.
fn overlaps(a: (u64, u64), b: (u64, u64)) -> bool {
    a.0.min(a.1) <= b.1 && b.0.min(b.1) <= a.1
}
