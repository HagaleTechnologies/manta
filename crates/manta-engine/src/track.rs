//! Track lifecycle state machine (SPEC §2.4) and adjacent-channel ownership
//! (SPEC §2.5), driving the decoder pool. This task covers only the
//! single-channel FSM in isolation; `TrackManager` (Task 5) wires it to
//! real per-channel floor/gate state and multi-channel ownership.

/// SPEC §9 `[detector]` table, plus ARCHITECTURE §4's track cap (not in the
/// literal SPEC table -- see the plan's Global Constraints).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DetectorConfig {
    /// Rise threshold in dB SNR. **Deviation from SPEC §9's literal 6.0 dB
    /// default (see `impl Default`).**
    pub on_snr_db: f32,
    /// SPEC §9: off (drop) threshold in dB SNR.
    pub off_snr_db: f32,
    /// SPEC §2.3/§2.4: rise sustained this many hops (~50ms) before CANDIDATE -> ACTIVE.
    pub confirm_hops: u64,
    /// SPEC §2.3/§2.4: drop sustained this many hops (5000ms) before ACTIVE/HANG -> CLOSED,
    /// or, for a track that decoded a character while qualified and
    /// `reacquire_hops > 0`, HANG -> DORMANT (MAN-108).
    pub hang_hops: u64,
    /// SPEC §2.4: no character emitted for this many hops (30000ms) -> CLOSED (garbage collect).
    pub gc_hops: u64,
    /// SPEC §2.2: track creation inhibited for this many hops (25ms) after
    /// the first real (non-pre-roll) hop. MAN-115: was 750 (2000ms).
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
    /// MAN-314 (SPEC §2.6): recognise two-tone FSK (RTTY) signals and
    /// start no CW track on them or their keying sidebands once marked.
    /// `false` restores the pre-MAN-314 behaviour exactly.
    pub rtty_filter: bool,
    /// MAN-314: how long an FSK signal must last before it is marked as
    /// RTTY, in hops. The detector works in whole 375-hop blocks
    /// (`rtty_dwell_hops / manta_dsp::fsk::BLOCK_HOPS`).
    pub rtty_dwell_hops: u64,
    /// MAN-313 (SPEC §2.8): a newly promoted track leases a decoder and
    /// emits `TrackPromoted` only once it shows CW keying; speech, noise-like
    /// wideband data and swept carriers never qualify, and a qualified track
    /// that stops keying closes `Unkeyed`. `false` restores the pre-MAN-313
    /// behaviour exactly.
    pub non_cw_filter: bool,
    /// MAN-313: with `non_cw_filter` on, both how long a promoted track may
    /// take to qualify as keyed CW (else it closes `NotCw`) and how long a
    /// qualified track may stay key-down with no keying, no decoded
    /// character and no callsign read (else it closes `Unkeyed`), in hops.
    pub unkeyed_close_hops: u64,
    /// MAN-108 (SPEC §2.4): after `hang_hops`, how long a track that decoded
    /// a character while qualified stays DORMANT -- no decoder, identity,
    /// centroid and ownership kept -- waiting to re-acquire its signal. 0
    /// restores the pre-MAN-108 close at `hang_hops` exactly.
    pub reacquire_hops: u64,
    /// MAN-312 (SPEC §2.7): what a band-wide static crash does to track
    /// starts and to running tracks' decoder input. Not in SPEC §9's
    /// original table; `[detector].crash_filter`.
    pub crash_filter: CrashFilter,
}

/// MAN-312 (SPEC §2.7): `[detector].crash_filter`. A crash (at least three
/// quarters of the passband's channels lifted 3 dB or more for 8 ms or
/// more) is detected and counted in every mode; the mode only picks what
/// `TrackManager` does about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CrashFilter {
    /// No track starts on a crash, and running tracks' decoders are held at
    /// their pre-onset input for its first `CRASH_HOLD_MAX_HOPS` hops.
    #[default]
    Full,
    /// No track starts on a crash; running tracks decode it as received.
    StartsOnly,
    /// No filtering: the event stream equals an inactive detector's.
    Off,
}

impl std::str::FromStr for CrashFilter {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "full" => Ok(CrashFilter::Full),
            "starts-only" => Ok(CrashFilter::StartsOnly),
            "off" => Ok(CrashFilter::Off),
            other => Err(format!(
                "unknown crash_filter `{other}`, expected one of `full`, `starts-only`, `off`"
            )),
        }
    }
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
    ///
    /// **`warmup_hops` deviates from SPEC v1's 750 (2 s)**, cut to 9 hops
    /// (25 ms), MAN-115, measured and pinned in
    /// `docs/DECISIONS/2026-10-11-man115-startup-latency.md`. The 2 s
    /// inhibit existed because the floor was garbage early on, and almost
    /// all of that garbage was the channelizer's zero pre-roll: hop 0 is an
    /// all-zero window (-200 dB) and hops 1-31 are partial, and a young
    /// ring's lower quartile is its minimum, so the floor sat near -140 dB
    /// (395 bogus V1 promotions at 0 ms, 391 of them inside the pre-roll).
    /// `TrackManager::skip_preroll_hops` now keeps the pre-roll out of the
    /// floor, and `FloorBank`'s 1 s block-median seed keeps a 1-4 entry
    /// ring from reading a single low chi-squared draw as the floor (0
    /// bogus promotions over 250 pure-noise seeds x 4 s at 0 ms). With
    /// both, the warm-up only has to cover the gate EMA's own first hops.
    /// 9 hops rather than 0: three golden asserts (V8w's 0-bogus, V1's
    /// 10 Hz frequency read and `spots.rs`'s report/spot frequency
    /// agreement) are single draws that move with the hop a track promotes
    /// on, and 25 ms is the value measured to clear all three; 0 ms turns
    /// V8w's garbled fading W8WLR decode into a bogus `<call> T` Beacon
    /// spot. `warmup_ms = 2000` restores the old behaviour.
    ///
    /// **`reacquire_hops` is new with MAN-108** (SPEC §2.4, not in SPEC
    /// v1's §9 table): 3750 hops = 10 s of DORMANT after the 5 s hang for a
    /// track that has decoded a character, so a Watterson-faded signal
    /// keeps one `track_id` instead of respawning per fade. A plain longer
    /// `hang_hops` was measured and rejected: it keeps one track but leaves
    /// a decoder that lost lock in the fade decoding at the wrong speed.
    /// 5 s left 2 of 18 fading scenes split; 10 s and 15 s left none. See
    /// `docs/DECISIONS/2026-10-17-man108-dormant-track-reacquisition.md`.
    /// `reacquire_ms = 0` restores the old behaviour.
    fn default() -> Self {
        DetectorConfig {
            on_snr_db: 12.0,
            off_snr_db: 3.0,
            confirm_hops: 19,
            hang_hops: 1875,
            gc_hops: 11250,
            warmup_hops: 9,
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
            crash_filter: CrashFilter::Full,
            // MAN-314: on; 7500 hops = 20 s = 20 detector blocks.
            rtty_filter: true,
            rtty_dwell_hops: 7500,
            // MAN-313: on; 3750 hops = 10 s.
            non_cw_filter: true,
            unkeyed_close_hops: 3750,
            // MAN-108: 3750 hops = 10 s of DORMANT after the hang.
            reacquire_hops: 3750,
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
    /// MAN-108 (SPEC §2.4): the hang timer expired on a track that decoded
    /// a character while qualified. No decoder; identity, centroid and
    /// ownership kept for `reacquire_hops`, waiting for the signal to come
    /// back on an owned channel.
    Dormant,
}

/// SPEC §2.4/§2.5: reason for track closure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloseReason {
    /// Rise never confirmed within `confirm_hops` (SPEC §2.4: CANDIDATE -> IDLE).
    Unconfirmed,
    /// Hang timer expired (SPEC §2.4: HANG -> CLOSED), or, MAN-108, the
    /// re-acquisition window of a DORMANT track did (DORMANT -> CLOSED).
    /// Both are an observed RF gap: `ClosureKind::SignalEnded`.
    HangExpired,
    /// No character emitted for `gc_hops` (SPEC §2.4: garbage collect).
    Silent,
    /// Converged with another track within 1.0 channel; this was the
    /// lower-SNR one (SPEC §2.5).
    Merged,
    /// Track cap exceeded; this was the lowest-current-SNR track
    /// (ARCHITECTURE §4).
    Evicted,
    /// MAN-314 (SPEC §2.6): the track sits on a marked RTTY signal's hard
    /// span.
    Rtty,
    /// MAN-313 (SPEC §2.8): the track never qualified as keyed CW within
    /// `unkeyed_close_hops` of promotion. It never held a decoder.
    NotCw,
    /// MAN-313 (SPEC §2.8): a qualified track stayed key-down for
    /// `unkeyed_close_hops` with no re-rise, no decoded character and no
    /// callsign read.
    Unkeyed,
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
    /// MAN-314: closed because the track sat on a marked RTTY signal.
    pub rtty: u64,
    /// MAN-313: closed because the track never qualified as keyed CW.
    pub not_cw: u64,
    /// MAN-313: closed because a qualified track stopped keying.
    pub unkeyed: u64,
}

impl std::ops::Add for CloseCounts {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        Self {
            unconfirmed: self.unconfirmed + rhs.unconfirmed,
            hang_expired: self.hang_expired + rhs.hang_expired,
            silent: self.silent + rhs.silent,
            merged: self.merged + rhs.merged,
            evicted: self.evicted + rhs.evicted,
            rtty: self.rtty + rhs.rtty,
            not_cw: self.not_cw + rhs.not_cw,
            unkeyed: self.unkeyed + rhs.unkeyed,
        }
    }
}

impl CloseCounts {
    fn record(&mut self, reason: CloseReason) {
        match reason {
            CloseReason::Unconfirmed => self.unconfirmed += 1,
            CloseReason::HangExpired => self.hang_expired += 1,
            CloseReason::Silent => self.silent += 1,
            CloseReason::Merged => self.merged += 1,
            CloseReason::Evicted => self.evicted += 1,
            CloseReason::Rtty => self.rtty += 1,
            CloseReason::NotCw => self.not_cw += 1,
            CloseReason::Unkeyed => self.unkeyed += 1,
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
    /// MAN-108 (SPEC §2.4): HANG -> DORMANT this hop: flush and release the
    /// decoder, keep the track.
    Dormant,
    /// MAN-108 (SPEC §2.4): DORMANT -> ACTIVE this hop: lease a fresh
    /// decoder under the same `track_id`.
    Reacquired,
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
    /// MAN-108: `DetectorConfig::reacquire_hops`.
    reacquire_hops: u64,
    /// MAN-108: latched once the track decodes a character while
    /// qualified; only such a track goes DORMANT instead of closing.
    reacquirable: bool,
    /// MAN-108: hops spent DORMANT so far.
    dormant_count: u64,
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
            reacquire_hops: cfg.reacquire_hops,
            reacquirable: false,
            dormant_count: 0,
        }
    }

