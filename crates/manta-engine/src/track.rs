//! Track lifecycle state machine (SPEC §2.4) and adjacent-channel ownership
//! (SPEC §2.5), driving the decoder pool. This task covers only the
//! single-channel FSM in isolation; `TrackManager` (Task 5) wires it to
//! real per-channel floor/gate state and multi-channel ownership.

/// SPEC §9 `[detector]` table, plus ARCHITECTURE §4's track cap (not in the
/// literal SPEC table -- see the plan's Global Constraints).
#[derive(Debug, Clone, Copy)]
pub struct DetectorConfig {
    /// Rise threshold in dB SNR. **Deviation from SPEC §9's literal 6.0 dB
    /// default (see `impl Default`).**
    pub on_snr_db: f32,
    /// SPEC §9: off (drop) threshold in dB SNR.
    pub off_snr_db: f32,
    /// SPEC §2.3/§2.4: rise sustained this many hops (~50ms) before CANDIDATE -> ACTIVE.
    pub confirm_hops: u64,
    /// SPEC §2.3/§2.4: drop sustained this many hops (5000ms) before ACTIVE/HANG -> CLOSED.
    pub hang_hops: u64,
    /// SPEC §2.4: no character emitted for this many hops (30000ms) -> CLOSED (garbage collect).
    pub gc_hops: u64,
    /// SPEC §2.1: track creation inhibited for this many hops (2000ms) after start.
    pub warmup_hops: u64,
    /// ARCHITECTURE §4 (not SPEC §9): max concurrent ACTIVE tracks.
    pub track_cap: usize,
    /// Not in SPEC §9. MAN-171: a channel whose track just closed
    /// `CloseReason::Silent` (ACTIVE/HANG for `gc_hops` with zero characters
    /// decoded) is barred from spawning a new CANDIDATE for this many hops.
    /// SPEC §2.2's neighborhood-floor clamp (`Track::effective_floor_db`)
    /// deliberately never lets a channel's own quantile raise its
    /// detection threshold by more than 3 dB relative to its neighbors --
    /// correct for a real parked carrier, but it also means a genuinely
    /// stationary, unmodulated interferer (an SDR/USB clock-harmonic
    /// "birdie": confirmed on real RSP1B hardware during MAN-171's
    /// investigation, an fs-independent comb at every exact 8 kHz RF
    /// multiple) keeps satisfying `rise` forever -- `gate.rise[k]` never
    /// clears just because a track already tried and failed to decode it.
    /// Without this cooldown, such a channel spawns, promotes, runs
    /// `gc_hops` with no `CharDecoded`, closes `Silent`, and immediately
    /// re-spawns on the very next hop -- forever, for the life of the
    /// process (measured live: 53% of all `TrackPromoted` events over a
    /// real 90 s RSP1B capture landed within 100 Hz of an exact-8kHz-
    /// multiple channel, each grid point promoting 2-3 times). Silent is
    /// the only `CloseReason` this applies to: `Unconfirmed` never
    /// allocated a decoder at all, `HangExpired`/`Merged`/`Evicted` all
    /// have other, more direct explanations (a real observed RF gap, or
    /// track-manager bookkeeping) that don't imply "this channel is a
    /// standing artifact."
    pub silent_respawn_cooldown_hops: u64,
}

impl Default for DetectorConfig {
    /// **`on_snr_db` deviates from SPEC §9's literal 6.0 dB**, empirically
    /// retuned to 12.0 dB (this repo's "measure, then pin the deviation"
    /// convention -- cf. docs/DECISIONS/2026-07-18-char-gap-threshold-fix.md
    /// and .../2026-07-18-m2-pfb-channelizer-pins.md).
    ///
    /// SPEC §2.3's 6.0 dB / confirm_hops=19 pair was specified against an
    /// implicit per-hop-independent noise model. The real channelizer output
    /// `power[k] = |X[k]|^2` is a single complex-Gaussian magnitude-squared
    /// per hop (SPEC §1.3): chi-squared, 2 DOF, dB-domain std ~5.57 dB raw,
    /// and after the Gate's tau=40 ms EMA still ~1.7 dB std with a ~15-hop
    /// autocorrelation time -- essentially confirm_hops=19 itself. So the
    /// "19 sustained hops" window buys almost no *independent* looks at the
    /// noise (once an EMA excursion crosses the threshold, autocorrelation
    /// holds it there for nearly the whole window). Over V1's real
    /// 1024-channel x 120 s render (~46 M channel-hop opportunities) the
    /// literal 6.0 dB threshold produced 298 spurious ACTIVE tracks for a
    /// single clean +20 dB signal.
    ///
    /// Raising `confirm_hops` instead was measured and rejected: CW elements
    /// are short (a 20 WPM dit is ~22 hops), so a longer sustained-rise
    /// window the noise must clear is also a window the *real* signal
    /// struggles to fill -- confirm_hops in 40..150 gave both worse false-
    /// track counts and worse decode accuracy. Raising `on_snr_db` is the
    /// clean lever: 12.0 dB drives false tracks to 0 across all measured
    /// noise seeds (empirical knee = 11.0 dB; +1 dB margin) while the
    /// channelizer's ~14 dB processing gain (2500 Hz SNR -> 93.75 Hz channel
    /// SNR) keeps even the weakest golden vector (V3, +6 dB-in-2500) ~14 dB
    /// clear of the threshold, so it still promotes and decodes.
    ///
    /// **`confirm_hops` stays at SPEC §2.3/§2.4's literal 19 (~50.7ms) --
    /// deliberately, after measuring a real problem with it and rejecting
    /// the obvious fix.** MAN-166, 2026-09-09, investigated further in
    /// `docs/DECISIONS/2026-09-09-man166-confirm-hops-and-track-cap.md`.
    /// The above paragraph's own "20 WPM dit is ~22 hops" observation
    /// already implies a real problem this never closed: 19 hops needs
    /// 50.7ms of *unbroken* rise from a CANDIDATE's very first hop, and a
    /// dit alone is shorter than that at any real contest speed above
    /// ~20 WPM (30ms at 40 WPM, 34ms at 35 WPM, 40ms at 30 WPM) -- so on
    /// real contest-speed CW, any word/character starting with a
    /// dit-leading letter (E, I, S, H, 5, ...) structurally cannot promote
    /// its own opening element, spawning a fresh CANDIDATE every dit-onset
    /// that immediately dies `Unconfirmed`, over and over, until a
    /// dah-leading element promotes it or the transmission ends.
    /// Confirmed as the majority contributor to `unconfirmed` being 68.8%
    /// of all real-recording track churn (`crates/manta-testkit`'s B2
    /// golden vector) -- but **lowering `confirm_hops` to fix it breaks
    /// V10** (a real, currently-passing golden vector at 15 WPM/Farnsworth
    /// char_wpm=25): the decoder's Farnsworth gap-classifier bootstrap
    /// (`manta_decode::timing::FARNS_MIN_COUNT`) is apparently sensitive
    /// to *which hop* a track promotes on, and there's a hard cliff, not a
    /// gradual tradeoff -- confirm_hops=17 reproduces the exact same
    /// broken decode as confirm_hops=8 (CER 0.2368 either way, letter-by-
    /// letter word-boundary corruption on the opening transmission),
    /// confirm_hops=18 passes clean, and 18 vs 19 (48.0ms vs 50.7ms) is
    /// too close to the original to meaningfully help any real
    /// contest-speed dit (still needs >40 WPM headroom this doesn't give).
    /// `track_cap` was verified independent of this (V10 passes at
    /// `confirm_hops: 19` regardless of `track_cap`). This confirm_hops/
    /// Farnsworth-bootstrap coupling needs a real manta-decode-level fix
    /// (decouple the gap-classifier's bootstrap from detector promotion
    /// timing) before `confirm_hops` can safely move -- out of scope for
    /// this change; folded into the wider real-world decode-accuracy
    /// investigation the same MAN-166 finding opened.
    ///
    /// **`track_cap` deviates from its prior 500** (ARCHITECTURE §4, not a
    /// SPEC value), raised to 1200, same MAN-166 decision doc. 500 was
    /// never stress-tested against real contest-band signal density: on
    /// the B2 recording it was pinned at its ceiling for the *entire* 15
    /// minutes, forcing 71,149 `evicted` closes (cap pressure alone --
    /// `evict_over_cap()` fires only when `tracks.len() > cap`) as real,
    /// confirmed signals were killed purely to make room for competitors.
    /// `merged` (52,330 closes, `merge_converged()`'s independent
    /// frequency-proximity convergence, SPEC §2.5, always run before
    /// `evict_over_cap()` in `step_hop`) is a *separate* mechanism, not
    /// cap pressure -- the decision doc's own uncapped experiment shows
    /// `merged` closes actually *rose* (52,330 -> 56,739) when the cap was
    /// removed, the opposite of what cap pressure would predict (Codex
    /// review, PR #152, round 10 -- corrected an earlier version of this
    /// comment that conflated the two). Removing the cap entirely on that
    /// same recording measured real organic peak demand at 886 concurrent
    /// active tracks; 1200 keeps meaningful headroom above that without
    /// picking an arbitrarily huge number. (Any relationship to manta's
    /// Pi4 CPU-budget gate, MAN-18/MAN-49, is explicitly out of scope for
    /// this change -- see that decision doc.)
    fn default() -> Self {
        DetectorConfig {
            on_snr_db: 12.0,
            off_snr_db: 3.0,
            confirm_hops: 19,
            hang_hops: 1875,
            gc_hops: 11250,
            warmup_hops: 750,
            track_cap: 1200,
            // MAN-171: same order as `gc_hops` itself -- a channel that
            // stayed silent for the whole GC window earns an equally long
            // rest before it's trusted to spawn a fresh CANDIDATE. This
            // roughly halves a persistent artifact's promotion rate
            // (spawn -> gc_hops ACTIVE -> Silent -> cooldown -> repeat)
            // rather than eliminating it -- a channel could, in principle,
            // host a real signal again later, so this is a rate limit, not
            // a permanent denylist.
            silent_respawn_cooldown_hops: 11250,
        }
    }
}

/// SPEC §2.4: track lifecycle state at any given hop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LifecycleState {
    /// Initial state: rise threshold crossed, confirmation in progress.
    Candidate,
    /// Confirmed active: decoder allocated and decoding.
    Active,
    /// Waiting for recovery: drop threshold sustained, hang timer running.
    Hang,
}

/// SPEC §2.4/§2.5: reason for track closure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloseReason {
    /// Rise never confirmed within `confirm_hops` (SPEC §2.4: CANDIDATE -> IDLE).
    Unconfirmed,
    /// Hang timer expired (SPEC §2.4: HANG -> CLOSED).
    HangExpired,
    /// No character emitted for `gc_hops` (SPEC §2.4: garbage collect).
    Silent,
    /// Converged with another track within 1.0 channel; this was the
    /// lower-SNR one (SPEC §2.5).
    Merged,
    /// Track cap exceeded; this was the lowest-current-SNR track
    /// (ARCHITECTURE §4).
    Evicted,
}

/// Per-`CloseReason` close counters (issue #26: SPEC §2.5 / ARCHITECTURE §4,
/// §8 both describe merges and evictions as "counted" -- this is that count.
/// Exposed via `TrackManager::close_counts` for the future M3 metrics
/// endpoint to read; nothing wires it externally yet, since the Prometheus
/// text endpoint itself is explicit M3 scope (ROADMAP.md).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CloseCounts {
    pub unconfirmed: u64,
    pub hang_expired: u64,
    pub silent: u64,
    pub merged: u64,
    pub evicted: u64,
}

impl CloseCounts {
    fn record(&mut self, reason: CloseReason) {
        match reason {
            CloseReason::Unconfirmed => self.unconfirmed += 1,
            CloseReason::HangExpired => self.hang_expired += 1,
            CloseReason::Silent => self.silent += 1,
            CloseReason::Merged => self.merged += 1,
            CloseReason::Evicted => self.evicted += 1,
        }
    }
}

/// SPEC §2.4: event produced by a single-hop state transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LifecycleEvent {
    /// No state change this hop.
    None,
    /// CANDIDATE -> ACTIVE: lease a decoder from the pool.
    Promoted,
    /// Track closed: cease decoding and release resources.
    Closed(CloseReason),
}

/// One channel-slot's SPEC §2.4 state machine, driven one hop at a time.
/// `Lifecycle` itself has no notion of "channel" or "ownership" -- the
/// caller (`TrackManager`, Task 5) decides which channel feeds `on_hop`
/// each cycle.
pub(crate) struct Lifecycle {
    state: LifecycleState,
    confirm_count: u64,
    hang_count: u64,
    silent_count: u64,
    confirm_hops: u64,
    hang_hops: u64,
    gc_hops: u64,
}

impl Lifecycle {
    /// A brand-new CANDIDATE, just born on a rise hop.
    pub(crate) fn new(cfg: &DetectorConfig) -> Self {
        Lifecycle {
            state: LifecycleState::Candidate,
            confirm_count: 1, // this hop's rise already counts as the first
            hang_count: 0,
            silent_count: 0,
            confirm_hops: cfg.confirm_hops,
            hang_hops: cfg.hang_hops,
            gc_hops: cfg.gc_hops,
        }
    }

    /// Query the current lifecycle state.
    // Temporary: no non-test reader yet -- `Track::state` (the only
    // present-day caller of this) is itself unused outside tests until a
    // later task filters/reports tracks by lifecycle state.
    #[allow(dead_code)]
    pub(crate) fn state(&self) -> LifecycleState {
        self.state
    }

    /// SPEC §2.4 GC-timer reset, driven by the *actual* decode result rather
    /// than a per-hop guess. `step_hop` advances the silent/GC counter every
    /// hop with `char_emitted = false`, because the decoder pool runs only
    /// *after* the whole hop batch -- so at hop time it cannot yet know
    /// whether a character was decoded. `TrackManager::process_hops` calls
    /// this once the batch's `CharDecoded` events are in hand, for every
    /// track that emitted one, keeping a continuously-decoding signal's GC
    /// timer from ever expiring. Without it the counter only ever climbs and
    /// every ACTIVE track is force-closed `CloseReason::Silent` after
    /// `gc_hops` (~30 s), fragmenting one continuous signal into a fresh
    /// track every 30 s.
    pub(crate) fn note_char_decoded(&mut self) {
        self.silent_count = 0;
    }

    /// Advance one hop. `rise`/`drop` are this hop's gate booleans for the
    /// channel currently feeding this track (its owned max-power channel,
    /// per SPEC §2.5). `char_emitted` marks whether a character was decoded
    /// this hop (drives the §2.4 GC timer; always pass `false` while
    /// CANDIDATE, since nothing is being decoded yet).
    pub(crate) fn on_hop(&mut self, rise: bool, drop: bool, char_emitted: bool) -> LifecycleEvent {
        match self.state {
            LifecycleState::Candidate => {
                if rise {
                    self.confirm_count += 1;
                    if self.confirm_count >= self.confirm_hops {
                        self.state = LifecycleState::Active;
                        return LifecycleEvent::Promoted;
                    }
                } else {
                    return LifecycleEvent::Closed(CloseReason::Unconfirmed);
                }
                LifecycleEvent::None
            }
            LifecycleState::Active => {
                if char_emitted {
                    self.silent_count = 0;
                } else {
                    self.silent_count += 1;
                    if self.silent_count >= self.gc_hops {
                        return LifecycleEvent::Closed(CloseReason::Silent);
                    }
                }
                if drop {
                    self.state = LifecycleState::Hang;
                    self.hang_count = 1;
                }
                LifecycleEvent::None
            }
            LifecycleState::Hang => {
                if char_emitted {
                    self.silent_count = 0;
                } else {
                    self.silent_count += 1;
                    if self.silent_count >= self.gc_hops {
                        return LifecycleEvent::Closed(CloseReason::Silent);
                    }
                }
                if rise {
                    self.state = LifecycleState::Active;
                    self.hang_count = 0;
                } else {
                    self.hang_count += 1;
                    if self.hang_count >= self.hang_hops {
                        return LifecycleEvent::Closed(CloseReason::HangExpired);
                    }
                }
                LifecycleEvent::None
            }
        }
    }
}

use manta_decode::decoder::{DecodeConfig, Engine, TrackDecoder};
use manta_decode::events::{ClosureKind, DecoderEvent};
use manta_dsp::channelizer::{
    interpolate_offset, odd_channel_sign_correction, power_db, HopOutput,
};
use manta_dsp::floor::{FloorBank, Gate};
use manta_dsp::refine::{Refiner, GROUP_DELAY_HOPS};
use std::collections::BTreeMap;

