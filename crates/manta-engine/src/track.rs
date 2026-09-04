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
    /// MAN-4: refuse to *spawn* tracks on channels whose baseband offset is
    /// within `guard_hz` of DC or of +/-Nyquist. `0.0` (the default) is no
    /// guard at all -- complex-IQ paths, including every golden vector, are
    /// unaffected. `listen()` (and `soak_with_metrics()`) raise it to the
    /// source's declared `IqSource::analytic_guard_hz` floor. Floor/gate
    /// estimation still runs on all channels; only spawning is gated (see
    /// docs/DECISIONS/2026-09-04-man-4-hilbert-guard-pins.md, decision D7).
    pub guard_hz: f64,
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
    fn default() -> Self {
        DetectorConfig {
            on_snr_db: 12.0,
            off_snr_db: 3.0,
            confirm_hops: 19,
            hang_hops: 1875,
            gc_hops: 11250,
            warmup_hops: 750,
            track_cap: 500,
            guard_hz: 0.0,
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

use manta_decode::decoder::{DecodeConfig, TrackDecoder};
use manta_decode::events::DecoderEvent;
use manta_dsp::channelizer::{interpolate_offset, power_db, HopOutput};
use manta_dsp::floor::{FloorBank, Gate};
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
    /// The channel index this track first spawned on (SPEC §2.1); the
    /// anchor for `Track::freq_hz`'s absolute-Hz conversion.
    pub(crate) birth_channel: usize,
    /// The track's leased decoder, allocated on CANDIDATE -> ACTIVE
    /// promotion (SPEC §2.4/§5); `None` before promotion.
    decoder: Option<TrackDecoder>,
    /// `(magnitude, sample_ts)` queued this hop-batch by `step_hop`,
    /// drained once per `process_hops` call by `drain_pool` (ARCHITECTURE
    /// §10's decoder pool).
    pending: Vec<(f32, u64)>,
    /// Set by `process_hops` once `drain_pool` has actually produced a
    /// `DecoderEvent` for this track. Distinct from `decoder.is_some()`:
    /// a track promoted and then merged/evicted within the *same*
    /// `process_hops` batch never gets a `drain_pool` pass before it's
    /// removed (that only runs once, after every hop in the batch), so it
    /// can have an allocated decoder yet have emitted nothing at all
    /// (MAN-19 review round 1). `TrackClosed` emission checks this, not
    /// decoder presence.
    has_emitted: bool,
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
            birth_channel,
            decoder: None,
            pending: Vec::new(),
            has_emitted: false,
        }
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
    /// MAN-4: per-channel count of tracks ever spawned on that channel,
    /// read via `spawns_by_channel` -- the assertion surface MAN-4's
    /// regression tests use, and (D10) a companion to `close_counts`
    /// (issue #26) for the future M3 metrics endpoint.
    spawns_by_channel: Vec<u32>,
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
            spawns_by_channel: vec![0; n_channels],
        }
    }

    fn n_channels(&self) -> usize {
        self.owner_of.len()
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

    /// MAN-4/D10: per-channel count of tracks ever spawned on that channel
    /// (length `n_channels`), for measuring "did this signal spawn more
    /// than one track" as a real assertion instead of manual inspection.
    /// Companion to `close_counts` (issue #26) for the future M3 metrics
    /// endpoint.
    // Temporary: no non-test reader yet -- unlike `close_counts`, no
    // production caller (soak_metrics.rs) reads this until the M3 metrics
    // endpoint lands.
    #[allow(dead_code)]
    pub fn spawns_by_channel(&self) -> &[u32] {
        &self.spawns_by_channel
    }

    /// Total tracks ever spawned (== the next id that would be assigned
    /// minus 1). MAN-4's end-to-end regression test uses this to assert "no
    /// spurious respawns" directly, rather than inferring it from id churn.
    // Temporary: no non-test reader yet -- same rationale as
    // `spawns_by_channel` above.
    #[allow(dead_code)]
    pub fn total_spawns(&self) -> u32 {
        self.next_id - 1
    }

    /// MAN-4: signed baseband offset of channel `k`, Hz -- the same
    /// circular FFT-bin convention as `Channelizer::channel_freq_hz` (SPEC
    /// §1.1), minus the center frequency.
    fn channel_offset_hz(&self, k: usize) -> f64 {
        wrapped_channel_offset(k, self.n_channels()) * self.channel_spacing_hz
    }

    /// MAN-4: is `k` inside the configured DC/Nyquist guard band? `false`
    /// unconditionally when `guard_hz <= 0.0` (the default), so every
    /// complex-IQ path is a structural no-op.
    fn is_guarded(&self, k: usize) -> bool {
        if self.cfg.guard_hz <= 0.0 {
            return false;
        }
        let off = self.channel_offset_hz(k).abs();
        let nyquist = self.channel_spacing_hz * self.n_channels() as f64 / 2.0;
        off < self.cfg.guard_hz || off > nyquist - self.cfg.guard_hz
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
    fn step_hop(&mut self, hop: &HopOutput, sample_ts: u64) -> Vec<u32> {
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
        self.recompute_ownership();

        let past_warmup = self.hop_counter >= self.cfg.warmup_hops;
        self.hop_counter += 1;

        // Drive existing tracks; collect closures to apply after the loop
        // (avoids mutating `self.tracks` while iterating it).
        let mut closed: Vec<u32> = Vec::new();
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
                    track.pending.push((hop.power[k].sqrt(), sample_ts));
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
                        track.pending.push((hop.power[k].sqrt(), sample_ts));
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
        closed.retain(|id| {
            self.tracks
                .remove(id)
                .is_some_and(|track| track.has_emitted)
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
                if rise[k] && self.owner_of[k].is_none() && !self.is_guarded(k) {
                    let mut winner = k;
                    if k + 1 < n
                        && rise[k + 1]
                        && self.owner_of[k + 1].is_none()
                        && !self.is_guarded(k + 1)
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
        closed.extend(self.merge_converged());
        closed.extend(self.evict_over_cap());
        closed
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
        self.spawns_by_channel[birth_channel] += 1;
        let mut track = Track::new(id, birth_channel, &self.cfg);
        let f = self.floor.effective_floor_db(birth_channel);
        track.current_snr_db = (self.gate.smoothed_db(birth_channel) - f) as f32;
        for ch in track.owned(self.n_channels()) {
            self.owner_of[ch] = Some(id);
        }
        self.tracks.insert(id, track);
    }

    /// SPEC §2.5: tracks whose centers converge within 1.0 channel merge;
    /// the lower-current-SNR one is closed.
    fn merge_converged(&mut self) -> Vec<u32> {
        let ids: Vec<u32> = self.tracks.keys().copied().collect();
        let mut to_close = Vec::new();
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                let (a, b) = (ids[i], ids[j]);
                if to_close.contains(&a) || to_close.contains(&b) {
                    continue;
                }
                let (ca, cb) = (self.tracks[&a].center, self.tracks[&b].center);
                if (ca - cb).abs() < 1.0 {
                    let loser = if self.tracks[&a].current_snr_db <= self.tracks[&b].current_snr_db
                    {
                        a
                    } else {
                        b
                    };
                    to_close.push(loser);
                }
            }
        }
        // MAN-19: only report a merge-loser as `TrackClosed`-worthy if it
        // actually emitted a real event -- see the matching comment in
        // `step_hop` (a track promoted this same batch can be merged away
        // before ever getting a `drain_pool` pass).
        let ever_emitted_closed = to_close
            .into_iter()
            .filter(|id| {
                self.close_counts.record(CloseReason::Merged);
                self.tracks
                    .remove(id)
                    .is_some_and(|track| track.has_emitted)
            })
            .collect();
        if !ids.is_empty() {
            self.recompute_ownership();
        }
        ever_emitted_closed
    }

    /// SPEC §2.4/ARCHITECTURE §4: track cap with lowest-current-SNR
    /// eviction.
    fn evict_over_cap(&mut self) -> Vec<u32> {
        let mut evicted = Vec::new();
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
            if self
                .tracks
                .remove(&loser)
                .is_some_and(|track| track.has_emitted)
            {
                evicted.push(loser);
            }
        }
        self.recompute_ownership();
        evicted
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
        let mut closed_ids: Vec<u32> = Vec::new();
        for h in hops {
            closed_ids.extend(self.step_hop(h, hop_to_sample_ts(h.m)));
        }
        let mut events = self.drain_pool();
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
        // correctly stays `false`.
        for e in &events {
            if let Some(t) = self.tracks.get_mut(&event_track_id(e)) {
                t.has_emitted = true;
            }
            if let DecoderEvent::CharDecoded { track_id, .. } = e {
                if let Some(t) = self.tracks.get_mut(track_id) {
                    t.lifecycle.note_char_decoded();
                }
            }
        }
        // MAN-19: emit one `TrackClosed` per track closed this batch (any
        // CloseReason) so downstream per-track_id state (`manta-spot`'s
        // `Validator::tracks`, `RepetitionGate::seen`) has a signal to
        // free it -- see events.rs's TrackClosed doc for the unbounded-
        // growth bug this fixes. Re-sorted with the rest per SPEC §6 rule
        // 6; `event_sample_ts` gives these no ordering claim (ties at 0,
        // same as SpeedUpdate/TrackMeta).
        events.extend(
            closed_ids
                .into_iter()
                .map(|track_id| DecoderEvent::TrackClosed { track_id }),
        );
        events.sort_by_key(|e| (event_sample_ts(e), event_track_id(e)));
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
                .map(|track_id| DecoderEvent::TrackClosed { track_id }),
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
                    Some((t.decoder.as_mut().unwrap(), pending))
                }
            })
            .par_bridge()
            .flat_map_iter(|(decoder, pending)| {
                pending
                    .into_iter()
                    .flat_map(|(mag, ts)| decoder.push_envelope(mag, ts))
                    .collect::<Vec<_>>()
            })
            .collect();
        events.sort_by_key(|e| (event_sample_ts(e), event_track_id(e)));
        events
    }
}