    /// MAN-108 (SPEC §2.4): latch this track as worth keeping through a
    /// fade. `TrackManager::process_hops` calls it when a qualified track
    /// decodes a character, batch-granular like `note_char_decoded`.
    pub(crate) fn set_reacquirable(&mut self) {
        self.reacquirable = true;
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
                        // MAN-108: a track that has decoded keeps its
                        // identity through the fade instead of closing.
                        if self.reacquirable && self.reacquire_hops > 0 {
                            self.state = LifecycleState::Dormant;
                            self.dormant_count = 0;
                            self.confirm_count = 0;
                            return LifecycleEvent::Dormant;
                        }
                        return LifecycleEvent::Closed(CloseReason::HangExpired);
                    }
                }
                LifecycleEvent::None
            }
            // MAN-108 (SPEC §2.4): no decoder runs, so the GC timer holds.
            // Waking needs the same `confirm_hops` of sustained rise a
            // fresh CANDIDATE needs: the decoder is sensitive to the hop it
            // starts on (MAN-166), and a single-hop wake was measured to
            // lose word boundaries.
            LifecycleState::Dormant => {
                if rise {
                    self.confirm_count += 1;
                    if self.confirm_count >= self.confirm_hops {
                        self.state = LifecycleState::Active;
                        self.hang_count = 0;
                        self.silent_count = 0;
                        return LifecycleEvent::Reacquired;
                    }
                } else {
                    self.confirm_count = 0;
                }
                self.dormant_count += 1;
                if self.dormant_count >= self.reacquire_hops {
                    return LifecycleEvent::Closed(CloseReason::HangExpired);
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
use manta_dsp::comod::CoModDetector;
use manta_dsp::crash::{CrashDetector, CRASH_MIN_HOPS};
use manta_dsp::floor::{FloorBank, Gate};
use manta_dsp::fsk::{FskDetector, BLOCK_HOPS as FSK_BLOCK_HOPS};
use manta_dsp::refine::{Refiner, GROUP_DELAY_HOPS};
use smallvec::{smallvec, SmallVec};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use crate::quality::DecodeQualityObserver;

/// `(amplitude, raw_power, spectral_ref_power, sample_ts, current_snr_db,
/// freq_hz, crash)` -- one `decoder_input`/refiner-backlog entry (MAN-194;
/// `current_snr_db` added for MAN-102 compatibility -- see `decoder_input`'s
/// doc comment; `freq_hz` is MAN-211's centroid of the hop at `sample_ts`,
/// captured per entry the same way; `crash` is MAN-312's hold flag, also
/// captured per entry, see `Track::queue_input`). Named to keep
/// `SmallVec<[DecoderInputEntry; 1]>` under clippy's `type_complexity`
/// threshold, which a bare tuple-in-a-SmallVec this wide trips.
type DecoderInputEntry = (f32, f32, Option<f32>, u64, f32, f64, bool);

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
    /// current_snr_db_at_that_hop, freq_hz)` queued this hop-batch by `step_hop`,
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
    /// `freq_hz` is the track's centroid at the hop the entry describes,
    /// set on the decoder before that entry's `push_hop` so `TrackMeta`
    /// pairs it with the same hop's `sample_ts` (MAN-211).
    pending: Vec<(f32, f32, Option<f32>, u64, f32, f64)>,
    /// Set by `process_hops` once `drain_pool` has actually produced a
    /// `DecoderEvent` for this track. Distinct from `decoder.is_some()`:
    /// a track promoted and then merged/evicted within the *same*
    /// `process_hops` batch never gets a `drain_pool` pass before it's
    /// removed (that only runs once, after every hop in the batch), so it
    /// can have an allocated decoder yet have emitted nothing at all
    /// (MAN-19 review round 1). `TrackClosed` emission checks this, not
    /// decoder presence. Also set when an emitting merge loser names this
    /// track as its survivor (MAN-174): consumers may hold state for it
    /// from that moment, so it owes its own `TrackClosed`.
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
    /// `decoder_input` withholds any report at all until this reaches
    /// `GROUP_DELAY_HOPS` (there is no valid delayed observation before
    /// then), reports the raw magnitude of the due delayed hop while this
    /// is between `GROUP_DELAY_HOPS` and `TAPS`, and reports the FIR's own
    /// output once it reaches `TAPS` (the full window is real) -- see
    /// `decoder_input`'s doc comment for the complete MAN-194 protocol.
    /// Reset to 0 alongside `refiner.reset()`.
    refiner_hops_since_reset: u64,
    /// MAN-194: buffered-but-not-yet-reported `(raw_amp, raw_power,
    /// spectral_ref_power, sample_ts, current_snr_db, freq_hz, crash)`
    /// entries, oldest first, one pushed per
    /// hop fed to `refiner`. Holds at most `GROUP_DELAY_HOPS` entries at any
    /// instant once steady state is reached (one is popped the same hop one
    /// is pushed, from `refiner_hops_since_reset > GROUP_DELAY_HOPS` on).
    /// `spectral_ref_power` is captured PER ENTRY, at the instant the hop
    /// was actually observed (MAN-194 fix: local codex review found the old
    /// design -- one `spectral_ref_power` value applied uniformly to a
    /// whole drained burst by the caller, recomputed from whatever
    /// centroid happens to be current at pop/drain time -- corrupts
    /// `NoiseTracker`'s discounting for a reset-drained old channel's
    /// entries when the old and new channels' noise floors differ). Drained
    /// in full (see `drain_refiner_backlog`/`drain_refiner_into_pending`)
    /// whenever this track's refiner stops receiving new input: a channel
    /// reset (inside `decoder_input` itself), or a closure/end-of-stream
    /// (`TrackManager`, Task 3/4).
    refiner_backlog: VecDeque<DecoderInputEntry>,
    /// MAN-312 (SPEC §2.7): the last `CRASH_MIN_HOPS` crash-free
    /// `(amplitude, raw_power)` pairs queued for the decoder, oldest first.
    /// A crash is only declared after `CRASH_MIN_HOPS` lifted hops, so the
    /// front entry is the value from before the lift crossed the threshold:
    /// the one `queue_input` substitutes for every held crash entry.
    clean_hist: VecDeque<(f32, f32)>,
    /// MAN-118: how much of this track's `TrackDecoder::garble_count` has
    /// already been added to the decode-quality observer, so each drain
    /// publishes only the new garbles. See `publish_garbles`.
    garbles_published: u32,
    /// MAN-313 (SPEC §2.8): keying evidence, created at promotion when
    /// `DetectorConfig::non_cw_filter` is on; `None` otherwise (and for a
    /// CANDIDATE).
    keying: Option<KeyingEvidence>,
    /// MAN-313: this promoted track qualified as keyed CW and holds a
    /// decoder. Set at promotion when the filter is off.
    qualified: bool,
}

/// MAN-118: adds `decoder`'s garbles since the last call to `quality`, and
/// moves `published` up to match. A no-op without an observer.
fn publish_garbles(
    decoder: &TrackDecoder,
    published: &mut u32,
    quality: Option<&DecodeQualityObserver>,
) {
    if let Some(q) = quality {
        let total = decoder.garble_count;
        q.add_garbles(u64::from(total.saturating_sub(*published)));
        *published = total;
    }
}

/// MAN-118: one rail-ratio sample per track per SPEC §5 reporting interval
/// (1 Hz), on the hop `TrackMeta` fires on. Called after every queued hop's
/// `push_hop`, in `drain_pool` and both `finish_decoder*` flushes, so a
/// boundary is sampled whichever path drains it. `None` before the rails
/// initialize and for engines without them records nothing. Without an
/// observer it returns before the boundary test, so those paths pay only
/// the `Option` check per hop.
fn sample_rail_ratio_at_boundary(decoder: &TrackDecoder, quality: Option<&DecodeQualityObserver>) {
    let Some(q) = quality else {
        return;
    };
    if !decoder
        .hop_count()
        .is_multiple_of(manta_decode::META_INTERVAL_HOPS)
    {
        return;
    }
    if let Some(ratio) = decoder.rail_ratio() {
        q.observe_rail_ratio(ratio);
    }
}

/// SPEC §1.1 f(k) mapping: signed channel offset from center, FFT bin order.
fn wrapped_channel_offset(k: usize, n_channels: usize) -> f64 {
    let half = n_channels as i64 / 2;
    let signed = ((k as i64 + half).rem_euclid(n_channels as i64)) - half;
    signed as f64
}

/// MAN-313 (SPEC §2.8): a keying cycle is an ACTIVE run of at most this
/// many hops (3 s at 375 hops/s), then HANG, then a re-rise.
pub const MAX_MARK_HOPS: u64 = 1125;
/// MAN-313: ACTIVE hops a track needs since the previous classifier
/// evaluation to qualify at this one.
pub const QUALIFY_MIN_ACTIVE_HOPS: u64 = 8;
/// MAN-313: key-down hops the frequency-slope fit needs.
pub const SLOPE_MIN_SAMPLES: u64 = 20;
/// MAN-313: the largest |frequency slope| over key-down hops a qualifying
/// track may show, in Hz/s. Rejects swept carriers.
pub const MAX_SWEEP_HZ_PER_S: f64 = 75.0;
/// MAN-313: the key-down hops must span at least this many hops.
pub const KEYDOWN_SPAN_MIN_HOPS: u64 = 150;
/// MAN-313: a key-down hop is non-flat when the strongest of the channels
/// two and three away differs from the track's channel by at least this
/// many dB (either way: a much louder neighbour is a different signal).
pub const ISO_DB: f64 = 6.0;
/// MAN-313: the fraction of key-down hops that must be non-flat.
pub const ISO_MIN_FRACTION: f64 = 0.5;
/// MAN-313: the most queued decoder inputs a track awaiting qualification
/// keeps (20 s of hops, twice the default `unkeyed_close_hops`, so the
/// default never reaches it). Only a raised `unkeyed_close_ms` can: the
/// oldest entries are then dropped, and a track that qualifies that late
/// is decoded from at most 20 s back.
pub const MAX_UNQUALIFIED_BACKLOG: usize = 7500;

/// MAN-313 (SPEC §2.8): one promoted track's evidence that it is keyed CW,
/// plus the state its unkeyed close reads. Every field is integer-hop
/// driven and summed in a fixed order, so the decision is deterministic.
#[derive(Debug, Clone)]
struct KeyingEvidence {
    /// `sample_ts` of the promotion hop, carried by the lazy
    /// `TrackPromoted`.
    promo_ts: u64,
    /// Centroid frequency at promotion, carried by the lazy
    /// `TrackPromoted`.
    promo_freq_hz: f64,
    /// `hop_counter` at promotion; the `NotCw` timeout counts from here.
    promo_hop: u64,
    /// HANG -> ACTIVE transitions whose preceding ACTIVE run was at most
    /// `MAX_MARK_HOPS`.
    cycles: u64,
    /// Length of the current (or, during HANG, the last) ACTIVE run.
    active_run: u64,
    /// ACTIVE hops since the last classifier evaluation.
    active_hops_since_eval: u64,
    /// Hop of the first key-down sample; `t` in the slope fit is relative
    /// to it.
    first_keydown_hop: Option<u64>,
    /// Least-squares running sums over key-down hops: n, Σt, Σf, Σt², Σtf.
    n: u64,
    sum_t: f64,
    sum_f: f64,
    sum_tt: f64,
    sum_tf: f64,
    /// Span of the key-down samples, in hops (last t).
    t_last: u64,
    /// Key-down hops whose neighbourhood was not flat (Discovery 7).
    non_flat: u64,
    /// Hop of the promotion or the latest HANG -> ACTIVE re-rise.
    last_rise_hop: u64,
    /// `hop_counter` at the end of the latest batch that decoded a
    /// character (batch-granular).
    last_char_hop: u64,
    /// The word being decoded.
    word: String,
    /// A decoded word passed `manta_spot::grammar::is_plausible`.
    callsign_read: bool,
}

impl KeyingEvidence {
    fn new(promo_ts: u64, promo_freq_hz: f64, promo_hop: u64) -> Self {
        KeyingEvidence {
            promo_ts,
            promo_freq_hz,
            promo_hop,
            cycles: 0,
            active_run: 0,
            active_hops_since_eval: 0,
            first_keydown_hop: None,
            n: 0,
            sum_t: 0.0,
            sum_f: 0.0,
            sum_tt: 0.0,
            sum_tf: 0.0,
            t_last: 0,
            non_flat: 0,
            last_rise_hop: promo_hop,
            last_char_hop: 0,
            word: String::new(),
            callsign_read: false,
        }
    }

    /// One hop of a promoted track. `prev`/`now` are its lifecycle state
    /// before and after this hop's `on_hop`; `k` its selected channel;
    /// `keydown` whether `power_db[k]` cleared the floor by `on_snr_db`.
    /// Cycle and re-rise bookkeeping always run; the slope and flatness
    /// sums only until the track qualifies.
    #[allow(clippy::too_many_arguments)]
    fn on_hop(
        &mut self,
        hop: u64,
        prev: LifecycleState,
        now: LifecycleState,
        qualified: bool,
        k: usize,
        birth_channel: usize,
        keydown: bool,
        power: &[f32],
        power_db: &[f64],
    ) {
        // MAN-108: a DORMANT track's wake is a re-rise too, so a woken
        // track without a callsign is not closed `Unkeyed` at once.
        if matches!(prev, LifecycleState::Hang | LifecycleState::Dormant)
            && now == LifecycleState::Active
        {
            if self.active_run <= MAX_MARK_HOPS {
                self.cycles += 1;
            }
            self.active_run = 0;
            self.last_rise_hop = hop;
        }
        if now == LifecycleState::Active {
            self.active_run += 1;
            self.active_hops_since_eval += 1;
        }
        if qualified || !keydown {
            return;
        }
        let n_ch = power.len();
        let first = *self.first_keydown_hop.get_or_insert(hop);
        let t = (hop - first) as f64;
        let offset = wrapped_channel_offset((k + n_ch - birth_channel % n_ch) % n_ch, n_ch);
        let fine = interpolate_offset(
            power[(k + n_ch - 1) % n_ch],
            power[k],
            power[(k + 1) % n_ch],
        )
        .unwrap_or(0.0);
        let f = offset + fine;
        self.n += 1;
        self.sum_t += t;
        self.sum_f += f;
        self.sum_tt += t * t;
        self.sum_tf += t * f;
        self.t_last = hop - first;
        let side = [2, 3]
            .iter()
            .flat_map(|&d| [power_db[(k + d) % n_ch], power_db[(k + n_ch - d) % n_ch]])
            .fold(f64::NEG_INFINITY, f64::max);
        if (power_db[k] - side).abs() >= ISO_DB {
            self.non_flat += 1;
        }
    }

    /// Least-squares frequency slope over the key-down hops, in Hz/s.
    /// The channelizer is 4x oversampled (hop = N/4), so hops/s is
    /// `4 * spacing` (375 at 96 kHz / 1024 channels). `0.0` with fewer
    /// than two distinct times.
    fn slope_hz_s(&self, spacing_hz: f64) -> f64 {
        let n = self.n as f64;
        let denom = n * self.sum_tt - self.sum_t * self.sum_t;
        if self.n < 2 || denom <= 0.0 {
            return 0.0;
        }
        let ch_per_hop = (n * self.sum_tf - self.sum_t * self.sum_f) / denom;
        ch_per_hop * spacing_hz * (4.0 * spacing_hz)
    }

    /// Fraction of key-down hops whose neighbourhood was not flat.
    fn non_flat_fraction(&self) -> f64 {
        if self.n == 0 {
            return 0.0;
        }
        self.non_flat as f64 / self.n as f64
    }

    /// SPEC §2.8's qualification rule, checked at a classifier evaluation
    /// for a track whose centre channel is `k`.
    fn qualifies(&self, comod: &CoModDetector, k: usize, spacing_hz: f64) -> bool {
        self.cycles >= 1
            && self.active_hops_since_eval >= QUALIFY_MIN_ACTIVE_HOPS
            && !comod.in_span(k)
            && !comod.comodulated_with_span(k)
            && self.n >= SLOPE_MIN_SAMPLES
            && self.slope_hz_s(spacing_hz).abs() <= MAX_SWEEP_HZ_PER_S
            && self.t_last >= KEYDOWN_SPAN_MIN_HOPS
            && self.non_flat_fraction() >= ISO_MIN_FRACTION
    }

    /// A decoded character (`None` for a prosign) at batch-end hop `hop`.
    /// A space ends the current word.
    fn note_char(&mut self, hop: u64, c: Option<char>) {
        self.last_char_hop = hop;
        match c {
            Some(' ') => self.end_word(),
            Some(c) => {
                self.word.push(c);
                if !self.callsign_read && manta_spot::grammar::is_plausible(&self.word) {
                    self.callsign_read = true;
                }
            }
            None => self.end_word(),
        }
    }

    /// A word boundary (or a space): the current word is complete.
    fn end_word(&mut self) {
        if !self.callsign_read && manta_spot::grammar::is_plausible(&self.word) {
            self.callsign_read = true;
        }
        self.word.clear();
    }
}

/// SPEC §2.5: a live centroid EMA responsive enough to follow realistic
/// drift (V9: 50 Hz/min == ~0.0089 channels/s at 93.75 Hz spacing) with
/// negligible lag relative to the 375 Hz hop rate; tuned empirically
/// against V9 in Task 9 and pinned there.
const CENTER_EMA_ALPHA: f64 = 0.01;

/// MAN-324: one channel's state for the live-state stream's band rollup,
/// read by `TrackManager::take_channel_levels`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChannelLevel {
    /// The channel's centre, Hz, uncalibrated (the caller applies the
    /// frequency calibration).
    pub freq_hz: f64,
    /// `FloorBank::effective_floor_db` for the channel: dB of per-channel
    /// power, not calibrated to dBm.
    pub floor_db: f32,
    /// The detector's rise test passed on at least one hop since the
    /// previous take.
    pub occupied: bool,
}

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
            refiner_backlog: VecDeque::new(),
            clean_hist: VecDeque::new(),
            garbles_published: 0,
            keying: None,
            qualified: false,
        }
    }

    /// The amplitude(s) to feed this hop's decoder push, the RAW
    /// (unrefined) power of the detector's owned channel `k` for the noise
    /// tracker, and the `sample_ts` each amplitude corresponds to (MAN-168/
    /// MAN-194, SPEC v2 §3). Falls back to a single `(raw_amp, raw_power,
    /// spectral_ref_power, sample_ts)` entry from the same channel when
    /// refinement is disabled (`refine_bw_hz <= 0.0`, the default) -- so
    /// `raw_power` always matches SPEC v2 §2.1's "tracked channel"
    /// regardless of whether refinement is active. `TrackDecoder::push_hop`'s calibrated `NoiseTracker` expects
    /// the full-bandwidth channel's power; the refiner's 30 Hz (vs the
    /// channel's ~93.75 Hz) passband integrates substantially less noise
    /// power by design, so `raw_power` must NOT come from the narrowband-
    /// filtered amplitude (Codex review, PR #178: measured ~8.6 dB low).
    ///
    /// **MAN-194: returns a burst, not a single entry.** `Refiner::push`'s
    /// output at hop `t` is evidence about hop `t - GROUP_DELAY_HOPS`, not
    /// hop `t` (a property of the FIR's linear phase, not a design choice --
    /// see `refine.rs`'s module doc). A one-report-per-hop interface cannot
    /// represent "no valid delayed observation yet" or "flush everything
    /// buffered, the source just stopped" -- exactly what correct delay
    /// compensation needs at track birth, a channel reset, and end of
    /// stream. `decoder_input` instead maintains `refiner_backlog`, a small
    /// ring of not-yet-reported `(raw_amp, raw_power, spectral_ref_power,
    /// sample_ts)` entries, one pushed per real hop fed to `refiner`:
    ///
    /// - While fewer than `GROUP_DELAY_HOPS` real hops have been fed since
    ///   the last reset, nothing is popped: this call returns an empty
    ///   `SmallVec`. There is no valid delayed observation yet, and reporting a
    ///   same-hop raw substitute here (PR #178's original stopgap) is
    ///   exactly what caused a content discontinuity at the warm-up/
    ///   converged transition (MAN-194) -- reporting nothing avoids ever
    ///   needing to contradict it later.
    /// - Once `GROUP_DELAY_HOPS` real hops have been fed, exactly one entry
    ///   pops per call (steady one-report-per-hop cadence, matching the
    ///   pre-MAN-194 interface's cardinality): the due delayed hop's raw
    ///   magnitude while its FIR window is still partly zero-padded
    ///   (`refiner_hops_since_reset < TAPS`), or the FIR's own output
    ///   (`refiner.push`'s return value THIS call, which already IS the
    ///   correct value for the due delayed hop) once the window is fully
    ///   real (`refiner_hops_since_reset >= TAPS`).
    /// - A channel reset (the centroid's rounded channel changes) first
    ///   drains every entry still in `refiner_backlog` -- up to
    ///   `GROUP_DELAY_HOPS` real, already-observed hops that will never get
    ///   more future context to improve their FIR quality, reported at raw
    ///   quality -- prepended to this call's own (necessarily empty, since
    ///   the new channel's counter just reset to 0) contribution. See
    ///   `drain_refiner_backlog`/`drain_refiner_into_pending` for the same
    ///   drain used by `TrackManager` at end-of-stream and every other
    ///   closure path (Task 3/4).
    ///
    /// **MAN-194 correctness fix (local codex review):** `spectral_ref_power`
    /// is now a PARAMETER, captured into each backlog entry at push time
    /// instead of being recomputed by the caller from the CURRENT centroid
    /// when an entry is later popped/drained. This matters for correctness,
    /// not just precision: in the reset-drain case above, the entries this
    /// call drains from the OLD channel and the (empty) contribution from
    /// the NEW channel used to be returned in one `Vec` and tagged
    /// uniformly by the caller with a spectral reference computed from the
    /// NEW centroid -- wrong for the old channel's samples whenever the two
    /// channels' noise floors differ. Tagging at observation time means
    /// every entry always carries the reference that was actually correct
    /// when it was observed. Strictly more accurate even in the steady-
    /// state (non-reset) case too: previously the value was computed at
    /// DEQUEUE time from a possibly-already-drifted centroid; now it's
    /// captured at the instant each hop was truly observed.
    ///
    /// **`snr_db` (MAN-102, merged after this branch was cut) gets the same
    /// per-entry-capture treatment, for the same reason (Codex review, PR
    /// #183).** MAN-102 added a 5th field to this entry, the raw per-hop
    /// SNR `TrackManager::drain_pool`/`Track::finish_decoder*` use for
    /// peak-hold `TrackMeta` reporting. An earlier version of this branch's
    /// merge-conflict resolution paired every entry with the CALLER's
    /// current-hop `current_snr_db` at push/drain time instead of capturing
    /// it per-entry -- since a delay-compensated entry describes a hop
    /// `GROUP_DELAY_HOPS` in the past, this systematically shifted SNR
    /// evidence `GROUP_DELAY_HOPS` hops forward in EVERY steady-state push,
    /// not just on a reset: near a 375-hop `TrackMeta` reporting boundary,
    /// a handful of hops' true SNR could be attributed to the wrong side of
    /// the window. Capturing `snr_db` at push time (like `spectral_ref_power`
    /// above) closes this exactly the same way. `freq_hz` (MAN-211) is
    /// captured the same way, so a delayed or reset-drained entry reports
    /// its own hop's centroid in `TrackMeta`, not the caller's current one.
    // MAN-312: `crash` is captured per entry, like `spectral_ref_power`
    // and `snr_db` above, so it cannot be applied after the fact.
    #[allow(clippy::too_many_arguments)]
    fn decoder_input(
        &mut self,
        k: usize,
        hop: &HopOutput,
        sample_ts: u64,
        refine_bw_hz: f32,
        spectral_ref_power: Option<f32>,
        snr_db: f32,
        freq_hz: f64,
        crash: bool,
    ) -> SmallVec<[DecoderInputEntry; 1]> {
        let raw_power = hop.power[k];
        if refine_bw_hz <= 0.0 {
            return smallvec![(
                raw_power.sqrt(),
                raw_power,
                spectral_ref_power,
                sample_ts,
                snr_db,
                freq_hz,
                crash
            )];
        }
        // SPEC v2 §3 requires `c = round(c_f)`: refine the CENTROID
        // channel, not `k` (the instantaneous max-power channel used for
        // detector ownership/gating elsewhere) -- see the original MAN-168
        // wiring comment (Codex review, PR #161) for why `k` can flicker
        // independently of the true centroid.
        let c = self.center.round() as usize;
        let delta = (self.center - c as f64) as f32;

        let mut out: SmallVec<[DecoderInputEntry; 1]> = SmallVec::new();
        if self.refiner_channel != Some(c) {
            out.extend(self.refiner_backlog.drain(..));
        }

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
        // artifact on odd channels at odd hops; a phase-sensitive consumer
        // like this derotating/filtering refiner must correct it before the
        // sample reaches `Refiner::push` (measured ~39 dB of spurious
        // attenuation before this correction was applied here).
        let corrected = hop.x[c] * odd_channel_sign_correction(hop.m, c);
        let refined_amp = refiner.push(corrected);
        self.refiner_hops_since_reset += 1;
        let n = self.refiner_hops_since_reset;

        self.refiner_backlog.push_back((
            hop.power[c].sqrt(),
            raw_power,
            spectral_ref_power,
            sample_ts,
            snr_db,
            freq_hz,
            crash,
        ));
        debug_assert!(
            self.refiner_backlog.len() <= GROUP_DELAY_HOPS + 1,
            "refiner_backlog must never hold more than GROUP_DELAY_HOPS + 1 entries \
             (one push then one pop per call once steady state is reached)"
        );

        let d = GROUP_DELAY_HOPS as u64;
        if n > d {
            let (
                due_raw_amp,
                due_raw_power,
                due_spectral_ref_power,
                due_ts,
                due_snr_db,
                due_freq_hz,
                due_crash,
            ) = self.refiner_backlog.pop_front().expect(
                "backlog must hold an entry once n > GROUP_DELAY_HOPS: exactly one is \
                     pushed per call and none are popped until this threshold",
            );
            let full_history_hops = 2 * d + 1; // == TAPS
            let amp = if n >= full_history_hops {
                refined_amp
            } else {
                due_raw_amp
            };
            out.push((
                amp,
                due_raw_power,
                due_spectral_ref_power,
                due_ts,
                due_snr_db,
                due_freq_hz,
                due_crash,
            ));
        }
        out
    }

    /// MAN-194: pop every buffered-but-undelivered `(raw_amp, raw_power,
    /// spectral_ref_power, sample_ts, current_snr_db, freq_hz, crash)`
    /// entry, in order, at raw quality --
    /// used both when `decoder_input` detects a channel reset (folded
    /// transparently into its own returned `SmallVec`, above) and by
    /// `TrackManager` at every other point this track's refiner stops
    /// receiving new input (see `drain_refiner_into_pending`). No attempt is
    /// made to improve FIR quality for these: no more real input is ever
    /// coming from this source, so the window can never become more real
    /// than it already is.
    fn drain_refiner_backlog(&mut self) -> Vec<DecoderInputEntry> {
        self.refiner_backlog.drain(..).collect()
    }

    /// MAN-194: `TrackManager`-facing wrapper around `drain_refiner_backlog`
    /// that appends every drained entry straight into `pending`. Takes no
    /// parameters: each backlog entry already carries its OWN
    /// `spectral_ref_power`, `current_snr_db` and `freq_hz` (MAN-211), all
    /// captured by `decoder_input` at the instant that hop was actually
    /// observed (Codex
    /// review, PR #183: an earlier version of this fix applied `snr_db`
    /// uniformly at drain/dequeue time instead of capturing it per-entry --
    /// exactly the bug already fixed for `spectral_ref_power`, just not yet
    /// extended to MAN-102's field when it merged in). `pending`'s tuple
    /// shape now matches the backlog's exactly, so there is no per-entry
    /// re-tagging left to do here at all. Called at every point a track's
    /// refiner stops receiving new input: `TrackManager::step_hop`'s
    /// closure handling (below), `merge_converged`, `evict_over_cap`, and
    /// `TrackManager::finish` (Task 4) -- NOT inside `decoder_input`'s own
    /// reset handling, which folds its drain directly into its own returned
    /// `SmallVec` instead.
    fn drain_refiner_into_pending(&mut self) {
        for e in self.drain_refiner_backlog() {
            self.queue_input(e);
        }
    }

    /// MAN-312 (SPEC §2.7): queue one decoder input entry onto `pending`,
    /// applying `crash_filter = "full"`'s decoder hold. An entry observed
    /// on a held crash hop (its `crash` flag, captured when the hop was
    /// observed so it stays right under the MAN-194 refiner delay) keeps
    /// its timestamp and SNR but takes `(amplitude, raw_power)` from
    /// before the band-wide lift crossed the threshold: the channelizer
    /// smears a crash over every channel for tens of hops, and the decoder
    /// would otherwise read that energy as a mark. The pre-onset value,
    /// not zero, so a crash inside a mark does not split it into two.
    fn queue_input(&mut self, e: DecoderInputEntry) {
        let (mut amp, mut raw_power, spectral_ref_power, sample_ts, snr_db, freq_hz, crash) = e;
        if crash {
            if let Some(&(a, p)) = self.clean_hist.front() {
                amp = a;
                raw_power = p;
            }
        } else {
            self.clean_hist.push_back((amp, raw_power));
            if self.clean_hist.len() > CRASH_MIN_HOPS as usize {
                self.clean_hist.pop_front();
            }
        }
        self.pending.push((
            amp,
            raw_power,
            spectral_ref_power,
            sample_ts,
            snr_db,
            freq_hz,
        ));
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
    ///
    /// MAN-118: garbles the flush produces, and the rail ratio at each
    /// reporting boundary it crosses, are published to `quality`.
    fn finish_decoder(&mut self, quality: Option<&DecodeQualityObserver>) -> Vec<DecoderEvent> {
        let Some(decoder) = self.decoder.as_mut() else {
            return Vec::new();
        };
        let pending = std::mem::take(&mut self.pending);
        // MAN-102: same peak-hold/reset-at-reporting-boundary pattern as
        // `TrackManager::drain_pool` -- see that function's doc comment
        // (review round 2, finding 2: keyed to `hop_count() %
        // META_INTERVAL_HOPS == 0`, not to `TrackMeta` actually emitting).
        let mut events: Vec<DecoderEvent> = Vec::new();
        for (amp, raw_power, spectral_ref_power, ts, snr_db, freq_hz) in pending {
            self.snr_peak_db = self.snr_peak_db.max(snr_db);
            decoder.set_snr_2500_db(self.snr_peak_db - manta_decode::SNR_BW_CORR_DB);
            decoder.set_freq_hz(freq_hz);
            let hop_events = decoder.push_hop(amp, raw_power, spectral_ref_power, ts);
            if decoder.hop_count() % manta_decode::META_INTERVAL_HOPS == 0 {
                self.snr_peak_db = f32::NEG_INFINITY;
            }
            sample_rail_ratio_at_boundary(decoder, quality);
            events.extend(hop_events);
        }
        events.extend(decoder.finish());
        publish_garbles(decoder, &mut self.garbles_published, quality);
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
    ///
    /// MAN-118: garbles the flush produces, and the rail ratio at each
    /// reporting boundary it crosses, are published to `quality`.
    fn finish_decoder_speed_only(
        &mut self,
        quality: Option<&DecodeQualityObserver>,
    ) -> Vec<DecoderEvent> {
        let Some(decoder) = self.decoder.as_mut() else {
            return Vec::new();
        };
        let pending = std::mem::take(&mut self.pending);
        // MAN-102: same peak-hold/reset-at-reporting-boundary pattern as
        // `TrackManager::drain_pool` -- see that function's doc comment
        // (review round 2, finding 2: keyed to `hop_count() %
        // META_INTERVAL_HOPS == 0`, not to `TrackMeta` actually emitting).
        let mut events: Vec<DecoderEvent> = Vec::new();
        for (amp, raw_power, spectral_ref_power, ts, snr_db, freq_hz) in pending {
            self.snr_peak_db = self.snr_peak_db.max(snr_db);
            decoder.set_snr_2500_db(self.snr_peak_db - manta_decode::SNR_BW_CORR_DB);
            decoder.set_freq_hz(freq_hz);
            let hop_events = decoder.push_hop(amp, raw_power, spectral_ref_power, ts);
            if decoder.hop_count() % manta_decode::META_INTERVAL_HOPS == 0 {
                self.snr_peak_db = f32::NEG_INFINITY;
            }
            sample_rail_ratio_at_boundary(decoder, quality);
            events.extend(hop_events);
        }
        events.extend(decoder.finish_speed_only());
        publish_garbles(decoder, &mut self.garbles_published, quality);
        events
    }

    /// MAN-108 (SPEC §2.4): HANG -> DORMANT. Flushes the decoder exactly as
    /// a `HangExpired` close does (refiner backlog drained, then the
    /// forcing `finish_decoder`: the hang is an observed RF gap) and drops
    /// it, resetting the per-decoder state a fresh decoder must not
    /// inherit. Identity, centroid, ownership, keying evidence and
    /// `has_emitted` are kept. Returns the flush.
    fn go_dormant(&mut self, quality: Option<&DecodeQualityObserver>) -> Vec<DecoderEvent> {
        self.drain_refiner_into_pending();
        let events = self.finish_decoder(quality);
        self.decoder = None;
        self.pending.clear();
        self.refiner = None;
        self.refiner_channel = None;
        self.refiner_hops_since_reset = 0;
        self.clean_hist.clear();
        self.snr_peak_db = f32::NEG_INFINITY;
        self.garbles_published = 0;
        events
    }

    /// MAN-108 (SPEC §2.4): DORMANT -> ACTIVE. Leases a fresh decoder under
    /// the same `id`, deliberately not the old one's timing: a decoder that
    /// lost lock in the fade was measured to stay at the wrong speed.
    fn reacquire(&mut self, id: u32, cfg: &DecodeConfig, freq_hz: f64) {
        let mut decoder = TrackDecoder::new(id, cfg.clone());
        decoder.set_freq_hz(freq_hz);
        self.decoder = Some(decoder);
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

    /// MAN-197 (Codex review, PR #301): the live-track counterpart of the
    /// spawn gate's `spawn_window_cooling_down`. A candidate born two bins
    /// from a cooling channel passes the spawn gate legitimately, yet a
    /// stronger neighbour can drag `center` across the rounding boundary
    /// until `owned` includes that channel -- so a live track's max-power
    /// selection must skip channels still serving a post-`Silent`
    /// cooldown, or the track keeps decoding the silenced birdie for a
    /// whole extra GC cycle inside the cooldown window. Identical to
    /// `select_channel` unless the max-power owned channel is cooling
    /// (only the absorbed-birdie case changes). Falls back to the plain
    /// max-power channel when *every* owned channel is cooling (possible
    /// only for a track that predates the close or drifted fully onto the
    /// cooldown window; the spawn gate blocks fresh births there).
    fn select_live_channel(
        &self,
        power: &[f32],
        n_channels: usize,
        channel_cooldown_until: &[u64],
        hop_counter: u64,
    ) -> usize {
        let plain = self.select_channel(power, n_channels);
        if channel_cooldown_until[plain] <= hop_counter {
            return plain;
        }
        self.owned(n_channels)
            .into_iter()
            .filter(|&ch| channel_cooldown_until[ch] <= hop_counter)
            .max_by(|&a, &b| power[a].partial_cmp(&power[b]).unwrap())
            .unwrap_or(plain)
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
    /// MAN-13 (D3): the first id this manager's input owns and the step
    /// between its ids (`crate::multi::TrackIdSpace`). `1`/`1` with one
    /// input, today's numbering.
    id_first: u32,
    id_stride: u32,
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
    /// MAN-115: channelizer hops with `m` below this are the zero-padding
    /// pre-roll and are skipped by `process_hops`. See `skip_preroll_hops`.
    preroll_hops: u64,
    /// MAN-118: where garbles and per-track rail ratios are published.
    /// `None` (the default) skips both. See `set_quality_observer`.
    quality: Option<Arc<DecodeQualityObserver>>,
    /// MAN-314 (SPEC §2.6): the RTTY detector, `None` when
    /// `DetectorConfig::rtty_filter` is off.
    fsk: Option<FskDetector>,
    /// MAN-314: every RTTY signal ever marked, in marking order (the
    /// oldest cleared entries are dropped past `RTTY_LOG_CAP` while
    /// `rtty_log_capped`).
    rtty_log: Vec<RttyLogEntry>,
    /// MAN-314: apply `RTTY_LOG_CAP` to `rtty_log`; true unless
    /// `keep_full_rtty_history` was called.
    rtty_log_capped: bool,
    /// MAN-314: bumped whenever a block changes the marked set or a marked
    /// signal's metadata.
    rtty_changes: u64,
    /// MAN-314: sample timestamp of the hop whose block made the latest
    /// `rtty_changes` bump; 0 before the first.
    rtty_changed_sample_ts: u64,
    /// MAN-313 (SPEC §2.8): the band co-modulation classifier, `None` when
    /// `DetectorConfig::non_cw_filter` is off.
    comod: Option<CoModDetector>,
    /// MAN-313: per-hop gate-smoothed SNR handed to `comod`, reused.
    comod_snr_buf: Vec<f64>,
    /// MAN-313: every CANDIDATE -> ACTIVE confirmation so far, qualified as
    /// keyed CW or not. Never reset. See `confirmations`.
    confirmations: u64,
    /// MAN-324: per channel, whether the rise test passed on any hop since
    /// the last `take_channel_levels`. `None` (the default) skips it; no
    /// decode logic reads it.
    rise_seen: Option<Vec<bool>>,
    /// MAN-324: per channel, whether its centre lies inside the source's
    /// RF passband, for `take_channel_levels`. `None` (full Nyquist band,
    /// the default) reports every channel.
    rf_channels: Option<Vec<bool>>,
    /// MAN-196: per channel, whether its centre lies inside the source's
    /// trusted band -- the band inside which the source's own filtering
    /// keeps channels within 1 dB of flat (a decimator's transition band
    /// beyond it corrupts frequency estimates, spawns duplicate tracks
    /// and folds above-Nyquist carriers into promotions). `None` (the
    /// default, no boundary supplied) vetoes nothing, so every
    /// non-decimated source is behaviour-identical to before. Read by the
    /// spawn scan ONLY (`trusted_vetoes_spawn`): never a close path,
    /// never a decode hold -- the MAN-314 key-up-gap lesson.
    trusted_channels: Option<Vec<bool>>,
    /// MAN-312 (SPEC §2.7): the band-wide static-crash detector. Every
    /// channel is live until `set_rf_passband_hz` narrows it to the
    /// source's real passband.
    crash: CrashDetector,
    /// MAN-108 (SPEC §2.4): the flush of every decoder released at
    /// HANG -> DORMANT since the last `process_hops` drained it. Kept apart
    /// from `drain_pool`'s output and placed ahead of it, so the old
    /// decoder's events (a `SpeedUpdate` sorts at ts 0) cannot sort after a
    /// same-batch woken decoder's under the batch's stable sort.
    dormant_flush: Vec<DecoderEvent>,
    /// MAN-108: DORMANT -> ACTIVE re-acquisitions so far. See
    /// `reacquisition_count`; counted in test builds only.
    #[cfg(test)]
    reacquisitions: u64,
}

/// MAN-312 (SPEC §2.7): the channels inside `rf_passband_hz` with one
/// channel of margin at each edge, or every channel when the passband
/// covers the full Nyquist band (the `IqSource` default; file, Soapy and
/// HPSDR sources; `DecimatingSource`'s clamp). A band-wide event has to be
/// judged where the source carries RF: outside a KiwiSDR's ±5 kHz the
/// channels hold only the stream's residual noise, which a crash never
/// lifts, so an all-channel quartile would miss every crash there.
fn crash_live_set(
    sample_rate_hz: f64,
    rf_passband_hz: (f64, f64),
    n_channels: usize,
) -> Vec<usize> {
    let (lo, hi) = rf_passband_hz;
    if lo <= -sample_rate_hz / 2.0 && hi >= sample_rate_hz / 2.0 {
        return (0..n_channels).collect();
    }
    let spacing = sample_rate_hz / n_channels as f64;
    (0..n_channels)
        .filter(|&k| {
            let f = wrapped_channel_offset(k, n_channels) * spacing;
            f >= lo + spacing && f <= hi - spacing
        })
        .collect()
}

/// MAN-312 (SPEC §2.7): how many channels the static-crash detector would
/// judge for a source at `sample_rate_hz` reporting `rf_passband_hz` --
/// the rule `TrackManager::set_rf_passband_hz` applies. Below
/// `CRASH_MIN_LIVE_CHANNELS` the detector is inactive; `manta run` uses
/// this to say so once at start-up.
pub fn crash_live_channels(sample_rate_hz: f64, rf_passband_hz: (f64, f64)) -> usize {
    let n_channels =
        ((sample_rate_hz / manta_dsp::channelizer::CHANNEL_SPACING_HZ).round() as usize).max(1);
    crash_live_set(sample_rate_hz, rf_passband_hz, n_channels).len()
}

/// MAN-314: bound on `TrackManager::rtty_signals` for a long-running
/// daemon; past it the oldest cleared entries are dropped. Not applied
/// after `TrackManager::keep_full_rtty_history`.
const RTTY_LOG_CAP: usize = 4096;

/// MAN-314: one RTTY signal the detector marked (SPEC §2.6). Frequencies
/// are absolute Hz before any ppm calibration.
#[derive(Debug, Clone, PartialEq)]
pub struct RttyLogEntry {
    /// `manta_dsp::fsk::FskSignal::id`, stable while the signal is marked.
    pub id: u32,
    /// Upper tone.
    pub mark_hz: f64,
    /// Lower tone.
    pub space_hz: f64,
    /// Nearest standard shift (170/200/425/450/850 Hz).
    pub shift_hz: f64,
    /// `mark_hz - space_hz` as measured.
    pub measured_shift_hz: f64,
    /// Sample timestamp of the hop that marked it.
    pub marked_sample_ts: u64,
    /// Sample timestamp of the hop that cleared it; `None` while marked.
    pub cleared_sample_ts: Option<u64>,
}

/// MAN-196: per-channel mask, true iff the channel's centre
/// (`wrapped_channel_offset(k, n) * spacing`, SPEC §1.1 f(k)) lies inside
/// `[lo, hi]`. The shared body of `set_rf_passband_hz`'s reporting mask
/// and `set_trusted_band_hz`'s veto mask (the crash set keeps its own
/// margin-channel variant in `crash_live_set`: a band-wide quartile
/// judgment wants one channel of margin, a per-channel centre test does
/// not).
fn channels_within(channel_spacing_hz: f64, lo: f64, hi: f64, n: usize) -> Vec<bool> {
    (0..n)
        .map(|k| {
            let f = wrapped_channel_offset(k, n) * channel_spacing_hz;
            f >= lo && f <= hi
        })
        .collect()
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
            id_first: 1,
            id_stride: 1,
            cfg: detector_cfg,
            decode_cfg,
            hop_counter: 0,
            center_freq_hz,
            channel_spacing_hz: fs / n_channels as f64,
            close_counts: CloseCounts::default(),
            channel_cooldown_until: vec![0; n_channels],
            preroll_hops: 0,
            quality: None,
            fsk: detector_cfg.rtty_filter.then(|| {
                let blocks = detector_cfg.rtty_dwell_hops / FSK_BLOCK_HOPS as u64;
                FskDetector::new(n_channels, u32::try_from(blocks).unwrap_or(u32::MAX))
            }),
            rtty_log: Vec::new(),
            rtty_log_capped: true,
            rtty_changes: 0,
            rtty_changed_sample_ts: 0,
            rise_seen: None,
            rf_channels: None,
            trusted_channels: None,
            crash: CrashDetector::new(n_channels),
            dormant_flush: Vec::new(),
            #[cfg(test)]
            reacquisitions: 0,
            comod: detector_cfg
                .non_cw_filter
                .then(|| CoModDetector::new(n_channels)),
            comod_snr_buf: Vec::with_capacity(n_channels),
            confirmations: 0,
        }
    }

    /// MAN-313: how many tracks the detector has confirmed (CANDIDATE ->
    /// ACTIVE) so far, whether or not they went on to qualify as keyed CW
    /// and emit `TrackPromoted`. Doctor reads it to tell "signals, but none
    /// keyed CW" (`NotCw`) apart from "no signal at all".
    pub fn confirmations(&self) -> u64 {
        self.confirmations
    }

    /// MAN-324: start recording, per channel, whether the rise test passed
    /// on any hop, for `take_channel_levels`' `occupied`. Write-only from
    /// the detector's point of view, so events are unchanged.
    pub fn enable_occupancy_window(&mut self) {
        self.rise_seen = Some(vec![false; self.n_channels()]);
    }

    /// MAN-324: every channel's centre (raw Hz), effective noise floor and
    /// occupancy since the previous take, ascending in frequency. Only
    /// channels whose centre lies inside the source's RF passband
    /// (`set_rf_passband_hz`), so band state never describes spectrum the
    /// source does not deliver. Resets the occupancy window. `occupied` is
    /// always false unless `enable_occupancy_window` was called.
    pub fn take_channel_levels(&mut self) -> Vec<ChannelLevel> {
        let n = self.n_channels();
        let mut levels: Vec<ChannelLevel> = (0..n)
            .filter(|&k| self.rf_channels.as_ref().is_none_or(|r| r[k]))
            .map(|k| ChannelLevel {
                freq_hz: self.center_freq_hz
                    + wrapped_channel_offset(k, n) * self.channel_spacing_hz,
                floor_db: self.floor.effective_floor_db(k) as f32,
                occupied: self.rise_seen.as_ref().is_some_and(|r| r[k]),
            })
            .collect();
        levels.sort_by(|a, b| a.freq_hz.total_cmp(&b.freq_hz));
        if let Some(r) = self.rise_seen.as_mut() {
            r.iter_mut().for_each(|x| *x = false);
        }
        levels
    }

    /// Whether a track is still open, including a bare promotion that
    /// will not receive TrackClosed. Used to retire live-tap state.
    pub(crate) fn contains_track(&self, track_id: u32) -> bool {
        self.tracks.contains_key(&track_id)
    }

    /// MAN-324: the raw frequency of every track holding a decoder (the
    /// tracks `decoding_track_count` counts), in track-id order.
    pub fn decoding_track_freqs_hz(&self) -> Vec<f64> {
        let n = self.n_channels();
        self.tracks
            .values()
            .filter(|t| t.decoder.is_some())
            .map(|t| t.freq_hz(self.center_freq_hz, self.channel_spacing_hz, n))
            .collect()
    }

    /// MAN-312 (SPEC §2.7): judge static crashes over the channels inside
    /// the source's real RF passband (`IqSource::rf_passband_hz`, Hz
    /// relative to the centre). See `crash_live_set` for the rule.
    pub fn set_rf_passband_hz(&mut self, lo: f64, hi: f64) {
        let n = self.n_channels();
        let fs = self.channel_spacing_hz * n as f64;
        self.crash.set_live(crash_live_set(fs, (lo, hi), n));
        // MAN-324: live band state covers the delivered passband only.
        self.rf_channels = if lo <= -fs / 2.0 && hi >= fs / 2.0 {
            None
        } else {
            Some(channels_within(self.channel_spacing_hz, lo, hi, n))
        };
    }

    /// MAN-196: never spawn a candidate on a channel whose centre lies
    /// outside `[lo, hi]` (Hz relative to the centre) -- the source's
    /// trusted band, the band inside which its own filtering keeps
    /// channels within 1 dB of flat. `listen` supplies it from
    /// `IqSource::trusted_band_hz`, exactly as it supplies the RF
    /// passband above. **Spawns only**: no close path, no decode hold --
    /// a track that already exists on an untrusted channel keeps
    /// decoding (the MAN-314 splatter-shadow lesson: applying a veto to
    /// running tracks killed real CW in key-up gaps). A band covering
    /// the full Nyquist clears the mask, making the call a no-op --
    /// pinning that no-op is `unset_trusted_band_is_bit_identical_to_today`.
    pub fn set_trusted_band_hz(&mut self, lo: f64, hi: f64) {
        let n = self.n_channels();
        let fs = self.channel_spacing_hz * n as f64;
        self.trusted_channels = if lo <= -fs / 2.0 && hi >= fs / 2.0 {
            None
        } else {
            Some(channels_within(self.channel_spacing_hz, lo, hi, n))
        };
    }

    /// MAN-196: may no candidate spawn on channel `k`? True when a
    /// trusted band was supplied and `k`'s centre lies outside it. The
    /// RTTY-veto slot's shape (`rtty_vetoes_spawn`); consulted by the
    /// spawn scan only.
    fn trusted_vetoes_spawn(&self, k: usize) -> bool {
        self.trusted_channels.as_ref().is_some_and(|t| !t[k])
    }

    /// MAN-312: static crashes detected by this manager so far, in every
    /// `crash_filter` mode (including `"off"`).
    pub fn static_crash_count(&self) -> u64 {
        self.crash.count()
    }

    /// MAN-314: how many blocks changed the marked RTTY set or a marked
    /// signal's metadata; a consumer re-reads `rtty_signals` when this
    /// moves.
    pub fn rtty_changes(&self) -> u64 {
        self.rtty_changes
    }

    /// MAN-314: sample timestamp of the hop whose block made the latest
    /// `rtty_changes` bump, so the stream time at which `rtty_signals`
    /// last changed; 0 before the first change.
    pub fn rtty_changed_sample_ts(&self) -> u64 {
        self.rtty_changed_sample_ts
    }

    /// MAN-314: every RTTY signal marked so far (SPEC §2.6), in marking
    /// order; the currently marked set is the entries whose
    /// `cleared_sample_ts` is `None`. Past `RTTY_LOG_CAP` entries the
    /// oldest cleared ones are dropped, unless `keep_full_rtty_history`
    /// was called.
    pub fn rtty_signals(&self) -> &[RttyLogEntry] {
        &self.rtty_log
    }

    /// MAN-314: keep every `rtty_signals` entry instead of dropping the
    /// oldest cleared ones past `RTTY_LOG_CAP`. For `decode_samples`,
    /// whose `DecodeReport::rtty` lists every marked signal of a finite
    /// input; a stream keeps the cap. Call it before the first
    /// `process_hops`: entries already dropped are not restored.
    pub fn keep_full_rtty_history(&mut self) {
        self.rtty_log_capped = false;
    }

    /// MAN-314: sync `rtty_log` with the detector's marked set after a
    /// block that changed it.
    fn update_rtty_log(&mut self, sample_ts: u64) {
        let Some(fsk) = self.fsk.as_ref() else {
            return;
        };
        let n = self.owner_of.len() as f64;
        let to_hz =
            |j: f64| self.center_freq_hz + (j - (n / 2.0).floor()) * self.channel_spacing_hz;
        for e in self.rtty_log.iter_mut() {
            if e.cleared_sample_ts.is_none() && !fsk.signals().iter().any(|s| s.id == e.id) {
                e.cleared_sample_ts = Some(sample_ts);
            }
        }
        for s in fsk.signals() {
            let mark_hz = to_hz(s.high_j);
            let space_hz = to_hz(s.low_j);
            match self
                .rtty_log
                .iter_mut()
                .find(|e| e.id == s.id && e.cleared_sample_ts.is_none())
            {
                Some(e) => {
                    e.mark_hz = mark_hz;
                    e.space_hz = space_hz;
                    e.shift_hz = s.nominal_shift_hz;
                    e.measured_shift_hz = s.measured_shift_hz;
                }
                None => self.rtty_log.push(RttyLogEntry {
                    id: s.id,
                    mark_hz,
                    space_hz,
                    shift_hz: s.nominal_shift_hz,
                    measured_shift_hz: s.measured_shift_hz,
                    marked_sample_ts: sample_ts,
                    cleared_sample_ts: None,
                }),
            }
        }
        while self.rtty_log_capped && self.rtty_log.len() > RTTY_LOG_CAP {
            match self
                .rtty_log
                .iter()
                .position(|e| e.cleared_sample_ts.is_some())
            {
                Some(i) => {
                    self.rtty_log.remove(i);
                }
                None => break,
            }
        }
        self.rtty_changes += 1;
        self.rtty_changed_sample_ts = sample_ts;
    }

    /// MAN-313 (SPEC §2.8): may no CW candidate spawn on channel `k`? True
    /// inside a marked co-modulated (speech) span.
    fn comod_vetoes_spawn(&self, k: usize) -> bool {
        self.comod.as_ref().is_some_and(|c| c.in_span(k))
    }

    /// MAN-314 (SPEC §2.6): may no CW candidate spawn on channel `k`? True
    /// inside a marked RTTY signal's hard span, or inside its shadow below
    /// the shadow depth.
    fn rtty_vetoes_spawn(&self, k: usize) -> bool {
        let Some(fsk) = self.fsk.as_ref() else {
            return false;
        };
        fsk.in_hard_span(k)
            || fsk.shadow_vetoes_spawn(
                k,
                self.gate.smoothed_db(k) - self.floor.effective_floor_db(k),
            )
    }

    fn n_channels(&self) -> usize {
        self.owner_of.len()
    }

    /// The id the next spawned track will receive. MAN-73: used by
    /// `listen()`'s segment restart to carry the id sequence across a
    /// discontinuity, so a fresh segment's `TrackManager` never reuses an
    /// id from the segment it replaced.
    pub fn next_track_id(&self) -> u32 {
        self.next_id
    }

    /// Resume track id assignment from `next` instead of this manager's
    /// default of 1. MAN-73: used by `listen()`'s segment restart so ids
    /// are never reused within a session.
    pub fn resume_track_ids_from(&mut self, next: u32) {
        self.next_id = next;
    }

    /// MAN-13 (D3): number this manager's tracks `space.first`,
    /// `space.first + space.stride`, ... so several inputs feeding one
    /// validator never share a track id. Restarts the sequence at
    /// `space.first`; a segment restart then resumes from
    /// `next_track_id()`, which keeps the input's residue.
    pub(crate) fn set_track_id_space(&mut self, space: crate::multi::TrackIdSpace) {
        self.id_first = space.first.max(1);
        self.id_stride = space.stride.max(1);
        self.next_id = self.id_first;
    }

    /// Ignore every channelizer hop with `m < n`. MAN-115: callers that
    /// prepend `filter_len` zero samples (`decode_samples`, `listen`'s
    /// segments, `soak`) pass their `pad_hops`. Those hops' windows are
    /// all-zero or partial, read as -200 dB or far below the real noise,
    /// and would otherwise drag the floor (SPEC §2.1) to garbage and count
    /// against `warmup_hops`. Skipped hops never reach the floor, the gate
    /// or `hop_counter`.
    pub fn skip_preroll_hops(&mut self, n: u64) {
        self.preroll_hops = n;
    }

    /// MAN-118: publish every decoder's garbles, and each `Legacy` track's
    /// rail ratio once per SPEC §5 reporting interval, to `quality`. The
    /// observer is write-only from here, so decode output is unchanged.
    pub fn set_quality_observer(&mut self, quality: Option<Arc<DecodeQualityObserver>>) {
        self.quality = quality;
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
    /// "counted" -- this is the count. Published by `TrackLifecycleObserver`
    /// for `soak_metrics::soak_with_metrics` (MAN-236).
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

    /// MAN-13: the ids of every track this manager still holds, in id
    /// order, for a multi-input live tap's bookkeeping.
    pub(crate) fn track_ids(&self) -> Vec<u32> {
        self.tracks.keys().copied().collect()
    }

    /// Count of tracks the manager has actually *promoted* -- every track
    /// holding a leased decoder, i.e. in `LifecycleState::Active` or
    /// `Hang` (the decoder is leased on CANDIDATE -> ACTIVE and kept
    /// across HANG, `step_hop`; with `DetectorConfig::non_cw_filter` on,
    /// MAN-313 defers the lease until the track qualifies as keyed CW, so
    /// a promoted track still awaiting qualification is not counted; a
    /// MAN-108 DORMANT track has released its decoder, so it is not
    /// counted either until it wakes). This is the daemon's live
    /// `manta_active_tracks` gauge (MAN-122), and it is deliberately a
    /// *lifecycle* count rather than one derived from the decoder event
    /// stream: a promoted track whose demodulator has not latched yet
    /// emits no `TrackMeta`/`CharDecoded` at all, and `TrackManager` can
    /// legitimately hold it ACTIVE until the ~30 s `gc_hops` silent GC --
    /// so a weak or unmodulated signal that a decoder is genuinely
    /// working on would report zero if the count were derived from
    /// emitted events (MAN-122 review round 1).
    ///
    /// Distinct from `active_track_count()`, which counts *every* entry in
    /// `self.tracks` including unconfirmed CANDIDATEs. Candidates are
    /// noise-blip rise crossings that mostly close `Unconfirmed` within
    /// `confirm_hops` (~50 ms) without ever leasing a decoder; counting
    /// them in an operator-facing "is it decoding?" line would inflate it
    /// with signals nothing is decoding. `active_track_count` keeps its
    /// existing meaning for `soak_metrics`' peak/eviction accounting.
    pub fn decoding_track_count(&self) -> usize {
        self.tracks.values().filter(|t| t.decoder.is_some()).count()
    }

    /// MAN-108 (SPEC §2.4): DORMANT -> ACTIVE re-acquisitions so far, each
    /// a faded signal that came back under its old `track_id` instead of
    /// spawning a new track. A test accessor: not exported as a metric.
    #[cfg(test)]
    pub(crate) fn reacquisition_count(&self) -> u64 {
        self.reacquisitions
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
        // MAN-312 (SPEC §2.7): a static crash is band-wide, so each channel
        // judging it on its own (the gate's 19-hop confirm) cannot reject
        // it -- the channelizer smears a few-ms crash over ~85 ms of every
        // channel at once. The detector judges the whole passband per hop,
        // armed from `warmup_hops` like the spawn scan. While it reports a
        // crash, "starts-only" and "full" hold the gate's EMA (rise/drop are
        // recomputed from the held state against the current floor) so no
        // channel rises on it; "full" also holds the spectral reference and
        // each running track's decoder input (`Track::queue_input`). The
        // floor rings keep receiving pushes, so a sustained level change
        // still releases at `CRASH_MAX_HOPS`.
        let crash_hop = self
            .crash
            .update(&power_db_vals, self.hop_counter >= self.cfg.warmup_hops);
        let suppress = crash_hop.suppress() && self.cfg.crash_filter != CrashFilter::Off;
        let hold = crash_hop.hold_decoder() && self.cfg.crash_filter == CrashFilter::Full;
        self.floor.update_holding_reference(&power_db_vals, hold);
        let (rise, drop) = if suppress {
            self.gate.hold(&self.floor)
        } else {
            self.gate.update(&power_db_vals, &self.floor)
        };
        // MAN-324: the live-state occupancy window; one `Option` check
        // when off.
        if let Some(seen) = self.rise_seen.as_mut() {
            for (s, &r) in seen.iter_mut().zip(&rise) {
                *s |= r;
            }
        }
        // MAN-314 (SPEC §2.6): feed the RTTY detector the same power and
        // floor; a block that changed the marked set updates the log.
        if let Some(fsk) = self.fsk.as_mut() {
            if fsk.push_hop(hop, &power_db_vals, &self.floor) {
                self.update_rtty_log(sample_ts);
            }
        }
        // MAN-313 (SPEC §2.8): feed the co-modulation classifier every
        // channel's gate-smoothed SNR; `true` when it evaluated this hop.
        let comod_evaluated = if let Some(comod) = self.comod.as_mut() {
            self.comod_snr_buf.clear();
            self.comod_snr_buf.extend(
                (0..self.owner_of.len())
                    .map(|k| self.gate.smoothed_db(k) - self.floor.effective_floor_db(k)),
            );
            comod.push_hop(&self.comod_snr_buf)
        } else {
            false
        };
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
        // MAN-108: for the DORMANT-entry flush inside the loop.
        let dormant_quality = self.quality.clone();
        let ids: Vec<u32> = self.tracks.keys().copied().collect();
        for id in ids {
            let n = self.n_channels();
            let track = self.tracks.get_mut(&id).unwrap();
            // MAN-197: selection skips channels still serving a post-Silent
            // cooldown, so a track whose window drifted onto a silenced
            // birdie cannot keep decoding it inside the cooldown window.
            let k = track.select_live_channel(
                &hop.power,
                n,
                &self.channel_cooldown_until,
                self.hop_counter,
            );
            let prev_state = track.lifecycle.state();
            // MAN-312: a crash's energy would drag the centroid toward
            // wherever the smear happens to peak this hop. MAN-108: a
            // DORMANT track's centroid is frozen (including on its wake
            // hop): the frequency it held is what re-acquisition matches
            // on, and noise or a neighbour must not drag it away.
            if !suppress && prev_state != LifecycleState::Dormant {
                track.update_centroid(k, &hop.power, n);
            }
            let f = self.floor.effective_floor_db(k);
            track.current_snr_db = (self.gate.smoothed_db(k) - f) as f32;
            // MAN-313 (SPEC §2.8 A): a track awaiting qualification has no
            // decoder, so "no character decoded" says nothing about it yet;
            // hold its garbage-collect timer at zero. `NotCw` bounds its
            // life instead, and the timer starts at qualification.
            if track.keying.is_some() && !track.qualified {
                track.lifecycle.note_char_decoded();
            }
            let char_emitted = false; // GC timer input; refined below once a decoder exists.
                                      // MAN-312: a CANDIDATE's confirm window is frozen through a
                                      // crash rather than counted for or against it; ACTIVE/HANG
                                      // tracks step on the held rise/drop so their hang timers run.
            let mut event = if suppress && prev_state == LifecycleState::Candidate {
                LifecycleEvent::None
            } else {
                track.lifecycle.on_hop(rise[k], drop[k], char_emitted)
            };
            // MAN-108 (SPEC §2.4): DORMANT entry flushes and releases the
            // decoder (the flush goes ahead of this batch's pool output);
            // a wake leases a fresh one. Either way the hop then proceeds
            // as a no-transition hop, so a woken track's decoder takes this
            // hop's input in the `None` arm below.
            match event {
                LifecycleEvent::Dormant => {
                    self.dormant_flush
                        .extend(track.go_dormant(dormant_quality.as_deref()));
                    event = LifecycleEvent::None;
                }
                LifecycleEvent::Reacquired => {
                    let freq_hz = track.freq_hz(self.center_freq_hz, self.channel_spacing_hz, n);
                    track.reacquire(id, &self.decode_cfg, freq_hz);
                    #[cfg(test)]
                    {
                        self.reacquisitions += 1;
                    }
                    event = LifecycleEvent::None;
                }
                _ => {}
            }
            // MAN-313 (SPEC §2.8): keying evidence for a promoted track, then
            // the two filter-only closes. `keying` is `None` with the filter
            // off, so none of this runs there.
            let hop_now = self.hop_counter;
            let qualified = track.qualified;
            if let Some(ev) = track.keying.as_mut() {
                let keydown = power_db_vals[k] - self.floor.effective_floor_db(k)
                    >= f64::from(self.cfg.on_snr_db);
                ev.on_hop(
                    hop_now,
                    prev_state,
                    track.lifecycle.state(),
                    qualified,
                    k,
                    track.birth_channel,
                    keydown,
                    &hop.power,
                    &power_db_vals,
                );
                if event == LifecycleEvent::None {
                    if !qualified {
                        if hop_now - ev.promo_hop >= self.cfg.unkeyed_close_hops {
                            event = LifecycleEvent::Closed(CloseReason::NotCw);
                        }
                    } else if track.lifecycle.state() == LifecycleState::Active
                        && !ev.callsign_read
                        && hop_now - ev.last_rise_hop.max(ev.last_char_hop)
                            >= self.cfg.unkeyed_close_hops
                    {
                        event = LifecycleEvent::Closed(CloseReason::Unkeyed);
                    }
                }
            }
            match event {
                // MAN-108: handled right after `on_hop` above.
                LifecycleEvent::Dormant | LifecycleEvent::Reacquired => {
                    unreachable!("DORMANT entry and wake are handled before this match")
                }
                LifecycleEvent::Closed(reason) => {
                    self.close_counts.record(reason);
                    close_reasons.insert(id, reason);
                    closed.push(id);
                }
                LifecycleEvent::Promoted => {
                    // MAN-313: every detector confirmation, qualified or not.
                    self.confirmations += 1;
                    // SPEC §1.1/§5 (freq_hz's doc comment covers the §1.4
                    // deviation): seed TrackMeta.freq_hz from this track's
                    // own centroid EMA (already warm from its
                    // CANDIDATE-period update_centroid calls above), not the
                    // pipeline's raw center_freq_hz -- every track otherwise
                    // reports the same wrong frequency regardless of which
                    // channel it actually lives on. MAN-211: captured into
                    // this hop's queued entry (`decoder_input` below), not
                    // written to the decoder here: the drain sets it before
                    // this entry's own `push_hop`, so a `TrackMeta` pairs it
                    // with this hop's `sample_ts`.
                    let freq_hz = track.freq_hz(self.center_freq_hz, self.channel_spacing_hz, n);
                    // MAN-313 (SPEC §2.8): with the filter on, the decoder is
                    // leased (and `TrackPromoted` emitted) only once the track
                    // qualifies as keyed CW; until then its input queues in
                    // `pending`. With the filter off, both happen here.
                    if self.cfg.non_cw_filter {
                        track.keying =
                            Some(KeyingEvidence::new(sample_ts, freq_hz, self.hop_counter));
                    } else {
                        track.qualified = true;
                        track.decoder = Some(TrackDecoder::new(id, self.decode_cfg.clone()));
                    }
                    let spectral_ref_power = compute_spectral_ref_power
                        .then(|| Self::spectral_ref_power(&self.floor, track.center))
                        .flatten();
                    // MAN-102: `current_snr_db` is passed INTO decoder_input
                    // and captured per-entry in the backlog (Codex review,
                    // PR #183 -- see decoder_input's doc comment), not
                    // re-applied uniformly here after the fact. MAN-211:
                    // `freq_hz` likewise.
                    let burst = track.decoder_input(
                        k,
                        hop,
                        sample_ts,
                        refine_bw_hz,
                        spectral_ref_power,
                        track.current_snr_db,
                        freq_hz,
                        hold,
                    );
                    for e in burst {
                        track.queue_input(e);
                    }
                    // With the filter on, deferred to qualification (MAN-313,
                    // see above), carrying this hop's `sample_ts`/`freq_hz`.
                    // With it off, emitted unconditionally here, at the
                    // exact hop the detector made this decision -- NOT gated by
                    // `has_emitted`/TrackClosed's same-batch-merge filter
                    // (MAN-19) further down. A track promoted and merged
                    // away within this same `process_hops` call still
                    // really was promoted; that's exactly the ground truth
                    // `doctor()`'s NoSignal check needs and the other event
                    // kinds can't reliably provide (see events.rs's doc
                    // comment on this variant).
                    if !self.cfg.non_cw_filter {
                        promoted_events.push(DecoderEvent::TrackPromoted {
                            track_id: id,
                            sample_ts,
                            freq_hz,
                        });
                    }
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
                    // MAN-313: a promoted track awaiting qualification queues
                    // too, so the decoder leased at qualification replays the
                    // whole track from its promotion hop.
                    let awaiting = track.keying.is_some() && !track.qualified;
                    if track.decoder.is_some() || awaiting {
                        // TrackMeta.freq_hz tracks the centroid (SPEC §5's
                        // 1 Hz cadence reports the *current* centroid, not a
                        // frozen promotion-time snapshot). MAN-211: this
                        // hop's centroid is captured into this hop's queued
                        // entry, so the report pairs it with this hop's
                        // `sample_ts` however long the entry waits in
                        // `pending` or the refiner backlog.
                        let freq_hz =
                            track.freq_hz(self.center_freq_hz, self.channel_spacing_hz, n);
                        let spectral_ref_power = compute_spectral_ref_power
                            .then(|| Self::spectral_ref_power(&self.floor, track.center))
                            .flatten();
                        // MAN-102 (see the Promoted arm above for the full
                        // rationale): current_snr_db and (MAN-211) freq_hz
                        // passed INTO decoder_input, captured per-entry.
                        let burst = track.decoder_input(
                            k,
                            hop,
                            sample_ts,
                            refine_bw_hz,
                            spectral_ref_power,
                            track.current_snr_db,
                            freq_hz,
                            hold,
                        );
                        for e in burst {
                            track.queue_input(e);
                        }
                        // MAN-313: bound the unqualified backlog, trimming
                        // in quarter-cap steps so the copy is amortised.
                        if awaiting
                            && track.pending.len()
                                > MAX_UNQUALIFIED_BACKLOG + MAX_UNQUALIFIED_BACKLOG / 4
                        {
                            let excess = track.pending.len() - MAX_UNQUALIFIED_BACKLOG;
                            track.pending.drain(..excess);
                        }
                    }
                }
            }
        }
        // MAN-19: only report a close as `TrackClosed`-worthy if the track
        // actually emitted a real event (`has_emitted`, or was named as a
        // merge survivor -- MAN-174, see `merge_converged`), not merely
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
        // MAN-313 (SPEC §2.8): at each classifier evaluation, qualify every
        // promoted, unqualified track that shows keyed-CW evidence: lease
        // its decoder (the next `drain_pool` replays its queued input) and
        // emit its deferred `TrackPromoted`. Every unqualified track's
        // per-evaluation ACTIVE count restarts.
        if comod_evaluated {
            if let Some(comod) = self.comod.as_ref() {
                let n = self.n_channels();
                let spacing = self.channel_spacing_hz;
                for (&id, track) in self.tracks.iter_mut() {
                    if track.qualified || close_reasons.contains_key(&id) {
                        continue;
                    }
                    let Some(ev) = track.keying.as_mut() else {
                        continue;
                    };
                    let k = (track.center.round() as i64).rem_euclid(n as i64) as usize;
                    let qualifies = ev.qualifies(comod, k, spacing);
                    ev.active_hops_since_eval = 0;
                    if qualifies {
                        let (promo_ts, promo_freq_hz) = (ev.promo_ts, ev.promo_freq_hz);
                        let freq_hz = track.freq_hz(self.center_freq_hz, spacing, n);
                        track.qualified = true;
                        let mut decoder = TrackDecoder::new(id, self.decode_cfg.clone());
                        decoder.set_freq_hz(freq_hz);
                        track.decoder = Some(decoder);
                        promoted_events.push(DecoderEvent::TrackPromoted {
                            track_id: id,
                            sample_ts: promo_ts,
                            freq_hz: promo_freq_hz,
                        });
                    }
                }
            }
        }
        // MAN-314 (SPEC §2.4/§2.6): close every open track whose centre
        // channel sits on a marked RTTY signal's hard span. Hard span only:
        // the splatter shadow must never close an existing track, since a
        // real CW track's key-up gaps put it below any relative level.
        if let Some(fsk) = self.fsk.as_ref() {
            let n = self.n_channels();
            for (&id, track) in &self.tracks {
                if !close_reasons.contains_key(&id) && fsk.in_hard_span(track.owned(n)[1]) {
                    self.close_counts.record(CloseReason::Rtty);
                    close_reasons.insert(id, CloseReason::Rtty);
                    closed.push(id);
                }
            }
        }
        let mut closure_flush_events: Vec<DecoderEvent> = Vec::new();
        let quality = self.quality.clone();
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
            // MAN-313: `Unkeyed` and `NotCw` likewise mark a channel holding
            // energy that is not keyed CW.
            if matches!(
                close_reasons.get(id),
                Some(CloseReason::Silent | CloseReason::Unkeyed | CloseReason::NotCw)
            ) {
                let until = self.hop_counter + self.cfg.silent_respawn_cooldown_hops;
                let n = self.channel_cooldown_until.len();
                for ch in track.owned(n) {
                    self.channel_cooldown_until[ch] = until;
                }
            }
            if !track.has_emitted {
                return false;
            }
            // MAN-194: this closure is the LAST point this track's refiner
            // will ever be fed -- drain its backlog into pending BEFORE
            // finish_decoder*/finish_decoder_speed_only* below (which then
            // push pending through the decoder as they already do), so a
            // buffered-but-undelivered real observation isn't silently
            // dropped along with the rest of the removed `Track`.
            track.drain_refiner_into_pending();
            // Only a genuine, sustained observed RF gap (HangExpired) may
            // force an in-progress mark to resolve; Silent's own trigger
            // (no character decoded) says nothing about whether the
            // signal is still there (round 8). MAN-108: a DORMANT track's
            // expiry is `HangExpired` too; its decoder was already flushed
            // at DORMANT entry (`Track::go_dormant`), so this flushes
            // nothing more.
            let events = if close_reasons.get(id) == Some(&CloseReason::HangExpired) {
                track.finish_decoder(quality.as_deref())
            } else {
                track.finish_decoder_speed_only(quality.as_deref())
            };
            closure_flush_events.extend(events);
            true
        });
        self.recompute_ownership();

        // SPEC §2.5 / MAN-172: scan rising, unowned local maxima of the
        // gate's smoothed power. Falling leakage skirts cannot start a
        // spawn decision. Preserve the same-hop adjacent-rise arbitration.
        // MAN-312: no track starts during a static crash ("starts-only"
        // and "full"). MAN-197: the respawn cooldown gates a candidate's
        // whole prospective ownership window, not just its birth channel.
        // MAN-196: no track starts on a channel outside the source's
        // trusted band, in both the k and k+1 slots -- the same-hop
        // adjacent-rise arbitration must not route around the veto.
        if past_warmup && !suppress {
            let n = self.n_channels();
            let mut k = 0;
            while k < n {
                if rise[k]
                    && self.owner_of[k].is_none()
                    && !self.spawn_window_cooling_down(k)
                    && self.is_local_peak(k)
                    && !self.rtty_vetoes_spawn(k)
                    && !self.comod_vetoes_spawn(k)
                    && !self.trusted_vetoes_spawn(k)
                {
                    let mut winner = k;
                    if k + 1 < n
                        && rise[k + 1]
                        && self.owner_of[k + 1].is_none()
                        && !self.spawn_window_cooling_down(k + 1)
                        && !self.rtty_vetoes_spawn(k + 1)
                        && !self.comod_vetoes_spawn(k + 1)
                        && !self.trusted_vetoes_spawn(k + 1)
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
        // RF gap -- proof the signal actually ended. MAN-108: that includes
        // a DORMANT track whose re-acquisition window ran out. Silent fires after 30 s
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

    /// MAN-172 / SPEC §2.5: a rising channel must be a local maximum of
    /// smoothed power to start a spawn decision (ties qualify). Same-hop
    /// pairing still chooses between adjacent rises by raw power. At high SNR a
    /// tone's falling skirt crosses the rise threshold outside its owned
    /// channels; suppressing those births prevents duplicate decoders.
    /// Neighbours wrap, like `Track::owned`.
    /// See docs/DECISIONS/2026-10-11-man172-high-snr-track-churn.md.
    fn is_local_peak(&self, k: usize) -> bool {
        let n = self.n_channels();
        let s = self.gate.smoothed_db(k);
        s >= self.gate.smoothed_db((k + n - 1) % n) && s >= self.gate.smoothed_db((k + 1) % n)
    }

    /// MAN-171: is `k` still serving out a post-`Silent`-closure spawn
    /// cooldown (`DetectorConfig::silent_respawn_cooldown_hops`)?
    fn channel_cooling_down(&self, k: usize) -> bool {
        self.channel_cooldown_until[k] > self.hop_counter
    }

    /// MAN-197: would a CANDIDATE born on `k` immediately come to own a
    /// channel still serving a post-`Silent` cooldown? `Track::owned` is
    /// the wrapping 3-channel window `{k-1, k, k+1}`, so gating only the
    /// birth channel let a neighbour-born candidate absorb the cooling
    /// channel via `select_channel`'s max-power rule and track it for a
    /// full illegitimate promote/decode cycle inside the cooldown window.
    fn spawn_window_cooling_down(&self, k: usize) -> bool {
        let n = self.n_channels();
        [(k + n - 1) % n, k, (k + 1) % n]
            .into_iter()
            .any(|ch| self.channel_cooling_down(ch))
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
        // MAN-13 (D3): step by the input's stride; on overflow the input
        // wraps to its own first id, never into another input's ids.
        self.next_id = self
            .next_id
            .checked_add(self.id_stride)
            .unwrap_or(self.id_first);
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
                    // MAN-172: ids are ascending, so `a` is the incumbent.
                    // Converged tracks often select the same channel and tie
                    // exactly. Keep its established decoder on that tie.
                    // MAN-313 (SPEC §2.8): with the filter on, a qualified
                    // track beats an unqualified one regardless of SNR.
                    let (qa, qb) = (self.tracks[&a].qualified, self.tracks[&b].qualified);
                    let (loser, survivor) = if self.cfg.non_cw_filter && qa != qb {
                        if qa {
                            (b, a)
                        } else {
                            (a, b)
                        }
                    } else if self.tracks[&a].current_snr_db < self.tracks[&b].current_snr_db {
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
                // MAN-174: the loser's TrackClosed names `survivor` to every
                // consumer, which may create per-track_id state for it
                // (manta-spot migrates pending beacons there). The survivor
                // therefore owes its own TrackClosed even if it has emitted
                // nothing yet. A survivor here can only be a loser in a
                // later `to_close` entry (pairs with a loser are skipped
                // above), so it is still in `self.tracks`.
                if let Some(s) = self.tracks.get_mut(&survivor) {
                    s.has_emitted = true;
                }
                // MAN-194: same reasoning as step_hop's closure handling --
                // a merge-loser's refiner also stops receiving input here.
                track.drain_refiner_into_pending();
                flush_events.extend(track.finish_decoder_speed_only(self.quality.as_deref()));
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
                    // MAN-194: same reasoning as step_hop's closure
                    // handling -- an evicted track's refiner also stops
                    // receiving input here.
                    track.drain_refiner_into_pending();
                    flush_events.extend(track.finish_decoder_speed_only(self.quality.as_deref()));
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
            if h.m < self.preroll_hops {
                continue;
            }
            let (closed, promoted, closure_flush) = self.step_hop(h, hop_to_sample_ts(h.m));
            closed_ids.extend(closed);
            promoted_events.extend(promoted);
            closure_flush_events.extend(closure_flush);
        }
        let pool_events = self.drain_pool();
        // MAN-108: this batch's DORMANT-entry flushes, taken here so the
        // loop below also runs over them: a DORMANT track lives on, so a
        // word or callsign its released decoder completed must reach its
        // MAN-313 `KeyingEvidence`, or the woken decoder's first word would
        // be appended to the stale partial one.
        let dormant_flush = std::mem::take(&mut self.dormant_flush);
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
        for e in dormant_flush.iter().chain(&pool_events) {
            if let Some(t) = self.tracks.get_mut(&event_track_id(e)) {
                t.has_emitted = true;
            }
            match e {
                DecoderEvent::CharDecoded {
                    track_id, glyph, ..
                } => {
                    if let Some(t) = self.tracks.get_mut(track_id) {
                        t.lifecycle.note_char_decoded();
                        // MAN-108 (SPEC §2.4): a qualified track that has
                        // decoded a character goes DORMANT, not CLOSED, when
                        // its hang timer runs out. Batch-granular, like the
                        // GC reset above (SPEC §2.8 G precedent).
                        if t.qualified {
                            t.lifecycle.set_reacquirable();
                        }
                        // MAN-313: the unkeyed close's character timer and
                        // callsign read.
                        if let Some(ev) = t.keying.as_mut() {
                            ev.note_char(self.hop_counter, glyph.text_char());
                        }
                    }
                }
                DecoderEvent::WordBoundary { track_id, .. } => {
                    if let Some(ev) = self
                        .tracks
                        .get_mut(track_id)
                        .and_then(|t| t.keying.as_mut())
                    {
                        ev.end_word();
                    }
                }
                _ => {}
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
        // MAN-108: this batch's DORMANT-entry flushes go first, so the
        // stable sort keeps a released decoder's events ahead of a
        // same-batch woken decoder's wherever their keys tie.
        let mut events = dormant_flush;
        events.extend(pool_events);
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
        // carries a real timestamp, MAN-102 review round 2). Among
        // themselves, a merge loser's `TrackClosed` sorts before its
        // survivor's own same-batch `TrackClosed` (MAN-174) -- see
        // `sequence_batch`.
        sequence_batch(events, closed_ids, &promoted_ts_by_track)
    }

    /// Flush every track's decoder (SPEC §5 end-of-stream). Call once,
    /// after the last `process_hops`.
    pub fn finish(&mut self) -> Vec<DecoderEvent> {
        self.close_all(false)
    }

    /// Close every track at a source discontinuity (MAN-73: a live source
    /// reconnected after an outage), not a stream end. A transport gap is
    /// not evidence the RF signal ended -- the same transmission can still
    /// be on the air when samples resume (PR #207 review) -- so each track
    /// closes like a Merged/Evicted one: `finish_decoder_speed_only` (no
    /// forced decode of a truncated trailing element) and
    /// `ClosureKind::Bookkeeping` with no survivor, which keeps
    /// `manta-spot`'s `Validator` from reading the post-reconnect track as a
    /// distinct message. Its pending beacons are discarded and counted
    /// (`pending_beacon_lost_to_eviction`, as for an Evicted/Silent close),
    /// never resolved against a truncated track's speed. Under the
    /// `EdgeLegacy`/`Hsmm` engines `TrackDecoder::finish_speed_only` flushes
    /// nothing (MAN-169), so those engines' buffered output is dropped here
    /// as at a merge/eviction. As with `finish`, the manager holds no tracks
    /// afterwards.
    pub fn finish_for_discontinuity(&mut self) -> Vec<DecoderEvent> {
        self.close_all(true)
    }

    /// Shared body of `finish` (`discontinuity == false`: genuine stream
    /// end, `SignalEnded`) and `finish_for_discontinuity` (`true`).
    fn close_all(&mut self, discontinuity: bool) -> Vec<DecoderEvent> {
        use rayon::prelude::*;
        // MAN-194: end-of-stream is another point where every remaining
        // track's refiner stops receiving new input -- drain each one's
        // backlog into pending before flushing, same as every other
        // closure path (Task 3). Done as a plain sequential pass first
        // (cheap: at most GROUP_DELAY_HOPS entries per track) so the
        // parallel decode pass below sees a fully-populated `pending`.
        for track in self.tracks.values_mut() {
            track.drain_refiner_into_pending();
        }
        // `finish_decoder` drains `pending` through `push_hop` before
        // calling `TrackDecoder::finish()` -- exactly what the backlog
        // just fed into `pending` above needs, and a no-op for any track
        // whose `pending` was already empty (every track, before MAN-194).
        let quality = self.quality.as_deref();
        let flushed: Vec<DecoderEvent> = self
            .tracks
            .values_mut()
            .par_bridge()
            .flat_map_iter(|t| {
                if discontinuity {
                    t.finish_decoder_speed_only(quality)
                } else {
                    t.finish_decoder(quality)
                }
            })
            .collect();
        // MAN-108: a DORMANT-entry flush not yet drained by `process_hops`
        // (only possible when `step_hop` ran outside it) goes first.
        let mut events = std::mem::take(&mut self.dormant_flush);
        events.extend(flushed);
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
                    // Overall stream end -- always a genuine end-of-signal
                    // (round 9). A discontinuity is not (MAN-73).
                    closure: if discontinuity {
                        ClosureKind::Bookkeeping {
                            survivor_track_id: None,
                        }
                    } else {
                        ClosureKind::SignalEnded
                    },
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
        let quality = self.quality.as_deref();
        let mut events: Vec<DecoderEvent> = self
            .tracks
            .values_mut()
            .filter_map(|t| {
                // MAN-313: a track awaiting qualification keeps its queue
                // until a decoder is leased.
                let decoder = t.decoder.as_mut()?;
                if t.pending.is_empty() {
                    None
                } else {
                    let pending = std::mem::take(&mut t.pending);
                    Some((
                        decoder,
                        &mut t.snr_peak_db,
                        &mut t.garbles_published,
                        pending,
                    ))
                }
            })
            .par_bridge()
            .flat_map_iter(|(decoder, peak, garbles_published, pending)| {
                let mut out = Vec::new();
                for (amp, raw_power, spectral_ref_power, ts, snr_db, freq_hz) in pending {
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
                    // MAN-211: this entry's own hop's centroid, so a
                    // `TrackMeta` this push emits pairs it with the same
                    // hop's `sample_ts`.
                    decoder.set_freq_hz(freq_hz);
                    let hop_events = decoder.push_hop(amp, raw_power, spectral_ref_power, ts);
                    if decoder.hop_count() % manta_decode::META_INTERVAL_HOPS == 0 {
                        *peak = f32::NEG_INFINITY;
                    }
                    sample_rail_ratio_at_boundary(decoder, quality);
                    out.extend(hop_events);
                }
                publish_garbles(decoder, garbles_published, quality);
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
        | DecoderEvent::TrackMeta { sample_ts, .. }
        | DecoderEvent::TimingUpdate { sample_ts, .. } => *sample_ts,
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
        // `TrackMeta` now carries a real `sample_ts` (MAN-102 review round
        // 2) and falls through to `event_sample_ts` below like any other
        // real-timestamped event -- it no longer needs the same-batch
        // promotion-timestamp pin `SpeedUpdate` still does (Codex review,
        // PR #134 round 1: pinning it here silently overrode that real
        // timestamp, so a `TrackMeta` after an earlier `CharDecoded`/
        // `WordBoundary` in the same batch sorted to the front instead of
        // where it actually happened, letting `Validator` apply the new
        // peak-held SNR to an earlier spot depending on batch chunking).
        DecoderEvent::SpeedUpdate { track_id, .. } => {
            promoted_ts_by_track.get(track_id).copied().unwrap_or(0)
        }
        other => event_sample_ts(other),
    }
}

/// SPEC §6 rule 6 resequencing for one `process_hops` batch, plus
/// MAN-174: a merge loser's `TrackClosed` sorts before its survivor's own
/// same-batch `TrackClosed`, so a consumer migrating the loser's deferred
/// evidence (manta-spot's pending beacons) finds the survivor still open.
/// Every other event keeps `(sample_ts, track_id, tier)` order: the
/// second key is non-zero only for a `TrackClosed`, and every
/// `TrackClosed` already sorts after every other event of the batch (its
/// synthetic `u64::MAX` timestamp, see `event_sample_ts`), so it reorders
/// closures among themselves and nothing else. Independent closures stay
/// in `track_id` order.
fn sequence_batch(
    mut events: Vec<DecoderEvent>,
    closed_ids: Vec<(u32, ClosureKind)>,
    promoted_ts_by_track: &std::collections::HashMap<u32, u64>,
) -> Vec<DecoderEvent> {
    let depth = closure_depths(&closed_ids);
    events.extend(
        closed_ids
            .into_iter()
            .map(|(track_id, closure)| DecoderEvent::TrackClosed { track_id, closure }),
    );
    events.sort_by_key(|e| {
        let closure_depth = match e {
            DecoderEvent::TrackClosed { track_id, .. } => depth.get(track_id).copied().unwrap_or(0),
            _ => 0,
        };
        (
            effective_sort_ts(e, promoted_ts_by_track),
            std::cmp::Reverse(closure_depth),
            event_track_id(e),
            event_kind_tier(e),
        )
    });
    events
}

/// For each track closed this batch: how many same-batch merge hops lead
/// from it, survivor to survivor, before reaching a track that is not
/// itself closed this batch (or that closed with no survivor). 0 for a
/// non-merge closure, or a merge whose survivor stays open. A loser's
/// depth is always its survivor's plus one, so sorting closures by
/// descending depth puts every loser ahead of its survivor (MAN-174).
/// The walk is bounded by the number of closures, so a malformed cycle
/// cannot hang it; only `BTreeMap`s are used (SPEC §6 rule 3).
fn closure_depths(closed: &[(u32, ClosureKind)]) -> BTreeMap<u32, usize> {
    let survivor_of: BTreeMap<u32, Option<u32>> = closed
        .iter()
        .map(|&(track_id, closure)| {
            let survivor = match closure {
                ClosureKind::Bookkeeping { survivor_track_id } => survivor_track_id,
                ClosureKind::SignalEnded => None,
            };
            (track_id, survivor)
        })
        .collect();
    survivor_of
        .keys()
        .map(|&track_id| {
            let mut depth = 0;
            let mut current = track_id;
            while depth < closed.len() {
                match survivor_of.get(&current).copied().flatten() {
                    Some(survivor) if survivor_of.contains_key(&survivor) => {
                        depth += 1;
                        current = survivor;
                    }
                    _ => break,
                }
            }
            (track_id, depth)
        })
        .collect()
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
        | DecoderEvent::TimingUpdate { track_id, .. }
        | DecoderEvent::TrackPromoted { track_id, .. }
        | DecoderEvent::TrackClosed { track_id, .. } => *track_id,
    }
}

#[cfg(test)]
mod tests {
    // MAN-13 (D3): interleaved track ids.

    fn id_space_manager() -> TrackManager {
        TrackManager::new(64, 96_000.0, 14_000_000.0, cfg(), DecodeConfig::default())
    }

    fn spawned_ids(tm: &mut TrackManager, n: usize) -> Vec<u32> {
        (0..n)
            .map(|k| {
                let id = tm.next_track_id();
                tm.spawn(5 + 8 * k);
                id
            })
            .collect()
    }

    #[test]
    fn track_ids_interleave_by_input() {
        let mut tm = id_space_manager();
        tm.set_track_id_space(crate::multi::TrackIdSpace {
            first: 2,
            stride: 3,
        });
        assert_eq!(spawned_ids(&mut tm, 3), vec![2, 5, 8]);
        let mut ids: Vec<u32> = tm.tracks.keys().copied().collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![2, 5, 8]);
    }

    #[test]
    fn one_input_track_ids_are_todays_numbering() {
        let mut tm = id_space_manager();
        tm.set_track_id_space(crate::multi::TrackIdSpace::SINGLE);
        assert_eq!(spawned_ids(&mut tm, 3), vec![1, 2, 3]);
        let mut fresh = id_space_manager();
        assert_eq!(spawned_ids(&mut fresh, 3), vec![1, 2, 3]);
    }

    #[test]
    fn resume_after_discontinuity_keeps_the_input_residue() {
        let space = crate::multi::TrackIdSpace {
            first: 3,
            stride: 4,
        };
        let mut tm = id_space_manager();
        tm.set_track_id_space(space);
        spawned_ids(&mut tm, 2);
        tm.finish_for_discontinuity();
        let next = tm.next_track_id();
        let mut seg = id_space_manager();
        seg.set_track_id_space(space);
        seg.resume_track_ids_from(next);
        for id in spawned_ids(&mut seg, 3) {
            assert_eq!(id % 4, 3, "id {id} left input 2's residue");
            assert!(id >= next);
        }
    }

    #[test]
    fn track_id_overflow_wraps_to_the_inputs_first_id() {
        let mut tm = id_space_manager();
        tm.set_track_id_space(crate::multi::TrackIdSpace {
            first: 2,
            stride: 3,
        });
        tm.resume_track_ids_from(u32::MAX - 1);
        tm.spawn(5);
        assert_eq!(tm.next_track_id(), 2);
    }

    use super::*;

    #[test]
    fn close_counts_adds_every_reason() {
        let a = CloseCounts {
            unconfirmed: 1,
            hang_expired: 2,
            silent: 3,
            merged: 4,
            evicted: 5,
            rtty: 6,
            not_cw: 7,
            unkeyed: 8,
        };
        let b = CloseCounts {
            unconfirmed: 10,
            hang_expired: 20,
            silent: 30,
            merged: 40,
            evicted: 50,
            rtty: 60,
            not_cw: 70,
            unkeyed: 80,
        };
        assert_eq!(
            a + b,
            CloseCounts {
                unconfirmed: 11,
                hang_expired: 22,
                silent: 33,
                merged: 44,
                evicted: 55,
                rtty: 66,
                not_cw: 77,
                unkeyed: 88
            }
        );
    }

    #[test]
    fn default_detector_config_has_rtty_on_and_7500_dwell_hops() {
        let d = DetectorConfig::default();
        assert!(d.rtty_filter);
        assert_eq!(d.rtty_dwell_hops, 7500);
        assert_eq!(d.rtty_dwell_hops / FSK_BLOCK_HOPS as u64, 20);
    }

    /// MAN-314: a 30 dB RTTY signal's tracks close as `CloseReason::Rtty`
    /// once it is marked, counted in `CloseCounts::rtty`, and its marked
    /// entry lands in `rtty_signals`.
    #[test]
    fn rtty_close_is_counted_and_bookkeeping() {
        use manta_testkit::fsk::{add_fsk, FskSpec};
        let fs = 96_000.0;
        let (mut iq, _) = manta_testkit::scene::render_scene(&[], fs, 40.0, Some(11)).unwrap();
        add_fsk(
            &mut iq,
            fs,
            &FskSpec {
                mark_offset_hz: 15_000.0,
                shift_hz: 170.0,
                baud: 45.45,
                snr_2500_db: 30.0,
                text: "CQ TEST DE W1AW W1AW RYRYRY 599 001 ".into(),
                start_s: 0.0,
                watterson: None,
                seed: 1,
            },
        );
        let mut ch = manta_dsp::channelizer::Channelizer::new(fs, 0.0).unwrap();
        let hop = ch.hop() as u64;
        let mut tm = TrackManager::new(
            ch.n_channels(),
            fs,
            0.0,
            DetectorConfig::default(),
            DecodeConfig::default(),
        );
        let mut events = Vec::new();
        for chunk in iq.chunks(4096) {
            events.extend(tm.process_hops(&ch.process(chunk), |m| m * hop));
        }
        assert_eq!(tm.rtty_signals().len(), 1, "{:?}", tm.rtty_signals());
        assert!(tm.rtty_changes() >= 1);
        assert!(tm.close_counts().rtty >= 1, "{:?}", tm.close_counts());

        let off = DetectorConfig {
            rtty_filter: false,
            ..DetectorConfig::default()
        };
        let mut ch = manta_dsp::channelizer::Channelizer::new(fs, 0.0).unwrap();
        let mut tm_off = TrackManager::new(ch.n_channels(), fs, 0.0, off, DecodeConfig::default());
        for chunk in iq.chunks(4096) {
            tm_off.process_hops(&ch.process(chunk), |m| m * hop);
        }
        assert!(tm_off.rtty_signals().is_empty());
        assert_eq!(tm_off.close_counts().rtty, 0);
    }

    /// MAN-314: past `RTTY_LOG_CAP` a stream's log drops the oldest
    /// cleared entries; after `keep_full_rtty_history` (what
    /// `decode_samples` calls) every entry stays.
    #[test]
    fn rtty_log_cap_is_lifted_by_keep_full_rtty_history() {
        let fill = |tm: &mut TrackManager| {
            for id in 0..=RTTY_LOG_CAP as u32 {
                tm.rtty_log.push(RttyLogEntry {
                    id,
                    mark_hz: 14_085_170.0,
                    space_hz: 14_085_000.0,
                    shift_hz: 170.0,
                    measured_shift_hz: 170.0,
                    marked_sample_ts: u64::from(id),
                    cleared_sample_ts: Some(u64::from(id) + 1),
                });
            }
            tm.update_rtty_log(RTTY_LOG_CAP as u64 + 2);
        };
        let new_tm = || {
            TrackManager::new(
                64,
                96_000.0,
                0.0,
                DetectorConfig::default(),
                DecodeConfig::default(),
            )
        };

        let mut stream = new_tm();
        fill(&mut stream);
        assert_eq!(stream.rtty_signals().len(), RTTY_LOG_CAP);
        assert_eq!(stream.rtty_signals()[0].id, 1);

        let mut file = new_tm();
        file.keep_full_rtty_history();
        fill(&mut file);
        assert_eq!(file.rtty_signals().len(), RTTY_LOG_CAP + 1);
        assert_eq!(file.rtty_signals()[0].id, 0);
    }

    fn cfg() -> DetectorConfig {
        DetectorConfig {
            confirm_hops: 5,
            hang_hops: 10,
            gc_hops: 20,
            ..DetectorConfig::default()
        }
    }

    /// `DetectorConfig::default()` with the MAN-313 non-CW filter off:
    /// promotion leases a decoder and emits `TrackPromoted` at once, as
    /// before MAN-313.
    fn default_filter_off() -> DetectorConfig {
        DetectorConfig {
            non_cw_filter: false,
            ..DetectorConfig::default()
        }
    }

    /// MAN-118 PR #277 review: a closing track's flush drains its queued
    /// hops through the decoder, so a 1 Hz reporting boundary it crosses
    /// must reach `manta_track_rail_ratio` just as one `drain_pool` crosses
    /// does. One interval of keyed envelope initializes the `Legacy` rails
    /// by its last hop, the boundary, so each path records one sample.
    #[test]
    fn every_pending_drain_samples_the_rail_ratio_at_a_reporting_boundary() {
        let keyed: Vec<f32> = (0..12)
            .flat_map(|_| [1.0f32; 15].into_iter().chain([0.01f32; 15]))
            .chain([1.0f32; 15])
            .collect();
        assert_eq!(keyed.len() as u64, manta_decode::META_INTERVAL_HOPS);
        let queue = || -> Vec<(f32, f32, Option<f32>, u64, f32, f64)> {
            keyed
                .iter()
                .enumerate()
                .map(|(i, &a)| (a, a * a, None, i as u64 * 256, 20.0, 0.0))
                .collect()
        };
        let rail_samples = |path: &str| {
            let obs = Arc::new(DecodeQualityObserver::new());
            let mut tm =
                TrackManager::new(64, 96_000.0, 14_000_000.0, cfg(), DecodeConfig::default());
            tm.set_quality_observer(Some(obs.clone()));
            tm.spawn(10);
            let track = tm.tracks.values_mut().next().unwrap();
            track.decoder = Some(TrackDecoder::new(track.id, DecodeConfig::default()));
            track.pending = queue();
            match path {
                "drain_pool" => drop(tm.drain_pool()),
                "finish_decoder" => drop(track.finish_decoder(Some(&obs))),
                _ => drop(track.finish_decoder_speed_only(Some(&obs))),
            }
            obs.snapshot().rail_ratio_count
        };
        for path in ["drain_pool", "finish_decoder", "finish_decoder_speed_only"] {
            assert_eq!(
                rail_samples(path),
                1,
                "{path} must sample the boundary it crosses"
            );
        }
    }

    /// MAN-211: each drain path sets the decoder's frequency from the
    /// entry it is about to push, so the `TrackMeta` a reporting boundary
    /// emits carries the centroid of the same hop as its `sample_ts`, not
    /// whatever `step_hop` last saw. Same keyed interval as the rail-ratio
    /// test above; entry `i` carries `14_000_000 + i` Hz, so the one report
    /// at the interval's last hop must read `14_000_374` Hz.
    #[test]
    fn every_pending_drain_sets_track_meta_freq_from_its_own_entry() {
        let keyed: Vec<f32> = (0..12)
            .flat_map(|_| [1.0f32; 15].into_iter().chain([0.01f32; 15]))
            .chain([1.0f32; 15])
            .collect();
        let last = manta_decode::META_INTERVAL_HOPS - 1;
        assert_eq!(keyed.len() as u64, last + 1);
        let queue = || -> Vec<(f32, f32, Option<f32>, u64, f32, f64)> {
            keyed
                .iter()
                .enumerate()
                .map(|(i, &a)| {
                    let freq_hz = 14_000_000.0 + i as f64;
                    (a, a * a, None, i as u64 * 256, 20.0, freq_hz)
                })
                .collect()
        };
        let metas = |path: &str| -> Vec<(u64, f64)> {
            let mut tm =
                TrackManager::new(64, 96_000.0, 14_000_000.0, cfg(), DecodeConfig::default());
            tm.spawn(10);
            let track = tm.tracks.values_mut().next().unwrap();
            track.decoder = Some(TrackDecoder::new(track.id, DecodeConfig::default()));
            track.pending = queue();
            let events = match path {
                "drain_pool" => tm.drain_pool(),
                "finish_decoder" => track.finish_decoder(None),
                _ => track.finish_decoder_speed_only(None),
            };
            events
                .into_iter()
                .filter_map(|e| match e {
                    DecoderEvent::TrackMeta {
                        sample_ts, freq_hz, ..
                    } => Some((sample_ts, freq_hz)),
                    _ => None,
                })
                .collect()
        };
        for path in ["drain_pool", "finish_decoder", "finish_decoder_speed_only"] {
            assert_eq!(
                metas(path),
                vec![(last * 256, 14_000_000.0 + last as f64)],
                "{path} must report the frequency of the entry whose sample_ts it reports"
            );
        }
    }

    #[test]
    fn decoder_input_bypasses_the_refiner_when_disabled() {
        // MAN-168: refine_bw_hz <= 0.0 (the default) must fall back to the
        // raw owned-channel magnitude exactly as before, and never
        // construct a Refiner at all.
        let mut track = Track::new(1, 5, &cfg());
        let h = hop_with_x(0, vec![num_complex::Complex32::new(3.0, 4.0); 8]); // |z|=5
        let out = track.decoder_input(5, &h, 1000, 0.0, None, 0.0, 0.0, false);
        let expected: SmallVec<[DecoderInputEntry; 1]> =
            smallvec![(5.0, 25.0, None, 1000, 0.0, 0.0, false)];
        assert_eq!(out, expected);
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
        track.decoder_input(5, &h, 0, 30.0, None, 0.0, 0.0, false);
        assert_eq!(track.refiner_hops_since_reset, 1);
        // Drift center within the same integer channel k=5 across several
        // more hops -- the counter must keep GROWING (never cleared by a
        // spurious reset) since `refiner_channel` stays `Some(5)`.
        for i in 1..4u64 {
            track.center += 0.1;
            track.decoder_input(5, &h, i * 512, 30.0, None, 0.0, 0.0, false);
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
        track.decoder_input(5, &h, 0, 30.0, None, 0.0, 0.0, false);
        track.decoder_input(5, &h, 512, 30.0, None, 0.0, 0.0, false);
        assert_eq!(track.refiner_hops_since_reset, 2);
        track.center = 6.0; // centroid crosses the rounding boundary 5 -> 6
        track.decoder_input(5, &h, 1024, 30.0, None, 0.0, 0.0, false);
        assert_eq!(
            track.refiner_hops_since_reset, 1,
            "a real centroid-channel change must reset the hop counter, not just increment it"
        );
        assert_eq!(track.refiner_channel, Some(6));
    }

    #[test]
    fn decoder_input_delays_reported_sample_ts_by_group_delay_hops() {
        // MAN-194: decoder_input now reports the CORRECTLY delayed
        // sample_ts (GROUP_DELAY_HOPS behind the hop it was fed on), not
        // the current hop's own ts (PR #178's since-superseded stopgap) --
        // Refiner::push's output at hop t is evidence about hop t - D.
        let mut track = Track::new(1, 5, &cfg());
        let h = hop_with_x(0, vec![num_complex::Complex32::new(1.0, 0.0); 8]);
        let d = GROUP_DELAY_HOPS as u64;
        let mut reported_ts: Vec<u64> = Vec::new();
        for i in 0..(d + 3) {
            let sample_ts = 1000 + i * 512;
            let out = track.decoder_input(5, &h, sample_ts, 30.0, None, 0.0, 0.0, false);
            reported_ts.extend(out.into_iter().map(|(_, _, _, ts, _, _, _)| ts));
        }
        // Hops 0..D-1 report nothing (no valid delayed observation yet);
        // hop D reports hop 0's ts, hop D+1 reports hop 1's ts, hop D+2
        // reports hop 2's ts.
        let expected: Vec<u64> = (0..3).map(|i| 1000 + i * 512).collect();
        assert_eq!(
            reported_ts, expected,
            "must report delayed timestamps starting from hop 0, once due"
        );
    }

    #[test]
    fn decoder_input_withholds_any_report_until_a_delayed_observation_exists() {
        // MAN-194: hops 0..GROUP_DELAY_HOPS-1 have no valid delayed
        // observation at all (there's nothing D hops in the past yet) --
        // decoder_input must report NOTHING for them, not a same-hop raw
        // substitute (the old stopgap this replaces, which caused a
        // content discontinuity at the warm-up/converged transition).
        let mut track = Track::new(1, 5, &cfg());
        let raw = num_complex::Complex32::new(3.0, 4.0); // |raw| = 5
        let h = hop_with_x(0, vec![raw; 8]);
        for i in 0..GROUP_DELAY_HOPS as u64 {
            let out = track.decoder_input(5, &h, i * 512, 30.0, None, 0.0, 0.0, false);
            assert!(
                out.is_empty(),
                "warm-up hop {i} must report nothing, not a same-hop raw substitute"
            );
        }
    }

    #[test]
    fn decoder_input_reports_raw_magnitude_for_a_not_yet_fully_converged_delayed_hop() {
        // Once a delayed observation becomes due (h >= GROUP_DELAY_HOPS)
        // but its FIR window is still partly zero-padded (h < TAPS), report
        // the RAW magnitude of that delayed hop -- real evidence, correctly
        // time-labeled -- not the FIR's own ramp-in transient (Codex
        // review, PR #178: a ramp-in transient measured ~40% below
        // steady-state amplitude on a constant tone).
        let mut track = Track::new(1, 5, &cfg());
        let raw = num_complex::Complex32::new(3.0, 4.0); // |raw| = 5, power = 25
        let h = hop_with_x(0, vec![raw; 8]);
        let d = GROUP_DELAY_HOPS as u64;
        let full_history_hops = 2 * d + 1;
        for i in 0..full_history_hops {
            let out = track.decoder_input(5, &h, i * 512, 30.0, None, 0.0, 0.0, false);
            if i >= d {
                let expected: SmallVec<[DecoderInputEntry; 1]> =
                    smallvec![(5.0, 25.0, None, (i - d) * 512, 0.0, 0.0, false)];
                assert_eq!(
                    out, expected,
                    "hop {i} (before full convergence at {full_history_hops}) must report the \
                     raw magnitude of its due delayed hop, not a partially-converged FIR output"
                );
            } else {
                assert!(out.is_empty());
            }
        }
    }

    #[test]
    fn decoder_input_does_not_replay_one_observation_as_multiple_delayed_hops() {
        // Regression guard for the exact bug three PR #178 review rounds
        // found: each delayed hop must report ITS OWN raw observation, not
        // a single stale one replayed across several due hops.
        let mut track = Track::new(1, 5, &cfg());
        let loud = num_complex::Complex32::new(3.0, 4.0); // |loud| = 5, power = 25
        let quiet = num_complex::Complex32::new(0.03, 0.04); // |quiet| = 0.05, power = 0.0025
        let h_loud = hop_with_x(0, vec![loud; 8]);
        let h_quiet = hop_with_x(0, vec![quiet; 8]);
        let d = GROUP_DELAY_HOPS as u64;

        track.decoder_input(5, &h_loud, 0, 30.0, None, 0.0, 0.0, false); // hop 0: loud
        for i in 1..d {
            track.decoder_input(5, &h_quiet, i * 512, 30.0, None, 0.0, 0.0, false);
            // hops 1..D-1: quiet
        }
        // Hop D: due observation is hop 0's (loud).
        let out_d = track.decoder_input(5, &h_quiet, d * 512, 30.0, None, 0.0, 0.0, false);
        let expected_d: SmallVec<[DecoderInputEntry; 1]> =
            smallvec![(5.0, 25.0, None, 0, 0.0, 0.0, false)];
        assert_eq!(
            out_d, expected_d,
            "hop D must report hop 0's loud observation"
        );
        // Hop D+1: due observation is hop 1's (quiet), not hop 0's replayed.
        let out_d1 = track.decoder_input(5, &h_quiet, (d + 1) * 512, 30.0, None, 0.0, 0.0, false);
        let expected_d1: SmallVec<[DecoderInputEntry; 1]> =
            smallvec![(0.05, 0.0025, None, 512, 0.0, 0.0, false)];
        assert_eq!(
            out_d1, expected_d1,
            "hop D+1 must report hop 1's OWN quiet observation, not hop 0's loud one replayed"
        );
    }

    #[test]
    fn decoder_input_reports_refined_amp_once_fully_converged() {
        // Full convergence needs TAPS == 2*GROUP_DELAY_HOPS+1 real hops fed
        // to the refiner since reset -- one hop past the raw-magnitude
        // regime the previous tests cover, the due delayed hop's window is
        // fully real and decoder_input must report the FIR's own output.
        let mut track = Track::new(1, 5, &cfg());
        let raw = num_complex::Complex32::new(3.0, 4.0); // |raw| = 5
        let h = hop_with_x(0, vec![raw; 8]);
        let d = GROUP_DELAY_HOPS as u64;
        let full_history_hops = 2 * d + 1;
        for _ in 0..(full_history_hops - 1) {
            track.decoder_input(5, &h, 0, 30.0, None, 0.0, 0.0, false);
        }
        let out = track.decoder_input(5, &h, 0, 30.0, None, 0.0, 0.0, false);
        assert_eq!(out.len(), 1);
        let (amp, _, _, _, _, _, _) = out[0];
        assert!(
            amp.is_finite() && amp > 0.0,
            "post-convergence amplitude must be a real, finite, positive refined value"
        );
    }

    #[test]
    fn decoder_input_reset_drains_the_old_channels_backlog() {
        // MAN-194: a real centroid-channel change must not silently drop
        // the old channel's buffered-but-undelivered backlog -- it must be
        // flushed (raw-quality, since no more real input will ever arrive
        // for it) as part of the SAME call that detects the reset.
        let mut track = Track::new(1, 5, &cfg());
        let h = hop_with_x(0, vec![num_complex::Complex32::new(3.0, 4.0); 8]); // |raw|=5, power=25
                                                                               // Feed 3 hops on channel 5 (n=1..3, all < D=5, all buffered, none
                                                                               // popped yet).
        for i in 0..3u64 {
            let out = track.decoder_input(5, &h, i * 512, 30.0, None, 0.0, 0.0, false);
            assert!(out.is_empty());
        }
        // Reassign to channel 6: the reset must drain exactly those 3
        // buffered hops (raw magnitude, their own original timestamps),
        // even though the new channel's own contribution this hop is
        // itself not yet due.
        track.center = 6.0;
        let out = track.decoder_input(6, &h, 3 * 512, 30.0, None, 0.0, 0.0, false);
        let expected: SmallVec<[DecoderInputEntry; 1]> = smallvec![
            (5.0, 25.0, None, 0, 0.0, 0.0, false),
            (5.0, 25.0, None, 512, 0.0, 0.0, false),
            (5.0, 25.0, None, 1024, 0.0, 0.0, false)
        ];
        assert_eq!(
            out, expected,
            "reset must drain the old channel's backlog in order, before starting the new one"
        );
    }

    #[test]
    fn decoder_input_raw_power_uses_the_owned_channel_k_even_when_it_differs_from_the_centroid() {
        // Codex review, PR #178: TrackDecoder's calibrated NoiseTracker
        // needs the tracked channel's real, full-bandwidth power (SPEC v2
        // §2.1), not a refined value -- confirmed here with k and c set to
        // channels with clearly different powers.
        let mut track = Track::new(1, 5, &cfg()); // center stays 5.0 -> c=5
        let mut samples = vec![num_complex::Complex32::new(1.0, 0.0); 8];
        samples[5] = num_complex::Complex32::new(3.0, 4.0); // channel c=5: power 25
        samples[6] = num_complex::Complex32::new(1.0, 0.0); // channel k=6: power 1
        let h = hop_with_x(0, samples);
        let d = GROUP_DELAY_HOPS as u64;
        let mut last_out: SmallVec<[DecoderInputEntry; 1]> = SmallVec::new();
        for i in 0..=d {
            last_out = track.decoder_input(6, &h, i * 512, 30.0, None, 0.0, 0.0, false);
        }
        let expected: SmallVec<[DecoderInputEntry; 1]> =
            smallvec![(5.0, 1.0, None, 0, 0.0, 0.0, false)];
        assert_eq!(
            last_out, expected,
            "raw_power must come from k=6 (power 1), not c=5 (power 25) or any refined value"
        );
    }

    #[test]
    fn decoder_input_applies_the_odd_channel_sign_correction() {
        // The real channelizer's rotation step bakes the checkerboard sign
        // artifact into `HopOutput::x[k]` itself; a synthetic `HopOutput`
        // for a physically-constant tone must reproduce that same artifact
        // to exercise decoder_input's fix. decoder_input must undo it
        // before the refiner sees the sample.
        let raw = num_complex::Complex32::new(1.0, 0.0);
        let mean_amp = |channel: usize| -> f32 {
            let mut track = Track::new(1, channel, &cfg());
            let mut sum = 0.0f32;
            let mut count = 0u32;
            for m in 0..200u64 {
                let artifacted = raw * odd_channel_sign_correction(m, channel);
                let h = hop_with_x(m, vec![artifacted; 8]);
                let out = track.decoder_input(channel, &h, m * 512, 30.0, None, 0.0, 0.0, false);
                if m > 50 {
                    for (amp, _, _, _, _, _, _) in out {
                        sum += amp;
                        count += 1;
                    }
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
    fn drain_refiner_into_pending_flushes_the_full_backlog() {
        // MAN-194: verifies the primitive TrackManager::finish (Task 4) and
        // the closure-handling paths below rely on -- draining a track's
        // refiner backlog directly into `pending`. Each backlog entry
        // already carries its own `spectral_ref_power`/`current_snr_db`,
        // captured by `decoder_input` at the instant it was observed;
        // there is nothing left for this drain to apply separately.
        let mut track = Track::new(1, 5, &cfg());
        let h = hop_with_x(0, vec![num_complex::Complex32::new(3.0, 4.0); 8]); // |raw|=5, power=25
        let d = GROUP_DELAY_HOPS as u64;
        // Steady state: backlog holds exactly d entries once n > d.
        for i in 0..(d + 3) {
            track.decoder_input(5, &h, i * 512, 30.0, None, 0.0, 0.0, false);
        }
        assert_eq!(track.refiner_backlog.len(), d as usize);
        track.drain_refiner_into_pending();
        assert!(
            track.refiner_backlog.is_empty(),
            "drain must flush the entire backlog"
        );
        assert_eq!(
            track.pending.len(),
            d as usize,
            "drain must deliver every buffered-but-undelivered hop into pending"
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

    // ---- MAN-108 (SPEC §2.4): DORMANT ----

    /// `cfg()` with an 8-hop re-acquisition window.
    fn reacq_cfg() -> DetectorConfig {
        DetectorConfig {
            reacquire_hops: 8,
            ..cfg()
        }
    }

    /// Promoted, then one drop hop: HANG with `hang_count == 1`.
    fn lc_in_hang(cfg: &DetectorConfig, reacquirable: bool) -> Lifecycle {
        let mut lc = Lifecycle::new(cfg);
        for _ in 0..4 {
            lc.on_hop(true, false, false); // the 4th promotes
        }
        if reacquirable {
            lc.set_reacquirable();
        }
        lc.on_hop(false, true, true); // -> HANG, hang_count=1
        assert_eq!(lc.state(), LifecycleState::Hang);
        lc
    }

    /// HANG run out on a reacquirable track: DORMANT.
    fn lc_dormant(cfg: &DetectorConfig) -> Lifecycle {
        let mut lc = lc_in_hang(cfg, true);
        for _ in 0..(cfg.hang_hops - 2) {
            assert_eq!(lc.on_hop(false, true, true), LifecycleEvent::None);
        }
        assert_eq!(lc.on_hop(false, true, true), LifecycleEvent::Dormant);
        lc
    }

    #[test]
    fn reacquirable_track_goes_dormant_instead_of_closing() {
        let lc = lc_dormant(&reacq_cfg());
        assert_eq!(lc.state(), LifecycleState::Dormant);
    }

    #[test]
    fn dormant_track_reacquires_after_confirm_hops_of_rise() {
        // A 20-hop window: the 4 + 1 + 4 non-waking hops below would
        // otherwise run out `reacq_cfg()`'s 8.
        let cfg = DetectorConfig {
            reacquire_hops: 20,
            ..cfg()
        };
        let mut lc = lc_dormant(&cfg);
        for _ in 0..4 {
            assert_eq!(lc.on_hop(true, false, false), LifecycleEvent::None);
        }
        // A gap resets the wake count.
        assert_eq!(lc.on_hop(false, true, false), LifecycleEvent::None);
        for _ in 0..4 {
            assert_eq!(lc.on_hop(true, false, false), LifecycleEvent::None);
        }
        assert_eq!(lc.state(), LifecycleState::Dormant);
        assert_eq!(lc.on_hop(true, false, false), LifecycleEvent::Reacquired);
        assert_eq!(lc.state(), LifecycleState::Active);
    }

    #[test]
    fn dormant_track_closes_hang_expired_after_reacquire_hops() {
        let mut lc = lc_dormant(&reacq_cfg());
        for _ in 0..7 {
            assert_eq!(lc.on_hop(false, true, false), LifecycleEvent::None);
        }
        assert_eq!(
            lc.on_hop(false, true, false),
            LifecycleEvent::Closed(CloseReason::HangExpired)
        ); // dormant_count=8 >= reacquire_hops=8
    }

    #[test]
    fn dormant_does_not_advance_the_gc_timer() {
        // A 30-hop window, longer than `gc_hops` (20): a GC timer running
        // while DORMANT would close it `Silent` first.
        let cfg = DetectorConfig {
            reacquire_hops: 30,
            ..cfg()
        };
        let mut lc = lc_dormant(&cfg);
        for _ in 0..29 {
            assert_eq!(lc.on_hop(false, true, false), LifecycleEvent::None);
        }
        assert_eq!(
            lc.on_hop(false, true, false),
            LifecycleEvent::Closed(CloseReason::HangExpired)
        );
    }

    #[test]
    fn unreacquirable_track_still_closes_at_hang_hops() {
        let mut lc = lc_in_hang(&reacq_cfg(), false);
        for _ in 0..8 {
            assert_eq!(lc.on_hop(false, true, true), LifecycleEvent::None);
        }
        assert_eq!(
            lc.on_hop(false, true, true),
            LifecycleEvent::Closed(CloseReason::HangExpired)
        );
    }

    #[test]
    fn reacquire_hops_zero_closes_at_hang_hops_even_when_reacquirable() {
        let cfg = DetectorConfig {
            reacquire_hops: 0,
            ..cfg()
        };
        let mut lc = lc_in_hang(&cfg, true);
        for _ in 0..8 {
            assert_eq!(lc.on_hop(false, true, true), LifecycleEvent::None);
        }
        assert_eq!(
            lc.on_hop(false, true, true),
            LifecycleEvent::Closed(CloseReason::HangExpired)
        );
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

    // ---- MAN-196: trusted-band spawn veto ----

    /// 512 channels at 48 kS/s: the channel grid the decimated path
    /// actually uses, with channel centres at `k' * 93.75` Hz.
    /// Filter off: these cells drive steady unkeyed carriers, which the
    /// MAN-313 non-CW filter (rightly) never qualifies.
    fn trusted_band_manager(n: usize) -> TrackManager {
        TrackManager::new(
            n,
            (n as f64) * 93.75,
            14_000_000.0,
            default_filter_off(),
            DecodeConfig::default(),
        )
    }

    fn strong_on(n: usize, k: usize) -> Vec<f32> {
        let mut power = quiet_power(n);
        power[k] = 1e-9 * 10f32.powf(20.0 / 10.0); // +20 dB above the floor
        power
    }

    /// MAN-196: a rising local peak outside the trusted band never
    /// spawns. Channel 251 of a 512-channel 48 kS/s grid has its centre
    /// at 251 * 93.75 = 23 531.25 Hz -- the -22.8 dB point of the
    /// decimator's transition band and where the duplicate-track
    /// regression was measured.
    #[test]
    fn spawn_beyond_the_trusted_band_is_vetoed() {
        let trusted = manta_dsp::decimate::TRUSTED_HALF_BAND_FRACTION * 48_000.0;
        let start = 250u64 * 15;
        for k in [251usize] {
            // Control (no trusted band supplied): a track spawns.
            let mut control = trusted_band_manager(512);
            feed_warmup(&mut control, 512);
            for m in start..start + 60 {
                control.step_hop(&hop(m, strong_on(512, k)), m);
            }
            assert!(
                !control.tracks.is_empty(),
                "control (no trusted band) must spawn on channel {k}"
            );

            // With the trusted band: zero tracks, ever.
            let mut vetoed = trusted_band_manager(512);
            feed_warmup(&mut vetoed, 512);
            vetoed.set_trusted_band_hz(-trusted, trusted);
            for m in start..start + 60 {
                vetoed.step_hop(&hop(m, strong_on(512, k)), m);
            }
            assert!(
                vetoed.tracks.is_empty(),
                "channel {k} (centre {} Hz) lies beyond the trusted band {trusted} Hz",
                k as f64 * 93.75
            );
        }
    }

    /// MAN-196: the veto is spawns-only -- narrowing the trusted band
    /// under a running track neither closes it nor holds its decode. The
    /// MAN-314 key-up-gap lesson, pinned: a rule applied to running
    /// tracks kills real CW in its key-up gaps.
    #[test]
    fn trusted_band_veto_never_closes_an_existing_track() {
        let mut tm = trusted_band_manager(512);
        feed_warmup(&mut tm, 512);
        let start = 250u64 * 15;
        // Spawn + promote well inside the band first.
        for m in start..start + 60 {
            tm.step_hop(&hop(m, strong_on(512, 10)), m);
        }
        assert!(tm
            .tracks
            .values()
            .any(|t| t.state() == LifecycleState::Active));

        // Narrow the trusted band to exclude channel 10 mid-run, as if it
        // had always been outside, and keep feeding the same carrier.
        tm.set_trusted_band_hz(-100.0, 100.0);
        let hops: Vec<HopOutput> = (start + 60..start + 160)
            .map(|m| hop(m, strong_on(512, 10)))
            .collect();
        let events = tm.process_hops(&hops, |m| m);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, DecoderEvent::TrackClosed { .. })),
            "narrowing the trusted band must not close the running track: {events:?}"
        );
        assert!(
            tm.decoding_track_count() >= 1,
            "the track outside the (re)narrowed band keeps its decoder"
        );
    }

    /// MAN-196: unset (or full-Nyquist) trusted band is bit-identical to
    /// today -- the no-decimator path is a no-op by construction.
    #[test]
    fn unset_trusted_band_is_bit_identical_to_today() {
        let n = 512;
        let start = 250u64 * 15;
        let hops: Vec<HopOutput> = {
            let mut hs: Vec<HopOutput> = (0..start).map(|m| hop(m, quiet_power(n))).collect();
            hs.extend((start..start + 200).map(|m| hop(m, strong_on(n, 200))));
            hs
        };
        let run = |full_band: bool| -> Vec<DecoderEvent> {
            let mut tm = trusted_band_manager(n);
            if full_band {
                // Covers the full Nyquist: the setter's documented no-op
                // arm, and exactly what a boundary-holding source whose
                // band happens to span everything would report.
                tm.set_trusted_band_hz(-24_000.0, 24_000.0);
            }
            tm.process_hops(&hops, |m| m)
        };
        assert_eq!(run(true), run(false));
    }

    /// MAN-196: boundary granularity is the channel centre, with no
    /// margin channel either side (the crash live set's +1-channel margin
    /// exists for quartile judgment, a different purpose). Channel 231's
    /// centre (231 * 93.75 = 21 656.25 Hz) is the last inside the
    /// 21 724.8 Hz boundary; channel 232's (21 750.0 Hz) is the first
    /// outside. Channels 232-255 per side (24 of 256) are conceded.
    #[test]
    fn boundary_granularity_is_the_channel_centre() {
        let trusted = manta_dsp::decimate::TRUSTED_HALF_BAND_FRACTION * 48_000.0;
        let start = 250u64 * 15;
        for (k, inside) in [(230usize, true), (231usize, true), (232usize, false)] {
            let mut tm = trusted_band_manager(512);
            feed_warmup(&mut tm, 512);
            tm.set_trusted_band_hz(-trusted, trusted);
            for m in start..start + 60 {
                tm.step_hop(&hop(m, strong_on(512, k)), m);
            }
            assert_eq!(
                tm.tracks.is_empty(),
                !inside,
                "channel {k} (centre {} Hz) vs boundary {trusted} Hz",
                k as f64 * 93.75
            );
        }
    }

    /// MAN-122 review round 1, the exact failure the reviewer described: a
    /// steady unmodulated carrier promotes a real track and leases it a
    /// real decoder, but the demodulator never latches, so the decoder
    /// emits *nothing* -- no `TrackMeta` (withheld until `snr_2500_db()` is
    /// `Some`), no `CharDecoded`, nothing. `TrackManager` legitimately
    /// holds that track ACTIVE until the ~30 s `gc_hops` GC. Any count
    /// derived from the `DecoderEvent` stream therefore reports 0 while a
    /// decoder is genuinely running; `decoding_track_count()` reports 1.
    #[test]
    fn decoding_track_count_sees_a_promoted_track_that_has_emitted_nothing() {
        let mut tm = TrackManager::new(
            64,
            96_000.0,
            14_000_000.0,
            default_filter_off(),
            DecodeConfig::default(),
        );
        feed_warmup(&mut tm, 64);
        let mut power = quiet_power(64);
        power[10] = 1e-9 * 10f32.powf(20.0 / 10.0);
        let start = 250 * 15;
        let hops: Vec<HopOutput> = (start..start + 120)
            .map(|m| hop(m, power.clone()))
            .collect();
        let events = tm.process_hops(&hops, |m| m);

        // `TrackPromoted` is a state-machine event `process_hops` emits for
        // `doctor()`'s NoSignal check (2026-09-09 decision doc), NOT decoder
        // output -- the point here is that the *decoder* produced nothing.
        let decoded: Vec<&DecoderEvent> = events
            .iter()
            .filter(|e| !matches!(e, DecoderEvent::TrackPromoted { .. }))
            .collect();
        assert!(
            decoded.is_empty(),
            "an unmodulated carrier must not decode anything -- got {decoded:?}"
        );
        assert_eq!(
            tm.decoding_track_count(),
            1,
            "the promoted track holds a leased decoder and must be counted \
             even though its decoder has emitted no event"
        );
        // The distinction this method exists for: `active_track_count()`
        // is a different question (every entry, CANDIDATEs included) and
        // keeps its own meaning for soak_metrics.
        assert!(tm.active_track_count() >= tm.decoding_track_count());
    }

    #[test]
    fn decoding_track_count_excludes_unpromoted_candidates() {
        let cfg = DetectorConfig {
            confirm_hops: 10_000, // nothing promotes within this test
            ..DetectorConfig::default()
        };
        let mut tm = TrackManager::new(64, 96_000.0, 14_000_000.0, cfg, DecodeConfig::default());
        feed_warmup(&mut tm, 64);
        let mut power = quiet_power(64);
        power[10] = 1e-9 * 10f32.powf(20.0 / 10.0);
        for m in (250 * 15)..(250 * 15 + 60) {
            tm.step_hop(&hop(m, power.clone()), m);
        }
        assert_eq!(tm.active_track_count(), 1, "a CANDIDATE exists");
        assert_eq!(
            tm.decoding_track_count(),
            0,
            "a CANDIDATE has leased no decoder and nothing is decoding it"
        );
    }

    /// `step_hop` itself must return a `TrackPromoted` event at the exact
    /// hop it promotes -- `manta_engine::doctor()`'s NoSignal check
    /// (2026-09-09) depends on this being real, ground-truth signal, not
    /// just an internal state-machine transition nothing outside
    /// `TrackManager` ever observes.
    // ---- MAN-324: live-state accessors ----

    #[test]
    fn channel_levels_cover_every_channel_in_ascending_frequency() {
        let fs = 96_000.0;
        let center = 14_000_000.0;
        let ch = manta_dsp::channelizer::Channelizer::new(fs, center).unwrap();
        let n = ch.n_channels();
        let mut tm = TrackManager::new(
            n,
            fs,
            center,
            DetectorConfig::default(),
            DecodeConfig::default(),
        );
        let levels = tm.take_channel_levels();
        assert_eq!(levels.len(), n);
        for w in levels.windows(2) {
            assert!((w[1].freq_hz - w[0].freq_hz - 93.75).abs() < 1e-6, "{w:?}");
        }
        let mut expected: Vec<f64> = (0..n).map(|k| ch.channel_freq_hz(k)).collect();
        expected.sort_by(f64::total_cmp);
        for (l, e) in levels.iter().zip(&expected) {
            assert!((l.freq_hz - e).abs() < 1e-6);
            assert!(!l.occupied, "occupancy is off by default");
        }
    }

    /// Codex P2 on #281: an audio or KiwiSDR source delivers RF only inside
    /// its passband; band state must not report the rest of the
    /// channelizer span.
    #[test]
    fn channel_levels_cover_only_the_source_rf_passband() {
        let fs = 48_000.0;
        let center = 14_000_000.0;
        let n = manta_dsp::channelizer::Channelizer::new(fs, center)
            .unwrap()
            .n_channels();
        let mut tm = TrackManager::new(
            n,
            fs,
            center,
            DetectorConfig::default(),
            DecodeConfig::default(),
        );
        tm.set_rf_passband_hz(300.0, 2700.0);
        let levels = tm.take_channel_levels();
        assert!(!levels.is_empty());
        for l in &levels {
            let off = l.freq_hz - center;
            assert!((300.0..=2700.0).contains(&off), "{l:?}");
        }
        // ~2400 Hz of passband at 93.75 Hz spacing.
        assert!((25..=27).contains(&levels.len()), "{}", levels.len());
        tm.set_rf_passband_hz(-fs / 2.0, fs / 2.0);
        assert_eq!(
            tm.take_channel_levels().len(),
            n,
            "full band: every channel"
        );
    }

    #[test]
    fn occupancy_window_reports_a_channel_that_rose_once_then_resets() {
        let mut tm = TrackManager::new(
            64,
            96_000.0,
            14_000_000.0,
            DetectorConfig::default(),
            DecodeConfig::default(),
        );
        tm.enable_occupancy_window();
        feed_warmup(&mut tm, 64);
        tm.take_channel_levels();
        let mut power = quiet_power(64);
        power[10] = 1e-9 * 10f32.powf(30.0 / 10.0);
        let m0 = 250 * 15;
        // A short burst (smoothed power must cross on_snr_db once).
        for m in m0..(m0 + 30) {
            tm.step_hop(&hop(m, power.clone()), m);
        }
        for m in (m0 + 30)..(m0 + 400) {
            tm.step_hop(&hop(m, quiet_power(64)), m);
        }
        let levels = tm.take_channel_levels();
        let occupied: Vec<usize> = levels
            .iter()
            .enumerate()
            .filter(|(_, l)| l.occupied)
            .map(|(i, _)| i)
            .collect();
        let k10_freq = 14_000_000.0 + wrapped_channel_offset(10, 64) * 1500.0;
        assert!(
            levels
                .iter()
                .any(|l| l.occupied && (l.freq_hz - k10_freq).abs() < 1e-6),
            "{occupied:?}"
        );
        for m in (m0 + 400)..(m0 + 800) {
            tm.step_hop(&hop(m, quiet_power(64)), m);
        }
        assert!(tm.take_channel_levels().iter().all(|l| !l.occupied));
    }

    #[test]
    fn occupancy_tracking_never_changes_events() {
        let run = |occupancy: bool| {
            let mut tm = TrackManager::new(
                64,
                96_000.0,
                14_000_000.0,
                DetectorConfig::default(),
                DecodeConfig::default(),
            );
            if occupancy {
                tm.enable_occupancy_window();
            }
            let mut out = Vec::new();
            for m in 0..(250 * 15 + 2_000) {
                let mut power = quiet_power(64);
                if m >= 250 * 15 && (m / 40) % 2 == 0 {
                    power[10] = 1e-9 * 10f32.powf(25.0 / 10.0);
                }
                let (closed, promoted, flush) = tm.step_hop(&hop(m, power), m);
                out.push(format!("{closed:?}{promoted:?}{flush:?}"));
            }
            out.push(format!("{:?}", tm.finish()));
            out
        };
        assert_eq!(run(false), run(true));
    }

    #[test]
    fn decoding_track_freqs_lists_only_tracks_holding_a_decoder() {
        let mut tm = TrackManager::new(
            64,
            96_000.0,
            14_000_000.0,
            default_filter_off(),
            DecodeConfig::default(),
        );
        feed_warmup(&mut tm, 64);
        assert!(tm.decoding_track_freqs_hz().is_empty());
        let mut power = quiet_power(64);
        power[10] = 1e-9 * 10f32.powf(20.0 / 10.0);
        let mut m = 250 * 15;
        let mut saw_candidate = false;
        while tm.decoding_track_count() == 0 && m < 250 * 15 + 400 {
            tm.step_hop(&hop(m, power.clone()), m);
            if tm.active_track_count() > tm.decoding_track_count() {
                // A CANDIDATE holds no decoder yet.
                saw_candidate = true;
                assert!(tm.decoding_track_freqs_hz().is_empty());
            }
            m += 1;
        }
        assert!(saw_candidate);
        let freqs = tm.decoding_track_freqs_hz();
        assert_eq!(freqs.len(), tm.decoding_track_count());
        assert_eq!(freqs.len(), 1);
        let k10_freq = 14_000_000.0 + wrapped_channel_offset(10, 64) * 1500.0;
        assert!((freqs[0] - k10_freq).abs() < 1500.0, "{freqs:?}");
    }

    #[test]
    fn step_hop_emits_track_promoted_at_the_promotion_hop() {
        let mut tm = TrackManager::new(
            64,
            96_000.0,
            14_000_000.0,
            default_filter_off(),
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

    #[test]
    fn step_hop_reset_drains_the_refiners_backlog_into_pending() {
        // Engine-level regression test for MAN-194 Scenario 1 (the
        // ticket's own Gherkin): a mid-mark channel reassignment must not
        // silently drop the refiner's buffered-but-undelivered backlog.
        let n = 64;
        let mut tm = TrackManager::new(
            n,
            96_000.0,
            14_000_000.0,
            DetectorConfig::default(),
            DecodeConfig {
                refine_bw_hz: 30.0,
                ..DecodeConfig::default()
            },
        );
        // decoder_input's refine-enabled path reads hop.x[c], so every
        // hop fed to a promoted track needs real complex spectrum data,
        // not the bare `hop()` helper (empty `x`, which would panic on
        // index-out-of-bounds). Build x from power so HopOutput::power
        // matches exactly (norm_sqr of the real value p.sqrt() is p).
        let mk_hop = |m: u64, power: &Vec<f32>| {
            let x: Vec<num_complex::Complex32> = power
                .iter()
                .map(|&p| num_complex::Complex32::new(p.sqrt(), 0.0))
                .collect();
            hop_with_x(m, x)
        };
        feed_warmup(&mut tm, n);
        let mut power = quiet_power(n);
        power[10] = 1e-9 * 10f32.powf(20.0 / 10.0);
        let mut m = 250 * 15;
        loop {
            tm.step_hop(&mk_hop(m, &power), m);
            m += 1;
            if tm
                .tracks
                .values()
                .any(|t| t.state() == LifecycleState::Active)
            {
                break;
            }
        }
        let id = *tm.tracks.keys().next().unwrap();
        let d = GROUP_DELAY_HOPS as u64;
        // Run well past GROUP_DELAY_HOPS so the backlog reaches steady
        // state (exactly d entries buffered at any instant).
        for _ in 0..(d + 5) {
            tm.step_hop(&mk_hop(m, &power), m);
            m += 1;
        }
        let pending_before = tm.tracks.get(&id).unwrap().pending.len();
        tm.step_hop(&mk_hop(m, &power), m); // one more normal hop
        m += 1;
        let pending_after_normal_hop = tm.tracks.get(&id).unwrap().pending.len();
        assert_eq!(
            pending_after_normal_hop - pending_before,
            1,
            "a steady-state hop with no reset must deliver exactly one pending entry"
        );

        // Force a channel reassignment: move the strong signal to channel
        // 11 and directly set the track's live centroid EMA to force the
        // rounding-boundary crossing this same hop (isolating the reset
        // from the EMA's normal, much slower settling behavior, which this
        // test isn't about).
        let mut power2 = quiet_power(n);
        power2[11] = 1e-9 * 10f32.powf(20.0 / 10.0);
        tm.tracks.get_mut(&id).unwrap().center = 11.0;
        tm.step_hop(&mk_hop(m, &power2), m);
        let pending_after_reset_hop = tm.tracks.get(&id).unwrap().pending.len();
        assert_eq!(
            pending_after_reset_hop - pending_after_normal_hop,
            d as usize,
            "a reset hop must drain the full backlog ({d} entries), not just deliver \
             one entry and silently drop the rest"
        );
    }

    #[test]
    fn step_hop_closure_drains_the_refiners_backlog_before_finishing() {
        // MAN-194: step_hop's own closure handling (the `closed.retain`
        // closure, below) drains a closing track's refiner backlog into
        // `pending` BEFORE calling finish_decoder/finish_decoder_speed_only
        // -- if a future edit moved the drain after the flush, a real
        // buffered-but-undelivered observation would be silently discarded
        // along with the rest of the removed `Track`, exactly the bug
        // class this branch exists to fix. Uses the same withheld-final-
        // dit technique as `finish_drains_every_remaining_tracks_refiner_
        // backlog_before_flushing` above: the decoded text must genuinely
        // end in "S", not truncate to "I", which is only possible if the
        // backlog's withheld dit actually reached the decoder before the
        // flush.
        let small_cfg = cfg(); // confirm_hops=5, hang_hops=10, gc_hops=20
        let mut tm = TrackManager::new(
            64,
            96_000.0,
            14_000_000.0,
            small_cfg,
            DecodeConfig::default(),
        );
        tm.spawn(10);
        let id = *tm.tracks.keys().next().unwrap();
        let env = rect_envelope_hops("PARIS", 18);
        // Withhold exactly the final dit of "S", same split point as the
        // `finish()` sibling test -- fed live via push_envelope up through
        // the second dit and its trailing gap (decoded so far: "PARI" plus
        // ".." == glyph I), with the third dit's real evidence seeded
        // directly into `refiner_backlog` below instead.
        let split = env.len() - 18;
        let mut decoder = TrackDecoder::new(id, DecodeConfig::default());
        for (i, &a) in env[..split].iter().enumerate() {
            decoder.push_envelope(a, i as u64);
        }
        {
            let track = tm.tracks.get_mut(&id).unwrap();
            // Force this track straight to ACTIVE (bypassing the
            // detector's own rise/gate confirmation machinery, which this
            // test isn't about) -- same field-level technique the
            // standalone Lifecycle tests above use.
            for _ in 1..small_cfg.confirm_hops {
                track.lifecycle.on_hop(true, false, false);
            }
            assert_eq!(
                track.state(),
                LifecycleState::Active,
                "test setup: track must be ACTIVE before driving it to closure"
            );
            // Seed the withheld final dit directly into refiner_backlog,
            // exactly the shape Track::decoder_input leaves behind
            // mid-burst (Task 1).
            for (j, &a) in env[split..].iter().enumerate() {
                track.refiner_backlog.push_back((
                    a,
                    a * a,
                    None,
                    (split + j) as u64,
                    0.0,
                    0.0,
                    false,
                ));
            }
            track.has_emitted = true;
            // Decoder deliberately NOT attached yet -- keeps the
            // intervening step_hop hops below (needed to genuinely drive
            // HangExpired through real step_hop) from queuing extra noise
            // samples into `pending` ahead of the backlog's real content,
            // which would pollute the decode timeline this test isolates.
        }

        // Real, uniform-quiet power: FloorBank/Gate both seed directly
        // from this very first hop's own sample (no warm-up needed), so
        // every hop's SNR reads ~0 dB -- `drop` (SPEC §2.3) immediately
        // and forever, exactly the sustained-drop condition step_hop's own
        // real ACTIVE -> HANG -> HangExpired path needs, with nothing
        // hand-rolled about the transition itself.
        let n = 64;
        let quiet = quiet_power(n);
        let mut m = 0u64;
        for _ in 0..(small_cfg.hang_hops - 1) {
            tm.step_hop(&hop(m, quiet.clone()), m);
            m += 1;
        }
        assert_eq!(
            tm.tracks.get(&id).unwrap().state(),
            LifecycleState::Hang,
            "test setup: track must be HANG, one hop away from HangExpired"
        );
        // Attach the decoder now, right before the closing hop -- the
        // drain this test targets happens inside THIS call, in step_hop's
        // real `closed.retain` closure.
        tm.tracks.get_mut(&id).unwrap().decoder = Some(decoder);

        let (closed, _, flush_events) = tm.step_hop(&hop(m, quiet.clone()), m);
        assert_eq!(
            closed,
            vec![(id, ClosureKind::SignalEnded)],
            "expected step_hop's own closure handling to close this track as HangExpired"
        );
        let decoded: String = flush_events
            .iter()
            .filter_map(|e| match e {
                DecoderEvent::CharDecoded { glyph, .. } => glyph.text_char(),
                _ => None,
            })
            .collect();
        assert!(
            decoded.ends_with('S'),
            "the backlog's final withheld dit must still reach the decoder before step_hop's \
             closure flush, so the track closes out decoding \"S\", not silently truncating to \
             \"I\" from a dropped backlog -- got decoded={decoded:?}, flush_events={flush_events:?}"
        );
    }

    #[test]
    fn finish_drains_every_remaining_tracks_refiner_backlog_before_flushing() {
        // MAN-194 Scenario 2 (EOF): TrackManager::finish must not silently
        // drop a still-open track's buffered-but-undelivered refiner
        // backlog -- decoder_input's own unit tests (Task 1) already cover
        // the underlying delay math exhaustively, so this is purely a
        // wiring check: does `finish()` actually route `refiner_backlog`
        // through the decoder before flushing?
        //
        // Deviation from the plan's original brief: that version drove a
        // constant-amplitude synthetic carrier through `step_hop` in a
        // loop and asserted a `TrackClosed` appeared in `finish()`'s
        // output. Empirically (verified by running it against both the
        // old and the new `finish()`), that never happens either way: the
        // Legacy engine's `Demod` requires >= `MIN_KEYING_RATIO` (2.0)
        // amplitude spread over its first 375-hop calibration window
        // before it ever leaves `Phase::Init` and can emit a `Run`
        // (envelope.rs); a truly constant carrier has a spread of 1.0 and
        // never calibrates, backlog drained or not, so `has_emitted`
        // never becomes true and the assertion cannot distinguish correct
        // from buggy code. Replaced with the same direct
        // decoder/backlog-seeding pattern already used by
        // `merge_converged_drains_queued_pending_samples_before_finishing`
        // just above -- real Morse envelope data (`rect_envelope_hops`),
        // fed far enough to calibrate and decode "PARI", with the final
        // letter's evidence withheld from the decoder and placed directly
        // in `refiner_backlog` instead of being pushed live.
        let mut tm = TrackManager::new(
            64,
            96_000.0,
            14_000_000.0,
            DetectorConfig::default(),
            DecodeConfig::default(),
        );
        tm.spawn(10);
        let id = *tm.tracks.keys().next().unwrap();
        {
            let track = tm.tracks.get_mut(&id).unwrap();
            track.decoder = Some(TrackDecoder::new(id, DecodeConfig::default()));
            let env = rect_envelope_hops("PARIS", 18);
            // "S" is the pattern "..." (3 dits, 18 hops each, separated by
            // 1-dit gaps): withhold exactly the final dit so the decoder,
            // fed only up through the second dit and its trailing gap,
            // has seen only ".." (glyph I) -- the third dit is real
            // evidence that must still reach the decoder through the
            // backlog for the track to correctly close out as "S", not
            // silently truncate to "I".
            let split = env.len() - 18;
            let decoder = track.decoder.as_mut().unwrap();
            for (i, &a) in env[..split].iter().enumerate() {
                decoder.push_envelope(a, i as u64);
            }
            // MAN-194: seed `refiner_backlog` directly with the withheld
            // tail, exactly the shape `Track::decoder_input` leaves behind
            // mid-burst (Task 1) -- `(amp, raw_power, spectral_ref_power,
            // sample_ts)` entries -- rather than routing it through
            // `decoder_input` itself (already covered by Task 1's own
            // exhaustive unit tests).
            for (j, &a) in env[split..].iter().enumerate() {
                track.refiner_backlog.push_back((
                    a,
                    a * a,
                    None,
                    (split + j) as u64,
                    0.0,
                    0.0,
                    false,
                ));
            }
            track.has_emitted = true;
        }
        let backlog_len = tm.tracks.get(&id).unwrap().refiner_backlog.len();
        assert_eq!(
            backlog_len, 18,
            "sanity check: the track must have a full backlog of undelivered hops before EOF"
        );

        let events = tm.finish();
        assert!(
            tm.tracks.is_empty(),
            "finish() must still clear all tracks after draining"
        );
        let decoded: String = events
            .iter()
            .filter(|e| event_track_id(e) == id)
            .filter_map(|e| match e {
                DecoderEvent::CharDecoded { glyph, .. } => glyph.text_char(),
                _ => None,
            })
            .collect();
        assert!(
            decoded.ends_with('S'),
            "the backlog's final withheld dit must still reach the decoder so the \
             track closes out decoding \"S\", not silently truncate to \"I\" from \
             a dropped backlog -- got decoded={decoded:?}, events={events:?}"
        );
    }

    /// MAN-210 (Codex review, PR #303): V51 -- the CLI-level golden gate in
    /// `manta-cli`'s `golden_v7_v9_v10.rs` -- proves the refine-enabled path
    /// runs end-to-end, but the JSON report it asserts on cannot pin the
    /// MAN-194 drain protocols themselves: measured on this exact scene, the
    /// end-of-stream backlog is trailing silence (the last keyed character
    /// closes ~0.24 s before EOF), and a backlog dropped at a reset or at
    /// EOF costs at most ~6 x `GROUP_DELAY_HOPS` hops of envelope, inside
    /// V51's CER bar -- so a regression that drops `refiner_backlog` at a
    /// channel reset or at EOF would leave V51 green. This test closes that
    /// gap one layer down, where the drains are directly observable: the
    /// real V9 drift scene (the exact `render_v9_drift` fixture V51
    /// decodes) through the real channelizer and `TrackManager`, refiner
    /// on, one hop per `process_hops` call:
    ///
    /// - the drift's channel crossing really resets the refiner mid-scene
    ///   with hops still held at the moment of the reset (the reset-drain
    ///   path runs with real work to do);
    /// - delivery is exactly-once: after every hop, the refined run's
    ///   delivered-plus-held hops (decoder `hop_count` + `pending` +
    ///   `refiner_backlog`) equal the refine-off control run's -- a
    ///   backlog dropped or duplicated at ANY reset diverges here and
    ///   never recovers;
    /// - a non-empty backlog is still held when the stream ends (the
    ///   end-of-stream drain has real work; its delivery wiring into the
    ///   decoder is pinned by
    ///   `finish_drains_every_remaining_tracks_refiner_backlog_before_flushing`
    ///   above).
    #[test]
    fn real_drift_scene_drains_refiner_backlog_at_resets_and_eof() {
        // Per-hop drain state of the decoding track: (delivered decoder
        // hop_count, queued pending, held refiner_backlog, refiner
        // channel) -- `None` while no track is promoted.
        type Snap = Option<(u64, usize, usize, Option<usize>)>;
        let spec = manta_testkit::vectors::v9();
        let rendered = manta_testkit::vectors::render_v9_drift(&spec).unwrap();

        // Decode the scene once at `refine_bw_hz`, hop by hop, snapshotting
        // after every `process_hops`.
        let run = |refine_bw_hz: f32| -> Vec<Snap> {
            let mut ch =
                manta_dsp::channelizer::Channelizer::new(spec.fs, spec.center_freq_hz).unwrap();
            let hop = ch.hop() as u64;
            let pad_hops = (ch.filter_len() as u64).div_ceil(hop);
            let mut padded = vec![num_complex::Complex32::new(0.0, 0.0); ch.filter_len()];
            padded.extend_from_slice(&rendered.samples);
            let decode = DecodeConfig {
                refine_bw_hz,
                ..DecodeConfig::default()
            };
            let mut tm = TrackManager::new(
                ch.n_channels(),
                spec.fs,
                spec.center_freq_hz,
                DetectorConfig::default(),
                decode,
            );
            tm.skip_preroll_hops(pad_hops);
            let mut snaps = Vec::new();
            for chunk in padded.chunks(hop as usize) {
                let hops = ch.process(chunk);
                tm.process_hops(&hops, |m| m.saturating_sub(pad_hops) * hop);
                let decoding = tm.tracks.values().filter(|t| t.decoder.is_some()).count();
                assert!(
                    decoding <= 1,
                    "V9 is a single-signal scene: at most one decoding track"
                );
                snaps.push(tm.tracks.values().find_map(|t| {
                    t.decoder.as_ref().map(|d| {
                        (
                            d.hop_count(),
                            t.pending.len(),
                            t.refiner_backlog.len(),
                            t.refiner_channel,
                        )
                    })
                }));
            }
            let backlog_at_eof = snaps
                .last()
                .and_then(|s| s.map(|(_, _, b, _)| b))
                .unwrap_or(0);
            if refine_bw_hz > 0.0 {
                assert!(
                    backlog_at_eof > 0,
                    "sanity: at EOF the refiner must still hold hops for the EOF drain"
                );
            } else {
                assert_eq!(
                    backlog_at_eof, 0,
                    "sanity: refine-off never holds a backlog"
                );
            }
            tm.finish();
            assert!(tm.tracks.is_empty(), "finish() must clear all tracks");
            snaps
        };

        let refined = run(30.0);
        let control = run(0.0);
        assert_eq!(refined.len(), control.len());

        // Promotion parity: both decodes promote their decoding track at
        // the same hop, and it stays live to EOF in both (V9 never closes
        // mid-scene), so the per-hop comparison below is like-for-like.
        let promote_at = |v: &[Snap]| v.iter().position(Option::is_some).expect("no promotion");
        assert_eq!(promote_at(&refined), promote_at(&control));
        for (i, (r, c)) in refined.iter().zip(&control).enumerate() {
            assert_eq!(
                r.is_some(),
                c.is_some(),
                "hop {i}: track lifetime must not depend on refine_bw_hz"
            );
        }

        // Reset drain: the refined run's refiner channel must change
        // mid-scene with hops still held at the moment of the change --
        // V9's +50 Hz/min drift crossing the channel grid, firing
        // `decoder_input`'s reset-drain branch through the real
        // channelizer with a real backlog to deliver.
        let resets_with_backlog = refined
            .windows(2)
            .filter(|w| match (w[0], w[1]) {
                (Some((_, _, held, prev_ch)), Some((_, _, _, ch))) => prev_ch != ch && held > 0,
                _ => false,
            })
            .count();
        assert!(
            resets_with_backlog >= 1,
            "the drift's channel crossing must reset the refiner with a \
             non-empty backlog at least once mid-scene"
        );
        // The control never engages the refiner at all.
        assert!(control.iter().all(|s| s
            .map(|(_, _, backlog, ch)| backlog == 0 && ch.is_none())
            .unwrap_or(true)));

        // Exactly-once delivery: the refiner may delay hops
        // (GROUP_DELAY_HOPS) and re-issue them at raw quality on a drain,
        // but every hop the refine-off decode delivers must be delivered
        // exactly once refine-on too -- nothing dropped at a reset drain,
        // nothing duplicated by a double drain.
        for (i, (r, c)) in refined.iter().zip(&control).enumerate() {
            let (Some((rh, rp, rb, _)), Some((ch, cp, _, _))) = (r, c) else {
                continue;
            };
            assert_eq!(
                rh + *rp as u64 + *rb as u64,
                ch + *cp as u64,
                "hop {i}: delivered ({rh}) + pending ({rp}) + backlog ({rb}) \
                 must equal the refine-off decode's deliveries ({ch} + {cp}): the \
                 refiner must deliver every held hop exactly once"
            );
        }
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
            default_filter_off(),
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
            default_filter_off(),
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

    /// Regression, MAN-197: a spawn candidate's prospective 3-channel
    /// ownership window must not intersect any channel still serving a
    /// post-`Silent` cooldown. Arming (`step_hop`'s Silent-close retain)
    /// and living (`Track::owned`/`select_channel`) are window-granularity
    /// operations, but the spawn gate checked only the birth channel -- so
    /// after a birdie on channel 10 closed Silent (arming {9,10,11}) and the
    /// round-4 per-channel clear released 9 and 11 (their own `rise` was
    /// false), a candidate born on 9 passed the gate and absorbed still-
    /// cooling channel 10 via `select_channel`'s max-power rule, tracking
    /// the birdie for a full illegitimate promote/decode cycle inside the
    /// cooldown window. Measured pre-fix: spawn at hop 15050 while channel
    /// 10's cooldown was armed until 26282.
    #[test]
    fn neighbour_spawn_cannot_absorb_a_cooling_down_channel() {
        let n = 64;
        let mut tm = TrackManager::new(
            n,
            96_000.0,
            14_000_000.0,
            default_filter_off(),
            DecodeConfig::default(),
        );
        feed_warmup(&mut tm, n);
        let mut power = quiet_power(n);
        power[10] = 1e-9 * 10f32.powf(20.0 / 10.0); // birdie: +20 dB, never decodes

        // One full promote -> gc_hops Silent-close cycle, driving step_hop
        // directly so no character is ever decoded (same harness as the
        // sibling MAN-171 tests).
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

        // One more hop: 9/11 cooldowns clear (own rise false), 10 stays armed.
        tm.step_hop(&hop(m, power.clone()), m);
        m += 1;
        assert!(
            tm.channel_cooling_down(10),
            "channel 10 must still be cooling down"
        );
        let until = tm.channel_cooldown_until[10];

        // A strong new signal appears on channel 9 (strong enough to be a
        // local peak against the birdie). Its prospective window {8,9,10}
        // intersects the cooling channel, so no track may spawn before
        // expiry. NOTE: `step_hop` increments hop_counter before the spawn
        // scan, so the gate sees the cooldown expired on calls with
        // m + 1 >= until -- stop one hop short.
        let mut neighbour = power.clone();
        neighbour[9] = 1e-9 * 10f32.powf(30.0 / 10.0);
        while m + 1 < until {
            tm.step_hop(&hop(m, neighbour.clone()), m);
            m += 1;
            assert!(
                tm.tracks.is_empty(),
                "MAN-197: a candidate spawned on channel 9 at hop {m} while channel 10's \
                 cooldown (until {until}) was still armed"
            );
        }
        // Rate limit, not denylist: once the cooldown expires the channel-9
        // signal must be allowed to spawn and promote.
        let mut repromoted = false;
        for _ in 0..300 {
            let (_, promoted, _) = tm.step_hop(&hop(m, neighbour.clone()), m);
            m += 1;
            if promoted
                .iter()
                .any(|e| matches!(e, DecoderEvent::TrackPromoted { .. }))
            {
                repromoted = true;
                break;
            }
        }
        assert!(
            repromoted,
            "channel 9 never promoted even after channel 10's cooldown expired"
        );
    }

    /// Regression, MAN-197 (Codex review, PR #301): the respawn cooldown
    /// must also police a *live* track's input selection, not just the
    /// birth-time window. A candidate born two bins away from the cooling
    /// channel passes `spawn_window_cooling_down` legitimately (its
    /// prospective window does not intersect the cooldown), but a stronger
    /// signal on the intervening bin drags `center` across the rounding
    /// boundary via `update_centroid` until `Track::owned` expands to
    /// include the cooling birdie channel -- and once the intervening
    /// signal ends, `select_channel`'s max-power rule feeds the decoder
    /// from the birdie for a whole extra illegitimate GC cycle inside the
    /// cooldown window, exactly the absorption the spawn gate closed.
    #[test]
    fn drifting_track_cannot_take_input_from_a_cooling_down_channel() {
        let n = 64;
        let mut tm = TrackManager::new(
            n,
            96_000.0,
            14_000_000.0,
            default_filter_off(),
            DecodeConfig::default(),
        );
        feed_warmup(&mut tm, n);
        let mut power = quiet_power(n);
        power[10] = 1e-9 * 10f32.powf(20.0 / 10.0); // birdie: +20 dB, never decodes

        // Phase A: one promote -> gc_hops Silent-close cycle on the birdie,
        // arming the {9,10,11} cooldown (same harness as the sibling test).
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
        // One more hop: 9/11 cooldowns clear (own rise false), 10 stays armed.
        tm.step_hop(&hop(m, power.clone()), m);
        m += 1;
        assert!(
            tm.channel_cooling_down(10),
            "channel 10 must still be cooling down"
        );
        let until = tm.channel_cooldown_until[10];

        // Phase B: a new signal on channel 8 -- two bins from the birdie, so
        // its prospective window {7,8,9} is legitimately clean and it must
        // spawn and promote (the accepted neighbour trade-off, not a bypass).
        let mut neighbour = power.clone();
        neighbour[8] = 1e-9 * 10f32.powf(30.0 / 10.0);
        let mut neighbour_promoted = false;
        for _ in 0..600 {
            let (_, promoted, _) = tm.step_hop(&hop(m, neighbour.clone()), m);
            m += 1;
            if promoted
                .iter()
                .any(|e| matches!(e, DecoderEvent::TrackPromoted { .. }))
            {
                neighbour_promoted = true;
                break;
            }
        }
        assert!(
            neighbour_promoted,
            "channel 8 never promoted (clean prospective window; it must spawn)"
        );

        // Phase C: a stronger signal on the intervening bin 9. Channel 9 is
        // owned by the channel-8 track, so it is absorbed rather than
        // spawned; `update_centroid` drags center from 8.0 toward ~8.8 until
        // `center.round() == 9` and the owned window becomes {8,9,10}.
        let mut dragging = neighbour.clone();
        dragging[9] = 1e-9 * 10f32.powf(40.0 / 10.0);
        for _ in 0..300 {
            tm.step_hop(&hop(m, dragging.clone()), m);
            m += 1;
        }
        assert_eq!(
            tm.tracks.len(),
            1,
            "the channel-8 track must still be the only live track"
        );
        let track = tm.tracks.values().next().unwrap();
        assert!(
            track.owned(n).contains(&10),
            "expected the dragged track's window to include cooling channel 10 (center {})",
            track.center
        );

        // Phase D: the real signals end; only the still-cooling birdie on
        // channel 10 remains. The track may NOT take its input from the
        // cooling channel: without enforcement it stayed ACTIVE on the
        // birdie for a whole extra gc_hops cycle; with it, the de-selected
        // track must close (HangExpired) within hang_hops.
        for _ in 0..2500 {
            tm.step_hop(&hop(m, power.clone()), m);
            m += 1;
        }
        assert!(
            tm.tracks.is_empty(),
            "MAN-197: a live track was still being driven by cooling channel 10 \
             (expected a hang close within hang_hops = 1875 hops)"
        );
        assert_eq!(
            tm.channel_cooldown_until[10], until,
            "no Silent re-arm may extend the cooldown out of an illegitimate cycle"
        );
        assert!(
            m < until,
            "test must run inside the cooldown window, not past its expiry"
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
            default_filter_off(),
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
        let (_, _, spectral_ref_power, _, _, _) = *track
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
            default_filter_off(),
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
        let (_, _, spectral_ref_power, _, _, _) = *track
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
            default_filter_off(),
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
        let (_, _, spectral_ref_power, _, _, _) = *track
            .pending
            .last()
            .expect("the promotion hop itself must have queued a pending entry");
        assert!(
            spectral_ref_power.is_none(),
            "the Legacy engine must never pay for computing spectral_ref_power"
        );
    }

    #[test]
    fn leakage_skirt_beside_an_owned_channel_does_not_spawn() {
        let mut tm = TrackManager::new(
            64,
            96_000.0,
            14_000_000.0,
            default_filter_off(),
            DecodeConfig::default(),
        );
        feed_warmup(&mut tm, 64);
        let mut power = quiet_power(64);
        for (k, db) in [(19, 30.0), (20, 40.0), (21, 30.0), (22, 15.0)] {
            power[k] = 1e-9 * 10f32.powf(db / 10.0);
        }
        let mut promotions = 0;
        for m in (250 * 15)..(250 * 15 + 400) {
            let (_, promoted, _) = tm.step_hop(&hop(m, power.clone()), m);
            promotions += promoted.len();
        }
        assert_eq!(
            promotions, 1,
            "a falling leakage skirt must not create another track"
        );
        assert_eq!(
            tm.close_counts().merged,
            0,
            "one signal must not churn through merges"
        );
    }

    #[test]
    fn a_real_second_peak_three_channels_away_still_spawns() {
        let mut tm = TrackManager::new(
            64,
            96_000.0,
            14_000_000.0,
            default_filter_off(),
            DecodeConfig::default(),
        );
        feed_warmup(&mut tm, 64);
        let mut power = quiet_power(64);
        for (k, db) in [(19, 30.0), (20, 40.0), (21, 30.0), (22, 5.0), (23, 20.0)] {
            power[k] = 1e-9 * 10f32.powf(db / 10.0);
        }
        let mut promotions = 0;
        for m in (250 * 15)..(250 * 15 + 400) {
            let (_, promoted, _) = tm.step_hop(&hop(m, power.clone()), m);
            promotions += promoted.len();
        }
        assert_eq!(
            promotions, 2,
            "an independent local maximum must still promote"
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

    /// MAN-194: `evict_over_cap` must drain an evicted track's REFINER
    /// BACKLOG into `pending` before calling `finish_decoder_speed_only` --
    /// same reasoning and same discriminating technique as
    /// `merge_converged_drains_refiner_backlog_before_finishing` above: a
    /// completely fresh decoder that never saw a single hop cannot produce
    /// a `SpeedUpdate`/held-run event on its own, so non-empty
    /// `flush_events` can only mean the backlog's samples genuinely
    /// reached the decoder through `push_hop` before the flush. Capable of
    /// failing: moving the drain to after `finish_decoder_speed_only()` in
    /// `evict_over_cap` leaves `pending` empty at flush time, and the
    /// untouched fresh decoder then reports nothing.
    #[test]
    fn evict_over_cap_drains_refiner_backlog_before_finishing() {
        let det_cfg = DetectorConfig {
            track_cap: 1,
            ..DetectorConfig::default()
        };
        let mut tm =
            TrackManager::new(64, 96_000.0, 14_000_000.0, det_cfg, DecodeConfig::default());
        tm.spawn(10);
        tm.spawn(40);
        let mut ids: Vec<u32> = tm.tracks.keys().copied().collect();
        ids.sort();
        let (weak_id, strong_id) = (ids[0], ids[1]);

        {
            let weak = tm.tracks.get_mut(&weak_id).unwrap();
            weak.current_snr_db = 8.0; // lower SNR -> evicted first
            weak.decoder = Some(TrackDecoder::new(weak_id, DecodeConfig::default()));
            // Seeded directly into refiner_backlog, NOT `pending` -- see
            // the matching merge_converged test's doc comment for why this
            // is the discriminating part.
            weak.refiner_backlog = rect_envelope_hops("PARIS", 18)
                .into_iter()
                .enumerate()
                .map(|(i, a)| (a, a * a, None, i as u64, 0.0, 0.0, false))
                .collect();
            weak.has_emitted = true;
        }
        {
            let strong = tm.tracks.get_mut(&strong_id).unwrap();
            strong.current_snr_db = 18.0;
        }

        let (evicted, flush_events) = tm.evict_over_cap();
        assert_eq!(
            evicted,
            vec![(
                weak_id,
                ClosureKind::Bookkeeping {
                    survivor_track_id: None
                }
            )],
            "the lower-SNR track must be the one evicted, with no survivor to migrate to"
        );
        assert!(
            !flush_events.is_empty(),
            "evict_over_cap must drain the refiner backlog into pending through the decoder \
             before finishing -- a fresh decoder that never saw a hop cannot produce this output \
             on its own, got nothing"
        );
    }

    #[test]
    fn merge_on_an_exact_snr_tie_keeps_the_older_track() {
        let mut tm = TrackManager::new(
            64,
            96_000.0,
            14_000_000.0,
            DetectorConfig::default(),
            DecodeConfig::default(),
        );
        tm.spawn(10);
        tm.spawn(40);
        for (id, center) in [(1, 20.4), (2, 21.1)] {
            let track = tm.tracks.get_mut(&id).unwrap();
            track.center = center;
            track.current_snr_db = 18.0;
            track.has_emitted = true;
        }
        let (closed, _) = tm.merge_converged();
        assert!(
            tm.tracks.contains_key(&1),
            "the incumbent decoder must survive an exact SNR tie"
        );
        assert!(!tm.tracks.contains_key(&2));
        assert_eq!(tm.close_counts().merged, 1);
        assert_eq!(
            closed,
            vec![(
                2,
                ClosureKind::Bookkeeping {
                    survivor_track_id: Some(1)
                }
            )]
        );
    }

    /// MAN-174 fixture: a `TrackManager` holding tracks 1..=n, each with
    /// a hand-set `(center, current_snr_db, has_emitted)`.
    fn man174_tracks(track_cap: usize, setup: &[(f64, f32, bool)]) -> TrackManager {
        let det_cfg = DetectorConfig {
            track_cap,
            ..DetectorConfig::default()
        };
        let mut tm =
            TrackManager::new(64, 96_000.0, 14_000_000.0, det_cfg, DecodeConfig::default());
        for (i, &(center, snr, emitted)) in setup.iter().enumerate() {
            tm.spawn(10 + 20 * i);
            let track = tm.tracks.get_mut(&(i as u32 + 1)).unwrap();
            track.center = center;
            track.current_snr_db = snr;
            track.has_emitted = emitted;
        }
        tm
    }

    fn closed_order(events: &[DecoderEvent]) -> Vec<u32> {
        events
            .iter()
            .filter_map(|e| match e {
                DecoderEvent::TrackClosed { track_id, .. } => Some(*track_id),
                _ => None,
            })
            .collect()
    }

    fn merged_into(survivor: u32) -> ClosureKind {
        ClosureKind::Bookkeeping {
            survivor_track_id: Some(survivor),
        }
    }

    /// MAN-174 finding 1: three tracks converge in one hop. Track 2 merges
    /// into track 1, then track 1 into track 3. Track 2's closure must
    /// reach consumers first, or its migrated evidence lands on track 1
    /// after track 1 has already closed.
    #[test]
    fn chained_merge_closures_are_sequenced_loser_before_survivor() {
        let mut tm = man174_tracks(
            8,
            &[(20.4, 12.0, true), (20.9, 8.0, true), (21.3, 18.0, true)],
        );
        let (closed, _) = tm.merge_converged();
        assert_eq!(closed, vec![(2, merged_into(1)), (1, merged_into(3))]);
        let events = sequence_batch(Vec::new(), closed, &std::collections::HashMap::new());
        assert_eq!(closed_order(&events), vec![2, 1]);
    }

    /// MAN-174: a survivor that also closes this batch (here
    /// `SignalEnded`) sorts after its loser; independent closures keep
    /// `track_id` order.
    #[test]
    fn a_merge_losers_closure_precedes_its_survivors_same_batch_close() {
        let closed = vec![
            (5, ClosureKind::SignalEnded),
            (2, merged_into(1)),
            (
                4,
                ClosureKind::Bookkeeping {
                    survivor_track_id: None,
                },
            ),
            (1, ClosureKind::SignalEnded),
        ];
        let events = vec![
            DecoderEvent::word_boundary(5, 100),
            DecoderEvent::word_boundary(1, 50),
        ];
        let events = sequence_batch(events, closed, &std::collections::HashMap::new());
        assert_eq!(closed_order(&events), vec![2, 1, 4, 5]);
        assert!(
            matches!(events[0], DecoderEvent::WordBoundary { track_id: 1, .. })
                && matches!(events[1], DecoderEvent::WordBoundary { track_id: 5, .. }),
            "non-closure events keep (sample_ts, track_id) order ahead of every closure: {events:?}"
        );
    }

    /// MAN-174 finding 1, end to end: real `merge_converged` output,
    /// sequenced, into a real `Validator`. Track 2's deferred Beacon must
    /// resolve at the final survivor's (track 3's) close.
    #[test]
    fn chained_merge_resolves_the_deferred_beacon_at_the_final_survivor() {
        let mut tm = man174_tracks(
            8,
            &[(20.4, 12.0, true), (20.9, 8.0, true), (21.3, 18.0, true)],
        );
        let (closed, _) = tm.merge_converged();
        let closures = sequence_batch(Vec::new(), closed, &std::collections::HashMap::new());

        let mut v = manta_spot::Validator::bundled(96_000.0);
        let meta = |track_id| DecoderEvent::TrackMeta {
            track_id,
            sample_ts: 0,
            snr_2500_db: 20.0,
            freq_hz: 14_000_000.0,
        };
        let mut events = vec![
            meta(2),
            DecoderEvent::SpeedUpdate {
                track_id: 2,
                wpm: 22.0,
            },
        ];
        let mut ts = 1_000;
        for word in ["K5ARH", "T"] {
            for c in word.chars() {
                events.push(DecoderEvent::char_decoded(
                    2,
                    ts,
                    manta_decode::tree::Glyph::Char(c),
                    0.95,
                ));
                ts += 100;
            }
            events.push(DecoderEvent::word_boundary(2, ts));
            ts += 100;
        }
        events.extend(closures);
        events.extend([
            meta(3),
            DecoderEvent::SpeedUpdate {
                track_id: 3,
                wpm: 25.0,
            },
            DecoderEvent::TrackClosed {
                track_id: 3,
                closure: ClosureKind::SignalEnded,
            },
        ]);
        let spots: Vec<_> = events.iter().flat_map(|e| v.ingest(e)).collect();
        assert_eq!(spots.len(), 1, "got {spots:?}");
        assert_eq!(spots[0].callsign, "K5ARH");
        assert_eq!(spots[0].spot_type, manta_spot::SpotType::Beacon);
    }

    /// MAN-174 finding 1b: track 1 merges into track 2, which has emitted
    /// nothing. Track 2 is now named to consumers, so when it later merges
    /// into track 3 it must get its own `TrackClosed`.
    #[test]
    fn a_never_emitted_merge_survivor_gets_its_own_closure_when_it_merges() {
        let mut tm = man174_tracks(
            8,
            &[(20.4, 8.0, true), (20.9, 12.0, false), (40.0, 18.0, false)],
        );
        let (closed, _) = tm.merge_converged();
        assert_eq!(closed, vec![(1, merged_into(2))]);
        tm.tracks.get_mut(&3).unwrap().center = 21.3;
        let (closed, _) = tm.merge_converged();
        assert_eq!(closed, vec![(2, merged_into(3))]);
    }

    /// MAN-174 finding 1b, eviction path: the never-emitted survivor of a
    /// merge is evicted, and still owes its `TrackClosed`.
    #[test]
    fn a_never_emitted_merge_survivor_gets_its_own_closure_when_evicted() {
        let mut tm = man174_tracks(
            1,
            &[(20.4, 8.0, true), (20.9, 12.0, false), (40.0, 18.0, false)],
        );
        let (closed, _) = tm.merge_converged();
        assert_eq!(closed, vec![(1, merged_into(2))]);
        let (evicted, _) = tm.evict_over_cap();
        assert_eq!(
            evicted,
            vec![(
                2,
                ClosureKind::Bookkeeping {
                    survivor_track_id: None
                }
            )]
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
        // the id the old tie-break closed (fixed by MAN-172). Confirmed across
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

    /// MAN-115: hops below `skip_preroll_hops`' bound are the zero-padding
    /// pre-roll. They must not count toward `warmup_hops` nor reach the
    /// floor: a -200 dB all-zero window would otherwise become the young
    /// ring's lower quartile.
    #[test]
    fn preroll_hops_do_not_advance_hop_counter_or_floor() {
        let n = 64;
        let mut tm = TrackManager::new(
            n,
            96_000.0,
            14_000_000.0,
            DetectorConfig::default(),
            DecodeConfig::default(),
        );
        tm.skip_preroll_hops(32);
        let zeros: Vec<HopOutput> = (0..32).map(|m| hop(m, vec![0.0; n])).collect();
        tm.process_hops(&zeros, |m| m * 256);
        assert_eq!(
            tm.hop_counter, 0,
            "pre-roll hops must not advance hop_counter"
        );
        assert_eq!(
            tm.floor.channel_floor_db(0),
            -140.0,
            "pre-roll hops must not reach the floor"
        );
        tm.process_hops(&[hop(32, quiet_power(n))], |m| m * 256);
        assert_eq!(
            tm.hop_counter, 1,
            "the first real hop is hop 0 of the segment"
        );
        assert!(
            (tm.floor.channel_floor_db(0) - -90.0).abs() < 0.5,
            "the first real hop seeds the ring at the real -90 dB noise, got {}",
            tm.floor.channel_floor_db(0)
        );
    }

    /// MAN-73: `listen()`'s segment restart resumes id assignment from the
    /// prior segment's `next_track_id()` so ids are never reused within a
    /// session.
    #[test]
    fn resume_track_ids_from_continues_numbering() {
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
        tm.resume_track_ids_from(42);
        let mut all_events = Vec::new();
        for chunk in rendered.samples.chunks(4096) {
            let hops = ch.process(chunk);
            all_events.extend(tm.process_hops(&hops, |m| m * hop_samples));
        }
        all_events.extend(tm.finish());
        let first_track_meta_id = all_events
            .iter()
            .find_map(|e| match e {
                DecoderEvent::TrackMeta { track_id, .. } => Some(*track_id),
                _ => None,
            })
            .expect("V1 should produce at least one TrackMeta event");
        assert_eq!(first_track_meta_id, 42);
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
            // spectral_ref_power, sample_ts, current_snr_db, freq_hz)
            // tuples in `pending` -- NOT fed through push_hop yet.
            let snr = weak.current_snr_db;
            weak.pending = rect_envelope_hops("PARIS", 18)
                .into_iter()
                .enumerate()
                .map(|(i, a)| (a, a * a, None, i as u64, snr, 0.0))
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

    /// MAN-194: `merge_converged` must drain a merge-loser's REFINER
    /// BACKLOG into `pending` before calling `finish_decoder_speed_only` --
    /// the sibling test above already proves finishing drains queued
    /// `pending`; this proves the NEW backlog -> pending wiring
    /// specifically. Uses a completely fresh decoder (`TrackDecoder::new`,
    /// no live `push_envelope` calls at all) with all of "PARIS" seeded
    /// directly into `refiner_backlog` -- a decoder that has never seen a
    /// single hop cannot produce a `SpeedUpdate`/held-run event on its own
    /// (a fresh `Demod` has no held run, and `SpeedTracker::wpm()` starts
    /// `None`), so `flush_events` non-empty can ONLY mean the backlog's
    /// samples genuinely reached the decoder through `push_hop` before
    /// `finish_decoder_speed_only` ran. This is capable of failing: moving
    /// `drain_refiner_into_pending()` to after `finish_decoder_speed_only()`
    /// in `merge_converged` leaves `pending` empty at flush time, and the
    /// untouched fresh decoder then reports nothing.
    #[test]
    fn merge_converged_drains_refiner_backlog_before_finishing() {
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
            // Seeded directly into refiner_backlog (exactly the shape
            // `Track::decoder_input` leaves behind), NOT `pending` -- this
            // is what distinguishes this test from the sibling above,
            // which already covers finish_decoder_speed_only's own
            // pending-draining behavior.
            weak.refiner_backlog = rect_envelope_hops("PARIS", 18)
                .into_iter()
                .enumerate()
                .map(|(i, a)| (a, a * a, None, i as u64, 0.0, 0.0, false))
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
        assert!(
            !flush_events.is_empty(),
            "merge_converged must drain the refiner backlog into pending through the decoder \
             before finishing -- a fresh decoder that never saw a hop cannot produce this output \
             on its own, got nothing"
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
    /// CER is asserted `< 0.02`, not `== 0.0`. Under the old 2 s floor warmup
    /// (`warmup_hops = 750`) the leading ~2.66 s of the 120 s stream (warmup
    /// plus confirm) was structurally unrecoverable -- a deterministic ~0.0155
    /// CER floor (measured identical across five noise seeds). MAN-115's
    /// 9-hop warmup promotes near the start of the stream, so only the
    /// first, mid-element character is still at risk; the bound is kept.
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
            "expected CER < 0.02 (pre-MAN-115 2 s warmup floor was ~0.0155), got {cer:.4}\nexpected {:?}\ngot      {:?}",
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
            dit_units: 1.0,
            element_gap_units: 1.0,
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

    /// MAN-211 (regression): `TrackMeta.freq_hz` must not depend on the
    /// caller's `process_hops` chunk size either. Before the fix `step_hop`
    /// wrote each hop's centroid to the decoder live, so a report drained
    /// from an older queued entry carried the batch's last centroid while
    /// its `sample_ts` named its own hop: 60 of V1's and V4's 119
    /// `TrackMeta`s differed between 4096 and 2048 (up to 8.1 Hz), and 59
    /// of V9's with the refiner on (up to 5.0 Hz). V9 with the refiner also
    /// covers the MAN-194 `refiner_backlog` delay and a drifting centroid.
    #[test]
    fn track_meta_freq_hz_is_invariant_to_process_hops_chunk_size() {
        use manta_dsp::channelizer::Channelizer;
        use manta_testkit::vectors::{self, RenderedVector, VectorSpec};

        fn metas_at_chunk_size(
            spec: &VectorSpec,
            rendered: &RenderedVector,
            decode_cfg: &DecodeConfig,
            chunk_samples: usize,
        ) -> Vec<(u32, u64, f64)> {
            let mut ch = Channelizer::new(spec.fs, spec.center_freq_hz).unwrap();
            let hop_samples = ch.hop() as u64;
            let mut tm = TrackManager::new(
                ch.n_channels(),
                spec.fs,
                spec.center_freq_hz,
                DetectorConfig::default(),
                decode_cfg.clone(),
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
                    DecoderEvent::TrackMeta {
                        track_id,
                        sample_ts,
                        freq_hz,
                        ..
                    } => Some((track_id, sample_ts, freq_hz)),
                    _ => None,
                })
                .collect()
        }

        let refined = DecodeConfig {
            refine_bw_hz: 30.0,
            ..DecodeConfig::default()
        };
        let v1 = vectors::v1();
        let v4 = vectors::v4();
        let v9 = vectors::v9();
        let cases = [
            (
                "V1",
                vectors::render(&v1).unwrap(),
                &v1,
                DecodeConfig::default(),
            ),
            (
                "V4",
                vectors::render(&v4).unwrap(),
                &v4,
                DecodeConfig::default(),
            ),
            (
                "V9 refined",
                vectors::render_v9_drift(&v9).unwrap(),
                &v9,
                refined,
            ),
        ];
        for (name, rendered, spec, decode_cfg) in &cases {
            // 4096 mirrors decode_samples's CHUNK_SAMPLES; 2048 mirrors
            // listen/soak_metrics's own chunk size.
            let at_4096 = metas_at_chunk_size(spec, rendered, decode_cfg, 4096);
            let at_2048 = metas_at_chunk_size(spec, rendered, decode_cfg, 2048);
            assert!(
                !at_4096.is_empty(),
                "{name} should produce at least one TrackMeta"
            );
            assert_eq!(
                at_4096, at_2048,
                "{name}: TrackMeta.freq_hz must not depend on the caller's process_hops chunk size"
            );
        }
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

    // ---- MAN-313 (SPEC §2.8): non-CW filter ----

    const NC_N: usize = 64;
    /// Channel 10's absolute Hz at 96 kHz / 64 channels (1500 Hz spacing).
    const NC_CH10_HZ: f64 = 14_015_000.0;

    fn nc_db(db: f32) -> f32 {
        1e-9 * 10f32.powf(db / 10.0)
    }

    fn nc_mgr(cfg: DetectorConfig) -> TrackManager {
        let mut tm = TrackManager::new(NC_N, 96_000.0, 14_000_000.0, cfg, DecodeConfig::default());
        feed_warmup(&mut tm, NC_N);
        tm
    }

    fn nc_cfg(unkeyed_close_hops: u64) -> DetectorConfig {
        DetectorConfig {
            unkeyed_close_hops,
            ..DetectorConfig::default()
        }
    }

    /// 40 hops key-down, 40 key-up.
    fn nc_keyed(i: u64) -> bool {
        (i / 40).is_multiple_of(2)
    }

    /// Steps `hops` hops after warm-up (`m` continues from it); `power(i)`
    /// is hop `i`'s power, `each(tm, i)` runs after each step. Returns
    /// every `TrackPromoted`.
    fn nc_drive(
        tm: &mut TrackManager,
        start: u64,
        hops: u64,
        mut power: impl FnMut(u64) -> Vec<f32>,
        mut each: impl FnMut(&mut TrackManager, u64),
    ) -> Vec<DecoderEvent> {
        let mut promoted = Vec::new();
        for i in 0..hops {
            let m = 250 * 15 + start + i;
            let (_, p, _) = tm.step_hop(&hop(m, power(i)), m);
            promoted.extend(p);
            each(tm, i);
        }
        promoted
    }

    fn nc_tone(ch: usize, on: bool, db: f32) -> Vec<f32> {
        let mut p = quiet_power(NC_N);
        if on {
            p[ch] = nc_db(db);
        }
        p
    }

    /// Keyed on channel 10 until it qualifies (at most 1500 hops); returns
    /// the hops used.
    fn nc_qualify(tm: &mut TrackManager) -> u64 {
        let mut used = 0;
        let mut done = false;
        for i in 0..1500 {
            if done {
                break;
            }
            let p = nc_drive(tm, i, 1, |_| nc_tone(10, nc_keyed(i), 30.0), |_, _| {});
            done = !p.is_empty();
            used = i + 1;
        }
        assert!(done, "the keyed tone never qualified");
        // Finish on a key-down hop so the steady carrier continues it.
        while !nc_keyed(used) {
            nc_drive(tm, used, 1, |_| nc_tone(10, false, 30.0), |_, _| {});
            used += 1;
        }
        used
    }

    #[test]
    fn keyed_tone_qualifies_and_emits_track_promoted_with_its_promotion_ts() {
        let mut tm = nc_mgr(DetectorConfig::default());
        let mut promo: Option<(u64, f64)> = None;
        let mut qualified_at = None;
        let promoted = nc_drive(
            &mut tm,
            0,
            1500,
            |i| nc_tone(10, nc_keyed(i), 30.0),
            |tm, i| {
                for t in tm.tracks.values() {
                    if let Some(ev) = t.keying.as_ref() {
                        promo.get_or_insert((ev.promo_ts, ev.promo_freq_hz));
                        if t.qualified && qualified_at.is_none() {
                            qualified_at = Some(i);
                        }
                    }
                }
            },
        );
        let (promo_ts, promo_freq) = promo.expect("the keyed tone was promoted");
        assert_eq!(promoted.len(), 1, "{promoted:?}");
        match &promoted[0] {
            DecoderEvent::TrackPromoted {
                sample_ts, freq_hz, ..
            } => {
                assert_eq!(*sample_ts, promo_ts);
                assert_eq!(*freq_hz, promo_freq);
                assert!((freq_hz - NC_CH10_HZ).abs() < 1500.0, "{freq_hz}");
            }
            e => panic!("{e:?}"),
        }
        let q = qualified_at.unwrap();
        assert!(250 * 15 + q > promo_ts, "qualified after promotion");
        assert_eq!(tm.decoding_track_count(), 1);
    }

    #[test]
    fn short_gc_does_not_close_a_track_awaiting_qualification() {
        // gc_hops (60) far below the time a keyed tone takes to qualify:
        // the silent GC must not close it before it can.
        let mut tm = nc_mgr(DetectorConfig {
            gc_hops: 60,
            ..DetectorConfig::default()
        });
        let promoted = nc_drive(
            &mut tm,
            0,
            1500,
            |i| nc_tone(10, nc_keyed(i), 30.0),
            |_, _| {},
        );
        // The first track qualifies. (Once qualified it has a decoder, so
        // the 60-hop GC applies to it as before: this step-only harness
        // decodes nothing, so it then closes `Silent` and respawns.)
        assert!(
            matches!(
                promoted.first(),
                Some(DecoderEvent::TrackPromoted { track_id: 1, .. })
            ),
            "{promoted:?}"
        );
    }

    #[test]
    fn steady_tone_never_emits_track_promoted_and_closes_not_cw_at_timeout() {
        let mut tm = nc_mgr(nc_cfg(1000));
        let mut promo_hop = None;
        let mut closed_at = None;
        let promoted = nc_drive(
            &mut tm,
            0,
            1300,
            |_| nc_tone(10, true, 30.0),
            |tm, _| {
                if let Some(ev) = tm.tracks.values().find_map(|t| t.keying.as_ref()) {
                    promo_hop.get_or_insert(ev.promo_hop);
                }
                if tm.close_counts().not_cw == 1 && closed_at.is_none() {
                    closed_at = Some(tm.hop_counter);
                }
            },
        );
        assert!(promoted.is_empty(), "{promoted:?}");
        assert_eq!(tm.close_counts().not_cw, 1);
        assert_eq!(closed_at.unwrap() - promo_hop.unwrap(), 1000);
        assert!(tm.confirmations() >= 1);
    }

    #[test]
    fn unqualified_track_is_not_a_decoding_track() {
        let mut tm = nc_mgr(DetectorConfig::default());
        nc_drive(&mut tm, 0, 300, |_| nc_tone(10, true, 30.0), |_, _| {});
        assert!(tm
            .tracks
            .values()
            .any(|t| t.state() == LifecycleState::Active && t.keying.is_some()));
        assert_eq!(tm.decoding_track_count(), 0);
        assert!(tm.tracks.values().all(|t| t.decoder.is_none()));
    }

    #[test]
    fn moving_peak_does_not_qualify() {
        // A keyed carrier stepping one channel every 40 hops.
        let mut tm = nc_mgr(DetectorConfig::default());
        let promoted = nc_drive(
            &mut tm,
            0,
            1600,
            |i| nc_tone(5 + (i / 40) as usize % 50, nc_keyed(i / 2), 30.0),
            |_, _| {},
        );
        assert!(promoted.is_empty(), "{promoted:?}");
        // The slope itself: 0.025 channel/hop at 1500 Hz spacing is far
        // past MAX_SWEEP_HZ_PER_S.
        let mut ev = KeyingEvidence::new(0, 0.0, 0);
        for t in 0..200u64 {
            ev.n += 1;
            let (tt, f) = (t as f64, t as f64 * 0.025);
            ev.sum_t += tt;
            ev.sum_f += f;
            ev.sum_tt += tt * tt;
            ev.sum_tf += tt * f;
        }
        assert!((ev.slope_hz_s(93.75) - 0.025 * 93.75 * 375.0).abs() < 1e-6);
        assert!(ev.slope_hz_s(93.75).abs() > MAX_SWEEP_HZ_PER_S);
    }

    #[test]
    fn flat_wideband_block_does_not_qualify() {
        let mut tm = nc_mgr(DetectorConfig::default());
        let promoted = nc_drive(
            &mut tm,
            0,
            1500,
            |i| {
                let mut p = quiet_power(NC_N);
                if nc_keyed(i) {
                    p[10..=30].iter_mut().for_each(|x| *x = nc_db(30.0));
                }
                p
            },
            |_, _| {},
        );
        assert!(promoted.is_empty(), "{promoted:?}");
    }

    #[test]
    fn louder_neighbour_three_channels_away_does_not_block_qualification() {
        let mut tm = nc_mgr(DetectorConfig::default());
        let promoted = nc_drive(
            &mut tm,
            0,
            1500,
            |i| {
                let mut p = nc_tone(10, nc_keyed(i), 20.0);
                p[13] = nc_db(40.0);
                p
            },
            |_, _| {},
        );
        assert!(
            promoted.iter().any(|e| matches!(e,
                DecoderEvent::TrackPromoted { freq_hz, .. } if (freq_hz - NC_CH10_HZ).abs() < 1500.0)),
            "{promoted:?}"
        );
    }

    #[test]
    fn spawn_is_vetoed_inside_a_comod_span() {
        let mut tm = nc_mgr(DetectorConfig::default());
        nc_drive(
            &mut tm,
            0,
            600,
            |i| {
                let mut p = quiet_power(NC_N);
                if nc_keyed(i) {
                    p[10..=30].iter_mut().for_each(|x| *x = nc_db(30.0));
                }
                p
            },
            |_, _| {},
        );
        assert!(tm.comod.as_ref().unwrap().in_span(20));
        assert!(tm.comod_vetoes_spawn(20));
        assert!(!tm.comod_vetoes_spawn(50));
        let off = nc_mgr(default_filter_off());
        assert!(!off.comod_vetoes_spawn(20));
    }

    #[test]
    fn merge_keeps_the_qualified_track() {
        for (filter, expect_survivor) in [(true, 1u32), (false, 2u32)] {
            let mut tm = nc_mgr(DetectorConfig {
                non_cw_filter: filter,
                ..DetectorConfig::default()
            });
            let cfg = tm.cfg;
            let mut a = Track::new(1, 10, &cfg);
            a.current_snr_db = 10.0;
            a.qualified = true;
            let mut b = Track::new(2, 10, &cfg);
            b.center = 10.4;
            b.current_snr_db = 20.0;
            b.keying = Some(KeyingEvidence::new(0, 0.0, 0));
            tm.tracks.insert(1, a);
            tm.tracks.insert(2, b);
            tm.merge_converged();
            let left: Vec<u32> = tm.tracks.keys().copied().collect();
            assert_eq!(left, vec![expect_survivor], "filter {filter}");
        }
    }

    #[test]
    fn drain_pool_skips_queued_tracks_without_a_decoder() {
        let mut tm = nc_mgr(DetectorConfig::default());
        let cfg = tm.cfg;
        let mut t = Track::new(1, 10, &cfg);
        t.keying = Some(KeyingEvidence::new(0, 0.0, 0));
        t.pending.push((1.0, 1.0, None, 0, 10.0, 0.0));
        tm.tracks.insert(1, t);
        assert!(tm.drain_pool().is_empty());
        assert_eq!(tm.tracks[&1].pending.len(), 1);
    }

    #[test]
    fn unqualified_backlog_is_bounded() {
        let mut tm = nc_mgr(nc_cfg(20_000));
        let mut most = 0;
        nc_drive(
            &mut tm,
            0,
            12_000,
            |_| nc_tone(10, true, 30.0),
            |tm, _| {
                for t in tm.tracks.values() {
                    most = most.max(t.pending.len());
                }
            },
        );
        assert_eq!(tm.close_counts().not_cw, 0);
        assert!(most > MAX_UNQUALIFIED_BACKLOG, "{most}");
        assert!(
            most <= MAX_UNQUALIFIED_BACKLOG + MAX_UNQUALIFIED_BACKLOG / 4,
            "{most}"
        );
    }

    #[test]
    fn unqualified_track_at_finish_emits_nothing() {
        let mut tm = nc_mgr(DetectorConfig::default());
        let mut events = Vec::new();
        for i in 0..400u64 {
            let m = 250 * 15 + i;
            events.extend(tm.process_hops(&[hop(m, nc_tone(10, true, 30.0))], |m| m));
        }
        assert!(tm
            .tracks
            .values()
            .any(|t| t.keying.is_some() && !t.pending.is_empty()));
        events.extend(tm.finish());
        assert!(events.is_empty(), "{events:?}");
    }

    #[test]
    fn confirmations_count_every_promotion_qualified_or_not() {
        for filter in [true, false] {
            let mut tm = nc_mgr(DetectorConfig {
                non_cw_filter: filter,
                ..DetectorConfig::default()
            });
            let promoted = nc_drive(
                &mut tm,
                0,
                1500,
                |i| {
                    let mut p = nc_tone(10, nc_keyed(i), 30.0);
                    p[40] = nc_db(30.0);
                    p
                },
                |_, _| {},
            );
            assert_eq!(tm.confirmations(), 2, "filter {filter}");
            assert_eq!(
                promoted.len(),
                if filter { 1 } else { 2 },
                "filter {filter}"
            );
        }
    }

    #[test]
    fn filter_on_is_deterministic() {
        let run = || {
            let mut tm = nc_mgr(nc_cfg(1000));
            let promoted = nc_drive(
                &mut tm,
                0,
                2000,
                |i| {
                    let mut p = nc_tone(10, nc_keyed(i), 30.0);
                    p[40] = nc_db(30.0);
                    if nc_keyed(i / 3) {
                        p[20..=30].iter_mut().for_each(|x| *x = nc_db(25.0));
                    }
                    p
                },
                |_, _| {},
            );
            format!(
                "{promoted:?} {:?} {}",
                tm.close_counts(),
                tm.confirmations()
            )
        };
        assert_eq!(run(), run());
    }

    /// Qualify a keyed tone, then key down for good; returns the hop
    /// counter when `unkeyed` first counted and the last re-rise hop.
    fn nc_unkeyed_run(
        cfg: DetectorConfig,
        steady_hops: u64,
        mut at: impl FnMut(&mut TrackManager, u64),
    ) -> (TrackManager, Option<(u64, u64)>) {
        let mut tm = nc_mgr(cfg);
        let used = nc_qualify(&mut tm);
        let mut last_rise = 0;
        let mut closed = None;
        nc_drive(
            &mut tm,
            used,
            steady_hops,
            |_| nc_tone(10, true, 30.0),
            |tm, i| {
                at(tm, i);
                if let Some(ev) = tm.tracks.values().find_map(|t| t.keying.as_ref()) {
                    last_rise = ev.last_rise_hop.max(ev.last_char_hop);
                }
                if tm.close_counts().unkeyed == 1 && closed.is_none() {
                    closed = Some((tm.hop_counter, last_rise));
                    // MAN-171 cooldown on the closed track's channel.
                    assert!(tm.channel_cooldown_until[10] > tm.hop_counter);
                }
            },
        );
        (tm, closed)
    }

    #[test]
    fn unkeyed_active_track_closes_after_unkeyed_close_hops() {
        let (tm, closed) = nc_unkeyed_run(nc_cfg(500), 800, |_, _| {});
        let (at, last_rise) = closed.expect("closed Unkeyed");
        assert_eq!(at - last_rise, 500);
        assert_eq!(tm.close_counts().unkeyed, 1);
    }

    #[test]
    fn callsign_read_suppresses_the_unkeyed_close() {
        let (tm, closed) = nc_unkeyed_run(nc_cfg(500), 1200, |tm, i| {
            if i == 0 {
                let ev = tm
                    .tracks
                    .values_mut()
                    .find_map(|t| t.keying.as_mut())
                    .unwrap();
                for c in "K5ARH".chars() {
                    ev.note_char(0, Some(c));
                }
                ev.end_word();
                assert!(ev.callsign_read);
            }
        });
        assert!(closed.is_none());
        assert_eq!(tm.close_counts().unkeyed, 0);
    }

    #[test]
    fn decoded_characters_restart_the_unkeyed_timer() {
        let mut char_hop = 0;
        let (_, closed) = nc_unkeyed_run(nc_cfg(500), 1200, |tm, i| {
            if i == 300 {
                char_hop = tm.hop_counter;
                let ev = tm
                    .tracks
                    .values_mut()
                    .find_map(|t| t.keying.as_mut())
                    .unwrap();
                ev.note_char(char_hop, Some('E'));
            }
        });
        let (at, _) = closed.expect("closed Unkeyed");
        assert_eq!(at, char_hop + 500);
    }

    #[test]
    fn unkeyed_close_starts_the_respawn_cooldown() {
        // `nc_unkeyed_run` asserts the cooldown at the close hop.
        let (tm, closed) = nc_unkeyed_run(nc_cfg(500), 800, |_, _| {});
        assert!(closed.is_some());
        assert!(tm.channel_cooldown_until[10] > 0);
        assert!(tm.tracks.values().all(|t| t.birth_channel.abs_diff(10) > 1));
    }

    #[test]
    fn filter_off_never_closes_unkeyed() {
        let mut tm = nc_mgr(DetectorConfig {
            non_cw_filter: false,
            unkeyed_close_hops: 500,
            ..DetectorConfig::default()
        });
        nc_drive(
            &mut tm,
            0,
            600,
            |i| nc_tone(10, nc_keyed(i), 30.0),
            |_, _| {},
        );
        nc_drive(&mut tm, 600, 1500, |_| nc_tone(10, true, 30.0), |_, _| {});
        let c = tm.close_counts();
        assert_eq!((c.unkeyed, c.not_cw), (0, 0));
        assert!(tm.tracks.values().all(|t| t.keying.is_none()));
    }

    // ---- MAN-108 (SPEC §2.4): DORMANT re-acquisition, TrackManager ----

    /// Per-hop key state for `text`: `dit` hops per dit, 3-dit character
    /// and 7-dit word gaps, and a trailing 7-dit gap.
    fn rq_keying(text: &str, dit: usize) -> Vec<bool> {
        let mut out = Vec::new();
        let mut push = |on: bool, hops: usize| out.extend(std::iter::repeat_n(on, hops));
        for (wi, word) in text.split(' ').enumerate() {
            if wi > 0 {
                push(false, 7 * dit);
            }
            for (ci, c) in word.chars().enumerate() {
                if ci > 0 {
                    push(false, 3 * dit);
                }
                let pat = manta_decode::tree::pattern_for(c).unwrap();
                for (ei, e) in pat.chars().enumerate() {
                    if ei > 0 {
                        push(false, dit);
                    }
                    push(true, if e == '.' { dit } else { 3 * dit });
                }
            }
        }
        push(false, 7 * dit);
        out
    }

    /// `text` keyed at +30 dB on channel `ch`, one power vector per hop.
    fn rq_keyed(text: &str, dit: usize, ch: usize) -> Vec<Vec<f32>> {
        rq_keying(text, dit)
            .into_iter()
            .map(|on| nc_tone(ch, on, 30.0))
            .collect()
    }

    fn rq_quiet(hops: u64) -> Vec<Vec<f32>> {
        (0..hops).map(|_| quiet_power(NC_N)).collect()
    }

    /// 22-hop dits: ~17 WPM.
    const RQ_DIT: usize = 22;
    const RQ_TEXT: &str = "CQ DE W1AW W1AW K";

    /// A warmed-up 64-channel manager, MAN-313 filter off (the decoder is
    /// leased at promotion) unless `cfg` says otherwise.
    fn rq_mgr(reacquire_hops: u64) -> TrackManager {
        nc_mgr(DetectorConfig {
            non_cw_filter: false,
            reacquire_hops,
            ..DetectorConfig::default()
        })
    }

    /// Feeds `powers` through `process_hops` in `batch`-hop slices; `m`
    /// continues from `tm.hop_counter`. `each` runs after every slice.
    fn rq_feed(
        tm: &mut TrackManager,
        powers: &[Vec<f32>],
        batch: usize,
        mut each: impl FnMut(&TrackManager, &[DecoderEvent]),
    ) -> Vec<DecoderEvent> {
        let mut events = Vec::new();
        for slice in powers.chunks(batch) {
            let hops: Vec<HopOutput> = slice
                .iter()
                .enumerate()
                .map(|(i, p)| hop(tm.hop_counter + i as u64, p.clone()))
                .collect();
            let out = tm.process_hops(&hops, |m| m);
            each(tm, &out);
            events.extend(out);
        }
        events
    }

    fn rq_promoted_ids(events: &[DecoderEvent]) -> Vec<u32> {
        events
            .iter()
            .filter_map(|e| match e {
                DecoderEvent::TrackPromoted { track_id, .. } => Some(*track_id),
                _ => None,
            })
            .collect()
    }

    fn rq_chars(events: &[DecoderEvent], after_ts: u64) -> Vec<(u32, char)> {
        events
            .iter()
            .filter_map(|e| match e {
                DecoderEvent::CharDecoded {
                    track_id,
                    sample_ts,
                    glyph,
                    ..
                } if *sample_ts > after_ts => glyph.text_char().map(|c| (*track_id, c)),
                _ => None,
            })
            .collect()
    }

    /// Keyed, `gap` quiet hops, keyed again, in 16-hop batches (`decode`'s
    /// 4096-sample chunks). Returns the batch events, the `finish()`
    /// events and the hop the second transmission starts on.
    fn rq_fade_scene(
        tm: &mut TrackManager,
        gap: u64,
        second_ch: usize,
    ) -> (Vec<DecoderEvent>, Vec<DecoderEvent>, u64) {
        let mut events = rq_feed(tm, &rq_keyed(RQ_TEXT, RQ_DIT, 10), 16, |_, _| {});
        events.extend(rq_feed(tm, &rq_quiet(gap), 16, |_, _| {}));
        let back = tm.hop_counter;
        events.extend(rq_feed(
            tm,
            &rq_keyed(RQ_TEXT, RQ_DIT, second_ch),
            16,
            |_, _| {},
        ));
        let fin = tm.finish();
        (events, fin, back)
    }

    #[test]
    fn fading_decoded_track_is_reacquired_under_the_same_id() {
        let mut tm = rq_mgr(DetectorConfig::default().reacquire_hops);
        // 8 s: past the 5 s hang, inside the 10 s window.
        let (events, fin, back) = rq_fade_scene(&mut tm, 3000, 10);
        let promoted = rq_promoted_ids(&events);
        assert_eq!(promoted.len(), 1, "{promoted:?}");
        let id = promoted[0];
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, DecoderEvent::TrackClosed { .. })),
            "no track may close before finish()"
        );
        assert_eq!(tm.reacquisition_count(), 1);
        let after = rq_chars(&events, back);
        assert!(!after.is_empty(), "the second transmission decoded nothing");
        assert!(after.iter().all(|&(t, _)| t == id), "{after:?}");
        assert!(fin.contains(&DecoderEvent::TrackClosed {
            track_id: id,
            closure: ClosureKind::SignalEnded,
        }));
    }

    #[test]
    fn gap_longer_than_hang_plus_reacquire_closes_signal_ended_and_respawns() {
        let cfg = DetectorConfig::default();
        let mut tm = rq_mgr(cfg.reacquire_hops);
        // Keyed, then 16 s of quiet, one hop per batch so each hop's state
        // is seen: the drop hop is the last ACTIVE -> HANG.
        let mut scene = rq_keyed(RQ_TEXT, RQ_DIT, 10);
        scene.extend(rq_quiet(6000));
        let mut prev = BTreeMap::new();
        let mut drop_hop = BTreeMap::new();
        let mut close_hop = None;
        let mut events = rq_feed(&mut tm, &scene, 1, |tm, out| {
            for (&id, t) in &tm.tracks {
                if prev.get(&id) == Some(&LifecycleState::Active)
                    && t.state() == LifecycleState::Hang
                {
                    drop_hop.insert(id, tm.hop_counter);
                }
                prev.insert(id, t.state());
            }
            for e in out {
                if let DecoderEvent::TrackClosed {
                    track_id,
                    closure: ClosureKind::SignalEnded,
                } = e
                {
                    close_hop = Some((*track_id, tm.hop_counter));
                }
            }
        });
        let id = rq_promoted_ids(&events)[0];
        let (closed_id, close_hop) = close_hop.expect("closed SignalEnded");
        assert_eq!(closed_id, id);
        let drop_hop = drop_hop[&id];
        // The drop hop is the hang's first hop.
        assert_eq!(close_hop - drop_hop + 1, cfg.hang_hops + cfg.reacquire_hops);
        events.extend(rq_feed(
            &mut tm,
            &rq_keyed(RQ_TEXT, RQ_DIT, 10),
            16,
            |_, _| {},
        ));
        let promoted = rq_promoted_ids(&events);
        assert_eq!(promoted.len(), 2, "{promoted:?}");
        assert_ne!(promoted[1], id);
        assert_eq!(tm.reacquisition_count(), 0);
    }

    #[test]
    fn undecoded_track_closes_at_hang_hops_as_before() {
        let cfg = DetectorConfig::default();
        let mut tm = rq_mgr(cfg.reacquire_hops);
        // An unmodulated carrier: promoted, never decodes a character.
        let carrier: Vec<Vec<f32>> = (0..600).map(|_| nc_tone(10, true, 30.0)).collect();
        let events = rq_feed(&mut tm, &carrier, 16, |_, _| {});
        assert!(rq_chars(&events, 0).is_empty());
        let id = rq_promoted_ids(&events)[0];
        let mut drop_hop = None;
        let mut gone_hop = None;
        rq_feed(&mut tm, &rq_quiet(3000), 1, |tm, _| {
            match tm.tracks.get(&id) {
                Some(t) if t.state() == LifecycleState::Hang && drop_hop.is_none() => {
                    drop_hop = Some(tm.hop_counter);
                }
                None if gone_hop.is_none() => gone_hop = Some(tm.hop_counter),
                _ => {}
            }
        });
        assert_eq!(gone_hop.unwrap() - drop_hop.unwrap() + 1, cfg.hang_hops);
        assert_eq!(tm.close_counts().hang_expired, 1);
        assert_eq!(tm.reacquisition_count(), 0);
    }

    #[test]
    fn dormant_track_has_no_decoder_and_frozen_centroid() {
        let cfg = DetectorConfig::default();
        let mut tm = rq_mgr(cfg.reacquire_hops);
        let events = rq_feed(&mut tm, &rq_keyed(RQ_TEXT, RQ_DIT, 10), 16, |_, _| {});
        let id = rq_promoted_ids(&events)[0];
        rq_feed(&mut tm, &rq_quiet(cfg.hang_hops + 200), 16, |_, _| {});
        let track = &tm.tracks[&id];
        assert_eq!(track.state(), LifecycleState::Dormant);
        assert!(track.decoder.is_none());
        assert_eq!(tm.decoding_track_count(), 0);
        let center = track.center;
        // A +20 dB neighbour two channels up (MAN-197's drag) for 2000
        // hops: the dormant centroid must not move.
        let neighbour: Vec<Vec<f32>> = (0..2000).map(|_| nc_tone(12, true, 20.0)).collect();
        rq_feed(&mut tm, &neighbour, 1, |tm, _| {
            let t = &tm.tracks[&id];
            assert_eq!(t.state(), LifecycleState::Dormant);
            assert_eq!(t.center, center);
        });
    }

    /// Runs keyed text then `quiet` hops; the batch events.
    fn rq_fade_out(reacquire_hops: u64, quiet: u64) -> Vec<DecoderEvent> {
        let mut tm = rq_mgr(reacquire_hops);
        let mut events = rq_feed(&mut tm, &rq_keyed(RQ_TEXT, RQ_DIT, 10), 16, |_, _| {});
        events.extend(rq_feed(&mut tm, &rq_quiet(quiet), 16, |_, _| {}));
        events
    }

    #[test]
    fn dormancy_flush_matches_hang_expired_flush() {
        let quiet = DetectorConfig::default().hang_hops + 100;
        let dormant = rq_fade_out(DetectorConfig::default().reacquire_hops, quiet);
        let closed = rq_fade_out(0, quiet);
        assert!(closed
            .iter()
            .any(|e| matches!(e, DecoderEvent::TrackClosed { .. })));
        let without_close: Vec<DecoderEvent> = closed
            .into_iter()
            .filter(|e| !matches!(e, DecoderEvent::TrackClosed { .. }))
            .collect();
        assert_eq!(dormant, without_close);
    }

    #[test]
    fn woken_track_is_not_closed_unkeyed_immediately() {
        // MAN-313 filter on; this step-only harness decodes nothing, so
        // the reacquirable latch is set by hand.
        let cfg = nc_cfg(500);
        let mut tm = nc_mgr(cfg);
        let used = nc_qualify(&mut tm);
        let id = *tm.tracks.keys().next().unwrap();
        tm.tracks.get_mut(&id).unwrap().lifecycle.set_reacquirable();
        nc_drive(
            &mut tm,
            used,
            cfg.hang_hops + 100,
            |_| quiet_power(NC_N),
            |_, _| {},
        );
        assert_eq!(tm.tracks[&id].state(), LifecycleState::Dormant);
        // Key down for good: wake, then no further keying.
        let start = used + cfg.hang_hops + 100;
        let mut woke = None;
        nc_drive(
            &mut tm,
            start,
            600,
            |_| nc_tone(10, true, 30.0),
            |tm, _| {
                if woke.is_none() && tm.tracks[&id].state() == LifecycleState::Active {
                    woke = Some(tm.hop_counter);
                }
                if let Some(w) = woke {
                    if tm.hop_counter - w < cfg.unkeyed_close_hops {
                        assert!(
                            tm.tracks.contains_key(&id),
                            "closed at {}",
                            tm.hop_counter - w
                        );
                    }
                }
            },
        );
        assert!(woke.is_some());
        assert_eq!(tm.reacquisition_count(), 1);
        assert_eq!(
            tm.close_counts().unkeyed,
            1,
            "it still closes Unkeyed later"
        );
    }

    fn rq_speed_updates(events: &[DecoderEvent], id: u32) -> Vec<f32> {
        events
            .iter()
            .filter_map(|e| match e {
                DecoderEvent::SpeedUpdate { track_id, wpm } if *track_id == id => Some(*wpm),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn dormancy_and_wake_in_one_batch_keep_old_decoder_events_first() {
        let hang = DetectorConfig::default().hang_hops;
        // Batch A: the old signal speeds up (14-hop dits) and fades past
        // the hang, so the old decoder's `SpeedUpdate`s (sort key ts 0) all
        // come out of the DORMANT-entry flush. Batch B: the signal returns
        // slower (30-hop dits), so the woken decoder reports its own.
        let mut a = rq_keyed("W1AW W1AW W1AW", 14, 10);
        a.extend(rq_quiet(hang + 200));
        let b = rq_keyed(RQ_TEXT, 30, 10);
        let run = |batches: &[Vec<Vec<f32>>]| {
            let mut tm = rq_mgr(DetectorConfig::default().reacquire_hops);
            let events = rq_feed(&mut tm, &rq_keyed(RQ_TEXT, RQ_DIT, 10), 16, |_, _| {});
            let id = rq_promoted_ids(&events)[0];
            let out: Vec<Vec<DecoderEvent>> = batches
                .iter()
                .map(|batch| rq_feed(&mut tm, batch, batch.len(), |_, _| {}))
                .collect();
            assert_eq!(tm.reacquisition_count(), 1);
            (id, out)
        };
        let (id, split) = run(&[a.clone(), b.clone()]);
        let old = rq_speed_updates(&split[0], id);
        let new = rq_speed_updates(&split[1], id);
        assert!(!old.is_empty() && !new.is_empty(), "{old:?} {new:?}");
        let mut ab = a;
        ab.extend(b);
        let (_, one) = run(&[ab]);
        let both = rq_speed_updates(&one[0], id);
        assert_eq!(both, [old, new].concat());
    }

    #[test]
    fn second_station_inside_the_window_wakes_the_dormant_track() {
        // The accepted trade-off (MAN-108 D5): a different station keying up
        // on an owned channel inside the window wakes the old track.
        let mut tm = rq_mgr(DetectorConfig::default().reacquire_hops);
        let (events, _, back) = rq_fade_scene(&mut tm, 3000, 11);
        let promoted = rq_promoted_ids(&events);
        assert_eq!(promoted.len(), 1, "{promoted:?}");
        let after = rq_chars(&events, back);
        assert!(!after.is_empty());
        assert!(after.iter().all(|&(t, _)| t == promoted[0]), "{after:?}");
        assert_eq!(tm.reacquisition_count(), 1);
    }

    #[test]
    fn reacquire_zero_is_pre_man108() {
        let mut tm = rq_mgr(0);
        let (events, _, _) = rq_fade_scene(&mut tm, 3000, 10);
        let promoted = rq_promoted_ids(&events);
        assert_eq!(promoted.len(), 2, "{promoted:?}");
        let (first, second) = (promoted[0], promoted[1]);
        assert_ne!(first, second);
        let close = events
            .iter()
            .position(|e| {
                *e == DecoderEvent::TrackClosed {
                    track_id: first,
                    closure: ClosureKind::SignalEnded,
                }
            })
            .expect("the first track closes SignalEnded");
        let second_promo = events
            .iter()
            .position(|e| matches!(e, DecoderEvent::TrackPromoted { track_id, .. } if *track_id == second))
            .unwrap();
        assert!(close < second_promo);
        assert_eq!(tm.reacquisition_count(), 0);
    }

    /// MAN-312 (SPEC §2.7): the live-channel rule. Rig audio is too narrow
    /// for the detector; a KiwiSDR's ±5 kHz holds 105 channels with one
    /// channel of margin; a full-Nyquist source is every channel.
    #[test]
    fn crash_live_channels_follows_the_passband() {
        assert!(
            crash_live_channels(48_000.0, (300.0, 3000.0))
                < manta_dsp::crash::CRASH_MIN_LIVE_CHANNELS
        );
        assert_eq!(crash_live_channels(96_000.0, (-5000.0, 5000.0)), 105);
        assert_eq!(crash_live_channels(96_000.0, (-48_000.0, 48_000.0)), 1024);
    }
}