/// One tracked signal. Owns channels `{round(center)-1, round(center),
/// round(center)+1}` per SPEC §2.5; `center` is a live, per-hop EMA of the
/// fine-frequency-interpolated channel position (fast enough to follow
/// realistic drift, e.g. SPEC §7 V9's 50 Hz/min). `Track::freq_hz` also
/// reports this same EMA converted to absolute Hz (Task 9 remediation --
/// see that function's doc comment for why this deviates from SPEC §1.4's
/// literal lifetime-average formula).
pub(crate) struct Track {
    /// This track's stable identity (SPEC §5/§6), assigned once at birth by
    /// `TrackManager::spawn` and carried into every `DecoderEvent` it emits.
    // Temporary: no non-test reader of the field itself yet -- callers
    // identify a track by its `BTreeMap` key, not this field, until a later
    // task needs the id independent of that map.
    #[allow(dead_code)]
    pub(crate) id: u32,
    lifecycle: Lifecycle,
    /// Live, per-hop EMA of the fine-frequency channel position (see the
    /// struct doc above); drives `owned`/`select_channel` ownership.
    pub(crate) center: f64,
    /// This hop's SNR estimate for the track's selected channel (SPEC
    /// §2.5), used by `merge_converged`/`evict_over_cap` tie-breaks.
    pub(crate) current_snr_db: f32,
    /// MAN-102: running maximum of `current_snr_db` since this track's
    /// last `TrackMeta`, maintained by `drain_pool` (not `step_hop` --
    /// see `pending`'s doc comment: the reset must happen at the exact hop
    /// `TrackMeta` is emitted, which `step_hop` cannot know in advance).
    /// `TrackMeta` fires once per 375 hops at an arbitrary phase of the
    /// keying, and `current_snr_db` is a 40 ms EMA that decays to the floor
    /// on every key-up -- sampling it instantaneously swings ~35 dB between
    /// key-down and key-up (measured; see the MAN-102 decision record). The
    /// peak over the interval is the settled key-down level, which is what
    /// RBN/CW Skimmer report.
    pub(crate) snr_peak_db: f32,
    /// The channel index this track first spawned on (SPEC §2.1); the
    /// anchor for `Track::freq_hz`'s absolute-Hz conversion.
    pub(crate) birth_channel: usize,
    /// The track's leased decoder, allocated on CANDIDATE -> ACTIVE
    /// promotion (SPEC §2.4/§5); `None` before promotion.
    decoder: Option<TrackDecoder>,
    /// `(amplitude, raw_power, spectral_ref_power, sample_ts,
    /// current_snr_db_at_that_hop)` queued this hop-batch by `step_hop`,
    /// drained once per `process_hops` call by `drain_pool` (ARCHITECTURE
    /// §10's decoder pool). `raw_power` is kept separate from `amplitude`
    /// (rather than derived as `amplitude * amplitude`) so the noise
    /// tracker always sees the tracked channel's real, full-bandwidth power
    /// even when `amplitude` is a narrowband-refined value (Codex review,
    /// PR #178) -- see `decoder_input`'s doc comment. `spectral_ref_power`
    /// (SPEC v2 §2.2's min-of-six-neighbors reference, linear) is computed
    /// by `step_hop` from `FloorBank`, not `decoder_input` (a `Track`
    /// method with no access to the floor bank, a `TrackManager` field) --
    /// always `Some` since `FloorBank` has a real value for every channel
    /// from construction onward (Codex review, PR #178: this was
    /// hard-coded `None` before, so `NoiseTracker`'s spectral-discounting
    /// branch never activated). The *raw* per-hop SNR (MAN-102) is queued,
    /// not a pre-computed peak: `drain_pool` walks a track's items in
    /// order, maintaining `snr_peak_db` and resetting it the instant that
    /// item's decoder push crosses a SPEC §5 reporting boundary
    /// (`hop_count() % META_INTERVAL_HOPS == 0`), so the window boundary
    /// lines up with the real report regardless of where a `process_hops`
    /// batch happens to end, and two boundaries inside one
    /// batch each get their own reset. (MAN-102 review round 1, findings
    /// 2/3: an earlier version paired a pre-accumulated peak here and
    /// reset it once per `drain_pool` call at batch end, which made the
    /// reported value depend on the caller's chunk size -- `decode_samples`
    /// uses 4096-sample chunks, `listen`/`soak_metrics` use their own --
    /// contradicting SPEC §6's determinism rule. Review round 2, finding 2:
    /// resetting only when `TrackMeta` actually emits left the window
    /// unbounded while `!Demod::running()`, so the reset now keys off the
    /// boundary itself, which fires every interval regardless.)
    pending: Vec<(f32, f32, Option<f32>, u64, f32)>,
    /// Set by `process_hops` once `drain_pool` has actually produced a
    /// `DecoderEvent` for this track. Distinct from `decoder.is_some()`:
    /// a track promoted and then merged/evicted within the *same*
    /// `process_hops` batch never gets a `drain_pool` pass before it's
    /// removed (that only runs once, after every hop in the batch), so it
    /// can have an allocated decoder yet have emitted nothing at all
    /// (MAN-19 review round 1). `TrackClosed` emission checks this, not
    /// decoder presence.
    has_emitted: bool,
    /// MAN-168 (SPEC v2 §3): this track's narrowband amplitude refiner,
    /// lazily constructed the first time `decoder_input` runs with
    /// `refine_bw_hz > 0.0`. `None` when refinement is disabled (the
    /// default) or not yet constructed.
    refiner: Option<Refiner>,
    /// The integer owned channel `refiner` was last fed from. A change
    /// here -- NOT a fractional `center` EMA nudge -- is what resets the
    /// refiner's FIR history/phase accumulator (SPEC v2 §3: "a change of
    /// c resets the FIR history"). Using the integer channel as the reset
    /// trigger, rather than every `center` update, keeps the reset-caused
    /// ramp-in transient (the FIR needs `GROUP_DELAY_HOPS * 2 + 1` hops to
    /// refill) rare -- it only fires on a real max-power-channel switch,
    /// not on continuous sub-channel drift.
    refiner_channel: Option<usize>,
    /// Real (non-zero-padded) hops fed to `refiner` since its last reset:
    /// `decoder_input` reports the raw (unrefined) magnitude, not
    /// `refiner`'s own output, until this reaches `TAPS` (the FIR's full
    /// window is real) -- see `decoder_input`'s doc comment for why a
    /// GROUP_DELAY_HOPS-delayed *pairing* on top of that convergence gate
    /// was tried and reverted (Codex review, PR #178, three consecutive
    /// rounds surfacing a new duplication/fabrication shape each time; see
    /// MAN-194). Reset to 0 alongside `refiner.reset()`.
    refiner_hops_since_reset: u64,
}

/// SPEC §1.1 f(k) mapping: signed channel offset from center, FFT bin order.
fn wrapped_channel_offset(k: usize, n_channels: usize) -> f64 {
    let half = n_channels as i64 / 2;
    let signed = ((k as i64 + half).rem_euclid(n_channels as i64)) - half;
    signed as f64
}

/// SPEC §2.5: a live centroid EMA responsive enough to follow realistic
/// drift (V9: 50 Hz/min == ~0.0089 channels/s at 93.75 Hz spacing) with
/// negligible lag relative to the 375 Hz hop rate; tuned empirically
/// against V9 in Task 9 and pinned there.
const CENTER_EMA_ALPHA: f64 = 0.01;

impl Track {
    /// A brand-new track: `Lifecycle` seeded fresh, `center` initialized to
    /// its birth channel, decoder unleased. SPEC §2.4/§2.5.
    fn new(id: u32, birth_channel: usize, cfg: &DetectorConfig) -> Self {
        Track {
            id,
            lifecycle: Lifecycle::new(cfg),
            center: birth_channel as f64,
            current_snr_db: 0.0,
            // Deliberately below any real SNR reading (rather than 0.0) so
            // `drain_pool`'s `.max()` against the first queued item is a
            // no-op clamp, not an accidental floor -- a genuinely negative
            // first reading (a weak signal just past the gate) must survive.
            snr_peak_db: f32::NEG_INFINITY,
            birth_channel,
            decoder: None,
            pending: Vec::new(),
            has_emitted: false,
            refiner: None,
            refiner_channel: None,
            refiner_hops_since_reset: 0,
        }
    }

    /// The amplitude to feed this hop's decoder push, the RAW (unrefined)
    /// power of the detector's owned channel `k` for the noise tracker,
    /// and the `sample_ts` the amplitude corresponds to (MAN-168, SPEC v2
    /// §3). Falls back to `(raw_amp, raw_power, sample_ts)` from the same
    /// channel when refinement is disabled (`refine_bw_hz <= 0.0`, the
    /// default) -- so `raw_power` always matches SPEC v2 §2.1's "tracked
    /// channel" regardless of whether refinement is active, which is the
    /// point: `raw_power` must NOT come from the narrowband-filtered
    /// amplitude. `TrackDecoder::push_hop`'s calibrated `NoiseTracker`
    /// expects the full-bandwidth channel's power; the refiner's 30 Hz
    /// (vs the channel's ~93.75 Hz) passband integrates substantially
    /// less noise power by design (that's what makes the SIGNAL amplitude
    /// more accurate) -- Codex review measured the refined amplitude's
    /// squared power reading ~8.6 dB low relative to `hop.power[k]`, and
    /// `push_envelope` (the old, amplitude-only call this replaced)
    /// reconstructed power as `amp * amp`, silently feeding that skewed,
    /// temporally-correlated series into noise calibration whenever
    /// refinement was enabled.
    ///
    /// Reports each hop's OWN `sample_ts` -- NOT a `GROUP_DELAY_HOPS`
    /// -delayed pairing with `refiner`'s output, despite `refine.rs`'s
    /// original doc comment describing that as the intended design.
    /// Three consecutive rounds of Codex review on PR #178 each found a
    /// new bug in an attempted delayed-pairing scheme (a duplicate/
    /// regressed timestamp; a duplicated single amplitude replayed across
    /// several hops; then the SAME warm-up observations reported a SECOND
    /// time once the delay ring filled) -- and it's provable, not just
    /// hard to get right: with an interface that must emit exactly one
    /// `(amp, raw_power, ts)` triple per input hop (this codebase has
    /// already learned the hard way that silently skipping a hop shortens
    /// the following space run and collapses character boundaries), a
    /// stream that starts at zero delay can never transition to
    /// `GROUP_DELAY_HOPS` hops of delay later without either skipping
    /// `GROUP_DELAY_HOPS` hops (to let the delayed index fall behind) or
    /// reporting some hop's observation twice. Neither is acceptable, so
    /// this reports amplitude with NO delay compensation instead: a
    /// constant, `GROUP_DELAY_HOPS`-hop (~13ms) systematic timestamp
    /// offset from true wall-clock time, which does not affect the
    /// RELATIVE durations (dit/dah/space ratios) `HsmmDecoder`/`Demod`
    /// actually decode from, unlike either alternative. See MAN-194 for
    /// the real fix: a burst-capable `decoder_input` (returning
    /// `Vec<(f32, f32, u64)>`) can emit zero or several triples for one
    /// input hop, which is what a correct drain/prime protocol actually
    /// requires.
    fn decoder_input(
        &mut self,
        k: usize,
        hop: &HopOutput,
        sample_ts: u64,
        refine_bw_hz: f32,
    ) -> (f32, f32, u64) {
        let raw_power = hop.power[k];
        if refine_bw_hz <= 0.0 {
            return (raw_power.sqrt(), raw_power, sample_ts);
        }
        // SPEC v2 §3 requires `c = round(c_f)`: refine the CENTROID
        // channel, not `k` (the instantaneous max-power channel used for
        // detector ownership/gating elsewhere). `k` can flicker on noise
        // or a competing near-edge channel even while the true centroid
        // stays put; each flicker would otherwise reset the FIR and
        // collapse its output to a single tap (Codex review, PR #161's
        // MAN-168 wiring).
        let c = self.center.round() as usize;
        let delta = (self.center - c as f64) as f32;
        let refiner = self
            .refiner
            .get_or_insert_with(|| Refiner::new(refine_bw_hz, delta));
        if self.refiner_channel != Some(c) {
            refiner.reset();
            self.refiner_channel = Some(c);
            self.refiner_hops_since_reset = 0;
        }
        refiner.set_delta(delta);
        // The channelizer's rotation step leaves a checkerboard sign
        // artifact on odd channels at odd hops (see
        // `channelizer::odd_channel_sign_correction`'s doc comment); a
        // phase-sensitive consumer like this derotating/filtering refiner
        // must correct it before the sample reaches `Refiner::push`, or an
        // odd-channel track's refined amplitude self-cancels (measured
        // ~39 dB of spurious attenuation before this correction was
        // applied here). Always feed the refiner, even during warm-up
        // below -- its FIR history and phase accumulator need every hop
        // to converge, regardless of whether THIS hop's output is used.
        let corrected = hop.x[c] * odd_channel_sign_correction(hop.m, c);
        let refined_amp = refiner.push(corrected);
        self.refiner_hops_since_reset += 1;

        // `TAPS == 2*GROUP_DELAY_HOPS + 1` for this odd-length symmetric
        // FIR: right after a reset, its 11-tap window is still partly
        // zero-padded, and `refiner.push`'s output is a ramp-in transient,
        // not a real observation -- measured ~40% below steady-state
        // amplitude on a constant tone partway through convergence, a dip
        // that can flip a legacy/HSMM decision, and (before that) SPEC v2
        // §3 requires reset loss to report neutral evidence rather than
        // the transient's spurious negative-LLR swing. Report the raw
        // (unrefined) magnitude -- exactly what a disabled refiner would
        // report, already-trusted real evidence -- until the FULL window
        // is real.
        let full_history_hops = 2 * GROUP_DELAY_HOPS as u64 + 1; // == TAPS
        let amp = if self.refiner_hops_since_reset >= full_history_hops {
            refined_amp
        } else {
            hop.power[c].sqrt()
        };
        (amp, raw_power, sample_ts)
    }

    /// Drain this track's queued `pending` samples through its decoder,
    /// then call `finish()` -- used for a genuine end-of-signal closure
    /// (HangExpired/Silent: a sustained real gap has already been
    /// observed), before its `pending` queue would otherwise be silently
    /// discarded along with the rest of the removed `Track`. Without the
    /// drain, `finish()`'s "true final speed" could be stale by up to a
    /// full batch's worth of already-queued samples (Codex review on
    /// PR #154, round 6) -- exactly the state a held-back `manta-spot`
    /// Beacon-WPM candidate needs at `TrackClosed` time. No-op (empty
    /// `Vec`) if this track never had a decoder. See `finish_decoder_speed_only`
    /// for Merged/Evicted, where forcing finalization would fabricate
    /// content instead (round 7).
    fn finish_decoder(&mut self) -> Vec<DecoderEvent> {
        let Some(decoder) = self.decoder.as_mut() else {
            return Vec::new();
        };
        let pending = std::mem::take(&mut self.pending);
        // MAN-102: same peak-hold/reset-at-reporting-boundary pattern as
        // `TrackManager::drain_pool` -- see that function's doc comment
        // (review round 2, finding 2: keyed to `hop_count() %
        // META_INTERVAL_HOPS == 0`, not to `TrackMeta` actually emitting).
        let mut events: Vec<DecoderEvent> = Vec::new();
        for (amp, raw_power, spectral_ref_power, ts, snr_db) in pending {
            self.snr_peak_db = self.snr_peak_db.max(snr_db);
            decoder.set_snr_2500_db(self.snr_peak_db - manta_decode::SNR_BW_CORR_DB);
            let hop_events = decoder.push_hop(amp, raw_power, spectral_ref_power, ts);
            if decoder.hop_count() % manta_decode::META_INTERVAL_HOPS == 0 {
                self.snr_peak_db = f32::NEG_INFINITY;
            }
            events.extend(hop_events);
        }
        events.extend(decoder.finish());
        events
    }

    /// Same pending-drain as `finish_decoder`, but calls
    /// `TrackDecoder::finish_speed_only` instead of `finish` -- for
    /// Merged/Evicted closures, which are pure track bookkeeping (another
    /// track claimed the channel, or the track cap was exceeded), not
    /// evidence the RF signal itself ended. The track could be genuinely
    /// mid-character at this instant; forcing that to resolve into a
    /// character (as `finish_decoder` legitimately does for a real
    /// trailing gap) would fabricate content that was never actually
    /// confirmed -- Codex review on PR #154, round 7 found this could
    /// synthesize a "T" character, forming the exact `<call> T` Beacon
    /// pattern out of noise.
    fn finish_decoder_speed_only(&mut self) -> Vec<DecoderEvent> {
        let Some(decoder) = self.decoder.as_mut() else {
            return Vec::new();
        };
        let pending = std::mem::take(&mut self.pending);
        // MAN-102: same peak-hold/reset-at-reporting-boundary pattern as
        // `TrackManager::drain_pool` -- see that function's doc comment
        // (review round 2, finding 2: keyed to `hop_count() %
        // META_INTERVAL_HOPS == 0`, not to `TrackMeta` actually emitting).
        let mut events: Vec<DecoderEvent> = Vec::new();
        for (amp, raw_power, spectral_ref_power, ts, snr_db) in pending {
            self.snr_peak_db = self.snr_peak_db.max(snr_db);
            decoder.set_snr_2500_db(self.snr_peak_db - manta_decode::SNR_BW_CORR_DB);
            let hop_events = decoder.push_hop(amp, raw_power, spectral_ref_power, ts);
            if decoder.hop_count() % manta_decode::META_INTERVAL_HOPS == 0 {
                self.snr_peak_db = f32::NEG_INFINITY;
            }
            events.extend(hop_events);
        }
        events.extend(decoder.finish_speed_only());
        events
    }

    /// Query the current lifecycle state. SPEC §2.4.
    // Temporary: no non-test caller yet until a later task filters/reports
    // tracks by lifecycle state (e.g. spot output limited to ACTIVE tracks).
    #[allow(dead_code)]
    pub(crate) fn state(&self) -> LifecycleState {
        self.lifecycle.state()
    }

    /// SPEC §2.5: owned channel indices for this hop's ownership checks.
    /// Wraps modulo `n_channels`: the channelizer's channel index is a
    /// circular FFT bin ordering over the full complex baseband (SPEC
    /// §1.1), so channel 0 and channel `n_channels-1` are genuinely
    /// frequency-adjacent, as are the ±Nyquist bins at the midpoint.
    fn owned(&self, n_channels: usize) -> [usize; 3] {
        let c = self.center.round() as i64;
        let n = n_channels as i64;
        [
            ((c - 1).rem_euclid(n)) as usize,
            (c.rem_euclid(n)) as usize,
            ((c + 1).rem_euclid(n)) as usize,
        ]
    }