/// SPEC §6 rule 6 resequencing key: the sample timestamp an event is
/// anchored to, `0` for events with no inherent timestamp (`SpeedUpdate`/
/// `TrackMeta`, which sort first among ties on `event_track_id`), or
/// `u64::MAX` for `TrackClosed` -- a synthetic "after everything" marker,
/// not `0` (round 7 review): `TrackClosed` isn't anchored to a real
/// timestamp either, but unlike `SpeedUpdate`/`TrackMeta` it must never
/// sort before another real event for the SAME track_id (a track's own
/// final `CharDecoded`/`WordBoundary`, `finish()`'s flush) -- doing so
/// lets a consumer free that track's state and then recreate it
/// processing the trailing events, with nothing left to ever clean that
/// up again (the exact leak this whole mechanism exists to prevent).
/// `MAX` guarantees that regardless of how large a real `sample_ts`
/// grows. This is what lets `finish()` (and `process_hops`) apply ONE
/// consistent `(sample_ts, track_id)` sort across the whole batch
/// (SPEC-decode-core.md §6 rule 6) instead of the append-without-
/// re-sorting workaround round 4 used.
fn event_sample_ts(e: &DecoderEvent) -> u64 {
    match e {
        DecoderEvent::CharDecoded { sample_ts, .. }
        | DecoderEvent::WordBoundary { sample_ts, .. } => *sample_ts,
        DecoderEvent::SpeedUpdate { .. } | DecoderEvent::TrackMeta { .. } => 0,
        DecoderEvent::TrackClosed { .. } => u64::MAX,
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
        | DecoderEvent::TrackClosed { track_id } => *track_id,
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

    #[test]
    fn guard_hz_defaults_to_zero_so_complex_iq_paths_are_unchanged() {
        assert_eq!(DetectorConfig::default().guard_hz, 0.0);
    }

    #[test]
    fn guard_band_blocks_spawning_near_dc_and_nyquist() {
        // fs=6000, n=64 -> channel spacing 93.75 Hz, the same spacing the
        // real 48 kHz/512-channel audio path uses -- so the guarded/
        // unguarded channels here scale directly to the ticket's 750 Hz
        // (channel 8) scenario.
        let n = 64;
        let cfg = DetectorConfig {
            guard_hz: 300.0,
            ..DetectorConfig::default()
        };
        let mut tm = TrackManager::new(n, 6_000.0, 0.0, cfg, DecodeConfig::default());
        feed_warmup(&mut tm, n);
        let mut power = quiet_power(n);
        power[1] = 1e-9 * 10f32.powf(20.0 / 10.0); // 93.75 Hz -- inside the 300 Hz DC guard
        power[32] = 1e-9 * 10f32.powf(20.0 / 10.0); // Nyquist midpoint (k = n/2) -- guarded
        power[8] = 1e-9 * 10f32.powf(20.0 / 10.0); // 750 Hz -- outside the guard
        for m in (250 * 15)..(250 * 15 + 60) {
            tm.step_hop(&hop(m, power.clone()), m);
        }
        assert_eq!(
            tm.spawns_by_channel()[1],
            0,
            "k=1 (93.75 Hz) is inside the guard"
        );
        assert_eq!(
            tm.spawns_by_channel()[32],
            0,
            "Nyquist midpoint is guarded"
        );
        assert!(
            tm.spawns_by_channel()[8] > 0,
            "k=8 (750 Hz) is outside the guard"
        );
    }

    #[test]
    fn guard_band_wraps_circularly_like_channel_freq_hz() {
        // k = n-1 is -93.75 Hz, i.e. INSIDE a 300 Hz guard despite its high
        // index -- the exact circular-index confusion MAN-4 turns on.
        let n = 64;
        let cfg = DetectorConfig {
            guard_hz: 300.0,
            ..DetectorConfig::default()
        };
        let mut tm = TrackManager::new(n, 6_000.0, 0.0, cfg, DecodeConfig::default());
        feed_warmup(&mut tm, n);
        let mut power = quiet_power(n);
        power[n - 1] = 1e-9 * 10f32.powf(20.0 / 10.0);
        for m in (250 * 15)..(250 * 15 + 60) {
            tm.step_hop(&hop(m, power.clone()), m);
        }
        assert_eq!(tm.spawns_by_channel()[n - 1], 0);
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