    /// Max-power channel among this track's owned set this hop. SPEC §2.5
    /// ("max-power selection").
    fn select_channel(&self, power: &[f32], n_channels: usize) -> usize {
        let owned = self.owned(n_channels);
        owned
            .into_iter()
            .max_by(|&a, &b| power[a].partial_cmp(&power[b]).unwrap())
            .unwrap()
    }

    /// Update `center` (live EMA), from this hop's selected channel's
    /// fine-frequency interpolation. SPEC §1.4/§2.5.
    fn update_centroid(&mut self, k: usize, power: &[f32], n_channels: usize) {
        let k_minus = (k + n_channels - 1) % n_channels;
        let k_plus = (k + 1) % n_channels;
        if let Some(delta) = interpolate_offset(power[k_minus], power[k], power[k_plus]) {
            let raw = k as f64 + delta;
            self.center += CENTER_EMA_ALPHA * (raw - self.center);
        }
    }

    /// Absolute Hz for this track's current centroid, reported as
    /// `TrackDecoder`'s live `TrackMeta.freq_hz` (SPEC §5). `step_hop` calls
    /// this every hop a decoder exists to keep it current.
    ///
    /// **Deviates from SPEC §1.4's literal formula.** §1.4 specifies a
    /// lifetime power-weighted running mean with no decay
    /// (`Σ(k₀+δ_m)·P₀[m] / ΣP₀[m]`, accumulated from track birth). That
    /// formula is structurally incompatible with SPEC §7 V9's own "final
    /// freq within ±15 Hz of the drifted end frequency" pass criterion: an
    /// undecayed lifetime average of a linearly-drifting quantity converges
    /// toward the midpoint of the observed range, not the current/final
    /// value (measured 48.3 Hz error on V9's 120 s / 100 Hz drift, ~3x over
    /// tolerance). Reporting `self.center` -- the same live, per-hop EMA
    /// (`CENTER_EMA_ALPHA`) already used for ownership/read-selection --
    /// instead tracks ongoing drift: measured in-pipeline at 9.7 Hz error on
    /// V9 (real pipeline; an offline centroid replica used to pick this
    /// estimator predicted 3.9 Hz -- the >2x gap is attributed to the
    /// channelizer's known parabolic-interpolation bias, deferred
    /// separately, not to this estimator choice) and 9.9 Hz on V1
    /// (non-drifting; improved from 16.5 Hz under the old lifetime-mean
    /// formula, not a regression). Both are within SPEC §7's ±15 Hz
    /// criterion. See `docs/DECISIONS/2026-07-19-m2-detector-track-pool-pins.md`
    /// (item 5) for the full rationale.
    fn freq_hz(&self, center_freq_hz: f64, channel_spacing_hz: f64, n_channels: usize) -> f64 {
        let k0_freq = center_freq_hz
            + wrapped_channel_offset(self.birth_channel, n_channels) * channel_spacing_hz;
        k0_freq + (self.center - self.birth_channel as f64) * channel_spacing_hz
    }
}

/// Orchestrates SPEC §2's real detector across all channels: per-channel
/// floor + gate (`manta-dsp::floor`), per-channel lifecycle state
/// machines (`Lifecycle`), and §2.5 adjacent-channel ownership, driving a
/// rayon-parallel decoder pool (ARCHITECTURE §10) via `process_hops`.
pub struct TrackManager {
    floor: FloorBank,
    gate: Gate,
    tracks: BTreeMap<u32, Track>,
    /// channel -> owning track_id, recomputed each hop. SPEC §2.5.
    owner_of: Vec<Option<u32>>,
    next_id: u32,
    cfg: DetectorConfig,
    decode_cfg: DecodeConfig,
    hop_counter: u64,
    center_freq_hz: f64,
    /// Hz per channel (`fs / n_channels`), the unit `Track::freq_hz` (SPEC
    /// §1.1/§1.4) converts its channel-index centroid into absolute Hz.
    channel_spacing_hz: f64,
    /// Issue #26: per-`CloseReason` close counts, read via `close_counts`.
    close_counts: CloseCounts,
    /// MAN-171: channel -> hop_counter before which a new CANDIDATE may not
    /// spawn on that channel, set on a `CloseReason::Silent` closure (see
    /// `DetectorConfig::silent_respawn_cooldown_hops`). `0` (the default,
    /// always `<= hop_counter`) means no active cooldown.
    channel_cooldown_until: Vec<u64>,
}

impl TrackManager {
    /// A fresh manager over `n_channels` channels, configured per
    /// `DetectorConfig`, with a decoder pool configured per `DecodeConfig`.
    /// SPEC §2, §5.
    pub fn new(
        n_channels: usize,
        fs: f64,
        center_freq_hz: f64,
        detector_cfg: DetectorConfig,
        decode_cfg: DecodeConfig,
    ) -> Self {
        TrackManager {
            floor: FloorBank::new(n_channels),
            gate: Gate::new(n_channels, detector_cfg.on_snr_db, detector_cfg.off_snr_db),
            tracks: BTreeMap::new(),
            owner_of: vec![None; n_channels],
            next_id: 1,
            cfg: detector_cfg,
            decode_cfg,
            hop_counter: 0,
            center_freq_hz,
            channel_spacing_hz: fs / n_channels as f64,
            close_counts: CloseCounts::default(),
            channel_cooldown_until: vec![0; n_channels],
        }
    }

    fn n_channels(&self) -> usize {
        self.owner_of.len()
    }

    /// SPEC v2 §2.2's min-of-six-neighbors spectral reference for a
    /// track's centroid channel, converted from dB to linear power (the
    /// unit `NoiseTracker::push`'s `spectral_ref_power` expects). Always
    /// `Some` since `FloorBank::spectral_reference_db` has a real value
    /// for every channel from construction onward. A free function (not
    /// a `TrackManager` method) taking `&FloorBank` directly, so call
    /// sites that already hold a mutable borrow of one `self.tracks`
    /// entry (via `self.tracks.get_mut`) can still call this with
    /// `&self.floor` -- a method call would instead borrow all of `self`,
    /// conflicting with that live `self.tracks` borrow.
    ///
    /// Centered on the rounded CENTROID channel (matching
    /// `decoder_input`'s own `c = round(c_f)`), NOT `k` (Codex review,
    /// PR #178 round 4): `k` is the instantaneous max-power channel and
    /// can flicker on noise/QRM even while the true centroid stays put --
    /// the guard band this reference defines must stay centered on where
    /// the track actually is, not wherever `k` momentarily points. A
    /// populated-neighbor probe measured -50 dB at `k` vs the correct
    /// -90 dB at the centroid when the two differed.
    ///
    /// Callers must only invoke this where the result will actually be
    /// consumed (a decoder-present, non-`Legacy`-engine push) -- Codex
    /// review, PR #178 round 4: computing this unconditionally for every
    /// open track on every hop (including `Legacy`, which never reads
    /// `spectral_ref_power`, and CANDIDATE tracks with no decoder to feed
    /// it to) wasted ~225k-900k transcendental evaluations/sec at
    /// 300-1200 tracks.
    fn spectral_ref_power(floor: &FloorBank, center: f64) -> Option<f32> {
        let c = center.round() as usize;
        Some(10f64.powf(floor.spectral_reference_db(c) / 10.0) as f32)
    }

    /// Issue #26: per-`CloseReason` counts of every track closed so far
    /// (`Unconfirmed`/`HangExpired`/`Silent` from `Lifecycle`'s state
    /// machine, `Merged`/`Evicted` from `merge_converged`/`evict_over_cap`).
    /// SPEC §2.5 and ARCHITECTURE §4/§8 both describe these closes as
    /// "counted" -- this is the count. Wired into `soak_metrics::soak_with_metrics`
    /// (MAN-19) ahead of the real M3 Prometheus endpoint.
    pub fn close_counts(&self) -> CloseCounts {
        self.close_counts
    }

    /// Count of tracks currently open (`self.tracks`, SPEC §2.5). Alongside
    /// `close_counts`, this is what MAN-19's 24h soak needs visible at
    /// every sample interval -- ROADMAP.md's M2 accept criterion ("track
    /// count and evictions visible in metrics").
    pub fn active_track_count(&self) -> usize {
        self.tracks.len()
    }

    /// Rebuild `owner_of` from scratch against the current `tracks` map.
    /// SPEC §2.5.
    fn recompute_ownership(&mut self) {
        self.owner_of.iter_mut().for_each(|o| *o = None);
        for (&id, track) in &self.tracks {
            for ch in track.owned(self.n_channels()) {
                self.owner_of[ch] = Some(id);
            }
        }
    }

    /// One hop: update floor/gate, drive every track's lifecycle, spawn new
    /// CANDIDATEs, apply ownership/merge, evict over cap. Newly-ACTIVE
    /// tracks lease a decoder; every currently-ACTIVE track queues this
    /// hop's `(magnitude, sample_ts)` onto its own `pending` (drained later,
    /// once per hop-batch, by `drain_pool`) -- `char_emitted` (GC timer
    /// input) is always `false` here; the decoder pool runs after this
    /// whole batch, so no per-hop decode result is available yet to feed
    /// back into the same hop's lifecycle bookkeeping.
    #[allow(clippy::type_complexity)]
    fn step_hop(
        &mut self,
        hop: &HopOutput,
        sample_ts: u64,
    ) -> (
        Vec<(u32, ClosureKind)>,
        Vec<DecoderEvent>,
        Vec<DecoderEvent>,
    ) {
        let mut promoted_events: Vec<DecoderEvent> = Vec::new();
        assert_eq!(
            hop.power.len(),
            self.n_channels(),
            "TrackManager::step_hop: hop.power length {} does not match n_channels {}",
            hop.power.len(),
            self.n_channels()
        );
        let power_db_vals: Vec<f64> = hop.power.iter().map(|&p| power_db(p)).collect();
        self.floor.update(&power_db_vals);
        let (rise, drop) = self.gate.update(&power_db_vals, &self.floor);
        // MAN-171 (Codex review, PR #174 round 4, correct): clear a
        // channel's respawn cooldown the moment its own `rise` genuinely
        // drops. `CloseReason::Silent` does NOT prove the RF signal ended
        // -- only `HangExpired` does (see this file's other comments on
        // that exact distinction) -- so a track that closes Silent while a
        // real, weak/marginal signal just wasn't producing decoded
        // characters, and *then* that signal actually ends, must not keep
        // blocking a genuinely different station that shows up on the same
        // channel during the remaining cooldown window. A true stationary
        // artifact's `rise` never drops on its own, so this never touches
        // its cooldown.
        for (k, &r) in rise.iter().enumerate() {
            if !r && self.channel_cooldown_until[k] > 0 {
                self.channel_cooldown_until[k] = 0;
            }
        }
        self.recompute_ownership();

        let past_warmup = self.hop_counter >= self.cfg.warmup_hops;
        self.hop_counter += 1;
        // MAN-168 (SPEC v2 §3): read once per hop-batch, not per track --
        // it doesn't vary per track and `Track::decoder_input` needs it
        // without also needing a borrow of `self.decode_cfg` alongside
        // `self.tracks.get_mut(&id)`.
        let refine_bw_hz = self.decode_cfg.refine_bw_hz;
        // Codex review, PR #178 round 4: `Engine::Legacy` (the default)
        // ignores `spectral_ref_power` entirely (`push_envelope_legacy`
        // never reads it), and a CANDIDATE track (no decoder yet) never
        // queues it either -- computing `FloorBank::spectral_reference_db`
        // (a six-neighbor scan plus `log10`/`powf`) unconditionally for
        // every open track on every hop wasted ~225k-900k transcendental
        // evaluations/sec at 300-1200 tracks for callers that can never
        // consume the result. Gated at each push site below (Promoted /
        // decoder-present) instead of computed here for every track.
        let compute_spectral_ref_power = !matches!(self.decode_cfg.engine, Engine::Legacy);

        // Drive existing tracks; collect closures to apply after the loop
        // (avoids mutating `self.tracks` while iterating it).
        let mut closed: Vec<u32> = Vec::new();
        // Codex review on PR #154, round 8: `Silent` (no character
        // decoded for `gc_hops`) does NOT imply an observed RF gap the
        // way `HangExpired` (sustained `drop` for `hang_hops`) does -- it
        // can fire on a track that's still ACTIVE with a real, continuous
        // signal, just not producing decoded characters. Only
        // `HangExpired` may use the forcing `finish_decoder`; `Silent`
        // must go through `finish_decoder_speed_only` below, same as
        // Merged/Evicted.
        let mut close_reasons: std::collections::HashMap<u32, CloseReason> =
            std::collections::HashMap::new();
        let ids: Vec<u32> = self.tracks.keys().copied().collect();
        for id in ids {
            let n = self.n_channels();
            let track = self.tracks.get_mut(&id).unwrap();
            let k = track.select_channel(&hop.power, n);
            track.update_centroid(k, &hop.power, n);
            let f = self.floor.effective_floor_db(k);
            track.current_snr_db = (self.gate.smoothed_db(k) - f) as f32;
            let char_emitted = false; // GC timer input; refined below once a decoder exists.
            let event = track.lifecycle.on_hop(rise[k], drop[k], char_emitted);
            match event {
                LifecycleEvent::Closed(reason) => {
                    self.close_counts.record(reason);
                    close_reasons.insert(id, reason);
                    closed.push(id);
                }
                LifecycleEvent::Promoted => {
                    // SPEC §1.1/§5 (freq_hz's doc comment covers the §1.4
                    // deviation): seed TrackMeta.freq_hz from this track's
                    // own centroid EMA (already warm from its
                    // CANDIDATE-period update_centroid calls above), not the
                    // pipeline's raw center_freq_hz -- every track otherwise
                    // reports the same wrong frequency regardless of which
                    // channel it actually lives on.
                    // Computed before `track.decoder` is touched: `freq_hz`
                    // borrows all of `track`, which would conflict with a
                    // simultaneous `track.decoder.as_mut()` borrow.
                    let freq_hz = track.freq_hz(self.center_freq_hz, self.channel_spacing_hz, n);
                    track.decoder = Some(TrackDecoder::new(id, self.decode_cfg.clone()));
                    track.decoder.as_mut().unwrap().set_freq_hz(freq_hz);
                    let (amp, raw_power, ts) = track.decoder_input(k, hop, sample_ts, refine_bw_hz);
                    let spectral_ref_power = compute_spectral_ref_power
                        .then(|| Self::spectral_ref_power(&self.floor, track.center))
                        .flatten();
                    // MAN-102: the raw per-hop SNR is paired, not a
                    // pre-computed peak -- `drain_pool` maintains the peak
                    // itself, in queue order, and resets it the instant a
                    // push actually emits `TrackMeta` (review round 1
                    // findings 2/3; see `pending`'s doc comment).
                    track.pending.push((
                        amp,
                        raw_power,
                        spectral_ref_power,
                        ts,
                        track.current_snr_db,
                    ));
                    // Emitted unconditionally here, at the exact hop the
                    // detector made this decision -- NOT gated by
                    // `has_emitted`/TrackClosed's same-batch-merge filter
                    // (MAN-19) further down. A track promoted and merged
                    // away within this same `process_hops` call still
                    // really was promoted; that's exactly the ground truth
                    // `doctor()`'s NoSignal check needs and the other event
                    // kinds can't reliably provide (see events.rs's doc
                    // comment on this variant).
                    promoted_events.push(DecoderEvent::TrackPromoted {
                        track_id: id,
                        sample_ts,
                        freq_hz,
                    });
                }
                LifecycleEvent::None => {
                    // Feed the decoder every hop once it exists (ACTIVE *or*
                    // HANG), not only while ACTIVE. `TrackDecoder` times its
                    // runs by hop *count* (`run.hops * HOP_MS`), so any hop
                    // silently skipped while the track sits in HANG (every
                    // inter-character/inter-word key-up gap, where the EMA
                    // decays below `off_snr_db`) would shorten the following
                    // space run and collapse character boundaries. During
                    // HANG the selected channel simply carries noise-floor
                    // magnitude, which the demod correctly reads as a space --
                    // so the continuous feed keeps the decoder's timeline
                    // hole-free. SPEC §3.3/§4.1 timing depends on this.
                    if track.decoder.is_some() {
                        // Keep TrackMeta.freq_hz live (SPEC §5's 1 Hz
                        // cadence reports the *current* centroid, not a
                        // frozen promotion-time snapshot) -- cheap, a plain
                        // field write, no per-hop allocation. Computed
                        // before the mutable `decoder` borrow, same reason
                        // as the `Promoted` arm above.
                        let freq_hz =
                            track.freq_hz(self.center_freq_hz, self.channel_spacing_hz, n);
                        if let Some(decoder) = track.decoder.as_mut() {
                            decoder.set_freq_hz(freq_hz);
                        }
                        let (amp, raw_power, ts) =
                            track.decoder_input(k, hop, sample_ts, refine_bw_hz);
                        let spectral_ref_power = compute_spectral_ref_power
                            .then(|| Self::spectral_ref_power(&self.floor, track.center))
                            .flatten();
                        track.pending.push((
                            amp,
                            raw_power,
                            spectral_ref_power,
                            ts,
                            track.current_snr_db,
                        ));
                    }
                }
            }
        }
        // MAN-19: only report a close as `TrackClosed`-worthy if the track
        // actually emitted a real event (`has_emitted`), not merely
        // whether it was ever promoted (`decoder.is_some()`) -- a track
        // promoted and then merged/evicted within this *same*
        // `process_hops` batch never gets a `drain_pool` pass before it's
        // removed here (that runs once, after every hop in the batch), so
        // it can have an allocated decoder yet have produced nothing at
        // all (round 1 review). A CANDIDATE that closes Unconfirmed
        // without ever being promoted never appeared in the event stream
        // either; surfacing a `TrackClosed` for either case would
        // introduce a track_id into the stream that callers like
        // `decode_samples` (which picks the *lowest* track_id present as
        // its single-track report) never used to see, silently changing
        // which track gets reported.
        // Codex review on PR #154, round 5: a track closed here
        // (HangExpired/Silent -- Merged/Evicted below have the matching
        // fix) previously had its `TrackDecoder` silently dropped without
        // ever calling `finish()`, discarding any buffered demod/beam
        // state -- including the true final speed estimate a held-back
        // `manta-spot` Beacon-WPM candidate needs to retry against
        // (SPEC-decode-core.md §5's `finish()` contract was previously
        // only honored at overall stream end, in `TrackManager::finish`
        // below). Draining it here and threading the events out gives
        // every closure path the same guarantee `TrackManager::finish`
        // already had. `Track::finish_decoder` (round 6) also drains any
        // samples still queued in `pending` THIS batch first -- without
        // that, `finish()` alone would reflect only whatever the decoder
        // processed as of the end of the PREVIOUS `process_hops` batch.
        let mut closure_flush_events: Vec<DecoderEvent> = Vec::new();
        closed.retain(|id| {
            let Some(mut track) = self.tracks.remove(id) else {
                return false;
            };
            // MAN-171: a track that ran the full GC window with zero
            // decoded characters is very likely a stationary artifact
            // (birdie/spur), not real CW -- bar its channels from
            // immediately re-spawning a fresh CANDIDATE (see
            // `DetectorConfig::silent_respawn_cooldown_hops`'s doc for why
            // `rise` alone can never clear on its own for a channel like
            // this). Checked ahead of the `has_emitted` gate below since
            // that gate is about whether a `TrackClosed` *event* is worth
            // surfacing downstream, not about whether this channel just
            // proved itself not real CW -- unrelated questions. Computed
            // from `track.owned` before `track` drops.
            if close_reasons.get(id) == Some(&CloseReason::Silent) {
                let until = self.hop_counter + self.cfg.silent_respawn_cooldown_hops;
                let n = self.channel_cooldown_until.len();
                for ch in track.owned(n) {
                    self.channel_cooldown_until[ch] = until;
                }
            }
            if !track.has_emitted {
                return false;
            }
            // Only a genuine, sustained observed RF gap (HangExpired) may
            // force an in-progress mark to resolve; Silent's own trigger
            // (no character decoded) says nothing about whether the
            // signal is still there (round 8).
            let events = if close_reasons.get(id) == Some(&CloseReason::HangExpired) {
                track.finish_decoder()
            } else {
                track.finish_decoder_speed_only()
            };
            closure_flush_events.extend(events);
            true
        });
        self.recompute_ownership();

        // Same-hop simultaneous-rise tie-break (SPEC §2.5): scan channels in
        // ascending order; a rise on an unowned channel spawns a CANDIDATE
        // unless a higher-power unowned neighbor also rose this hop, in
        // which case only the higher-power one spawns.
        if past_warmup {
            let n = self.n_channels();
            let mut k = 0;
            while k < n {
                if rise[k] && self.owner_of[k].is_none() && !self.channel_cooling_down(k) {
                    let mut winner = k;
                    if k + 1 < n
                        && rise[k + 1]
                        && self.owner_of[k + 1].is_none()
                        && !self.channel_cooling_down(k + 1)
                    {
                        if hop.power[k + 1] > hop.power[winner] {
                            winner = k + 1;
                        }
                        // both channels claimed by this decision; skip past k+1
                        // next iteration so we don't re-evaluate it as its own birth.
                        self.spawn(winner);
                        k += 2;
                        continue;
                    }
                    self.spawn(winner);
                }
                k += 1;
            }
        }
        self.recompute_ownership();
        // Round 10 (PR #154, Codex): only HangExpired is a genuine observed
        // RF gap -- proof the signal actually ended. Silent fires after 30 s
        // of no *decoded character*, which a continuous carrier or other
        // undecodable signal satisfies just as well as a real end-of-signal;
        // `finish_decoder_speed_only()` above deliberately can't force such
        // a still-open run to resolve, so the WPM it reports can still be a
        // stale, transient pre-carrier estimate. Judging a deferred Beacon
        // against that stale value would readmit exactly the false-positive
        // class this PR exists to close. So Silent gets `Bookkeeping` (no
        // survivor) like Merged/Evicted -- its pending beacons are counted
        // and discarded, never resolved, since no genuine signal-ending
        // observation is available for them.
        let mut closed_with_kind: Vec<(u32, ClosureKind)> = closed
            .into_iter()
            .map(|id| {
                let kind = if close_reasons.get(&id) == Some(&CloseReason::HangExpired) {
                    ClosureKind::SignalEnded
                } else {
                    ClosureKind::Bookkeeping {
                        survivor_track_id: None,
                    }
                };
                (id, kind)
            })
            .collect();
        let (merged_ids, merged_flush) = self.merge_converged();
        closed_with_kind.extend(merged_ids);
        closure_flush_events.extend(merged_flush);
        let (evicted_ids, evicted_flush) = self.evict_over_cap();
        closed_with_kind.extend(evicted_ids);
        closure_flush_events.extend(evicted_flush);
        (closed_with_kind, promoted_events, closure_flush_events)
    }

    /// MAN-171: is `k` still serving out a post-`Silent`-closure spawn
    /// cooldown (`DetectorConfig::silent_respawn_cooldown_hops`)?
    fn channel_cooling_down(&self, k: usize) -> bool {
        self.channel_cooldown_until[k] > self.hop_counter
    }

    /// Birth a new CANDIDATE track on `birth_channel`. Its `current_snr_db`
    /// is seeded from this hop's live gate/floor state (not left at
    /// `Track::new`'s placeholder `0.0`) so a track born this same hop is
    /// not unfairly evicted against a mature incumbent by
    /// `evict_over_cap`/`merge_converged`, which both compare
    /// `current_snr_db` and would otherwise always prefer any already-driven
    /// track over one that (correctly, but only from next hop on) hasn't
    /// had `current_snr_db` populated yet.
    fn spawn(&mut self, birth_channel: usize) {
        let id = self.next_id;
        self.next_id += 1;
        let mut track = Track::new(id, birth_channel, &self.cfg);
        let f = self.floor.effective_floor_db(birth_channel);
        track.current_snr_db = (self.gate.smoothed_db(birth_channel) - f) as f32;
        // `snr_peak_db` is left at `Track::new`'s `NEG_INFINITY`: it is only
        // ever read/updated by `drain_pool`, which `.max()`s it against the
        // first queued `current_snr_db` the moment this track's decoder
        // exists -- nothing reads it before then.
        for ch in track.owned(self.n_channels()) {
            self.owner_of[ch] = Some(id);
        }
        self.tracks.insert(id, track);
    }

    /// SPEC §2.5: tracks whose centers converge within 1.0 channel merge;
    /// the lower-current-SNR one is closed. Returns the closed ids (with
    /// `ClosureKind::Bookkeeping` naming the surviving track -- round 9,
    /// PR #154: the loser's identity may continue there, so a consumer
    /// holding deferred per-identity evidence must migrate it rather than
    /// judge it now) plus any final `finish()` events their decoders
    /// produced (round 5 -- see `step_hop`'s matching comment).
    fn merge_converged(&mut self) -> (Vec<(u32, ClosureKind)>, Vec<DecoderEvent>) {
        let ids: Vec<u32> = self.tracks.keys().copied().collect();
        let mut to_close: Vec<(u32, u32)> = Vec::new(); // (loser, survivor)
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                let (a, b) = (ids[i], ids[j]);
                if to_close.iter().any(|&(loser, _)| loser == a || loser == b) {
                    continue;
                }
                let (ca, cb) = (self.tracks[&a].center, self.tracks[&b].center);
                if (ca - cb).abs() < 1.0 {
                    let (loser, survivor) =
                        if self.tracks[&a].current_snr_db <= self.tracks[&b].current_snr_db {
                            (a, b)
                        } else {
                            (b, a)
                        };
                    to_close.push((loser, survivor));
                }
            }
        }
        // MAN-19: only report a merge-loser as `TrackClosed`-worthy if it
        // actually emitted a real event -- see the matching comment in
        // `step_hop` (a track promoted this same batch can be merged away
        // before ever getting a `drain_pool` pass).
        let mut flush_events: Vec<DecoderEvent> = Vec::new();
        let ever_emitted_closed = to_close
            .into_iter()
            .filter_map(|(loser, survivor)| {
                self.close_counts.record(CloseReason::Merged);
                let mut track = self.tracks.remove(&loser)?;
                if !track.has_emitted {
                    return None;
                }
                flush_events.extend(track.finish_decoder_speed_only());
                Some((
                    loser,
                    ClosureKind::Bookkeeping {
                        survivor_track_id: Some(survivor),
                    },
                ))
            })
            .collect();
        if !ids.is_empty() {
            self.recompute_ownership();
        }
        (ever_emitted_closed, flush_events)
    }

    /// SPEC §2.4/ARCHITECTURE §4: track cap with lowest-current-SNR
    /// eviction. Returns the evicted ids plus any final `finish()` events
    /// their decoders produced (Codex review on PR #154, round 5 -- see
    /// `step_hop`'s matching comment).
    fn evict_over_cap(&mut self) -> (Vec<(u32, ClosureKind)>, Vec<DecoderEvent>) {
        let mut evicted = Vec::new();
        let mut flush_events: Vec<DecoderEvent> = Vec::new();
        while self.tracks.len() > self.cfg.track_cap {
            let loser = *self
                .tracks
                .iter()
                .min_by(|(_, a), (_, b)| a.current_snr_db.partial_cmp(&b.current_snr_db).unwrap())
                .map(|(id, _)| id)
                .unwrap();
            self.close_counts.record(CloseReason::Evicted);
            // MAN-19: only report as `TrackClosed`-worthy if it actually
            // emitted a real event -- see `step_hop`'s matching comment.
            if let Some(mut track) = self.tracks.remove(&loser) {
                if track.has_emitted {
                    flush_events.extend(track.finish_decoder_speed_only());
                    // No survivor -- an eviction just stops tracking this
                    // identity, with no successor to migrate deferred
                    // evidence to (round 9).
                    evicted.push((
                        loser,
                        ClosureKind::Bookkeeping {
                            survivor_track_id: None,
                        },
                    ));
                }
            }
        }
        self.recompute_ownership();
        (evicted, flush_events)
    }

    /// Process one `Channelizer::process()` slice: sequential per-hop
    /// state-machine bookkeeping (ownership/promotion/eviction), then a
    /// single rayon-parallel decode pass across all currently-queued
    /// ACTIVE tracks (the decoder pool -- ARCHITECTURE §10), then
    /// resequence by `(sample_ts, track_id)` per SPEC §6 rule 6.
    /// `hop_to_sample_ts` mirrors the existing `decode_samples`/`listen`
    /// convention of converting a channelizer hop index to a raw-sample
    /// timestamp.
    pub fn process_hops(
        &mut self,
        hops: &[HopOutput],
        hop_to_sample_ts: impl Fn(u64) -> u64,
    ) -> Vec<DecoderEvent> {
        let mut closed_ids: Vec<(u32, ClosureKind)> = Vec::new();
        let mut promoted_events: Vec<DecoderEvent> = Vec::new();
        let mut closure_flush_events: Vec<DecoderEvent> = Vec::new();
        for h in hops {
            let (closed, promoted, closure_flush) = self.step_hop(h, hop_to_sample_ts(h.m));
            closed_ids.extend(closed);
            promoted_events.extend(promoted);
            closure_flush_events.extend(closure_flush);
        }
        let pool_events = self.drain_pool();
        // SPEC §2.4 GC timer: reset the silent counter for every track that
        // actually decoded a character this batch. `step_hop` advances it
        // every hop with `char_emitted = false` (the pool has not run yet),
        // so this post-pool reset is the only thing that keeps a
        // continuously-decoding signal from being force-closed
        // `CloseReason::Silent` after `gc_hops` and re-spawned as a new track.
        //
        // Also marks `has_emitted` for MAN-19's `TrackClosed` filter (see
        // `step_hop`'s comment on `closed.retain`) -- every event kind
        // here counts as real output, not just `CharDecoded`. A track
        // closed by a LATER `process_hops` call reads whatever this set,
        // which is exactly what "did this track ever actually emit
        // anything" needs; a track promoted and closed within THIS same
        // call never reaches this loop before being removed, so it
        // correctly stays `false`. `events` here is still just
        // `drain_pool()`'s output (decoder-produced events only) --
        // `promoted_events` is deliberately extended in AFTER this loop,
        // not before, so a bare promotion (no decoder output at all before
        // a same-batch merge/evict) does NOT set `has_emitted` and does
        // NOT retroactively earn that track a `TrackClosed` -- preserving
        // MAN-19's exclusion. `TrackPromoted` is real, permanent signal
        // for a *different* consumer (`doctor()`'s NoSignal check) with no
        // per-track_id state to leak (manta-spot's `Validator` treats it as
        // a pure no-op, never touching `self.tracks`).
        for e in &pool_events {
            if let Some(t) = self.tracks.get_mut(&event_track_id(e)) {
                t.has_emitted = true;
            }
            if let DecoderEvent::CharDecoded { track_id, .. } = e {
                if let Some(t) = self.tracks.get_mut(track_id) {
                    t.lifecycle.note_char_decoded();
                }
            }
        }
        // Per-track promotion timestamp, for `effective_sort_ts` below --
        // only tracks promoted THIS batch appear here, which is exactly
        // the set `effective_sort_ts` needs to special-case (see its doc
        // comment).
        let promoted_ts_by_track: std::collections::HashMap<u32, u64> = promoted_events
            .iter()
            .map(|e| (event_track_id(e), event_sample_ts(e)))
            .collect();
        let mut events = pool_events;
        events.extend(promoted_events);
        // Final `finish()` events from tracks closed THIS batch via
        // HangExpired/Silent/Merged/Evicted (Codex review on PR #154,
        // round 5) -- kept out of `promoted_ts_by_track`'s source data
        // above deliberately: these events' own `event_sample_ts` (0 for
        // SpeedUpdate) must not overwrite a same-track genuine promotion
        // timestamp were one to exist this same batch (it can't in
        // practice -- a track promoted and closed within the same batch
        // never reaches `has_emitted`, per the existing MAN-19 exclusion
        // below -- but keeping this a separate vector makes that
        // non-interaction structural rather than incidental).
        events.extend(closure_flush_events);
        // MAN-19: emit one `TrackClosed` per track closed this batch (any
        // CloseReason) so downstream per-track_id state (`manta-spot`'s
        // `Validator::tracks`, `RepetitionGate::seen`) has a signal to
        // free it -- see events.rs's TrackClosed doc for the unbounded-
        // growth bug this fixes. Re-sorted with the rest per SPEC §6 rule
        // 6; `event_sample_ts` gives these no ordering claim of their own
        // (synthetic `u64::MAX`, same treatment as `SpeedUpdate`'s
        // synthetic `0` -- see that function's doc; `TrackMeta` now
        // carries a real timestamp, MAN-102 review round 2).
        events.extend(
            closed_ids
                .into_iter()
                .map(|(track_id, closure)| DecoderEvent::TrackClosed { track_id, closure }),
        );
        events.sort_by_key(|e| {
            (
                effective_sort_ts(e, &promoted_ts_by_track),
                event_track_id(e),
                event_kind_tier(e),
            )
        });
        events
    }

    /// Flush every track's decoder (SPEC §5 end-of-stream). Call once,
    /// after the last `process_hops`.
    pub fn finish(&mut self) -> Vec<DecoderEvent> {
        use rayon::prelude::*;
        let mut events: Vec<DecoderEvent> = self
            .tracks
            .values_mut()
            .filter_map(|t| t.decoder.as_mut())
            .par_bridge()
            .flat_map_iter(|d| d.finish())
            .collect();
        events.sort_by_key(|e| (event_sample_ts(e), event_track_id(e)));
        // MAN-19 round 3: honor the same teardown contract `process_hops`
        // does -- a track still open when the stream ends (EOF/Ctrl-C/
        // error) must still get its `TrackClosed` if it ever emitted a
        // real event, including one this very flush just produced (mark
        // `has_emitted` from `events` first, same as `process_hops`).
        // Without this, a caller that keeps the `Validator`/
        // `RepetitionGate` alive past `finish()` (this crate's own
        // `listen()` doesn't, but the contract shouldn't depend on that)
        // would leak exactly the state this whole mechanism exists to
        // free.
        for e in &events {
            if let Some(t) = self.tracks.get_mut(&event_track_id(e)) {
                t.has_emitted = true;
            }
        }
        let closed_ids: Vec<u32> = self
            .tracks
            .iter()
            .filter(|(_, t)| t.has_emitted)
            .map(|(&id, _)| id)
            .collect();
        // MAN-19 round 7: append, then apply ONE consistent global
        // `(sample_ts, track_id)` sort (SPEC-decode-core.md §6 rule 6) --
        // round 4's version appended without re-sorting specifically to
        // dodge `TrackClosed`'s old ts=0 treatment (which would've put a
        // track's own closure *before* the real content this same flush
        // just produced for it), but that broke the spec's single-global-
        // sort requirement. Now that `event_sample_ts` gives `TrackClosed`
        // a synthetic `u64::MAX` (guaranteed after every real event, for
        // every track, not just this one), a normal full re-sort is safe
        // and correct again.
        events.extend(
            closed_ids
                .into_iter()
                .map(|track_id| DecoderEvent::TrackClosed {
                    track_id,
                    // Overall stream end -- always a genuine end-of-signal (round 9).
                    closure: ClosureKind::SignalEnded,
                }),
        );
        events.sort_by_key(|e| (event_sample_ts(e), event_track_id(e)));
        self.tracks.clear();
        events
    }

    /// Drain every track's `pending` queue through its decoder, in
    /// parallel (ARCHITECTURE §10's decoder pool: tracks are work items,
    /// decoders are independent `Send` state machines processed in any
    /// order). Called once per `process_hops` batch, against the *current*
    /// `self.tracks` map -- after this batch's merges/evictions have
    /// already removed any tracks whose `pending` would otherwise be
    /// stale, so there is no path to draining a since-removed track's
    /// queue.
    fn drain_pool(&mut self) -> Vec<DecoderEvent> {
        use rayon::prelude::*;
        let mut events: Vec<DecoderEvent> = self
            .tracks
            .values_mut()
            .filter_map(|t| {
                if t.pending.is_empty() {
                    None
                } else {
                    let pending = std::mem::take(&mut t.pending);
                    Some((t.decoder.as_mut().unwrap(), &mut t.snr_peak_db, pending))
                }
            })
            .par_bridge()
            .flat_map_iter(|(decoder, peak, pending)| {
                let mut out = Vec::new();
                for (amp, raw_power, spectral_ref_power, ts, snr_db) in pending {
                    // MAN-102: SPEC §2.3's floor-based estimate, in the
                    // 2500 Hz reference bandwidth, held at its peak since
                    // this track's last SPEC §5 reporting boundary (see
                    // `snr_peak_db`'s and `pending`'s doc comments on
                    // `Track`). The reset below is keyed to the boundary
                    // itself (`hop_count() % META_INTERVAL_HOPS == 0`), not
                    // to `TrackMeta` actually being emitted: before
                    // `Demod::running()` (init/retrying, SPEC §3.2) no
                    // `TrackMeta` fires at all, and resetting only on
                    // emission left the peak window unbounded for however
                    // long init took instead of one interval (review round
                    // 2, finding 2). This still lands on the exact hop a
                    // report fires when one does (review round 1 findings
                    // 2/3): the boundary check is the same hop the decoder
                    // itself gates `TrackMeta` emission on. Consumed only by
                    // the `Legacy` engine (`TrackDecoder::tick_meta`);
                    // `EdgeLegacy`/`Hsmm` source their own SPEC v2 §2.3
                    // evidence-derived estimate instead, so this call is
                    // harmless but inert for those two engines.
                    *peak = peak.max(snr_db);
                    decoder.set_snr_2500_db(*peak - manta_decode::SNR_BW_CORR_DB);
                    let hop_events = decoder.push_hop(amp, raw_power, spectral_ref_power, ts);
                    if decoder.hop_count() % manta_decode::META_INTERVAL_HOPS == 0 {
                        *peak = f32::NEG_INFINITY;
                    }
                    out.extend(hop_events);
                }
                out
            })
            .collect();
        events.sort_by_key(|e| (event_sample_ts(e), event_track_id(e)));
        events
    }
}

/// SPEC §6 rule 6 resequencing key: the sample timestamp an event is
/// anchored to, `0` for `SpeedUpdate` (no inherent timestamp -- sorts
/// first among ties on `event_track_id`), or `u64::MAX` for `TrackClosed`
/// -- a synthetic "after everything" marker, not `0` (round 7 review):
/// `TrackClosed` isn't anchored to a real timestamp either, but unlike
/// `SpeedUpdate` it must never sort before another real event for the
/// SAME track_id (a track's own final `CharDecoded`/`WordBoundary`,
/// `finish()`'s flush) -- doing so lets a consumer free that track's state
/// and then recreate it processing the trailing events, with nothing left
/// to ever clean that up again (the exact leak this whole mechanism exists
/// to prevent). `MAX` guarantees that regardless of how large a real
/// `sample_ts` grows. This is what lets `finish()` (and `process_hops`)
/// apply ONE consistent `(sample_ts, track_id)` sort across the whole
/// batch (SPEC-decode-core.md §6 rule 6) instead of the append-without-
/// re-sorting workaround round 4 used.
///
/// `TrackMeta` carries a real `sample_ts` rather than tying at `0` like
/// `SpeedUpdate` (MAN-102 review round 2 finding: pinning it to `0` sorted
/// every `TrackMeta` ahead of its *entire* emitting batch, including
/// earlier-hop `CharDecoded`/`WordBoundary` events from the same batch, so
/// a just-reset/just-reported SNR value could retroactively attach to
/// characters decoded before it -- with the magnitude depending on the
/// caller's chunk size (`decode_samples` vs. `listen`/`soak_metrics`)).
fn event_sample_ts(e: &DecoderEvent) -> u64 {
    match e {
        DecoderEvent::CharDecoded { sample_ts, .. }
        | DecoderEvent::WordBoundary { sample_ts, .. }
        | DecoderEvent::TrackPromoted { sample_ts, .. }
        | DecoderEvent::TrackMeta { sample_ts, .. } => *sample_ts,
        DecoderEvent::SpeedUpdate { .. } => 0,
        DecoderEvent::TrackClosed { .. } => u64::MAX,
    }
}

/// `SpeedUpdate`/`TrackMeta` carry no real timestamp of their own (0,
/// "no ordering claim" -- true whenever their track had no same-batch
/// promotion, the overwhelming common case, left completely unchanged
/// here). But when the SAME track also has a `TrackPromoted` THIS batch,
/// sorting them at a flat 0 can place them before that promotion's real,
/// later timestamp -- backwards, since promotion logically precedes any
/// output the newly-created decoder produces. Two earlier attempts got
/// this wrong: pinning `TrackPromoted` to 0 too "fixed" same-track order
/// but broke cross-track order (a promotion could then sort before an
/// unrelated OTHER track's genuinely-earlier event, round-7 review); a
/// post-sort remove/insert pass fixed that but could itself jump a
/// promotion across an unrelated track's real, intervening timestamp
/// (round-8 review) -- neither extra pass composes safely with a plain
/// `(sample_ts, track_id)` sort.
///
/// The actual fix: express it ENTIRELY as a sort key, no post-processing.
/// A pinned event borrows its OWN track's same-batch promotion timestamp
/// (if one exists) instead of a flat 0 -- landing it in the correct
/// global chronological neighborhood, exactly where that promotion
/// already correctly sorts -- and an explicit tier (`TrackPromoted` = 0,
/// everything else = 1) breaks the resulting tie in the promotion's
/// favor. A single total-order sort composes safely by construction;
/// there is nothing left to accidentally disturb.
fn effective_sort_ts(
    e: &DecoderEvent,
    promoted_ts_by_track: &std::collections::HashMap<u32, u64>,
) -> u64 {
    match e {
        DecoderEvent::SpeedUpdate { track_id, .. } | DecoderEvent::TrackMeta { track_id, .. } => {
            promoted_ts_by_track.get(track_id).copied().unwrap_or(0)
        }
        other => event_sample_ts(other),
    }
}

/// Tertiary sort key, after `(effective_sort_ts, track_id)`: `TrackPromoted`
/// sorts before every other kind whenever they tie (which only happens
/// when a pinned event borrows its own track's promotion timestamp via
/// `effective_sort_ts` above, or by pure numeric coincidence -- itself
/// harmless, since SPEC doesn't define a relative order for two
/// genuinely-identical real timestamps beyond `(sample_ts, track_id)`).
fn event_kind_tier(e: &DecoderEvent) -> u8 {
    match e {
        DecoderEvent::TrackPromoted { .. } => 0,
        _ => 1,
    }
}

/// SPEC §6 rule 6 resequencing key: the owning track's id, every
/// `DecoderEvent` variant carries one.
pub(crate) fn event_track_id(e: &DecoderEvent) -> u32 {
    match e {
        DecoderEvent::CharDecoded { track_id, .. }
        | DecoderEvent::WordBoundary { track_id, .. }
        | DecoderEvent::SpeedUpdate { track_id, .. }
        | DecoderEvent::TrackMeta { track_id, .. }
        | DecoderEvent::TrackPromoted { track_id, .. }
        | DecoderEvent::TrackClosed { track_id, .. } => *track_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> DetectorConfig {
        DetectorConfig {
            confirm_hops: 5,
            hang_hops: 10,
            gc_hops: 20,
            ..DetectorConfig::default()
        }
    }

    #[test]
    fn decoder_input_bypasses_the_refiner_when_disabled() {
        // MAN-168: refine_bw_hz <= 0.0 (the default) must fall back to the
        // raw owned-channel magnitude exactly as before, and never
        // construct a Refiner at all.
        let mut track = Track::new(1, 5, &cfg());
        let h = hop_with_x(0, vec![num_complex::Complex32::new(3.0, 4.0); 8]); // |z|=5
        let (amp, raw_power, ts) = track.decoder_input(5, &h, 1000, 0.0);
        assert_eq!(amp, 5.0);
        assert_eq!(raw_power, 25.0);
        assert_eq!(ts, 1000);
        assert!(track.refiner.is_none());
    }

    #[test]
    fn decoder_input_does_not_reset_the_refiner_on_center_drift_alone() {
        // MAN-168 (Codex review on PR #161's MAN-168 ticket design):
        // resetting must be keyed on the INTEGER owned channel changing,
        // not the continuous `center` EMA -- otherwise every hop's
        // fractional drift would retrigger the FIR ramp-in transient.
        let mut track = Track::new(1, 5, &cfg());
        let h = hop_with_x(0, vec![num_complex::Complex32::new(1.0, 0.0); 8]);
        track.decoder_input(5, &h, 0, 30.0);
        assert_eq!(track.refiner_hops_since_reset, 1);
        // Drift center within the same integer channel k=5 across several
        // more hops -- the counter must keep GROWING (never cleared by a
        // spurious reset) since `refiner_channel` stays `Some(5)`.
        for i in 1..4u64 {
            track.center += 0.1;
            track.decoder_input(5, &h, i * 512, 30.0);
        }
        assert_eq!(
            track.refiner_hops_since_reset, 4,
            "center drift within the same channel must not reset the hop counter"
        );
        assert_eq!(track.refiner_channel, Some(5));
    }

    #[test]
    fn decoder_input_resets_on_a_real_channel_change() {
        // A genuine centroid-channel switch (round(center) changes) must
        // reset the refiner's FIR/phase and hop counter, per SPEC v2 §3.
        // `k` (the raw ownership channel) is passed but no longer drives
        // refiner selection at all -- SPEC v2 §3 requires `c = round(c_f)`,
        // so only `center` crossing a rounding boundary can trigger this.
        let mut track = Track::new(1, 5, &cfg());
        let h = hop_with_x(0, vec![num_complex::Complex32::new(1.0, 0.0); 8]);
        track.decoder_input(5, &h, 0, 30.0);
        track.decoder_input(5, &h, 512, 30.0);
        assert_eq!(track.refiner_hops_since_reset, 2);
        track.center = 6.0; // centroid crosses the rounding boundary 5 -> 6
        track.decoder_input(5, &h, 1024, 30.0);
        assert_eq!(
            track.refiner_hops_since_reset, 1,
            "a real centroid-channel change must reset the hop counter, not just increment it"
        );
        assert_eq!(track.refiner_channel, Some(6));
    }

    #[test]
    fn decoder_input_reports_each_hops_own_sample_ts_with_no_delay_compensation() {
        // Codex review, PR #178 (three consecutive rounds, each surfacing
        // a new duplication/fabrication shape in an attempted
        // GROUP_DELAY_HOPS-delayed pairing scheme -- see decoder_input's
        // doc comment): decoder_input reports each hop's OWN sample_ts,
        // never an earlier one, regardless of convergence state.
        let mut track = Track::new(1, 5, &cfg());
        let h = hop_with_x(0, vec![num_complex::Complex32::new(1.0, 0.0); 8]);
        for i in 0..(GROUP_DELAY_HOPS as u64 + 2) {
            let sample_ts = 1000 + i * 512;
            let (_, _, ts) = track.decoder_input(5, &h, sample_ts, 30.0);
            assert_eq!(
                ts, sample_ts,
                "hop {i} must report its OWN sample_ts, not an earlier hop's"
            );
        }
    }

    #[test]
    fn decoder_input_reports_raw_magnitude_during_warm_up_not_the_fir_ramp_in() {
        // Codex review: a freshly-reset FIR's ramp-in output is not a real
        // observation -- feeding it as evidence produced five spurious
        // negative-LLR hops on a genuinely steady tone, violating SPEC v2
        // §3's requirement that reset loss report neutral (`llr = 0`)
        // evidence. During warm-up decoder_input must report the RAW
        // magnitude (matching the disabled-refiner fallback), not
        // `Refiner::push`'s transient output.
        let mut track = Track::new(1, 5, &cfg());
        let raw = num_complex::Complex32::new(3.0, 4.0); // |raw| = 5
        let h = hop_with_x(0, vec![raw; 8]);
        for i in 0..GROUP_DELAY_HOPS as u64 {
            let (amp, _, _) = track.decoder_input(5, &h, i * 512, 30.0);
            assert_eq!(
                amp, 5.0,
                "warm-up hop {i} must report the raw magnitude, not a FIR ramp-in transient"
            );
        }
    }

    #[test]
    fn decoder_input_warm_up_does_not_replay_the_first_observation_as_later_hops() {
        // Codex review, PR #178: an earlier version of the warm-up fix
        // reported the RING's stored (oldest/front) raw magnitude on
        // every warm-up call, rather than each hop's OWN fresh
        // raw_amp -- since the front entry never advances until the ring
        // starts popping, that replayed hop 0's single observation
        // unchanged across every warm-up hop. A loud sample right after a
        // reset, followed by pure noise, would then fabricate a
        // synthetic sustained mark the real signal never produced --
        // `Demod`/HSMM evidence record every push's amplitude, not just
        // distinct timestamps, so holding the timestamp flat (a
        // deliberate, separate, and still-correct part of this fix) does
        // NOT prevent that fabrication on its own. Drive a changing
        // envelope (loud, then near-silent) through warm-up and confirm
        // each hop's reported amplitude tracks its OWN input.
        let mut track = Track::new(1, 5, &cfg());
        let loud = num_complex::Complex32::new(3.0, 4.0); // |loud| = 5
        let quiet = num_complex::Complex32::new(0.03, 0.04); // |quiet| = 0.05
        let h_loud = hop_with_x(0, vec![loud; 8]);
        let h_quiet = hop_with_x(0, vec![quiet; 8]);

        let (amp0, _, _) = track.decoder_input(5, &h_loud, 0, 30.0);
        assert_eq!(amp0, 5.0, "hop 0's own loud observation must be reported");

        for i in 1..GROUP_DELAY_HOPS as u64 {
            let (amp, _, _) = track.decoder_input(5, &h_quiet, i * 512, 30.0);
            assert_eq!(
                amp, 0.05,
                "warm-up hop {i} must report ITS OWN (quiet) observation, not hop 0's loud one \
                 replayed"
            );
        }
    }

    #[test]
    fn decoder_input_withholds_refined_amp_until_the_fir_is_fully_converged() {
        // Codex review, PR #178: forwarding refined_amp as soon as the
        // delay-pairing ring fills (GROUP_DELAY_HOPS+1 real hops) still
        // has GROUP_DELAY_HOPS zero-padded taps in the 11-tap window --
        // measured ~40% below steady-state amplitude on a constant tone.
        // Full convergence needs TAPS == 2*GROUP_DELAY_HOPS+1 real hops.
        // Verify: every reported amp up through hop `full_history_hops`
        // exactly matches the raw magnitude (never the FIR's own,
        // different, ramp-in output), and by then the ring has been
        // popping (delay-pairing already active) for several hops.
        let mut track = Track::new(1, 5, &cfg());
        let raw = num_complex::Complex32::new(3.0, 4.0); // |raw| = 5
        let h = hop_with_x(0, vec![raw; 8]);
        let full_history_hops = 2 * GROUP_DELAY_HOPS as u64 + 1;
        for i in 0..full_history_hops {
            let (amp, _, _) = track.decoder_input(5, &h, i * 512, 30.0);
            assert_eq!(
                amp, 5.0,
                "hop {i} (before full convergence at hop {full_history_hops}) must still report \
                 the raw magnitude, not a partially-converged FIR output"
            );
        }
        // One hop past full convergence: refined_amp is now safe to
        // forward. A constant real input at an even channel/hop (no odd
        // sign-correction flips) converges to something close to the raw
        // magnitude too, so assert the amplitude is finite and no longer
        // artificially forced to exactly the raw value by warm-up logic
        // -- i.e. confirm the code path actually switched, not just that
        // the numbers happen to coincide.
        let (converged_amp, _, _) = track.decoder_input(5, &h, full_history_hops * 512, 30.0);
        assert!(
            converged_amp.is_finite() && converged_amp > 0.0,
            "post-convergence amplitude must be a real, finite, positive refined value"
        );
    }

    #[test]
    fn decoder_input_raw_power_uses_the_owned_channel_k_even_when_it_differs_from_the_centroid() {
        // Codex review, PR #178: TrackDecoder's calibrated NoiseTracker
        // needs the tracked channel's real, full-bandwidth power (SPEC v2
        // §2.1), not `amp * amp` -- the refiner's narrowband output
        // integrates substantially less noise power by design (measured
        // ~8.6 dB low), so reconstructing power from it (what the old
        // push_envelope-based call site did) silently skewed noise
        // calibration whenever refinement was enabled. The detector's
        // owned/gating channel `k` and the refiner's centroid-rounded
        // channel `c` can also legitimately differ (the whole point of
        // the centroid-selection fix earlier in this file's history), so
        // this must come from `k` specifically, never from `c` or from
        // any refined value, confirmed here with `k` and `c` set to
        // channels with clearly different powers.
        let mut track = Track::new(1, 5, &cfg()); // center stays 5.0 -> c=5
        let mut samples = vec![num_complex::Complex32::new(1.0, 0.0); 8];
        samples[5] = num_complex::Complex32::new(3.0, 4.0); // channel c=5: power 25
        samples[6] = num_complex::Complex32::new(1.0, 0.0); // channel k=6: power 1
        let h = hop_with_x(0, samples);
        // k=6 passed explicitly, differing from center's rounded channel c=5.
        let (_, raw_power, _) = track.decoder_input(6, &h, 0, 30.0);
        assert_eq!(
            raw_power, 1.0,
            "raw_power must come from k=6 (power 1), not c=5 (power 25) or any refined value"
        );
    }

    #[test]
    fn decoder_input_applies_the_odd_channel_sign_correction() {
        // The real channelizer's rotation step bakes the checkerboard sign
        // artifact into `HopOutput::x[k]` itself (see
        // `channelizer::odd_channel_sign_correction`'s doc comment); a
        // synthetic `HopOutput` for a physically-constant tone must
        // reproduce that same artifact to exercise decoder_input's fix, so
        // pre-multiply by the same factor here. `decoder_input` must undo
        // it (the two factors cancel, `(-1)^2 = 1`) before the refiner
        // sees the sample -- without that, the refiner sees the raw
        // alternating input and self-cancels it (the ~39 dB attenuation
        // the Codex review measured against the real channelizer).
        let raw = num_complex::Complex32::new(1.0, 0.0);
        let mean_amp = |channel: usize| -> f32 {
            let mut track = Track::new(1, channel, &cfg());
            let mut sum = 0.0f32;
            let mut count = 0u32;
            for m in 0..200u64 {
                let artifacted = raw * odd_channel_sign_correction(m, channel);
                let h = hop_with_x(m, vec![artifacted; 8]);
                let (amp, _, _) = track.decoder_input(channel, &h, m * 512, 30.0);
                if m > 50 {
                    sum += amp;
                    count += 1;
                }
            }
            sum / count as f32
        };
        let even = mean_amp(4);
        let odd = mean_amp(5);
        assert!(
            (odd - even).abs() < 1e-3,
            "odd-channel track must match even-channel amplitude once \
             odd_channel_sign_correction is applied to decoder_input's \
             refiner feed, got odd={odd} even={even}"
        );
    }

    #[test]
    fn promotes_after_confirm_hops_of_sustained_rise() {
        let mut lc = Lifecycle::new(&cfg()); // hop 1 (birth) already counted
        for _ in 0..3 {
            assert_eq!(lc.on_hop(true, false, false), LifecycleEvent::None);
        }
        assert_eq!(lc.on_hop(true, false, false), LifecycleEvent::Promoted); // hop 5
        assert_eq!(lc.state(), LifecycleState::Active);
    }

    #[test]
    fn candidate_closes_unconfirmed_on_any_non_rise_hop() {
        let mut lc = Lifecycle::new(&cfg());
        assert_eq!(lc.on_hop(true, false, false), LifecycleEvent::None);
        assert_eq!(
            lc.on_hop(false, true, false),
            LifecycleEvent::Closed(CloseReason::Unconfirmed)
        );
    }

    #[test]
    fn active_to_hang_to_active_resets_hang_timer() {
        let mut lc = Lifecycle::new(&cfg());
        for _ in 0..3 {
            lc.on_hop(true, false, false);
        }
        assert_eq!(lc.on_hop(true, false, false), LifecycleEvent::Promoted);
        assert_eq!(lc.on_hop(false, true, true), LifecycleEvent::None); // -> HANG
        assert_eq!(lc.state(), LifecycleState::Hang);
        for _ in 0..8 {
            assert_eq!(lc.on_hop(false, true, true), LifecycleEvent::None); // still within hang_hops=10
        }
        assert_eq!(lc.on_hop(true, false, true), LifecycleEvent::None); // recovers -> ACTIVE
        assert_eq!(lc.state(), LifecycleState::Active);
    }

    #[test]
    fn hang_expires_after_hang_hops() {
        let mut lc = Lifecycle::new(&cfg());
        for _ in 0..3 {
            lc.on_hop(true, false, false);
        }
        lc.on_hop(true, false, false); // Promoted
        lc.on_hop(false, true, true); // -> HANG, hang_count=1
        for _ in 0..8 {
            assert_eq!(lc.on_hop(false, true, true), LifecycleEvent::None); // hang_count 2..9
        }
        assert_eq!(
            lc.on_hop(false, true, true),
            LifecycleEvent::Closed(CloseReason::HangExpired)
        ); // hang_count=10 >= hang_hops=10
    }

    #[test]
    fn garbage_collected_after_gc_hops_silent() {
        let mut lc = Lifecycle::new(&cfg());
        for _ in 0..3 {
            lc.on_hop(true, false, false);
        }
        lc.on_hop(true, false, false); // Promoted, ACTIVE
        for _ in 0..19 {
            assert_eq!(lc.on_hop(true, false, false), LifecycleEvent::None); // no char emitted, silent_count 1..19
        }
        assert_eq!(
            lc.on_hop(true, false, false),
            LifecycleEvent::Closed(CloseReason::Silent)
        ); // silent_count=20 >= gc_hops=20
    }

    #[test]
    fn char_emission_resets_silent_counter() {
        let mut lc = Lifecycle::new(&cfg());
        for _ in 0..3 {
            lc.on_hop(true, false, false);
        }
        lc.on_hop(true, false, false); // Promoted
        for _ in 0..19 {
            lc.on_hop(true, false, false);
        }
        assert_eq!(lc.on_hop(true, false, true), LifecycleEvent::None); // char emitted at what would be the GC boundary -- resets
        for _ in 0..19 {
            assert_eq!(lc.on_hop(true, false, false), LifecycleEvent::None);
        }
    }

    use manta_decode::decoder::DecodeConfig;
    use manta_dsp::channelizer::HopOutput;

    fn hop(m: u64, power: Vec<f32>) -> HopOutput {
        HopOutput {
            m,
            x: vec![],
            power,
        }
    }

    /// Like `hop`, but also populates the complex spectrum `x` -- needed
    /// for MAN-168's `decoder_input` tests, which read `hop.x[k]` (every
    /// other existing test only reads `hop.power`).
    fn hop_with_x(m: u64, x: Vec<num_complex::Complex32>) -> HopOutput {
        let power = x.iter().map(|c| c.norm_sqr()).collect();
        HopOutput { m, x, power }
    }

    fn quiet_power(n: usize) -> Vec<f32> {
        vec![1e-9; n] // ~ -90 dBFS
    }

    fn feed_warmup(tm: &mut TrackManager, n: usize) {
        let hops_needed = 250u64 * 15; // floor ring fill, same as manta-dsp::floor tests
        for m in 0..hops_needed {
            tm.step_hop(&hop(m, quiet_power(n)), m);
        }
    }

    #[test]
    fn spawns_and_promotes_a_track_on_a_strong_channel() {
        let mut tm = TrackManager::new(
            64,
            96_000.0,
            14_000_000.0,
            DetectorConfig::default(),
            DecodeConfig::default(),
        );
        feed_warmup(&mut tm, 64);
        let mut power = quiet_power(64);
        power[10] = 1e-9 * 10f32.powf(20.0 / 10.0); // +20 dB above the ~-90 dBFS floor
                                                    // Window is generous enough to cover the EMA settle time: at the
                                                    // on_snr_db=12.0 default the gate's tau=40ms EMA must climb ~14 hops
                                                    // from the -90 dB floor before the first rise, then confirm_hops=19
                                                    // more sustained rise hops before promotion (~33 hops total).
        let mut promoted = false;
        for m in (250 * 15)..(250 * 15 + 60) {
            tm.step_hop(&hop(m, power.clone()), m);
            if tm
                .tracks
                .values()
                .any(|t| t.state() == LifecycleState::Active)
            {
                promoted = true;
                break;
            }
        }
        assert!(
            promoted,
            "a strong channel should spawn and promote a track"
        );
        assert_eq!(tm.tracks.len(), 1);
    }

    /// `step_hop` itself must return a `TrackPromoted` event at the exact
    /// hop it promotes -- `manta_engine::doctor()`'s NoSignal check
    /// (2026-09-09) depends on this being real, ground-truth signal, not
    /// just an internal state-machine transition nothing outside
    /// `TrackManager` ever observes.
    #[test]
    fn step_hop_emits_track_promoted_at_the_promotion_hop() {
        let mut tm = TrackManager::new(
            64,
            96_000.0,
            14_000_000.0,
            DetectorConfig::default(),
            DecodeConfig::default(),
        );
        feed_warmup(&mut tm, 64);
        let mut power = quiet_power(64);
        power[10] = 1e-9 * 10f32.powf(20.0 / 10.0);
        let mut saw_promotion = false;
        for m in (250 * 15)..(250 * 15 + 60) {
            let (_, promoted, _) = tm.step_hop(&hop(m, power.clone()), m);
            if promoted
                .iter()
                .any(|e| matches!(e, DecoderEvent::TrackPromoted { .. }))
            {
                saw_promotion = true;
                break;
            }
        }
        assert!(
            saw_promotion,
            "step_hop must return a TrackPromoted event at the hop it promotes a track"
        );
    }

    /// Regression, MAN-171: a channel that stays `rise`-true forever (a
    /// stationary, never-decoding interferer -- confirmed live as a real
    /// ~8kHz-spaced SDR/USB clock-harmonic comb on RSP1B hardware) must not
    /// spawn a fresh CANDIDATE the instant its previous track closes
    /// `Silent`. Without `silent_respawn_cooldown_hops`, `rise[k]` never
    /// clearing is exactly what let such a channel spawn/promote/run
    /// `gc_hops` with zero decoded characters/close `Silent`/immediately
    /// re-spawn, forever, for the life of the process (measured live: 53%
    /// of all `TrackPromoted` events over a real 90 s capture landed on
    /// this exact grid, most points promoting 2-3 times).
    #[test]
    fn silent_close_starts_a_respawn_cooldown_on_that_channel() {
        let n = 64;
        let mut tm = TrackManager::new(
            n,
            96_000.0,
            14_000_000.0,
            DetectorConfig::default(),
            DecodeConfig::default(),
        );
        feed_warmup(&mut tm, n);
        let mut power = quiet_power(n);
        power[10] = 1e-9 * 10f32.powf(20.0 / 10.0); // +20 dB above floor, never decoded (no process_hops/drain_pool call)

        let mut m = 250u64 * 15;
        let mut promotions = 0u32;
        let mut saw_close = false;
        // Run well past a full promote -> gc_hops(11250) Silent-close cycle,
        // driving step_hop directly (bypassing process_hops/drain_pool, so
        // no char is ever decoded -- the same "never produces real content"
        // signature a stationary artifact has). A never-decoding track's
        // `has_emitted` stays false forever, so `step_hop`'s own `closed`
        // return value (MAN-19-filtered on `has_emitted`) never surfaces
        // this closure -- `tracks.len()` dropping back to 0 is the only
        // reliable signal here.
        for _ in 0..12_000 {
            let (_, promoted, _) = tm.step_hop(&hop(m, power.clone()), m);
            promotions += promoted
                .iter()
                .filter(|e| matches!(e, DecoderEvent::TrackPromoted { .. }))
                .count() as u32;
            m += 1;
            if promotions > 0 && tm.tracks.is_empty() {
                saw_close = true;
                break;
            }
        }
        assert_eq!(
            promotions, 1,
            "expected exactly one promotion before the Silent close"
        );
        assert!(
            saw_close,
            "expected the track to close (Silent) within the run window"
        );
        assert_eq!(tm.tracks.len(), 0, "the closed track must be gone");

        // Immediately after the close, the channel is STILL rise-true --
        // without the cooldown fix this respawns and promotes again inside
        // a handful of hops (confirm_hops=19). It must not.
        for _ in 0..30 {
            let (_, promoted, _) = tm.step_hop(&hop(m, power.clone()), m);
            assert!(
                promoted.is_empty(),
                "channel 10 re-promoted at hop {m}, only {} hops after its Silent close -- \
                 the post-Silent respawn cooldown did not hold",
                m - (250 * 15)
            );
            m += 1;
        }

        // Once the cooldown (silent_respawn_cooldown_hops = gc_hops =
        // 11250) has fully elapsed, the channel must be allowed to spawn
        // again -- this is a rate limit, not a permanent denylist.
        let mut repromoted = false;
        for _ in 0..(11_250 + 150) {
            let (_, promoted, _) = tm.step_hop(&hop(m, power.clone()), m);
            if promoted
                .iter()
                .any(|e| matches!(e, DecoderEvent::TrackPromoted { .. }))
            {
                repromoted = true;
                break;
            }
            m += 1;
        }
        assert!(
            repromoted,
            "channel 10 never re-promoted even after the full cooldown window elapsed"
        );
    }

    /// Regression, MAN-171 (Codex review, PR #174 round 4): `CloseReason::
    /// Silent` does not prove the RF signal actually ended -- a real, weak/
    /// marginal signal can close Silent (30s with no decoded character)
    /// while still genuinely present, or shortly before it actually ends.
    /// The respawn cooldown must not keep blocking a genuinely *different*
    /// station that shows up on the same channel once the old signal's own
    /// `rise` has dropped -- that drop is the real observed RF gap the
    /// cooldown's persistent-artifact rationale depends on. Only a channel
    /// whose `rise` never drops (a true stationary artifact) should still
    /// be blocked for the full cooldown window (covered by the sibling
    /// test above).
    #[test]
    fn respawn_cooldown_clears_once_the_channel_actually_goes_quiet() {
        let n = 64;
        let mut tm = TrackManager::new(
            n,
            96_000.0,
            14_000_000.0,
            DetectorConfig::default(),
            DecodeConfig::default(),
        );
        feed_warmup(&mut tm, n);
        let mut power = quiet_power(n);
        power[10] = 1e-9 * 10f32.powf(20.0 / 10.0); // +20 dB above floor, never decoded

        let mut m = 250u64 * 15;
        let mut promotions = 0u32;
        for _ in 0..12_000 {
            let (_, promoted, _) = tm.step_hop(&hop(m, power.clone()), m);
            promotions += promoted
                .iter()
                .filter(|e| matches!(e, DecoderEvent::TrackPromoted { .. }))
                .count() as u32;
            m += 1;
            if promotions > 0 && tm.tracks.is_empty() {
                break;
            }
        }
        assert_eq!(
            promotions, 1,
            "expected exactly one promotion before the Silent close"
        );
        assert_eq!(tm.tracks.len(), 0, "the closed track must be gone");

        // The old signal genuinely ends -- channel 10 goes quiet, `rise`
        // drops. A few hops is enough for the gate's EMA to settle below
        // threshold given the +20 dB jump.
        let quiet = quiet_power(n);
        for _ in 0..40 {
            tm.step_hop(&hop(m, quiet.clone()), m);
            m += 1;
        }

        // A genuinely different station now starts on the same channel,
        // well within what would still be the flat cooldown window (30s =
        // 11250 hops) -- it must be allowed to promote normally, not be
        // silently suppressed by a cooldown that no longer applies.
        let mut repromoted = false;
        for _ in 0..60 {
            let (_, promoted, _) = tm.step_hop(&hop(m, power.clone()), m);
            if promoted
                .iter()
                .any(|e| matches!(e, DecoderEvent::TrackPromoted { .. }))
            {
                repromoted = true;
                break;
            }
            m += 1;
        }
        assert!(
            repromoted,
            "a new signal on channel 10 was suppressed by a stale cooldown after the old \
             signal's rise genuinely dropped -- the cooldown must clear on a real RF gap"
        );
    }

    #[test]
    fn step_hop_populates_spectral_ref_power_instead_of_hard_coded_none() {
        // Codex review, PR #178: `pending`'s spectral_ref_power was
        // hard-coded `None` at both `decoder_input` push sites in
        // `step_hop`, so `NoiseTracker`'s SPEC v2 §2.2 spectral-
        // discounting branch (`max(N_temp, β·N_spec)`) never activated
        // for the edge-legacy/hsmm engines -- QRM/clicks weren't
        // discounted as the spec requires. `Engine::Hsmm` here (not
        // `DecodeConfig::default()`'s `Legacy`) is required for this
        // assertion to be meaningful post round-4's perf fix, which
        // correctly stops computing spectral_ref_power for Legacy.
        let mut tm = TrackManager::new(
            64,
            96_000.0,
            14_000_000.0,
            DetectorConfig::default(),
            DecodeConfig {
                engine: Engine::Hsmm,
                ..DecodeConfig::default()
            },
        );
        feed_warmup(&mut tm, 64);
        let mut power = quiet_power(64);
        power[10] = 1e-9 * 10f32.powf(20.0 / 10.0);
        let mut promoted_id = None;
        for m in (250 * 15)..(250 * 15 + 60) {
            let (_, promoted, _) = tm.step_hop(&hop(m, power.clone()), m);
            if let Some(id) = promoted.iter().find_map(|e| match e {
                DecoderEvent::TrackPromoted { track_id, .. } => Some(*track_id),
                _ => None,
            }) {
                promoted_id = Some(id);
                break;
            }
        }
        let id = promoted_id.expect("must promote a track within the loop");
        let track = tm.tracks.get(&id).unwrap();
        let (_, _, spectral_ref_power, _, _) = *track
            .pending
            .last()
            .expect("the promotion hop itself must have queued a pending entry");
        assert!(
            spectral_ref_power.is_some(),
            "spectral_ref_power must be populated from FloorBank, not hard-coded None"
        );
    }

    #[test]
    fn spectral_ref_power_centers_on_the_centroid_not_the_flickering_owned_channel() {
        // Codex review, PR #178 round 4: `select_channel` only ever picks
        // within the track's owned window {c-1, c, c+1} (`c =
        // round(center)`), so `k` and `c` can differ by at most one
        // channel -- but SPEC v2 §2.2's spectral reference must stay
        // centered on `c`, not wherever `k` momentarily points, or a
        // strong neighbor just inside `k`'s guard band (but outside
        // `c`'s) spuriously elevates the reference and suppresses valid
        // evidence. This reproduces Codex's own `FloorBank` probe: with
        // channels [7,8,9,13,14,15] loud (-50 dB) and everything else at
        // the -90 dB floor, c=10's neighbor set {6,7,8,12,13,14} still
        // has two quiet escapes (6, 12) and correctly reads -90 dB;
        // k=11's neighbor set {7,8,9,13,14,15} is entirely loud and would
        // (incorrectly) read -50 dB if used instead. `Engine::Hsmm` (not
        // `DecodeConfig::default()`'s `Legacy`) so round 4's perf fix
        // doesn't skip computing spectral_ref_power entirely.
        let mut tm = TrackManager::new(
            64,
            96_000.0,
            14_000_000.0,
            DetectorConfig::default(),
            DecodeConfig {
                engine: Engine::Hsmm,
                ..DecodeConfig::default()
            },
        );
        feed_warmup(&mut tm, 64);
        let mut power = quiet_power(64);
        power[10] = 1e-9 * 10f32.powf(20.0 / 10.0);
        let mut promoted_id = None;
        for m in (250 * 15)..(250 * 15 + 60) {
            let (_, promoted, _) = tm.step_hop(&hop(m, power.clone()), m);
            if let Some(id) = promoted.iter().find_map(|e| match e {
                DecoderEvent::TrackPromoted { track_id, .. } => Some(*track_id),
                _ => None,
            }) {
                promoted_id = Some(id);
                break;
            }
        }
        let id = promoted_id.expect("must promote a track within the loop");
        {
            let track = tm.tracks.get_mut(&id).unwrap();
            track.center = 10.0; // pin exactly at c=10, sidestepping interpolation drift
        }

        let floor = 1e-9f32; // -90 dB
        let loud = 1e-9 * 10f32.powf(40.0 / 10.0); // -50 dB
        let mid = 1e-9 * 10f32.powf(50.0 / 10.0); // -40 dB (channel 10)
        let strong = 1e-9 * 10f32.powf(60.0 / 10.0); // -30 dB (channel 11, the k-winner)
        let mut divergent_power = vec![floor; 64];
        for &ch in &[7usize, 8, 9, 13, 14, 15] {
            divergent_power[ch] = loud;
        }
        divergent_power[10] = mid;
        divergent_power[11] = strong; // max in owned window {9,10,11} -> k=11

        let m2 = 250 * 15 + 60;
        tm.step_hop(&hop(m2, divergent_power), m2);

        let track = tm.tracks.get(&id).unwrap();
        let (_, _, spectral_ref_power, _, _) = *track
            .pending
            .last()
            .expect("this hop must have queued a pending entry");
        let ref_db = 10.0 * spectral_ref_power.unwrap().log10();
        assert!(
            ref_db < -80.0,
            "spectral reference must stay centered on the centroid c=10 (correct: ~-90 dB), not \
             the flickering owned channel k=11 (buggy: ~-50 dB); got {ref_db} dB"
        );
    }

    #[test]
    fn step_hop_skips_spectral_ref_power_for_the_legacy_engine() {
        // Codex review, PR #178 round 4: `Engine::Legacy` never reads
        // `spectral_ref_power` (`push_envelope_legacy` doesn't accept
        // it), so computing `FloorBank::spectral_reference_db` (a
        // six-neighbor scan plus `log10`/`powf`) for every open Legacy
        // track on every hop was pure waste -- measured at ~225k-900k
        // unnecessary transcendental evaluations/sec at 300-1200 tracks.
        // `DecodeConfig::default()`'s engine is `Legacy`.
        let mut tm = TrackManager::new(
            64,
            96_000.0,
            14_000_000.0,
            DetectorConfig::default(),
            DecodeConfig::default(),
        );
        feed_warmup(&mut tm, 64);
        let mut power = quiet_power(64);
        power[10] = 1e-9 * 10f32.powf(20.0 / 10.0);
        let mut promoted_id = None;
        for m in (250 * 15)..(250 * 15 + 60) {
            let (_, promoted, _) = tm.step_hop(&hop(m, power.clone()), m);
            if let Some(id) = promoted.iter().find_map(|e| match e {
                DecoderEvent::TrackPromoted { track_id, .. } => Some(*track_id),
                _ => None,
            }) {
                promoted_id = Some(id);
                break;
            }
        }
        let id = promoted_id.expect("must promote a track within the loop");
        let track = tm.tracks.get(&id).unwrap();
        let (_, _, spectral_ref_power, _, _) = *track
            .pending
            .last()
            .expect("the promotion hop itself must have queued a pending entry");
        assert!(
            spectral_ref_power.is_none(),
            "the Legacy engine must never pay for computing spectral_ref_power"
        );
    }

    #[test]
    fn adjacent_strong_channel_is_absorbed_not_a_new_track() {
        let mut tm = TrackManager::new(
            64,
            96_000.0,
            14_000_000.0,
            DetectorConfig::default(),
            DecodeConfig::default(),
        );
        feed_warmup(&mut tm, 64);
        let mut power = quiet_power(64);
        power[10] = 1e-9 * 10f32.powf(20.0 / 10.0);
        power[11] = 1e-9 * 10f32.powf(15.0 / 10.0); // weaker neighbor, inside the owned window
        for m in (250 * 15)..(250 * 15 + 25) {
            tm.step_hop(&hop(m, power.clone()), m);
        }
        assert_eq!(
            tm.tracks.len(),
            1,
            "channel 11 is inside channel 10's owned window {{9,10,11}} and must be absorbed"
        );
    }

    #[test]
    fn two_well_separated_strong_channels_yield_two_tracks() {
        let mut tm = TrackManager::new(
            64,
            96_000.0,
            14_000_000.0,
            DetectorConfig::default(),
            DecodeConfig::default(),
        );
        feed_warmup(&mut tm, 64);
        let mut power = quiet_power(64);
        power[10] = 1e-9 * 10f32.powf(20.0 / 10.0);
        power[40] = 1e-9 * 10f32.powf(20.0 / 10.0);
        for m in (250 * 15)..(250 * 15 + 25) {
            tm.step_hop(&hop(m, power.clone()), m);
        }
        assert_eq!(tm.tracks.len(), 2);
    }

    #[test]
    fn track_cap_evicts_lowest_snr() {
        let cfg = DetectorConfig {
            track_cap: 1,
            ..DetectorConfig::default()
        };
        let mut tm = TrackManager::new(64, 96_000.0, 14_000_000.0, cfg, DecodeConfig::default());
        feed_warmup(&mut tm, 64);
        let mut power = quiet_power(64);
        power[10] = 1e-9 * 10f32.powf(20.0 / 10.0); // strong, spawns first
        for m in (250 * 15)..(250 * 15 + 25) {
            tm.step_hop(&hop(m, power.clone()), m);
        }
        assert_eq!(tm.tracks.len(), 1);
        power[40] = 1e-9 * 10f32.powf(25.0 / 10.0); // stronger second signal, over cap
        for m in (250 * 15 + 25)..(250 * 15 + 50) {
            tm.step_hop(&hop(m, power.clone()), m);
        }
        assert_eq!(
            tm.tracks.len(),
            1,
            "cap=1 must hold even with a second strong signal"
        );
        assert!(
            tm.tracks.values().next().unwrap().birth_channel == 40,
            "the lower-SNR (weaker) track must be the one evicted"
        );
        assert!(
            tm.close_counts().evicted >= 1,
            "issue #26: eviction must be counted, got {:?}",
            tm.close_counts()
        );
    }

    #[test]
    fn merge_closes_the_lower_snr_track_when_centers_converge() {
        // SPEC §2.5: "Two tracks whose centers converge within 1.0 channel
        // (interference or drift-collision) are merged: the lower-SNR
        // track is CLOSED with reason `merged`."
        //
        // A full step_hop-driven simulation (spawn two well-separated
        // tracks, then walk one track's selected channel toward the other
        // hop by hop via the real select_channel/update_centroid path) was
        // tried first and found impractical: `Track::owned`'s +/-1-channel
        // ownership window and the 1.0-channel merge threshold are
        // numerically coincident, so by the time a walked track's center is
        // within 1.0 of the target's, its *own owned window already
        // contains the target's channel* (the window flips half a channel
        // *before* the merge threshold fires). `select_channel` then picks
        // the stronger track's peak channel for *both* tracks, so their
        // `current_snr_db` reads become exactly, bit-for-bit tied at the
        // moment of merge, and the actual survivor is decided by BTreeMap
        // id order, not SNR -- and the stronger signal reliably reaches its
        // rise threshold (and so spawns, and gets its id) first, which is
        // exactly the id the tie-break closes. Confirmed empirically across
        // several parameter choices while developing this test; not a
        // tautology to route around, a real property of this design when
        // two tracks are driven onto the *same physical channel*.
        //
        // So this test exercises `merge_converged` directly, against real
        // `Track` state built the normal way (`TrackManager::spawn`, same
        // constructor path `step_hop` itself uses), with `center` then set
        // by hand to the converged pair `update_centroid` would eventually
        // produce over many hops of real (non-colliding) drift -- e.g. two
        // signals converging from opposite directions without yet sharing a
        // channel. This isolates and directly verifies the SPEC §2.5
        // "converge -> merge -> lower SNR closes" logic without depending
        // on gate/floor EMA timing or the window-overlap collision above.
        let mut tm = TrackManager::new(
            64,
            96_000.0,
            14_000_000.0,
            DetectorConfig::default(),
            DecodeConfig::default(),
        );
        tm.spawn(10);
        tm.spawn(40); // any two non-adjacent channels; overwritten below
        assert_eq!(
            tm.tracks.len(),
            2,
            "two spawns on non-adjacent channels must yield two tracks"
        );

        let mut ids: Vec<u32> = tm.tracks.keys().copied().collect();
        ids.sort();
        let (weak_id, strong_id) = (ids[0], ids[1]);
        {
            let weak = tm.tracks.get_mut(&weak_id).unwrap();
            weak.center = 20.4;
            weak.current_snr_db = 8.0;
        }
        {
            let strong = tm.tracks.get_mut(&strong_id).unwrap();
            strong.center = 21.1; // within 1.0 channel of weak's center (SPEC §2.5)
            strong.current_snr_db = 18.0;
        }

        tm.merge_converged();

        assert_eq!(
            tm.tracks.len(),
            1,
            "converged centers (within 1.0 channel) must merge to a single track (SPEC §2.5)"
        );
        let survivor = tm.tracks.values().next().unwrap();
        assert_eq!(
            survivor.current_snr_db, 18.0,
            "the higher-SNR track must survive; the lower-SNR one must be closed"
        );
        assert_eq!(survivor.center, 21.1);
        assert_eq!(
            tm.close_counts().merged,
            1,
            "issue #26: merge must be counted"
        );
    }

    /// Minimal rectangular CW envelope, one amplitude sample per HOP (not
    /// per raw sample) -- mirrors `manta_decode::decoder`'s own private
    /// test helper of the same shape, duplicated here since it isn't
    /// exported across the crate boundary. Just enough to get a
    /// `TrackDecoder`'s speed tracker ready (needs 5 marks).
    /// Real inter-character gaps included (unlike a single-character
    /// helper), so earlier characters can decode live via ordinary
    /// `push_envelope` calls -- but deliberately no final trailing gap,
    /// so the LAST character's last mark element stays genuinely "open"
    /// (matches a real mid-transmission truncation, e.g. a merge/eviction
    /// landing mid-character). Round 7: this distinction between "real
    /// gaps already observed" and "still open, no gap yet" is exactly
    /// what `finish_speed_only` must respect and `finish` legitimately
    /// may not.
    fn rect_envelope_hops(text: &str, dit_hops: u32) -> Vec<f32> {
        let mut env = Vec::new();
        let mut push = |level: f32, hops: u32| {
            for _ in 0..hops {
                env.push(level);
            }
        };
        let chars: Vec<char> = text.chars().collect();
        for (ci, c) in chars.iter().enumerate() {
            let pat = manta_decode::tree::pattern_for(*c).unwrap();
            let els: Vec<char> = pat.chars().collect();
            for (ei, e) in els.iter().enumerate() {
                push(1.0, if *e == '.' { dit_hops } else { 3 * dit_hops });
                if ei < els.len() - 1 {
                    push(0.0, dit_hops);
                }
            }
            if ci < chars.len() - 1 {
                push(0.0, 3 * dit_hops);
            }
        }
        env
    }

    /// Codex review on PR #154, round 5: a track closed via `merge_converged`
    /// previously had its `TrackDecoder` silently dropped without calling
    /// `finish()`, discarding its true final speed estimate -- exactly the
    /// state a held-back `manta-spot` Beacon-WPM candidate needs to retry
    /// against once the track legitimately closes (not just at overall
    /// stream EOF, which `TrackManager::finish` already handled).
    #[test]
    fn merge_converged_never_fabricates_a_character_from_a_dangling_mark() {
        // "PARIS" with real inter-character gaps but no final trailing
        // gap: P/A/R/I decode live during setup; "S" is genuinely still
        // open (mid-mark) when merge happens -- exactly the round-7
        // scenario (a merge/eviction landing mid-character).
        let mut tm = TrackManager::new(
            64,
            96_000.0,
            14_000_000.0,
            DetectorConfig::default(),
            DecodeConfig::default(),
        );
        tm.spawn(10);
        tm.spawn(40);
        let mut ids: Vec<u32> = tm.tracks.keys().copied().collect();
        ids.sort();
        let (weak_id, strong_id) = (ids[0], ids[1]);

        {
            let weak = tm.tracks.get_mut(&weak_id).unwrap();
            weak.center = 20.4;
            weak.current_snr_db = 8.0;
            let mut decoder = TrackDecoder::new(weak_id, DecodeConfig::default());
            for (i, &a) in rect_envelope_hops("PARIS", 18).iter().enumerate() {
                decoder.push_envelope(a, i as u64);
            }
            weak.decoder = Some(decoder);
            weak.has_emitted = true;
        }
        {
            let strong = tm.tracks.get_mut(&strong_id).unwrap();
            strong.center = 21.1;
            strong.current_snr_db = 18.0;
        }

        let (closed, flush_events) = tm.merge_converged();
        assert_eq!(
            closed,
            vec![(
                weak_id,
                ClosureKind::Bookkeeping {
                    survivor_track_id: Some(strong_id)
                }
            )],
            "merge_converged must name the surviving track (round 9) so the \
             loser's deferred evidence can be migrated to it"
        );
        // Before this fix, merge_converged called the full (forcing)
        // `finish()`, which would resolve "S"'s dangling mark into a
        // fabricated character/word that was never actually confirmed by
        // a real on-air gap.
        assert!(
            !flush_events
                .iter()
                .any(|e| matches!(e, DecoderEvent::CharDecoded { .. } | DecoderEvent::WordBoundary { .. })),
            "merge_converged must never fabricate a character/word from an in-progress mark, got {flush_events:?}"
        );
        assert!(
            flush_events.iter().all(|e| event_track_id(e) == weak_id),
            "flush events must belong to the closed track, got {flush_events:?}"
        );
    }

    /// Codex review on PR #154, round 6: `step_hop` queues each hop's
    /// sample into `track.pending`, drained through the decoder only once
    /// per `process_hops` batch via `drain_pool` -- which never runs for a
    /// track closed mid-batch (it removes the track first). Simulates
    /// that exact scenario: samples queued in `pending`, never yet fed to
    /// the decoder, at the moment of merge closure.
    #[test]
    fn merge_converged_drains_queued_pending_samples_before_finishing() {
        let mut tm = TrackManager::new(
            64,
            96_000.0,
            14_000_000.0,
            DetectorConfig::default(),
            DecodeConfig::default(),
        );
        tm.spawn(10);
        tm.spawn(40);
        let mut ids: Vec<u32> = tm.tracks.keys().copied().collect();
        ids.sort();
        let (weak_id, strong_id) = (ids[0], ids[1]);

        {
            let weak = tm.tracks.get_mut(&weak_id).unwrap();
            weak.center = 20.4;
            weak.current_snr_db = 8.0;
            weak.decoder = Some(TrackDecoder::new(weak_id, DecodeConfig::default()));
            // Queued exactly as `step_hop` would (amplitude, raw_power,
            // spectral_ref_power, sample_ts, current_snr_db) tuples in
            // `pending` -- NOT fed through push_hop yet.
            let snr = weak.current_snr_db;
            weak.pending = rect_envelope_hops("PARIS", 18)
                .into_iter()
                .enumerate()
                .map(|(i, a)| (a, a * a, None, i as u64, snr))
                .collect();
            weak.has_emitted = true;
        }
        {
            let strong = tm.tracks.get_mut(&strong_id).unwrap();
            strong.center = 21.1;
            strong.current_snr_db = 18.0;
        }

        let (closed, flush_events) = tm.merge_converged();
        assert_eq!(
            closed,
            vec![(
                weak_id,
                ClosureKind::Bookkeeping {
                    survivor_track_id: Some(strong_id)
                }
            )],
            "merge_converged must name the surviving track (round 9) so the \
             loser's deferred evidence can be migrated to it"
        );
        // Before this fix, the queued `pending` samples were discarded
        // along with the rest of the removed `Track` -- `finish()` alone
        // (on a decoder that never saw a single push_envelope call) would
        // produce nothing.
        assert!(
            !flush_events.is_empty(),
            "merge_converged must drain queued pending samples through the decoder before finishing, got nothing"
        );
    }

    /// Full-scale end-to-end detector test: a real 1024-channel, 120 s render
    /// of SPEC §7's V1 golden vector (one clean +20 dB CW signal) must yield
    /// exactly one track that decodes V1's text. Formerly `#[ignore]`d
    /// because `DetectorConfig::default()`'s literal SPEC §9 thresholds
    /// spawned 298 spurious ACTIVE tracks here; three fixes now make it pass
    /// (all recorded on `impl Default for DetectorConfig`, `Lifecycle::
    /// note_char_decoded`, and `TrackManager::step_hop`'s envelope-feed):
    /// (1) `on_snr_db` retuned 6.0 -> 12.0 against the channelizer's real
    /// per-hop variance; (2) the decoder is fed every hop while ACTIVE *or*
    /// HANG (a hole-free timeline for `TrackDecoder`'s hop-count timing);
    /// (3) the GC/silent timer is reset from actual `CharDecoded` results, so
    /// a continuously-keyed signal is not force-closed every `gc_hops` (~30 s)
    /// and re-spawned as a new track.
    ///
    /// CER is asserted `< 0.03`, not `== 0.0`: SPEC §2.1's 2 s floor warmup
    /// (`warmup_hops = 750`) inhibits all track creation for the first 2 s, so
    /// the leading ~2.66 s of the 120 s stream (warmup + confirm) is
    /// structurally unrecoverable -- a deterministic ~0.0155 CER floor
    /// (measured identical across five noise seeds), i.e. 98.4% char accuracy.
    #[test]
    fn active_track_decodes_real_text() {
        use manta_dsp::channelizer::Channelizer;
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let mut ch = Channelizer::new(spec.fs, spec.center_freq_hz).unwrap();
        let hop_samples = ch.hop() as u64;
        let mut tm = TrackManager::new(
            ch.n_channels(),
            spec.fs,
            spec.center_freq_hz,
            DetectorConfig::default(),
            DecodeConfig::default(),
        );
        let mut all_events = Vec::new();
        for chunk in rendered.samples.chunks(4096) {
            let hops = ch.process(chunk);
            all_events.extend(tm.process_hops(&hops, |m| m * hop_samples));
        }
        all_events.extend(tm.finish());
        let track_ids: std::collections::BTreeSet<u32> =
            all_events.iter().map(event_track_id).collect();
        assert_eq!(
            track_ids.len(),
            1,
            "V1 is single-signal, expected exactly 1 track"
        );
        let text = manta_decode::decoder::events_to_text(&all_events);
        let cer = manta_testkit::cer::cer(&rendered.keyed_texts[0], &text);
        assert!(
            cer < 0.02,
            "expected CER < 0.02 (2 s warmup floor ~0.0155), got {cer:.4}\nexpected {:?}\ngot      {:?}",
            rendered.keyed_texts[0],
            text
        );
    }

    /// Regression (round-7 review): an earlier fix for the same-track
    /// ordering problem below (pinning `TrackPromoted` to ts=0, like
    /// SpeedUpdate/TrackMeta) broke this instead -- a DIFFERENT track's
    /// genuinely-earlier real-timestamped `CharDecoded` must still sort
    /// before a LATER track's `TrackPromoted`, honoring `TrackPromoted`'s
    /// real, meaningful timestamp for cross-track (SPEC §6 rule 6, global)
    /// ordering.
    /// Sorts `events` exactly as `process_hops` does: builds the
    /// per-track promotion-timestamp map, then applies the same
    /// `(effective_sort_ts, track_id, event_kind_tier)` key.
    fn sort_like_process_hops(events: &mut [DecoderEvent]) {
        let promoted_ts_by_track: std::collections::HashMap<u32, u64> = events
            .iter()
            .filter(|e| matches!(e, DecoderEvent::TrackPromoted { .. }))
            .map(|e| (event_track_id(e), event_sample_ts(e)))
            .collect();
        events.sort_by_key(|e| {
            (
                effective_sort_ts(e, &promoted_ts_by_track),
                event_track_id(e),
                event_kind_tier(e),
            )
        });
    }

    #[test]
    fn sort_preserves_cross_track_chronological_order() {
        let mut events = vec![
            DecoderEvent::TrackPromoted {
                track_id: 2,
                sample_ts: 100,
                freq_hz: 14_012_340.0,
            },
            DecoderEvent::CharDecoded {
                track_id: 1,
                sample_ts: 50,
                glyph: manta_decode::tree::Glyph::Char('W'),
                confidence: 1.0,
                alternatives: Vec::new(),
            },
        ];
        sort_like_process_hops(&mut events);
        let char_idx = events
            .iter()
            .position(|e| matches!(e, DecoderEvent::CharDecoded { .. }))
            .unwrap();
        let promoted_idx = events
            .iter()
            .position(|e| matches!(e, DecoderEvent::TrackPromoted { .. }))
            .unwrap();
        assert!(
            char_idx < promoted_idx,
            "track 1's real ts=50 CharDecoded must sort before track 2's real ts=100 \
             TrackPromoted -- global chronological order, not per-track"
        );
    }

    /// Regression (round-6 review, still required after later fixes):
    /// within the SAME track, a promotion must still sort before that
    /// track's own same-batch SpeedUpdate/TrackMeta (both pinned at
    /// ts=0 by default), even though `TrackPromoted` now carries its own
    /// real, larger timestamp.
    #[test]
    fn sort_puts_a_promotion_before_its_own_tracks_pinned_events() {
        let mut events = vec![
            DecoderEvent::SpeedUpdate {
                track_id: 5,
                wpm: 20.0,
            },
            DecoderEvent::TrackPromoted {
                track_id: 5,
                sample_ts: 5000,
                freq_hz: 14_012_340.0,
            },
        ];
        sort_like_process_hops(&mut events);
        assert!(
            matches!(events[0], DecoderEvent::TrackPromoted { .. }),
            "TrackPromoted must sort before its own track's SpeedUpdate, got {events:?}"
        );
    }

    /// Regression (round-8 review): the exact scenario an earlier
    /// remove/insert-based fix for the test above got wrong -- track 2
    /// has a pinned SpeedUpdate AND is promoted (ts=100) in the same
    /// batch; track 1 has a real CharDecoded at ts=50, chronologically
    /// BETWEEN the pinned event's old ts=0 and the promotion's ts=100.
    /// The promotion must still land before its own track's SpeedUpdate,
    /// but must NOT jump across track 1's genuinely-earlier event to do
    /// it.
    #[test]
    fn sort_does_not_jump_a_promotion_across_an_earlier_unrelated_track_event() {
        let mut events = vec![
            DecoderEvent::SpeedUpdate {
                track_id: 2,
                wpm: 20.0,
            },
            DecoderEvent::CharDecoded {
                track_id: 1,
                sample_ts: 50,
                glyph: manta_decode::tree::Glyph::Char('W'),
                confidence: 1.0,
                alternatives: Vec::new(),
            },
            DecoderEvent::TrackPromoted {
                track_id: 2,
                sample_ts: 100,
                freq_hz: 14_012_340.0,
            },
        ];
        sort_like_process_hops(&mut events);
        let char1_idx = events
            .iter()
            .position(|e| matches!(e, DecoderEvent::CharDecoded { track_id: 1, .. }))
            .unwrap();
        let promoted2_idx = events
            .iter()
            .position(|e| matches!(e, DecoderEvent::TrackPromoted { track_id: 2, .. }))
            .unwrap();
        let speedupdate2_idx = events
            .iter()
            .position(|e| matches!(e, DecoderEvent::SpeedUpdate { track_id: 2, .. }))
            .unwrap();
        assert!(
            char1_idx < promoted2_idx,
            "track 1's real ts=50 CharDecoded must still sort before track 2's ts=100 \
             TrackPromoted, got {events:?}"
        );
        assert!(
            promoted2_idx < speedupdate2_idx,
            "track 2's TrackPromoted must still sort before its own track's SpeedUpdate, got \
             {events:?}"
        );
    }

    /// Regression (round-9 review): at an EQUAL effective timestamp
    /// across different tracks, `track_id` must be compared before the
    /// kind tier -- SPEC §6 rule 6 is `(sample_ts, track_id)`, not
    /// `(sample_ts, kind, track_id)`. Track 1's real ts=100 CharDecoded
    /// and track 2's ts=100 TrackPromoted tie on timestamp; track 1 must
    /// win the tie-break since 1 < 2, even though `TrackPromoted`'s own
    /// kind tier would otherwise put it first.
    #[test]
    fn sort_breaks_ties_by_track_id_before_kind() {
        let mut events = vec![
            DecoderEvent::TrackPromoted {
                track_id: 2,
                sample_ts: 100,
                freq_hz: 14_012_340.0,
            },
            DecoderEvent::CharDecoded {
                track_id: 1,
                sample_ts: 100,
                glyph: manta_decode::tree::Glyph::Char('W'),
                confidence: 1.0,
                alternatives: Vec::new(),
            },
        ];
        sort_like_process_hops(&mut events);
        assert!(
            matches!(events[0], DecoderEvent::CharDecoded { track_id: 1, .. }),
            "track 1's CharDecoded must sort before track 2's TrackPromoted at an equal \
             timestamp (track_id 1 < 2), got {events:?}"
        );
    }

    /// Regression (round-6 review): a single `process_hops` call spanning
    /// enough hops to include both a track's promotion and its first
    /// decoder-output event (a long unchunked batch -- e.g. `listen()`'s
    /// single startup-calibration `process_hops` call, or a caller feeding
    /// large chunks) must never sort that later decoder update before the
    /// `TrackPromoted` that logically preceded it.
    ///
    /// MAN-171 (Codex review, PR #174 round 1): originally rendered the
    /// full 120 s V1 vector as one unchunked batch. Since `note_char_
    /// decoded()` (the GC/silent-timer reset) only ever runs *after*
    /// `drain_pool()`, which a single-call batch defers to the very end,
    /// EVERY track promoted inside that one call was structurally doomed
    /// to close `Silent` at exactly `gc_hops` (30 s) regardless of real
    /// decode progress -- V1's real signal cycled through 4 promote/
    /// Silent-close births (at 2.06 s, 32.16 s, 62.29 s, 92.34 s) before
    /// the *file* ran out, an artifact of this test's own oversized batch
    /// with nothing to do with real decode convergence (confirmed:
    /// `manta-cli/tests/golden_v1.rs`'s `v1_passes_end_to_end_from_wav`
    /// decodes the same V1 vector through production's real 4096-sample
    /// chunking as a single, continuous track_id 1). A short (10 s, well
    /// under `gc_hops`) render still spans plenty of hops past warmup
    /// (2 s) + confirm (~50 ms) for a promotion and its own real decoder
    /// output to land in the one batch this test's actual point requires
    /// -- without ever needing a track to survive `gc_hops` unassisted.
    #[test]
    fn process_hops_orders_track_promoted_before_same_batch_decoder_updates() {
        use manta_dsp::channelizer::Channelizer;
        use manta_testkit::scene::{render_scene, SignalSpec};
        let fs = 96_000.0;
        let center_freq_hz = 14_000_000.0;
        let sig = SignalSpec {
            text: "CQ CQ DE W1AW W1AW K".into(),
            loop_text: true,
            wpm: 20.0,
            offset_hz: 12_340.0,
            snr_2500_db: 20.0,
            jitter: None,
            qsb: None,
            watterson: None,
            char_wpm: None,
            weight: 3.0,
            char_gap_units: 3.0,
            word_gap_units: 7.0,
            rise_ms: 5.0,
        };
        let (samples, _texts) =
            render_scene(std::slice::from_ref(&sig), fs, 10.0, Some(0x534B_494D_5631)).unwrap();
        let mut ch = Channelizer::new(fs, center_freq_hz).unwrap();
        let hop_samples = ch.hop() as u64;
        let mut tm = TrackManager::new(
            ch.n_channels(),
            fs,
            center_freq_hz,
            DetectorConfig::default(),
            DecodeConfig::default(),
        );
        // The whole (short) render as ONE process_hops call (not chunked
        // the way listen()'s real-time main loop feeds it) -- guarantees
        // this track's promotion and its first decoder-output event land
        // in the same returned batch, without needing a track to survive
        // a full `gc_hops` unassisted (see the doc comment above).
        let hops = ch.process(&samples);
        let events = tm.process_hops(&hops, |m| m * hop_samples);
        let promoted_idx = events
            .iter()
            .position(|e| matches!(e, DecoderEvent::TrackPromoted { .. }));
        let first_decoder_update_idx = events.iter().position(|e| {
            matches!(
                e,
                DecoderEvent::TrackMeta { .. }
                    | DecoderEvent::SpeedUpdate { .. }
                    | DecoderEvent::CharDecoded { .. }
                    | DecoderEvent::WordBoundary { .. }
            )
        });
        let (Some(promoted_idx), Some(decoder_idx)) = (promoted_idx, first_decoder_update_idx)
        else {
            panic!(
                "expected both a TrackPromoted and at least one decoder-output event in this \
                 batch -- got {events:?}"
            );
        };
        assert!(
            promoted_idx < decoder_idx,
            "TrackPromoted (index {promoted_idx}) must sort before the first decoder-output \
             event (index {decoder_idx})"
        );
    }

    /// MAN-102 review round 1, findings 2/3 (regression): `TrackMeta`'s
    /// reported SNR must not depend on the caller's `process_hops` chunk
    /// size. `decode_samples` chunks raw samples at 4096; `listen`/
    /// `soak_metrics` use their own (2048) chunking. Neither
    /// `chunking_determinism.rs` nor `channelizer_chunking_determinism.rs`
    /// reaches this layer -- both drive `TrackDecoder`/`Channelizer`
    /// directly and never construct a `TrackManager`.
    #[test]
    fn track_meta_snr_is_invariant_to_process_hops_chunk_size() {
        use manta_dsp::channelizer::Channelizer;

        fn snr_events_at_chunk_size(chunk_samples: usize) -> Vec<f32> {
            let spec = manta_testkit::vectors::v1();
            let rendered = manta_testkit::vectors::render(&spec).unwrap();
            let mut ch = Channelizer::new(spec.fs, spec.center_freq_hz).unwrap();
            let hop_samples = ch.hop() as u64;
            let mut tm = TrackManager::new(
                ch.n_channels(),
                spec.fs,
                spec.center_freq_hz,
                DetectorConfig::default(),
                DecodeConfig::default(),
            );
            let mut all_events = Vec::new();
            for chunk in rendered.samples.chunks(chunk_samples) {
                let hops = ch.process(chunk);
                all_events.extend(tm.process_hops(&hops, |m| m * hop_samples));
            }
            all_events.extend(tm.finish());
            all_events
                .into_iter()
                .filter_map(|e| match e {
                    DecoderEvent::TrackMeta { snr_2500_db, .. } => Some(snr_2500_db),
                    _ => None,
                })
                .collect()
        }

        // 4096 mirrors decode_samples's CHUNK_SAMPLES; 2048 mirrors listen/
        // soak_metrics's own chunk size -- the exact pair the review
        // finding measured diverging (3/117 TrackMetas, up to 0.29 dB).
        let at_4096 = snr_events_at_chunk_size(4096);
        let at_2048 = snr_events_at_chunk_size(2048);
        assert!(
            !at_4096.is_empty(),
            "V1 should produce at least one TrackMeta"
        );
        assert_eq!(
            at_4096, at_2048,
            "TrackMeta.snr_2500_db must not depend on the caller's process_hops chunk size"
        );
    }

    /// MAN-19 round 4: `finish()`'s own `TrackClosed` must be ordered
    /// after every other event it emits for the same track_id -- a
    /// `TrackClosed` sorted in *before* that track's final `CharDecoded`/
    /// `WordBoundary` (both carry real, positive timestamps;
    /// `TrackClosed` sorts at ts=0) would let a consumer see the closure
    /// first, free the track's state, then recreate it processing the
    /// trailing events, with nothing left to ever clean that up again --
    /// reintroducing the exact leak this mechanism exists to prevent.
    #[test]
    fn finish_orders_track_closed_after_every_other_event_for_that_track() {
        use manta_dsp::channelizer::Channelizer;
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let mut ch = Channelizer::new(spec.fs, spec.center_freq_hz).unwrap();
        let hop_samples = ch.hop() as u64;
        let mut tm = TrackManager::new(
            ch.n_channels(),
            spec.fs,
            spec.center_freq_hz,
            DetectorConfig::default(),
            DecodeConfig::default(),
        );
        for chunk in rendered.samples.chunks(4096) {
            let hops = ch.process(chunk);
            tm.process_hops(&hops, |m| m * hop_samples);
        }
        let finish_events = tm.finish();
        assert!(
            finish_events
                .iter()
                .any(|e| matches!(e, DecoderEvent::TrackClosed { .. })),
            "V1's track should still be open at EOF, so finish() should close it"
        );

        let mut closed: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
        for e in &finish_events {
            let id = event_track_id(e);
            if matches!(e, DecoderEvent::TrackClosed { .. }) {
                closed.insert(id);
            } else {
                assert!(
                    !closed.contains(&id),
                    "track {id} got a real event after its own TrackClosed -- \
                     finish() must order TrackClosed after every other event for that track"
                );
            }
        }
    }
}
