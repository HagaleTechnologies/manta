//! Streaming pipeline: live/replayed audio -> PFB channelizer ->
//! `TrackManager` (SPEC §2), run continuously until Ctrl-C or EOF, emitting
//! the merged multi-track decode event stream as it's produced. No actor/
//! ring-thread split; see design doc §4.

use crate::latency::DecodeLatencyObserver;
use crate::live::{LiveStateSink, LiveTap};
use crate::quality::DecodeQualityObserver;
use crate::track::CloseCounts;
use crate::PipelineConfig;
use anyhow::Result;
use manta_decode::events::DecoderEvent;
use manta_input::IqSource;
use manta_spot::reference::ReferenceLists;
use manta_spot::{Blocklist, NotchList, Validator};
use num_complex::Complex32;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// One chunk read per loop iteration, in samples.
const CHUNK_SAMPLES: usize = 2048;
/// Seconds of audio buffered before the channelizer is built and streaming
/// begins. This buffer is no longer a one-shot channel-pick calibration
/// (`TrackManager` detects and tracks continuously, SPEC §2) -- it's fed
/// through `TrackManager::process_hops` like any other chunk, just like the
/// startup lead-in padding below it.
const CALIBRATION_SECONDS: f64 = 2.0;

/// Optional live handles into a running `listen()` loop, for a caller that
/// needs to observe engine-owned state the callbacks can't see.
///
/// MAN-45 (PR #63 round-9 finding): the daemon's `manta_active_tracks` gauge
/// reported a constant 0 because `listen()` owns its `TrackManager`
/// internally and exposed no live count -- `Metrics::set_active_tracks` had
/// no non-test caller. A shared atomic rather than another callback: the
/// consumer (`manta-cli`'s server runtime) polls on its own schedule and
/// must never be able to block the decode loop, which is exactly the shape
/// `IqSource::confirmed_live_handle` already uses for source liveness
/// (MAN-55).
#[derive(Clone, Default)]
pub struct ListenObservers {
    /// Updated after each processed chunk with `TrackManager::decoding_track_count()`.
    /// `None` (the default) skips the store entirely, so `listen()`'s
    /// existing callers -- including `soak()` and the CPU-budget bench --
    /// pay nothing.
    pub active_tracks: Option<Arc<AtomicU64>>,
    /// MAN-128: wall-clock time spent on one steady-state input chunk --
    /// channelize, track/decode, validate, `on_spot` -- measured from after
    /// `src.read()` returns until the chunk's processing ends. Excludes the
    /// source read wait and the startup calibration/padding blocks (a 2s
    /// block processed as one chunk would otherwise skew the distribution).
    /// `None` (the default) skips both the `Instant::now()` calls and the
    /// observation entirely, so `listen()`'s existing callers pay nothing.
    pub decode_latency: Option<Arc<DecodeLatencyObserver>>,
    /// MAN-118: decode-quality signals -- characters decoded, garbles, each
    /// `legacy` track's keying-rail ratio once per second, and the
    /// validator's repetition-gate results. Write-only from the decode
    /// path, so output is unchanged. `None` (the default) skips every
    /// count and sample, so `listen()`'s existing callers pay nothing.
    pub decode_quality: Option<Arc<DecodeQualityObserver>>,
    /// MAN-78: the one inbound field -- replacement operator lists from a
    /// live config reload, taken before each decoder event reaches the
    /// running `Validator`. `None` (the default) skips the check
    /// entirely, so `listen()`'s existing callers pay nothing.
    pub operator_lists: Option<Arc<OperatorListsUpdate>>,
    /// MAN-316: the country table and known-call set a scheduled list
    /// refresh replaces. The running `Validator` takes their current tables
    /// at the start (so the caller builds this from the same
    /// `PipelineConfig`, `ReferenceLists::new(cfg.cty_table(),
    /// cfg.scp_set())`) and again before the next decoder event after a
    /// `store`. `None` (the default) skips the check entirely, so `listen()`'s
    /// existing callers pay nothing.
    pub reference_lists: Option<Arc<ReferenceLists>>,
    /// MAN-312: static crashes detected so far this session
    /// (`TrackManager::static_crash_count`, summed across the segments a
    /// discontinuity starts), stored with `fetch_max` after each processed
    /// chunk so the published value never goes backwards. A counter, so it
    /// is never zeroed on exit. `None` (the default) skips the store.
    pub static_crashes: Option<Arc<AtomicU64>>,
    /// MAN-234: input samples the channelizer replaced with silence
    /// (`Channelizer::bad_samples`, summed across the segments a
    /// discontinuity starts), stored with `fetch_max` after each processed
    /// chunk so it never goes backwards. `None` (the default) skips it.
    pub bad_samples: Option<Arc<AtomicU64>>,
    /// MAN-314: the currently marked RTTY signals (SPEC §2.6), stored
    /// whenever the marked set or a marked signal's metadata changes (so a
    /// store is not by itself a mark or clear), emptied on a segment
    /// restart, a read error and every exit. `None` (the default) skips it
    /// entirely.
    pub rtty: Option<Arc<RttyObserver>>,
    /// MAN-313: every CANDIDATE -> ACTIVE confirmation the detector has
    /// made since `listen()` started (`TrackManager::confirmations`),
    /// whether or not the track went on to qualify as keyed CW. Summed
    /// across outage segments, so it only ever grows. Stored wherever the
    /// active-track count is. `None` (the default) skips it entirely.
    pub detector_confirmations: Option<Arc<AtomicU64>>,
    /// MAN-324: the live-state stream -- once per stream second each
    /// channel's floor and occupancy, each track's running copy and
    /// candidates when its text changes, every validation decision and
    /// every track closure. Write-only from the decode path, so spots are
    /// unchanged. `None` (the default) skips everything, so `listen()`'s
    /// existing callers pay nothing.
    pub live_state: Option<Arc<dyn LiveStateSink>>,
    /// MAN-236: every open track (including unconfirmed candidates) and its
    /// peak, per-reason closes across outage segments, gate-record total,
    /// and steady-state chunk count. Use a fresh observer for each run.
    /// `None` skips all lifecycle reads and stores. Stores are relaxed atomics.
    pub track_lifecycle: Option<Arc<crate::TrackLifecycleObserver>>,
}

/// MAN-314: the decode loop's currently marked RTTY signals, calibrated.
/// `generation` moves on every store, so a poller re-reads `snapshot` only
/// when it changed.
#[derive(Debug, Default)]
pub struct RttyObserver {
    generation: AtomicU64,
    /// The marked signals and the stream sample timestamp they reflect.
    signals: Mutex<(Vec<crate::RttySignalReport>, u64)>,
}

impl RttyObserver {
    /// Bumped on every store.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// The currently marked signals.
    pub fn snapshot(&self) -> Vec<crate::RttySignalReport> {
        self.snapshot_with_sample_ts().0
    }

    /// The currently marked signals and the stream sample timestamp of the
    /// store that published them: the hop whose block changed the marked
    /// set, or the end of the segment when an outage restart, a read error
    /// or the end of the stream emptied it. A signal missing here but
    /// present in an earlier snapshot cleared at this timestamp if the
    /// poller saw every store, and no later than it otherwise.
    pub fn snapshot_with_sample_ts(&self) -> (Vec<crate::RttySignalReport>, u64) {
        self.signals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn store(&self, signals: Vec<crate::RttySignalReport>, sample_ts: u64) {
        *self.signals.lock().unwrap_or_else(|e| e.into_inner()) = (signals, sample_ts);
        self.generation.fetch_add(1, Ordering::Release);
    }
}

/// MAN-314: empties the RTTY observer on construction and on every exit
/// path, like `ActiveTracksGuard`, and publishes the segment's marked set
/// when the track manager reports a change.
struct RttyPublisher {
    observer: Option<Arc<RttyObserver>>,
    seen_changes: u64,
    calibration_factor: f64,
    /// Stream sample timestamp of the latest store or restart, for the
    /// clear on drop.
    last_sample_ts: u64,
}

impl RttyPublisher {
    fn new(observer: Option<Arc<RttyObserver>>, calibration_factor: f64) -> Self {
        let p = Self {
            observer,
            seen_changes: 0,
            calibration_factor,
            last_sample_ts: 0,
        };
        p.clear();
        p
    }

    fn clear(&self) {
        if let Some(obs) = &self.observer {
            if !obs.snapshot().is_empty() {
                obs.store(Vec::new(), self.last_sample_ts);
            }
        }
    }

    /// A fresh segment's track manager starts with nothing marked.
    /// `end_sample_ts` is the stream sample timestamp at which the ending
    /// segment's marks clear.
    fn restart(&mut self, end_sample_ts: u64) {
        self.seen_changes = 0;
        self.last_sample_ts = end_sample_ts;
        self.clear();
    }

    fn update(&mut self, tm: &crate::track::TrackManager) {
        let Some(obs) = &self.observer else {
            return;
        };
        if tm.rtty_changes() == self.seen_changes {
            return;
        }
        self.seen_changes = tm.rtty_changes();
        self.last_sample_ts = tm.rtty_changed_sample_ts();
        obs.store(
            tm.rtty_signals()
                .iter()
                .filter(|e| e.cleared_sample_ts.is_none())
                .map(|e| crate::RttySignalReport::from_entry(e, self.calibration_factor))
                .collect(),
            self.last_sample_ts,
        );
    }
}

impl Drop for RttyPublisher {
    fn drop(&mut self) {
        self.clear();
    }
}

/// MAN-78: the operator lists a live reload replaces, as one unit.
#[derive(Debug, Clone, Default)]
pub struct OperatorLists {
    pub allowlist: Vec<String>,
    pub blocklist: Blocklist,
    pub notch: NotchList,
}

/// MAN-78: hands replacement lists from the reload thread to the decode
/// loop. The newest offer wins; the validator's next decoder event uses it.
#[derive(Default)]
pub struct OperatorListsUpdate {
    pending: AtomicBool,
    next: Mutex<Option<OperatorLists>>,
}

impl OperatorListsUpdate {
    /// Stores `lists` as the next update, replacing any not yet taken.
    pub fn offer(&self, lists: OperatorLists) {
        // Poison-tolerant: the slot holds plain data, so a panic in another
        // holder cannot leave it half-written.
        let mut next = self.next.lock().unwrap_or_else(|e| e.into_inner());
        *next = Some(lists);
        self.pending.store(true, Ordering::Release);
    }

    /// The newest offer not yet taken, if any. A relaxed load is the whole
    /// cost when nothing was offered.
    pub fn take(&self) -> Option<OperatorLists> {
        if !self.pending.load(Ordering::Relaxed) {
            return None;
        }
        if !self.pending.swap(false, Ordering::Acquire) {
            return None;
        }
        self.next.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// MAN-316: the decode loop's view of `ListenObservers::reference_lists`:
/// the generation of each list its validator already holds, so a `store`
/// costs one table swap and an unchanged list costs two atomic loads per
/// event.
struct ReferenceWatch {
    lists: Arc<ReferenceLists>,
    seen_cty: u64,
    seen_scp: u64,
}

impl ReferenceWatch {
    /// Starts in step with `lists`: `validator` takes their current tables,
    /// so a `store` made before `listen()` began is not missed.
    fn new(lists: Arc<ReferenceLists>, validator: &mut Validator) -> Self {
        let mut watch = Self {
            lists,
            seen_cty: 0,
            seen_scp: 0,
        };
        watch.apply(validator);
        watch
    }

    /// Hands `validator` the current tables if either list moved since the
    /// last call.
    fn sync(&mut self, validator: &mut Validator) {
        if self.lists.cty.generation() != self.seen_cty
            || self.lists.scp.generation() != self.seen_scp
        {
            self.apply(validator);
        }
    }

    fn apply(&mut self, validator: &mut Validator) {
        // Generations before values: a `store` landing in between leaves a
        // generation behind its value, so the next `sync` swaps again rather
        // than recording the new generation against the old table.
        let (cty_gen, scp_gen) = (self.lists.cty.generation(), self.lists.scp.generation());
        validator.replace_reference_tables(self.lists.cty.load(), Some(self.lists.scp.load()));
        self.seen_cty = cty_gen;
        self.seen_scp = scp_gen;
    }
}

/// Zeroes the active-track observer on EVERY exit path out of
/// `listen_with_observers`, not just the happy one.
///
/// MAN-45 (PR #63 round-19 finding, P2): `IqSource::read` failing after at
/// least one track had become active returned through `?` before
/// `tm.finish()` and the final `report_active_tracks`, leaving the observer
/// pinned at its last nonzero value while the `TrackManager` behind it was
/// already dropped -- a ghost count any long-lived consumer of
/// `listen_with_observers` would keep publishing after a device disconnect
/// or a file-read error. `manta-cli` happened to paper over this by zeroing
/// its own separate `Metrics` gauge; nothing made that true for other
/// callers. A `Drop` guard rather than cleanup appended to the end of the
/// function, because the error paths are precisely the ones that never
/// reached the end.
struct ActiveTracksGuard(Option<Arc<AtomicU64>>);

impl ActiveTracksGuard {
    /// Also zeroes on construction: a caller reusing one handle across runs
    /// (or one that seeded it with a nonzero value) must not see the
    /// previous run's tail count until the first chunk is processed.
    fn new(gauge: Option<Arc<AtomicU64>>) -> Self {
        let guard = Self(gauge);
        guard.clear();
        guard
    }

    fn clear(&self) {
        if let Some(gauge) = &self.0 {
            gauge.store(0, Ordering::Relaxed);
        }
    }
}

impl Drop for ActiveTracksGuard {
    fn drop(&mut self) {
        self.clear();
    }
}

/// One continuous decode segment: its own channelizer and track manager,
/// spanning the sample clock until the next discontinuity (MAN-73). The
/// `Validator` spans every segment of a `listen()` call; only the segment
/// itself (and its sample clock) restarts.
struct Segment {
    ch: manta_dsp::channelizer::Channelizer,
    tm: crate::track::TrackManager,
    hop: u64,
    pad_hops: u64,
}

#[allow(clippy::too_many_arguments)]
fn new_segment(
    fs: f64,
    center_freq_hz: f64,
    rf_passband_hz: (f64, f64),
    trusted_band_hz: Option<(f64, f64)>,
    cfg: &PipelineConfig,
    next_id: u32,
    quality: Option<Arc<DecodeQualityObserver>>,
    occupancy: bool,
) -> Result<Segment> {
    let ch = manta_dsp::channelizer::Channelizer::new(fs, center_freq_hz)
        .map_err(|e| anyhow::anyhow!(e))?;
    let hop = ch.hop() as u64;
    let pad_samples = ch.filter_len();
    let pad_hops = (pad_samples as u64).div_ceil(hop);
    let mut tm = crate::track::TrackManager::new(
        ch.n_channels(),
        fs,
        center_freq_hz,
        cfg.detector,
        cfg.decode.clone(),
    );
    tm.resume_track_ids_from(next_id);
    tm.skip_preroll_hops(pad_hops);
    // MAN-312 (SPEC §2.7): judge static crashes over the source's real
    // passband, not channels that only carry the stream's residual noise.
    tm.set_rf_passband_hz(rf_passband_hz.0, rf_passband_hz.1);
    // MAN-196: never spawn tracks outside the source's trusted band (the
    // decimator's <= 1 dB boundary on a decimated capture; `None`, no
    // decimator, leaves the manager exactly as it was). Spawns-only per
    // segment, same threading as the RF passband above.
    if let Some((lo, hi)) = trusted_band_hz {
        tm.set_trusted_band_hz(lo, hi);
    }
    // MAN-118: a post-discontinuity segment keeps reporting.
    tm.set_quality_observer(quality);
    // MAN-324: a post-discontinuity segment keeps the live occupancy window.
    if occupancy {
        tm.enable_occupancy_window();
    }
    Ok(Segment {
        ch,
        tm,
        hop,
        pad_hops,
    })
}

/// What `emit` needs besides the events and callbacks, built once per
/// `listen_with_observers` call.
struct EmitCtx<'a> {
    lists: Option<&'a OperatorListsUpdate>,
    quality: Option<&'a DecodeQualityObserver>,
    calibration_factor: f64,
    /// MAN-324: `Some` only when `ListenObservers::live_state` is set.
    live: Option<LiveTap>,
    /// MAN-316: `Some` only when `ListenObservers::reference_lists` is set.
    references: Option<ReferenceWatch>,
}

fn emit(
    events: Vec<DecoderEvent>,
    validator: &mut Validator,
    ctx: &mut EmitCtx<'_>,
    on_event: &mut impl FnMut(&DecoderEvent),
    on_spot: &mut impl FnMut(&crate::Spot),
) {
    // MAN-324: record decisions only while the live stream has a
    // subscriber, so an idle listener allocates nothing per decision.
    if let Some(tap) = ctx.live.as_ref() {
        let want = tap.wants_records();
        if want != validator.records_decisions() {
            validator.record_decisions(want);
        }
    }
    let mut chars = 0u64;
    for ev in events {
        if matches!(ev, DecoderEvent::CharDecoded { .. }) {
            chars += 1;
        }
        // MAN-78: the one place the validator is fed, so the one place a
        // reload's lists are taken, for calibration, steady state, outage
        // and end-of-stream batches alike.
        if let Some(l) = ctx.lists.and_then(OperatorListsUpdate::take) {
            validator.replace_operator_lists(&l.allowlist, l.blocklist, l.notch);
        }
        // MAN-316: likewise for a list refresh's new cty/scp tables.
        if let Some(watch) = ctx.references.as_mut() {
            watch.sync(validator);
        }
        let calibrated = crate::calibrate_freq_events(&ev, ctx.calibration_factor);
        on_event(&calibrated);
        // MAN-324: state first (a closing track's last text is flushed
        // while the validator still holds its candidates); decisions are
        // queued and go out after the batch's states, or before a closure.
        if let Some(tap) = ctx.live.as_mut() {
            tap.observe(&calibrated, validator);
        }
        for spot in validator.ingest(&ev) {
            on_spot(&spot);
        }
        if let Some(tap) = ctx.live.as_mut() {
            tap.decisions(validator.take_decisions());
            if let DecoderEvent::TrackClosed { track_id, closure } = &ev {
                tap.closed(*track_id, *closure, validator);
            }
        }
    }
    if let Some(tap) = ctx.live.as_mut() {
        tap.end_batch(validator);
    }
    // MAN-118: once per batch. The validator's repetition counts are
    // already cumulative, so they overwrite rather than add.
    if let Some(q) = ctx.quality {
        q.add_chars(chars);
        q.store_repetitions(validator.repetition_counts());
    }
}

/// Run the streaming decode loop against `src` until `read` returns 0 (EOF,
/// file replay) or `stop` is set (Ctrl-C, live audio). Each decoded event is
/// passed to `on_event` as it's produced. Design doc §4.
///
/// MAN-73: a live source may report a discontinuity via
/// `IqSource::take_discontinuity()` (e.g. after a reconnect). When it does,
/// the current segment (channelizer + track manager) is closed -- every
/// open track gets its `TrackClosed`, as `ClosureKind::Bookkeeping` rather
/// than `SignalEnded`, since the transmission may still be on the air --
/// and a fresh segment starts, with its sample clock advanced by the
/// reported gap and its track ids continuing from the closed segment's.
/// This keeps spot timestamps
/// wall-clock-true across the outage without splicing pre-outage audio
/// onto post-outage audio or zero-filling the gap (zero-fill pins the
/// noise floor and floods false tracks on resume -- see
/// `IqSource::take_discontinuity`'s doc comment). File replay never
/// reports a discontinuity, so this is a no-op there. An end of stream
/// reported with a gap pending (MAN-258: a stop while the source was down)
/// closes the segment the same way and starts no new one.
///
/// Unchanged entry point: `listen_with_observers` with no observers. Kept so
/// MAN-45's engine addition costs its four existing call sites nothing.
pub fn listen(
    src: Box<dyn IqSource>,
    cfg: &PipelineConfig,
    stop: Arc<AtomicBool>,
    on_event: impl FnMut(&DecoderEvent),
    on_spot: impl FnMut(&crate::Spot),
) -> Result<()> {
    listen_with_observers(
        src,
        cfg,
        stop,
        ListenObservers::default(),
        on_event,
        on_spot,
        |_n| {},
    )
}

/// Like `listen`, but additionally publishes engine-owned live state (the
/// active-track count) to the caller two ways: into `observers`, a shared
/// atomic a consumer on another thread can poll on its own schedule
/// (MAN-45), and to `on_tracks`, a synchronous per-batch callback (MAN-122).
/// Both carry the same number; they exist side by side because they answer
/// different questions -- see `ListenObservers`'s doc comment for why the
/// gauge is an atomic, and the `on_tracks` paragraphs below for why the
/// daemon additionally needs the per-batch *edge*.
///
/// `on_tracks` is called with
/// `TrackManager::decoding_track_count()` after every batch the pipeline
/// processes, including the final `finish()`, which reports 0.
///
/// MAN-122 review round 2: it fires on EVERY batch, not only when the
/// count changes. The call is the daemon's only per-batch signal that the
/// synchronous decode loop is still turning over -- a status line derived
/// from change-only notifications cannot tell "quiet band, count steady at
/// 2" from "`IqSource::read` wedged, count frozen at 2 since an hour ago",
/// which is exactly the question the line exists to answer. Suppressing
/// repeats is the caller's business now (`manta-cli` keeps the gauge store
/// unconditional -- one relaxed atomic per ~43 ms chunk is nothing against
/// the per-batch DSP work it follows).
///
/// MAN-122 review round 1: the daemon's `manta_active_tracks` gauge and its
/// status line's `tracks=` field must come from the manager's own lifecycle
/// state, not from the `DecoderEvent` stream `on_event` already sees. A
/// track that `TrackManager` has promoted but whose demodulator has not
/// latched emits no events at all -- `TrackDecoder` withholds `TrackMeta`
/// until `snr_2500_db()` is `Some`, and a silent ACTIVE track survives to
/// the ~30 s `gc_hops` GC -- so an event-derived count reports zero while
/// real decoders are running on weak or unmodulated signals. This is the
/// count that cannot lie about that.
///
/// Kept as a separate entry point rather than extra parameters on
/// `listen()` so the existing callers and tests are untouched; `listen()`
/// is now a no-op-observer wrapper over this.
pub fn listen_with_observers(
    mut src: Box<dyn IqSource>,
    cfg: &PipelineConfig,
    stop: Arc<AtomicBool>,
    observers: ListenObservers,
    mut on_event: impl FnMut(&DecoderEvent),
    mut on_spot: impl FnMut(&crate::Spot),
    mut on_tracks: impl FnMut(usize),
) -> Result<()> {
    // Declared before the first `?` below so that EVERY early return from
    // here on clears the observer -- see `ActiveTracksGuard`'s doc comment.
    let _active_tracks_guard = ActiveTracksGuard::new(observers.active_tracks.clone());

    // Validated up front so a bad config value fails fast, before spending
    // CALIBRATION_SECONDS reading from a live device (MAN-29).
    let calibration_factor = manta_spot::calibration_factor_from_ppm(cfg.freq_correction_ppm)
        .map_err(|e| anyhow::anyhow!(e))?;

    let mut rtty = RttyPublisher::new(observers.rtty.clone(), calibration_factor);

    let fs = src.sample_rate();
    let center_freq_hz = src.center_freq_hz();
    let rf_passband_hz = src.rf_passband_hz();
    // MAN-196: the source's trusted band, read once beside the RF
    // passband and applied to every segment (including the
    // discontinuity rebuild below) -- `None` for every non-decimated
    // source leaves each segment's manager exactly as before.
    let trusted_band_hz = src.trusted_band_hz();

    let mut validator = cfg.validator(fs)?;
    let references = observers
        .reference_lists
        .clone()
        .map(|lists| ReferenceWatch::new(lists, &mut validator));
    // MAN-324: the live tap turns the validator's decision log on per
    // batch, only while someone is subscribed (`emit`).
    let live = observers
        .live_state
        .clone()
        .map(|sink| LiveTap::new(sink, fs, calibration_factor));
    let occupancy = live.is_some();
    let mut ctx = EmitCtx {
        lists: observers.operator_lists.as_deref(),
        quality: observers.decode_quality.as_deref(),
        calibration_factor,
        live,
        references,
    };

    // One relaxed store over a count that is a single filtered pass across
    // the (cap-bounded) track map, once per processed chunk and skipped
    // entirely when no observer is registered -- immaterial against the Pi4
    // CPU budget and against the channelizer + TrackManager work it follows.
    //
    // MAN-122: the published number is `decoding_track_count()`, NOT
    // `active_track_count()`. MAN-45 introduced this gauge against the
    // latter (every entry in `tracks`, unconfirmed CANDIDATEs included);
    // candidates are mostly noise-blip rise crossings that close within
    // `confirm_hops` without ever leasing a decoder, so counting them
    // inflates an operator-facing "is it decoding?" reading with signals
    // nothing is decoding. See `TrackManager::decoding_track_count`'s doc
    // comment for the full argument. `active_track_count` keeps its
    // existing meaning for `soak_metrics`' peak/eviction accounting.
    let report_active_tracks = |n_tracks: usize| {
        if let Some(gauge) = &observers.active_tracks {
            gauge.store(n_tracks as u64, Ordering::Relaxed);
        }
    };

    // MAN-313: each segment has its own `TrackManager`, so its
    // confirmation count restarts at 0; `confirmations_base` holds the
    // closed segments' totals so the published count never goes back.
    let mut confirmations_base: u64 = 0;
    let report_confirmations = |n: u64| {
        if let Some(counter) = &observers.detector_confirmations {
            counter.store(n, Ordering::Relaxed);
        }
    };

    // MAN-312: crashes counted by segments a discontinuity already replaced.
    let mut crash_base: u64 = 0;
    let report_static_crashes = |n: u64| {
        if let Some(counter) = &observers.static_crashes {
            counter.fetch_max(n, Ordering::Relaxed);
        }
    };
    // MAN-234: samples replaced by segments a discontinuity already replaced.
    let mut bad_base: u64 = 0;
    let report_bad_samples = |n: u64| {
        if let Some(counter) = &observers.bad_samples {
            counter.fetch_max(n, Ordering::Relaxed);
        }
    };

    let lifecycle = observers.track_lifecycle.as_deref();
    let mut close_base = CloseCounts::default();

    let mut seg = new_segment(
        fs,
        center_freq_hz,
        rf_passband_hz,
        trusted_band_hz,
        cfg,
        1,
        observers.decode_quality.clone(),
        occupancy,
    )?;
    let mut seg_base: u64 = 0;

    let padding = vec![Complex32::new(0.0, 0.0); seg.ch.filter_len()];

    let calib_n = (fs * CALIBRATION_SECONDS).round() as usize;
    let mut calib = vec![Complex32::new(0.0, 0.0); calib_n];
    let mut filled = 0;
    while filled < calib_n {
        // MAN-122 review round 7: checked before EVERY calibration read, not
        // only once the decode loop below starts. The daemon installs its
        // Ctrl-C handler before it logs the `listening:` banner, so a stop
        // request can land at any point while this two-second buffer fills;
        // without this check it was honoured only once the buffer was full.
        // A read already in flight is not interrupted; it returns with
        // whatever the source yields next, or fails after that source's own
        // stall bound (`IqSource::read`'s implementations in `manta-input`).
        // `break`, not `return`: whatever was read is still processed below
        // and flushed by `finish()`, so a watchdog-bounded caller (`doctor`,
        // `soak`) still analyses every sample it read, and the decode loop's
        // own `stop` check then ends the run without another read. The same
        // holds for an EOF returned because `stop` was set during the read
        // (MAN-252: `manta-cli`'s reconnecting wrapper ends a backoff or
        // reopen that way).
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let n = src.read(&mut calib[filled..])?;
        if n == 0 {
            // MAN-252: a live source's wrapper reports a stop request during
            // an outage as EOF; that is the stop, not a source that ended.
            if stop.load(Ordering::Relaxed) {
                break;
            }
            anyhow::bail!("audio source ended during startup calibration");
        }
        if let Some(gap) = src.take_discontinuity() {
            // A discontinuity during calibration: the pre-gap partial
            // buffer was never processed through the channelizer, so it's
            // simply discarded (not spliced) -- keep the post-gap samples
            // just read, advance the sample clock by the discarded samples
            // plus the gap, and keep filling from there.
            calib.copy_within(filled..filled + n, 0);
            seg_base += filled as u64 + gap;
            filled = n;
            continue;
        }
        filled += n;
    }
    // The startup lead-in padding is processed only once the calibration
    // fill has returned, not before it: its `on_tracks` call is the first
    // per-batch callback, and the daemon reads that first call as "the
    // calibration read has returned" (MAN-122's `ready: decoding` event).
    let events = seg.tm.process_hops(&seg.ch.process(&padding), |m| {
        seg_base + m.saturating_sub(seg.pad_hops) * seg.hop
    });
    emit(
        events,
        &mut validator,
        &mut ctx,
        &mut on_event,
        &mut on_spot,
    );
    let n_tracks = seg.tm.decoding_track_count();
    report_active_tracks(n_tracks);
    report_confirmations(confirmations_base + seg.tm.confirmations());
    report_static_crashes(crash_base + seg.tm.static_crash_count());
    report_bad_samples(bad_base + seg.ch.bad_samples());
    rtty.update(&seg.tm);
    if let Some(obs) = lifecycle {
        obs.publish_counts(
            close_base + seg.tm.close_counts(),
            validator.gate_records_total(),
        );
    }
    on_tracks(n_tracks);
    // `..filled`: the whole buffer unless a stop request cut the fill short.
    let events = seg.tm.process_hops(&seg.ch.process(&calib[..filled]), |m| {
        seg_base + m.saturating_sub(seg.pad_hops) * seg.hop
    });
    emit(
        events,
        &mut validator,
        &mut ctx,
        &mut on_event,
        &mut on_spot,
    );
    let n_tracks = seg.tm.decoding_track_count();
    report_active_tracks(n_tracks);
    report_confirmations(confirmations_base + seg.tm.confirmations());
    report_static_crashes(crash_base + seg.tm.static_crash_count());
    report_bad_samples(bad_base + seg.ch.bad_samples());
    rtty.update(&seg.tm);
    if let Some(obs) = lifecycle {
        obs.publish_counts(
            close_base + seg.tm.close_counts(),
            validator.gate_records_total(),
        );
    }
    on_tracks(n_tracks);
    let mut seg_consumed: u64 = filled as u64;
    if let Some(tap) = ctx.live.as_mut() {
        tap.maybe_tick(&mut seg.tm, seg_base + seg_consumed);
    }

    let mut chunk = vec![Complex32::new(0.0, 0.0); CHUNK_SAMPLES];
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        // A read error must not escape via `?` before the gauge is
        // zeroed. The end-of-stream `on_tracks(0)` below is only reached
        // on a clean EOF or a `stop` request, so a mid-stream failure --
        // an SDR disconnecting, say -- would otherwise leave
        // `manta_active_tracks` (and the status line's `tracks=`) frozen
        // at its last nonzero value for the whole shutdown drain, up to
        // `SHUTDOWN_DRAIN_DEADLINE` (25 s), while the metrics listener is
        // still answering scrapes with decoders that no longer exist.
        // Publish 0 first, then propagate the original error unchanged.
        let n = match src.read(&mut chunk) {
            Ok(n) => n,
            Err(e) => {
                report_active_tracks(0);
                if let Some(obs) = lifecycle {
                    obs.clear_active();
                }
                rtty.restart(seg_base + seg_consumed);
                on_tracks(0);
                return Err(e);
            }
        };
        if n == 0 {
            break;
        }
        if let Some(gap) = src.take_discontinuity() {
            // Not `finish()`: an outage is not an end of signal (see
            // `TrackManager::finish_for_discontinuity`).
            if let Some(obs) = lifecycle {
                obs.observe_peak(seg.tm.active_track_count());
            }
            let finish_events = seg.tm.finish_for_discontinuity();
            emit(
                finish_events,
                &mut validator,
                &mut ctx,
                &mut on_event,
                &mut on_spot,
            );
            let next_id = seg.tm.next_track_id();
            if lifecycle.is_some() {
                close_base = close_base + seg.tm.close_counts();
            }
            confirmations_base += seg.tm.confirmations();
            crash_base += seg.tm.static_crash_count();
            bad_base += seg.ch.bad_samples();
            // The old segment's marks clear where its samples end.
            rtty.restart(seg_base + seg_consumed);
            seg_base += seg_consumed + gap;
            seg_consumed = 0;
            seg = new_segment(
                fs,
                center_freq_hz,
                rf_passband_hz,
                trusted_band_hz,
                cfg,
                next_id,
                observers.decode_quality.clone(),
                occupancy,
            )?;
            let events = seg.tm.process_hops(&seg.ch.process(&padding), |m| {
                seg_base + m.saturating_sub(seg.pad_hops) * seg.hop
            });
            emit(
                events,
                &mut validator,
                &mut ctx,
                &mut on_event,
                &mut on_spot,
            );
        }
        // MAN-128: steady-state chunk latency, timed from here (after the
        // read and after any outage-segment rebuild above, so a one-off
        // `new_segment` does not skew the distribution) to the end of the
        // chunk's processing.
        let t0 = observers
            .decode_latency
            .as_ref()
            .map(|_| std::time::Instant::now());
        let events = seg.tm.process_hops(&seg.ch.process(&chunk[..n]), |m| {
            seg_base + m.saturating_sub(seg.pad_hops) * seg.hop
        });
        emit(
            events,
            &mut validator,
            &mut ctx,
            &mut on_event,
            &mut on_spot,
        );
        let n_tracks = seg.tm.decoding_track_count();
        report_active_tracks(n_tracks);
        report_confirmations(confirmations_base + seg.tm.confirmations());
        report_static_crashes(crash_base + seg.tm.static_crash_count());
        report_bad_samples(bad_base + seg.ch.bad_samples());
        rtty.update(&seg.tm);
        if let Some(obs) = lifecycle {
            obs.publish_counts(
                close_base + seg.tm.close_counts(),
                validator.gate_records_total(),
            );
            obs.publish_chunk(seg.tm.active_track_count());
        }
        on_tracks(n_tracks);
        if let (Some(obs), Some(t0)) = (&observers.decode_latency, t0) {
            obs.observe(t0.elapsed());
        }
        seg_consumed += n as u64;
        // MAN-324: once per stream second, after the chunk's batch.
        if let Some(tap) = ctx.live.as_mut() {
            tap.maybe_tick(&mut seg.tm, seg_base + seg_consumed);
        }
    }
    if let Some(obs) = lifecycle {
        obs.observe_peak(seg.tm.active_track_count());
    }
    // MAN-258: an end of stream with an outage pending (a stop while a live
    // source was down) is not an end of signal either.
    let events = if src.take_discontinuity().is_some() {
        seg.tm.finish_for_discontinuity()
    } else {
        seg.tm.finish()
    };
    emit(
        events,
        &mut validator,
        &mut ctx,
        &mut on_event,
        &mut on_spot,
    );
    report_static_crashes(crash_base + seg.tm.static_crash_count());
    report_bad_samples(bad_base + seg.ch.bad_samples());
    // `finish()` flushes and drops every decoder and closes every remaining
    // track: nothing is being decoded once the stream has ended, so both
    // observers must settle back to 0 rather than be left holding the last
    // live value after a source disconnects or a replay hits EOF.
    report_active_tracks(0);
    if let Some(obs) = lifecycle {
        obs.publish_counts(
            close_base + seg.tm.close_counts(),
            validator.gate_records_total(),
        );
        obs.clear_active();
    }
    report_confirmations(confirmations_base + seg.tm.confirmations());
    rtty.restart(seg_base + seg_consumed);
    on_tracks(0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn lifecycle_close_fields(c: crate::track::CloseCounts) -> [u64; 8] {
        [
            c.unconfirmed,
            c.hang_expired,
            c.silent,
            c.merged,
            c.evicted,
            c.rtty,
            c.not_cw,
            c.unkeyed,
        ]
    }

    #[test]
    fn track_lifecycle_observer_leaves_events_and_spots_unchanged() {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let run = |observers| {
            let mut events = Vec::new();
            let mut spots = Vec::new();
            listen_with_observers(
                Box::new(FixedFreqSource {
                    samples: rendered.samples.clone(),
                    cursor: 0,
                    fs: spec.fs,
                    center_freq_hz: spec.center_freq_hz,
                }),
                &PipelineConfig::default(),
                Arc::new(AtomicBool::new(false)),
                observers,
                |ev| events.push(ev.clone()),
                |spot| spots.push(spot.clone()),
                |_| {},
            )
            .unwrap();
            (events, spots)
        };
        let expected = run(ListenObservers::default());
        let obs = Arc::new(crate::TrackLifecycleObserver::default());
        assert_eq!(
            expected,
            run(ListenObservers {
                track_lifecycle: Some(obs.clone()),
                ..Default::default()
            })
        );
        let snap = obs.snapshot();
        assert!(snap.peak_active_tracks >= 1);
        assert!(snap.gate_records_total >= 1);
        assert!(
            lifecycle_close_fields(snap.close_counts)
                .iter()
                .sum::<u64>()
                >= 1
        );
        assert_eq!(snap.active_tracks, 0);
        let calib_n = (spec.fs * CALIBRATION_SECONDS).round() as usize;
        assert_eq!(
            snap.chunks as usize,
            (rendered.samples.len() - calib_n).div_ceil(CHUNK_SAMPLES)
        );
    }

    #[test]
    fn track_lifecycle_observer_counts_one_chunk_per_steady_state_read() {
        let obs = Arc::new(crate::TrackLifecycleObserver::default());
        listen_with_observers(
            Box::new(FixedFreqSource {
                samples: vec![Complex32::new(0.0, 0.0); 96_000 + 3 * CHUNK_SAMPLES + 100],
                cursor: 0,
                fs: 48_000.0,
                center_freq_hz: 14_000_000.0,
            }),
            &PipelineConfig::default(),
            Arc::new(AtomicBool::new(false)),
            ListenObservers {
                track_lifecycle: Some(obs.clone()),
                ..Default::default()
            },
            |_| {},
            |_| {},
            |_| {},
        )
        .unwrap();
        let snap = obs.snapshot();
        assert_eq!(snap.chunks, 4);
        assert_eq!(snap.peak_active_tracks, 0);
        assert_eq!(snap.close_counts, crate::track::CloseCounts::default());
    }

    #[test]
    fn track_lifecycle_counts_never_go_back_across_a_discontinuity() {
        let src = GapSource::new(steady_carrier_scene(6.0), 96_000.0, 14_000_000.0, 96_000);
        let gap_taken = src.gap_taken.clone();
        let obs = Arc::new(crate::TrackLifecycleObserver::default());
        let mut previous = crate::TrackLifecycleSnapshot::default();
        let mut before_gap = 0;
        let mut largest_active = 0;
        listen_with_observers(
            Box::new(src),
            &PipelineConfig::default(),
            Arc::new(AtomicBool::new(false)),
            ListenObservers {
                track_lifecycle: Some(obs.clone()),
                ..Default::default()
            },
            |_| {},
            |_| {},
            |_| {
                let now = obs.snapshot();
                for (old, new) in lifecycle_close_fields(previous.close_counts)
                    .into_iter()
                    .zip(lifecycle_close_fields(now.close_counts))
                {
                    assert!(new >= old, "close count went back: {previous:?} -> {now:?}");
                }
                assert!(now.gate_records_total >= previous.gate_records_total);
                largest_active = largest_active.max(now.active_tracks);
                assert!(now.peak_active_tracks >= largest_active);
                if !gap_taken.load(Ordering::Relaxed) {
                    before_gap = lifecycle_close_fields(now.close_counts).iter().sum::<u64>();
                }
                previous = now;
            },
        )
        .unwrap();
        assert!(before_gap >= 1, "must close a track before the gap");
        let total = lifecycle_close_fields(obs.snapshot().close_counts)
            .iter()
            .sum::<u64>();
        assert!(
            total > before_gap,
            "post-gap closes must add to {before_gap}, got {total}"
        );
        assert_eq!(obs.snapshot().active_tracks, 0);
    }

    #[test]
    fn listen_spots_a_call_only_an_override_cty_allocates() {
        let mut spec = manta_testkit::vectors::v1();
        spec.duration_s = 30.0;
        spec.signals[0].text = "CQ CQ DE QQ9ZZZ QQ9ZZZ K".into();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let calls = |cfg: &PipelineConfig| {
            let src = Box::new(FixedFreqSource {
                samples: rendered.samples.clone(),
                cursor: 0,
                fs: spec.fs,
                center_freq_hz: spec.center_freq_hz,
            });
            let mut calls = Vec::new();
            listen(
                src,
                cfg,
                Arc::new(AtomicBool::new(false)),
                |_| {},
                |s| calls.push(s.callsign.clone()),
            )
            .unwrap();
            calls
        };
        assert!(!calls(&PipelineConfig::default()).contains(&"QQ9ZZZ".into()));
        let table = manta_spot::cty::Table::parse(&format!(
            "{}Test DXpedition: 14: 27: EU: 50.0: -5.0: 0.0: QQ9:\n QQ9;\n",
            manta_spot::CTY_DAT
        ));
        let cfg = PipelineConfig {
            cty: Some(Arc::new(table)),
            patterns: Some(Arc::new(
                manta_spot::pattern::Table::parse("+QQ9@@@\n").unwrap(),
            )),
            ..Default::default()
        };
        assert!(calls(&cfg).contains(&"QQ9ZZZ".into()));
    }

    // ---- MAN-324: the live tap over real vectors ----

    use crate::live::tests::RecordingSink;
    use crate::live::LiveRecord;

    fn run_vector(
        spec: &manta_testkit::vectors::VectorSpec,
        cfg: &PipelineConfig,
        live: Option<Arc<RecordingSink>>,
        on_event: impl FnMut(&DecoderEvent),
    ) -> Vec<crate::Spot> {
        let rendered = manta_testkit::vectors::render(spec).unwrap();
        let src = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });
        let observers = ListenObservers {
            live_state: live.map(|s| s as Arc<dyn LiveStateSink>),
            ..Default::default()
        };
        let mut spots = Vec::new();
        listen_with_observers(
            src,
            cfg,
            Arc::new(AtomicBool::new(false)),
            observers,
            on_event,
            |s| spots.push(s.clone()),
            |_| {},
        )
        .unwrap();
        spots
    }

    fn v1_30s() -> manta_testkit::vectors::VectorSpec {
        let mut spec = manta_testkit::vectors::v1();
        spec.duration_s = 30.0;
        spec
    }

    #[test]
    fn live_tap_sends_v1_band_state_every_stream_second() {
        let sink = RecordingSink::wanting();
        run_vector(
            &v1_30s(),
            &PipelineConfig::default(),
            Some(sink.clone()),
            |_| {},
        );
        let records = sink.take();
        let bands: Vec<&crate::BandSample> = records
            .iter()
            .filter_map(|r| match r {
                LiveRecord::Bands(b) => Some(b),
                _ => None,
            })
            .collect();
        assert!((29..=30).contains(&bands.len()), "{}", bands.len());
        assert!(bands.iter().all(|b| b.channels.len() == 1024));
        let with_track: Vec<&&crate::BandSample> = bands
            .iter()
            .filter(|b| !b.decoding_track_freqs_hz.is_empty())
            .collect();
        assert!(!with_track.is_empty());
        for b in with_track {
            assert_eq!(
                b.decoding_track_freqs_hz.len(),
                1,
                "{:?}",
                b.decoding_track_freqs_hz
            );
            let f = b.decoding_track_freqs_hz[0];
            assert!((14_000_000.0..14_350_000.0).contains(&f), "{f}");
        }
    }

    #[test]
    fn live_tap_reports_w1aw_text_candidates_and_its_repetition_decision() {
        let sink = RecordingSink::wanting();
        let spots = run_vector(
            &v1_30s(),
            &PipelineConfig::default(),
            Some(sink.clone()),
            |_| {},
        );
        let w1aw = spots
            .iter()
            .find(|s| s.callsign == "W1AW")
            .expect("V1 spots W1AW within 30 s");
        let records = sink.take();
        assert!(records.iter().any(|r| matches!(r,
            LiveRecord::Track(t) if t.text.contains("W1AW")
                && t.candidates.iter().any(|c| c.call == "W1AW"))));
        let decisions: Vec<&manta_spot::Decision> = records
            .iter()
            .filter_map(|r| match r {
                LiveRecord::Decision(d) if d.call == "W1AW" => Some(d),
                _ => None,
            })
            .collect();
        assert!(decisions.len() >= 2, "{decisions:?}");
        assert_eq!(decisions[0].verdict, manta_spot::Verdict::Withheld);
        assert_eq!(decisions[0].reason, Some(manta_spot::Reason::Repetition));
        assert_eq!(decisions[0].reps, Some(1));
        let spotted = decisions
            .iter()
            .find(|d| d.verdict == manta_spot::Verdict::Spotted)
            .expect("a spotted decision");
        assert_eq!(spotted.spot.as_ref(), Some(w1aw));
    }

    #[test]
    fn live_tap_never_changes_spots() {
        let mut v7 = manta_testkit::vectors::v7();
        v7.duration_s = 30.0;
        for spec in [v1_30s(), v7] {
            let cfg = PipelineConfig::default();
            let without = run_vector(&spec, &cfg, None, |_| {});
            let with = run_vector(&spec, &cfg, Some(RecordingSink::wanting()), |_| {});
            assert!(!without.is_empty());
            assert_eq!(without, with);
        }
    }

    #[test]
    fn live_tap_applies_the_frequency_calibration() {
        let cfg = PipelineConfig {
            freq_correction_ppm: 10.0,
            ..Default::default()
        };
        let factor = manta_spot::calibration_factor_from_ppm(10.0).unwrap();
        let sink = RecordingSink::wanting();
        let mut seen_freqs = Vec::new();
        run_vector(&v1_30s(), &cfg, Some(sink.clone()), |ev| match ev {
            DecoderEvent::TrackMeta { freq_hz, .. }
            | DecoderEvent::TrackPromoted { freq_hz, .. } => seen_freqs.push(*freq_hz),
            _ => {}
        });
        let records = sink.take();
        let mut saw_track = false;
        for r in &records {
            match r {
                LiveRecord::Bands(b) => {
                    let n = b.channels.len() as f64;
                    for (i, c) in b.channels.iter().enumerate() {
                        let raw = 14_000_000.0 + (i as f64 - n / 2.0) * 93.75;
                        assert!(
                            (c.freq_hz - raw * factor).abs() < 1e-3,
                            "{} vs {}",
                            c.freq_hz,
                            raw * factor
                        );
                    }
                    for f in &b.decoding_track_freqs_hz {
                        assert!(seen_freqs.iter().any(|s| (s - f).abs() < 93.75), "{f}");
                    }
                }
                LiveRecord::Track(t) => {
                    if let Some(f) = t.freq_hz {
                        saw_track = true;
                        assert!(
                            seen_freqs.contains(&f),
                            "{f} is a calibrated event frequency"
                        );
                    }
                }
                _ => {}
            }
        }
        assert!(saw_track);
    }

    /// A minimal in-memory IqSource for testing, reporting a fixed,
    /// caller-chosen `center_freq_hz` (unlike `AudioIqSource`, which always
    /// reports 0.0) -- this is what proves `listen()` actually reads
    /// `src.center_freq_hz()` instead of hardcoding 0.0.
    struct FixedFreqSource {
        samples: Vec<Complex32>,
        cursor: usize,
        fs: f64,
        center_freq_hz: f64,
    }

    impl manta_input::IqSource for FixedFreqSource {
        fn sample_rate(&self) -> f64 {
            self.fs
        }
        fn center_freq_hz(&self) -> f64 {
            self.center_freq_hz
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            let n = buf.len().min(self.samples.len() - self.cursor);
            buf[..n].copy_from_slice(&self.samples[self.cursor..self.cursor + n]);
            self.cursor += n;
            Ok(n)
        }
    }

    /// MAN-312: a source whose RF passband is narrower than its stream,
    /// like rig audio.
    struct NarrowSource {
        inner: FixedFreqSource,
        rf_passband_hz: (f64, f64),
    }

    impl manta_input::IqSource for NarrowSource {
        fn sample_rate(&self) -> f64 {
            self.inner.sample_rate()
        }
        fn center_freq_hz(&self) -> f64 {
            self.inner.center_freq_hz()
        }
        fn rf_passband_hz(&self) -> (f64, f64) {
            self.rf_passband_hz
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            self.inner.read(buf)
        }
    }

    /// MAN-196: a source that reports a trusted band, as a decimated one
    /// does (`DecimatingSource`). `None` mimics every non-decimated
    /// source (the trait default).
    struct TrustedBandSource {
        inner: FixedFreqSource,
        trusted_band_hz: Option<(f64, f64)>,
    }

    impl manta_input::IqSource for TrustedBandSource {
        fn sample_rate(&self) -> f64 {
            self.inner.sample_rate()
        }
        fn center_freq_hz(&self) -> f64 {
            self.inner.center_freq_hz()
        }
        fn trusted_band_hz(&self) -> Option<(f64, f64)> {
            self.trusted_band_hz
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            self.inner.read(buf)
        }
    }

    /// Runs `listen_with_observers` with a static-crash observer, returning
    /// the value it held at every `on_tracks` call.
    fn static_crash_readings(src: Box<dyn manta_input::IqSource>) -> Vec<u64> {
        let counter = Arc::new(AtomicU64::new(0));
        let mut readings = Vec::new();
        listen_with_observers(
            src,
            &PipelineConfig::default(),
            Arc::new(AtomicBool::new(false)),
            ListenObservers {
                static_crashes: Some(counter.clone()),
                ..Default::default()
            },
            |_| {},
            |_| {},
            |_| readings.push(counter.load(Ordering::Relaxed)),
        )
        .unwrap();
        readings.push(counter.load(Ordering::Relaxed));
        readings
    }

    /// MAN-312: the crash count reaches the observer, and a discontinuity's
    /// fresh segment adds to it rather than restarting it at 0.
    #[test]
    fn listen_publishes_static_crashes_across_a_discontinuity() {
        let fs = 96_000.0;
        let mut iq = manta_testkit::crash::vc1_scene(fs);
        manta_testkit::crash::add_crash(
            &mut iq,
            fs,
            &manta_testkit::crash::vc1_crash(fs, 5.0, 40.0),
        );
        let readings = static_crash_readings(Box::new(GapSource::new(iq, fs, 0.0, 48_000)));
        assert!(readings.windows(2).all(|w| w[0] <= w[1]), "{readings:?}");
        assert!(readings.contains(&1), "{readings:?}");
        assert_eq!(readings.last(), Some(&2), "{readings:?}");
    }

    /// Runs `listen_with_observers` with a bad-sample observer, returning
    /// the value it held at every `on_tracks` call and once more at the end.
    fn bad_sample_readings(src: Box<dyn manta_input::IqSource>) -> Vec<u64> {
        let counter = Arc::new(AtomicU64::new(0));
        let mut readings = Vec::new();
        listen_with_observers(
            src,
            &PipelineConfig::default(),
            Arc::new(AtomicBool::new(false)),
            ListenObservers {
                bad_samples: Some(counter.clone()),
                ..Default::default()
            },
            |_| {},
            |_| {},
            |_| readings.push(counter.load(Ordering::Relaxed)),
        )
        .unwrap();
        readings.push(counter.load(Ordering::Relaxed));
        readings
    }

    fn v1_6s() -> (manta_testkit::vectors::VectorSpec, Vec<Complex32>) {
        let spec = manta_testkit::vectors::VectorSpec {
            duration_s: 6.0,
            ..manta_testkit::vectors::v1()
        };
        let samples = manta_testkit::vectors::render(&spec).unwrap().samples;
        (spec, samples)
    }

    /// MAN-234: the bad-sample count reaches the observer, and a
    /// discontinuity's fresh segment (a fresh channelizer, counting from 0)
    /// adds to it rather than restarting it. `GapSource` serves its buffer
    /// once, reports the gap, then serves it again: a NaN in the first pass
    /// and an `+Inf` at another index in the second are one bad sample per
    /// segment, two in all.
    #[test]
    fn bad_samples_observer_counts_across_a_reconnect_segment() {
        let (spec, mut iq) = v1_6s();
        iq[300_000].re = f32::NAN;
        let src = GapSource::new(iq, spec.fs, spec.center_freq_hz, 48_000);
        // The replay after the gap carries an `+Inf` at another index
        // instead of the NaN, so each segment has exactly one bad sample.
        let mut second = src.samples.clone();
        second[300_000].re = 0.25;
        second[400_000].im = f32::INFINITY;
        let readings = bad_sample_readings(Box::new(SwapOnGapSource {
            inner: src,
            second: Some(second),
        }));
        assert!(readings.windows(2).all(|w| w[0] <= w[1]), "{readings:?}");
        assert!(readings.contains(&1), "{readings:?}");
        assert_eq!(readings.last(), Some(&2), "{readings:?}");
    }

    /// `GapSource`, but the buffer it replays after the gap is `second`.
    struct SwapOnGapSource {
        inner: GapSource,
        second: Option<Vec<Complex32>>,
    }

    impl manta_input::IqSource for SwapOnGapSource {
        fn sample_rate(&self) -> f64 {
            self.inner.fs
        }
        fn center_freq_hz(&self) -> f64 {
            self.inner.center_freq_hz
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            if !self.inner.armed && self.inner.served >= self.inner.arm_at {
                if let Some(second) = self.second.take() {
                    self.inner.samples = second;
                }
            }
            self.inner.read(buf)
        }
        fn take_discontinuity(&mut self) -> Option<u64> {
            manta_input::IqSource::take_discontinuity(&mut self.inner)
        }
    }

    #[test]
    fn bad_samples_observer_stays_zero_on_clean_input() {
        let (spec, iq) = v1_6s();
        let readings = bad_sample_readings(Box::new(GapSource::new(
            iq,
            spec.fs,
            spec.center_freq_hz,
            48_000,
        )));
        assert!(readings.iter().all(|&n| n == 0), "{readings:?}");
    }

    /// MAN-312: a source whose passband holds fewer than
    /// `CRASH_MIN_LIVE_CHANNELS` channels gets an inactive detector.
    #[test]
    fn a_narrow_passband_source_counts_no_static_crash() {
        let fs = 48_000.0;
        let mut iq = manta_testkit::crash::vc1_scene(fs);
        manta_testkit::crash::add_crash(
            &mut iq,
            fs,
            &manta_testkit::crash::vc1_crash(fs, 5.0, 40.0),
        );
        assert!(crate::crash_live_channels(fs, (300.0, 3000.0)) < crate::CRASH_MIN_LIVE_CHANNELS);
        let src = NarrowSource {
            inner: FixedFreqSource {
                samples: iq,
                cursor: 0,
                fs,
                center_freq_hz: 0.0,
            },
            rf_passband_hz: (300.0, 3000.0),
        };
        let readings = static_crash_readings(Box::new(src));
        assert_eq!(readings.last(), Some(&0), "{readings:?}");
    }

    /// MAN-196: `listen` reads the source's trusted band once and
    /// threads it into every segment's track manager -- including the
    /// discontinuity rebuild's fresh segment -- so a real, detectable CW
    /// signal beyond the band never spawns a track, while a source
    /// without a band (`None`, every non-decimated source) spawns as
    /// before.
    #[test]
    fn listen_vetoes_spawns_beyond_the_sources_trusted_band() {
        // +23_500 Hz / SNR 10 dB on a 48 kS/s stream: beyond the
        // 21_724.8 Hz trusted boundary, at the offset/SNR of the measured
        // duplicate-track cell (research section 2). The native-48k
        // control there spawned one track: the signal is real and
        // detectable, so the control below asserting >= 1 promotion pins
        // detectability, not the duplicate itself.
        let mut spec = manta_testkit::vectors::v1();
        spec.fs = 48_000.0;
        spec.duration_s = 20.0;
        spec.signals[0].offset_hz = 23_500.0;
        spec.signals[0].snr_2500_db = 10.0;
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let trusted = manta_dsp::decimate::TRUSTED_HALF_BAND_FRACTION * 48_000.0;

        let promotions = |src: Box<dyn manta_input::IqSource>| {
            let mut n = 0usize;
            listen(
                src,
                &PipelineConfig::default(),
                Arc::new(AtomicBool::new(false)),
                |ev| {
                    if matches!(ev, DecoderEvent::TrackPromoted { .. }) {
                        n += 1;
                    }
                },
                |_| {},
            )
            .unwrap();
            n
        };
        let fixed = || FixedFreqSource {
            samples: rendered.samples.clone(),
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        };

        // Control, no trusted band (the trait default): the signal spawns.
        assert!(
            promotions(Box::new(fixed())) >= 1,
            "native control must detect the signal"
        );

        // With the trusted band: zero promotions over the whole render.
        assert_eq!(
            promotions(Box::new(TrustedBandSource {
                inner: fixed(),
                trusted_band_hz: Some((-trusted, trusted)),
            })),
            0,
            "no track may spawn beyond the trusted band"
        );

        // ...and across a mid-stream discontinuity: the rebuild path
        // applies the same band (read once, applied per segment).
        let half = rendered.samples.len() / 2;
        assert_eq!(
            promotions(Box::new(
                GapSource::new(
                    rendered.samples.clone(),
                    spec.fs,
                    spec.center_freq_hz,
                    48_000
                )
                .arm_after_samples(half)
                .with_trusted_band(Some((-trusted, trusted)))
            )),
            0,
            "the post-discontinuity segment must keep the trusted band"
        );
    }

    /// MAN-73 test double: serves `samples` on a loop. Once the cumulative
    /// number of samples served reaches `arm_at` (default: the full
    /// vector length, i.e. "after exhaustion"), the *next* `read()` call
    /// rewinds to the start of `samples` and arms a one-shot
    /// `take_discontinuity()` of `gap` samples -- simulating a
    /// `ReconnectingSource` that lost the connection, waited out `gap`
    /// worth of wall-clock time, and resumed producing real samples.
    /// `chunk_cap` bounds how many samples a single `read()` call serves,
    /// so a caller reading into a buffer much larger than `chunk_cap`
    /// (e.g. the startup calibration buffer) still sees the gap arm
    /// partway through, not only at a whole-buffer boundary.
    struct GapSource {
        samples: Vec<Complex32>,
        cursor: usize,
        fs: f64,
        center_freq_hz: f64,
        gap: u64,
        arm_at: usize,
        chunk_cap: usize,
        served: usize,
        armed: bool,
        pending_gap: Option<u64>,
        /// MAN-196: the trusted band this double reports (`None` unless
        /// `with_trusted_band` set one).
        trusted_band: Option<(f64, f64)>,
        /// Set once `take_discontinuity()` has handed the gap to `listen()`,
        /// so a test can tell the events `listen()` emits after the
        /// discontinuity from those before it.
        gap_taken: Arc<AtomicBool>,
    }

    impl GapSource {
        fn new(samples: Vec<Complex32>, fs: f64, center_freq_hz: f64, gap_samples: u64) -> Self {
            let arm_at = samples.len();
            GapSource {
                chunk_cap: samples.len().max(1),
                samples,
                cursor: 0,
                fs,
                center_freq_hz,
                gap: gap_samples,
                arm_at,
                served: 0,
                armed: false,
                pending_gap: None,
                trusted_band: None,
                gap_taken: Arc::new(AtomicBool::new(false)),
            }
        }

        /// Arm the gap after `k` cumulative samples served instead of at
        /// exhaustion -- used to simulate a discontinuity arriving
        /// partway through startup calibration.
        fn arm_after_samples(mut self, k: usize) -> Self {
            self.arm_at = k;
            self.chunk_cap = (k / 4).max(64);
            self
        }

        /// MAN-196: report a trusted band, as a decimated source would.
        fn with_trusted_band(mut self, band: Option<(f64, f64)>) -> Self {
            self.trusted_band = band;
            self
        }
    }

    impl manta_input::IqSource for GapSource {
        fn sample_rate(&self) -> f64 {
            self.fs
        }
        fn center_freq_hz(&self) -> f64 {
            self.center_freq_hz
        }
        fn trusted_band_hz(&self) -> Option<(f64, f64)> {
            self.trusted_band
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            if !self.armed && self.served >= self.arm_at {
                self.cursor = 0;
                self.armed = true;
                self.pending_gap = Some(self.gap);
            }
            let avail = self.samples.len() - self.cursor;
            let n = buf.len().min(avail).min(self.chunk_cap);
            buf[..n].copy_from_slice(&self.samples[self.cursor..self.cursor + n]);
            self.cursor += n;
            self.served += n;
            Ok(n)
        }
        fn take_discontinuity(&mut self) -> Option<u64> {
            let gap = self.pending_gap.take();
            if gap.is_some() {
                self.gap_taken.store(true, Ordering::Relaxed);
            }
            gap
        }
    }

    /// MAN-73: a discontinuity reported mid-stream closes the current
    /// segment (every open track gets `TrackClosed`), starts a fresh one
    /// whose sample clock is advanced by the gap, and never re-reads any
    /// pre-gap data -- so no false spot is produced by splicing pre- and
    /// post-gap audio together, track ids are never reused, and spot
    /// timestamps land on the correct (post-gap) side of the clock.
    #[test]
    fn listen_restarts_the_segment_on_a_discontinuity_and_advances_sample_ts() {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let len = rendered.samples.len() as u64;
        let gap = (spec.fs as u64) * 3600; // 1 hour
        let src: Box<dyn manta_input::IqSource> = Box::new(GapSource::new(
            rendered.samples.clone(),
            spec.fs,
            spec.center_freq_hz,
            gap,
        ));

        let stop = Arc::new(AtomicBool::new(false));
        let mut spots = Vec::new();
        let mut track_meta_ids: Vec<(u64, u32)> = Vec::new(); // (approx order, track_id)
        let mut closed_before_first_post_gap_char = true;
        let mut seen_post_gap_char = false;
        let mut closed_ids: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
        let mut order = 0u64;
        manta_engine_listen_with_discontinuity_probe(src, &stop, &mut spots, |ev| {
            order += 1;
            match ev {
                DecoderEvent::TrackMeta { track_id, .. } => {
                    track_meta_ids.push((order, *track_id));
                }
                DecoderEvent::TrackClosed { track_id, .. } => {
                    closed_ids.insert(*track_id);
                }
                DecoderEvent::CharDecoded { track_id, .. } => {
                    if *track_id > 1 && !closed_ids.contains(&1) {
                        // A post-gap track emitted a char before the
                        // pre-gap track (id 1) was ever closed.
                        closed_before_first_post_gap_char = false;
                    }
                    if *track_id > 1 {
                        seen_post_gap_char = true;
                    }
                }
                _ => {}
            }
        })
        .unwrap();
        let _ = stop;

        assert!(!spots.is_empty(), "expected at least one spot");
        assert!(
            spots.iter().all(|s: &crate::Spot| s.callsign == "W1AW"),
            "every spot must be W1AW (no false spot from splicing pre/post-gap audio): {spots:?}"
        );
        assert!(
            spots.iter().any(|s| s.sample_ts < len),
            "expected at least one pre-gap spot, got {spots:?}"
        );
        assert!(
            spots.iter().any(|s| s.sample_ts >= len + gap),
            "expected at least one post-gap spot with sample_ts >= {}, got {spots:?}",
            len + gap
        );

        let pre_gap_ids: std::collections::BTreeSet<u32> = track_meta_ids
            .iter()
            .filter(|(_, id)| *id == 1)
            .map(|(_, id)| *id)
            .collect();
        let post_gap_ids: std::collections::BTreeSet<u32> = track_meta_ids
            .iter()
            .map(|(_, id)| *id)
            .filter(|id| !pre_gap_ids.contains(id))
            .collect();
        assert!(
            !post_gap_ids.is_empty(),
            "expected at least one post-gap track id, got {track_meta_ids:?}"
        );
        assert!(
            post_gap_ids.iter().all(|id| pre_gap_ids.iter().all(|p| id > p)),
            "every post-gap track id must be greater than every pre-gap id: pre={pre_gap_ids:?} post={post_gap_ids:?}"
        );
        assert!(seen_post_gap_char, "expected a post-gap CharDecoded event");
        assert!(
            closed_before_first_post_gap_char,
            "the pre-gap track's TrackClosed must arrive before any post-gap CharDecoded"
        );
    }

    /// MAN-73 (PR #207 review): a transport outage is not an end of
    /// signal. Every track the discontinuity tears down must close as
    /// `Bookkeeping { survivor_track_id: None }`, never `SignalEnded`:
    /// `SignalEnded` tells `manta-spot`'s `Validator` the transmission
    /// ended, so the post-reconnect track's next call utterance of the same
    /// CQ would count as a distinct message, and pending beacons would
    /// resolve early.
    #[test]
    fn a_discontinuity_closes_open_tracks_as_bookkeeping_not_signal_ended() {
        use manta_decode::events::ClosureKind;

        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        // 20 s of V1's looped CQ with the gap armed 12 s in: past startup
        // calibration and mid-transmission, so a track is open at the gap.
        let samples = rendered.samples[..(spec.fs * 20.0) as usize].to_vec();
        let src = GapSource::new(samples, spec.fs, spec.center_freq_hz, spec.fs as u64)
            .arm_after_samples((spec.fs * 12.0) as usize);
        let gap_taken = src.gap_taken.clone();

        let stop = Arc::new(AtomicBool::new(false));
        let mut spots = Vec::new();
        let mut seen_before_gap: std::collections::BTreeSet<u32> = Default::default();
        let mut closed_at_gap: Vec<(u32, ClosureKind)> = Vec::new();
        manta_engine_listen_with_discontinuity_probe(Box::new(src), &stop, &mut spots, |ev| {
            let id = crate::track::event_track_id(ev);
            if !gap_taken.load(Ordering::Relaxed) {
                seen_before_gap.insert(id);
            } else if let DecoderEvent::TrackClosed { track_id, closure } = ev {
                // Only the pre-gap segment's tracks: its `TrackManager` is
                // dropped right after the teardown, so any of its tracks
                // closing after the gap was closed BY the teardown.
                if seen_before_gap.contains(track_id) {
                    closed_at_gap.push((*track_id, *closure));
                }
            }
        })
        .unwrap();

        assert!(
            !closed_at_gap.is_empty(),
            "test setup: a track must still be open when the gap arrives"
        );
        assert!(
            closed_at_gap.iter().all(|(_, closure)| *closure
                == ClosureKind::Bookkeeping {
                    survivor_track_id: None
                }),
            "a discontinuity must close tracks as Bookkeeping, not SignalEnded: {closed_at_gap:?}"
        );
    }

    /// Serves `samples`, then ends the stream (`Ok(0)`). With `gap_at_end`
    /// set, that last read leaves a gap pending, as a `ReconnectingSource`
    /// stopped mid-outage does (MAN-258); without it, a plain end of stream.
    struct EndsWithGapSource {
        samples: Vec<Complex32>,
        cursor: usize,
        fs: f64,
        center_freq_hz: f64,
        gap_at_end: Option<u64>,
        pending_gap: Option<u64>,
        /// Set by the read that returns 0, so a test can tell the
        /// teardown's events from those before it.
        ended: Arc<AtomicBool>,
    }

    impl manta_input::IqSource for EndsWithGapSource {
        fn sample_rate(&self) -> f64 {
            self.fs
        }
        fn center_freq_hz(&self) -> f64 {
            self.center_freq_hz
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            if self.cursor == self.samples.len() {
                self.pending_gap = self.gap_at_end.take();
                self.ended.store(true, Ordering::Relaxed);
                return Ok(0);
            }
            let n = buf.len().min(self.samples.len() - self.cursor);
            buf[..n].copy_from_slice(&self.samples[self.cursor..self.cursor + n]);
            self.cursor += n;
            Ok(n)
        }
        fn take_discontinuity(&mut self) -> Option<u64> {
            self.pending_gap.take()
        }
    }

    /// The closures `listen()` emits at the end of 12 s of V1's CQ, which
    /// is mid-transmission there, ended with `gap_at_end` pending.
    fn closures_at_end_of_stream(
        gap_at_end: Option<u64>,
    ) -> Vec<(u32, manta_decode::events::ClosureKind)> {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let ended = Arc::new(AtomicBool::new(false));
        let src = EndsWithGapSource {
            samples: rendered.samples[..(spec.fs * 12.0) as usize].to_vec(),
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
            gap_at_end,
            pending_gap: None,
            ended: ended.clone(),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let mut spots = Vec::new();
        let mut closed: Vec<(u32, manta_decode::events::ClosureKind)> = Vec::new();
        manta_engine_listen_with_discontinuity_probe(Box::new(src), &stop, &mut spots, |ev| {
            if let DecoderEvent::TrackClosed { track_id, closure } = ev {
                if ended.load(Ordering::Relaxed) {
                    closed.push((*track_id, *closure));
                }
            }
        })
        .unwrap();
        closed
    }

    /// MAN-258: an end of stream reported with a gap pending -- a live
    /// source stopped while it was down -- closes the open tracks with the
    /// outage teardown, as a mid-stream discontinuity does, not as
    /// `SignalEnded`.
    #[test]
    fn an_eof_with_a_pending_gap_closes_tracks_as_bookkeeping() {
        use manta_decode::events::ClosureKind;

        let closed = closures_at_end_of_stream(Some(48_000));
        assert!(
            !closed.is_empty(),
            "test setup: a track must still be open at the end of stream"
        );
        assert!(
            closed.iter().all(|(_, closure)| *closure
                == ClosureKind::Bookkeeping {
                    survivor_track_id: None
                }),
            "an end of stream with a gap pending must close tracks as Bookkeeping: {closed:?}"
        );
    }

    /// The control for the test above: with no gap pending, an end of
    /// stream is still an end of signal.
    #[test]
    fn a_plain_eof_still_closes_tracks_as_signal_ended() {
        use manta_decode::events::ClosureKind;

        let closed = closures_at_end_of_stream(None);
        assert!(
            !closed.is_empty(),
            "test setup: a track must still be open at the end of stream"
        );
        assert!(
            closed
                .iter()
                .all(|(_, closure)| *closure == ClosureKind::SignalEnded),
            "a plain end of stream must close tracks as SignalEnded: {closed:?}"
        );
    }

    /// Thin wrapper around `listen()` used only by the discontinuity tests
    /// above/below, so the on_event closure can observe every event
    /// (including TrackClosed) while still routing spots to a Vec the way
    /// the other tests in this module do.
    fn manta_engine_listen_with_discontinuity_probe(
        src: Box<dyn manta_input::IqSource>,
        stop: &Arc<AtomicBool>,
        spots: &mut Vec<crate::Spot>,
        mut on_event: impl FnMut(&DecoderEvent),
    ) -> Result<()> {
        listen(
            src,
            &PipelineConfig::default(),
            stop.clone(),
            |ev| on_event(ev),
            |spot| spots.push(spot.clone()),
        )
    }

    /// MAN-73: a discontinuity reported *during* startup calibration
    /// discards the pre-gap partial calibration buffer (it's never
    /// processed through the channelizer) rather than splicing it onto
    /// post-gap data, and still advances the sample clock correctly.
    #[test]
    fn listen_handles_a_discontinuity_during_startup_calibration() {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let gap = (spec.fs as u64) * 5; // 5 s
        let arm_after = (spec.fs * 0.5).round() as usize; // 0.5 s into the 2 s calibration fill
        let src: Box<dyn manta_input::IqSource> = Box::new(
            GapSource::new(rendered.samples.clone(), spec.fs, spec.center_freq_hz, gap)
                .arm_after_samples(arm_after),
        );

        let stop = Arc::new(AtomicBool::new(false));
        let mut spots = Vec::new();
        listen(
            src,
            &PipelineConfig::default(),
            stop,
            |_ev| {},
            |spot| spots.push(spot.clone()),
        )
        .unwrap();

        assert!(!spots.is_empty(), "expected at least one spot");
        let min_expected_ts = arm_after as u64 + gap;
        assert!(
            spots.iter().all(|s| s.sample_ts >= min_expected_ts),
            "every spot's sample_ts must be >= {min_expected_ts} (discarded pre-gap calibration \
             samples + the gap), got {spots:?}"
        );
    }

    /// MAN-73: a source that never reports a discontinuity (every source
    /// today, and file replay always) must decode identically across runs
    /// -- this is the regression the golden/determinism suites already
    /// pin globally; this test documents the contract locally for this
    /// module.
    #[test]
    fn listen_without_discontinuity_is_unchanged() {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();

        let run = || {
            let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
                samples: rendered.samples.clone(),
                cursor: 0,
                fs: spec.fs,
                center_freq_hz: spec.center_freq_hz,
            });
            let mut spots = Vec::new();
            listen(
                src,
                &PipelineConfig::default(),
                Arc::new(AtomicBool::new(false)),
                |_ev| {},
                |spot| spots.push((spot.callsign.clone(), spot.sample_ts, spot.track_id)),
            )
            .unwrap();
            spots
        };

        assert_eq!(run(), run(), "identical input must decode identically");
    }

    #[test]
    fn operator_lists_update_hands_over_only_the_newest_offer() {
        assert!(OperatorListsUpdate::default().take().is_none());
        let update = OperatorListsUpdate::default();
        update.offer(OperatorLists {
            allowlist: vec!["A".into()],
            ..OperatorLists::default()
        });
        update.offer(OperatorLists {
            allowlist: vec!["B".into()],
            ..OperatorLists::default()
        });
        assert_eq!(update.take().unwrap().allowlist, vec!["B".to_string()]);
        assert!(update.take().is_none());
    }

    /// Vector v7 through `listen_with_observers`, offering `lists` from
    /// `on_spot` the first time N2BB spots (sample 1,407,232; N1AA follows
    /// at 1,555,968, a later chunk). Returns every spotted callsign.
    fn v7_with_lists_offered_at_n2bb(cfg: &PipelineConfig, lists: OperatorLists) -> Vec<String> {
        let spec = manta_testkit::vectors::v7();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });
        let update = Arc::new(OperatorListsUpdate::default());
        let observers = ListenObservers {
            operator_lists: Some(update.clone()),
            ..ListenObservers::default()
        };
        let mut lists = Some(lists);
        let mut spots = Vec::new();
        listen_with_observers(
            src,
            cfg,
            Arc::new(AtomicBool::new(false)),
            observers,
            |_ev| {},
            |spot| {
                if spot.callsign == "N2BB" {
                    if let Some(l) = lists.take() {
                        update.offer(l);
                    }
                }
                spots.push(spot.callsign.clone());
            },
            |_n| {},
        )
        .unwrap();
        spots
    }

    fn v7_spots(cfg: &PipelineConfig) -> Vec<String> {
        let spec = manta_testkit::vectors::v7();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });
        let mut spots = Vec::new();
        listen(
            src,
            cfg,
            Arc::new(AtomicBool::new(false)),
            |_ev| {},
            |spot| spots.push(spot.callsign.clone()),
        )
        .unwrap();
        spots
    }

    /// Offers replacement lists from inside a read, including the EOF read.
    struct OfferingSource {
        inner: FixedFreqSource,
        update: Arc<OperatorListsUpdate>,
        offer_at: Option<u64>,
        reads: Arc<AtomicU64>,
    }

    impl IqSource for OfferingSource {
        fn sample_rate(&self) -> f64 {
            self.inner.sample_rate()
        }

        fn center_freq_hz(&self) -> f64 {
            self.inner.center_freq_hz()
        }

        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            let k = self.reads.fetch_add(1, Ordering::Relaxed) + 1;
            if self.offer_at == Some(k) {
                self.update.offer(OperatorLists {
                    allowlist: vec!["W1AW".into()],
                    blocklist: Blocklist::parse("W1AW\n"),
                    ..Default::default()
                });
            }
            self.inner.read(buf)
        }
    }

    fn fast_w1aw(
        wpm: f32,
        truncate: Option<usize>,
        offer_at: Option<u64>,
    ) -> (Vec<(String, u64)>, u64) {
        let mut spec = manta_testkit::vectors::v1();
        spec.duration_s = 3.0;
        spec.signals[0].wpm = wpm;
        spec.signals[0].text = "W1AW W1AW W1AW W1AW W1AW".into();
        let mut samples = manta_testkit::vectors::render(&spec).unwrap().samples;
        if let Some(n) = truncate {
            samples.truncate(n);
        }
        let update = Arc::new(OperatorListsUpdate::default());
        let reads = Arc::new(AtomicU64::new(0));
        let src = OfferingSource {
            inner: FixedFreqSource {
                samples,
                cursor: 0,
                fs: spec.fs,
                center_freq_hz: spec.center_freq_hz,
            },
            update: update.clone(),
            offer_at,
            reads: reads.clone(),
        };
        let mut cfg = PipelineConfig {
            allowlist: vec!["W1AW".into()],
            ..Default::default()
        };
        cfg.detector.warmup_hops = 0;
        let mut spots = Vec::new();
        listen_with_observers(
            Box::new(src),
            &cfg,
            Arc::new(AtomicBool::new(false)),
            ListenObservers {
                operator_lists: Some(update),
                ..Default::default()
            },
            |_| {},
            |s| spots.push((s.callsign.clone(), reads.load(Ordering::Relaxed))),
            |_| {},
        )
        .unwrap();
        // warmup_hops = 0 lets W1AW promote on the first real hop. Before
        // MAN-115 this started on the uninitialised -140 dB floor and spawned
        // hundreds of noise tracks; the pre-roll skip and block-median seed
        // removed that, but any stray noise call still depends on the spawn
        // layout, so only W1AW's spots are kept: these tests measure when
        // lists apply to W1AW.
        spots.retain(|(call, _)| call == "W1AW");
        (spots, reads.load(Ordering::Relaxed))
    }

    #[test]
    fn lists_offered_during_calibration_apply_to_the_calibration_buffer() {
        let (control, _) = fast_w1aw(72.0, None, None);
        assert_eq!(control, vec![("W1AW".to_string(), 1)]);
        let (spots, _) = fast_w1aw(72.0, None, Some(1));
        assert!(spots.is_empty(), "got {spots:?}");
    }

    #[test]
    fn lists_offered_during_a_read_apply_to_its_chunk() {
        let (control, _) = fast_w1aw(60.0, None, None);
        let k = control.first().expect("control spots W1AW").1;
        assert!(
            k > 1,
            "spot must come from a steady-state chunk, got {control:?}"
        );
        let (spots, _) = fast_w1aw(60.0, None, Some(k));
        assert!(spots.is_empty(), "got {spots:?}");
    }

    #[test]
    fn lists_offered_during_the_eof_read_apply_to_the_final_flush() {
        let (control, total) = fast_w1aw(60.0, Some(205_000), None);
        assert_eq!(control, vec![("W1AW".to_string(), total)]);
        let (spots, _) = fast_w1aw(60.0, Some(205_000), Some(total));
        assert!(spots.is_empty(), "got {spots:?}");
    }

    /// MAN-316: the bundled country table plus an entity for QQ9, which no
    /// real `cty.dat` allocates.
    fn cty_with_qq9() -> Arc<manta_spot::cty::Table> {
        Arc::new(manta_spot::cty::Table::parse(&format!(
            "{}Test DXpedition: 14: 27: EU: 50.0: -5.0: 0.0: QQ9:\n QQ9;\n",
            manta_spot::CTY_DAT
        )))
    }

    fn bundled_reference_lists() -> Arc<ReferenceLists> {
        Arc::new(ReferenceLists::new(
            Arc::new(manta_spot::cty::Table::bundled()),
            Arc::new(manta_spot::scp::Set::bundled()),
        ))
    }

    /// Stores `cty_with_qq9()` from inside a read, including the EOF read:
    /// `OfferingSource` for a list refresh.
    struct StoringSource {
        inner: FixedFreqSource,
        lists: Arc<ReferenceLists>,
        store_at: Option<u64>,
        reads: Arc<AtomicU64>,
    }

    impl IqSource for StoringSource {
        fn sample_rate(&self) -> f64 {
            self.inner.sample_rate()
        }

        fn center_freq_hz(&self) -> f64 {
            self.inner.center_freq_hz()
        }

        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            let k = self.reads.fetch_add(1, Ordering::Relaxed) + 1;
            if self.store_at == Some(k) {
                self.lists.cty.store(cty_with_qq9());
            }
            self.inner.read(buf)
        }
    }

    /// A V1-style scene sending `text` once at `wpm` for `seconds` (looped
    /// when `loop_text`), through `listen_with_observers` with a
    /// `ReferenceLists` observer that `StoringSource` stores `cty_with_qq9()`
    /// into at read `store_at`. Returns the spots of calls under QQ9 (which
    /// only `cty_with_qq9()` allocates), each with the reads made when it
    /// came out, and the total reads. With `reference_lists` set, the
    /// validator starts from the observer's tables.
    fn qq9_spots(
        seconds: f64,
        wpm: f32,
        text: &str,
        loop_text: bool,
        store_at: Option<u64>,
    ) -> (Vec<(String, u64)>, u64) {
        let mut spec = manta_testkit::vectors::v1();
        spec.duration_s = seconds;
        spec.signals[0].wpm = wpm;
        spec.signals[0].text = text.into();
        spec.signals[0].loop_text = loop_text;
        let samples = manta_testkit::vectors::render(&spec).unwrap().samples;
        let lists = bundled_reference_lists();
        let reads = Arc::new(AtomicU64::new(0));
        let src = StoringSource {
            inner: FixedFreqSource {
                samples,
                cursor: 0,
                fs: spec.fs,
                center_freq_hz: spec.center_freq_hz,
            },
            lists: lists.clone(),
            store_at,
            reads: reads.clone(),
        };
        let mut cfg = PipelineConfig::default();
        cfg.detector.warmup_hops = 0;
        // MAN-95: QQ9 calls' patterns are common, so the repetition gate
        // needs two messages, as these scenes were timed for.
        cfg.patterns = Some(Arc::new(
            manta_spot::pattern::Table::parse("+QQ9@@@\n+QQ9@\n").unwrap(),
        ));
        let mut spots = Vec::new();
        listen_with_observers(
            Box::new(src),
            &cfg,
            Arc::new(AtomicBool::new(false)),
            ListenObservers {
                reference_lists: Some(lists),
                ..Default::default()
            },
            |_| {},
            |s| spots.push((s.callsign.clone(), reads.load(Ordering::Relaxed))),
            |_| {},
        )
        .unwrap();
        spots.retain(|(call, _)| call.starts_with("QQ9"));
        (spots, reads.load(Ordering::Relaxed))
    }

    /// "CQ CQ DE QQ9ZZZ QQ9ZZZ K" looped for 10 s at 60 WPM: two
    /// transmissions, so the repetition gate's second message (word 9) is
    /// decoded in the steady-state loop, about read 360 of 377.
    fn looped_qq9(store_at: Option<u64>) -> (Vec<(String, u64)>, u64) {
        qq9_spots(10.0, 60.0, "CQ CQ DE QQ9ZZZ QQ9ZZZ K", true, store_at)
    }

    #[test]
    fn reference_lists_swapped_mid_stream_apply_to_the_next_event() {
        let (control, _) = looped_qq9(None);
        assert!(
            control.is_empty(),
            "QQ9 is unallocated in the bundled table: {control:?}"
        );
        for k in [2, 40] {
            let (spots, _) = looped_qq9(Some(k));
            assert!(
                spots.iter().any(|(call, _)| call == "QQ9ZZZ"),
                "a store at read {k} must let QQ9ZZZ spot, got {spots:?}"
            );
            assert!(
                spots.iter().all(|(_, read)| *read >= k),
                "no spot may come out before the store at read {k}: {spots:?}"
            );
        }
    }

    /// MAN-78 remediate-9 timing, for a list refresh: "V DE QQ9E K DE QQ9E
    /// K" sent once at 72 WPM puts the first QQ9E in the two-second
    /// calibration buffer (read 1) and the confirming one in the steady
    /// state. A store during the calibration read reaches the calibration
    /// buffer's events, so the first QQ9E counts and the second spots; a
    /// store one read later misses it, leaving one occurrence and no spot.
    #[test]
    fn reference_lists_stored_during_calibration_apply_to_the_calibration_buffer() {
        let scene = |store_at| qq9_spots(4.0, 72.0, "V DE QQ9E K DE QQ9E K", false, store_at);
        let (control, _) = scene(None);
        assert!(control.is_empty(), "got {control:?}");
        let (spots, _) = scene(Some(1));
        assert_eq!(spots.len(), 1, "got {spots:?}");
        assert_eq!(spots[0].0, "QQ9E");
        let (late, _) = scene(Some(2));
        assert!(
            late.is_empty(),
            "a store after the calibration buffer was evaluated must miss its QQ9E, got {late:?}"
        );
    }

    /// V7 through `listen_with_observers`, with `observers`; every spot's
    /// `Debug` rendering, so any field that moved shows.
    fn v7_spot_records(observers: ListenObservers) -> Vec<String> {
        let spec = manta_testkit::vectors::v7();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });
        let mut spots = Vec::new();
        listen_with_observers(
            src,
            &PipelineConfig::default(),
            Arc::new(AtomicBool::new(false)),
            observers,
            |_| {},
            |spot| spots.push(format!("{spot:?}")),
            |_| {},
        )
        .unwrap();
        spots
    }

    #[test]
    fn no_reference_lists_observer_is_byte_identical() {
        let without = v7_spot_records(ListenObservers::default());
        assert!(!without.is_empty(), "V7 must spot something");
        let with = v7_spot_records(ListenObservers {
            reference_lists: Some(bundled_reference_lists()),
            ..Default::default()
        });
        assert_eq!(with, without);
    }

    /// MAN-78: lists offered mid-run reach the live validator before a
    /// later call is evaluated.
    #[test]
    fn lists_offered_mid_run_block_a_later_call() {
        let control = v7_spots(&PipelineConfig::default());
        assert!(
            control.iter().any(|c| c == "N1AA") && control.iter().any(|c| c == "N2BB"),
            "control run must spot both N2BB and N1AA, got {control:?}"
        );
        let spots = v7_with_lists_offered_at_n2bb(
            &PipelineConfig::default(),
            OperatorLists {
                blocklist: Blocklist::parse("N1AA\n"),
                ..OperatorLists::default()
            },
        );
        assert_eq!(spots, vec!["N2BB".to_string()]);
    }

    #[test]
    fn lists_offered_mid_run_lift_a_block() {
        let cfg = PipelineConfig {
            blocklist: Blocklist::parse("N1AA\n"),
            ..PipelineConfig::default()
        };
        assert!(
            !v7_spots(&cfg).iter().any(|c| c == "N1AA"),
            "control: the startup blocklist must hold N1AA back"
        );
        let spots = v7_with_lists_offered_at_n2bb(&cfg, OperatorLists::default());
        assert!(spots.iter().any(|c| c == "N1AA"), "got {spots:?}");
    }

    /// MAN-45 (PR #63 round-9 finding): `manta_active_tracks` reported a
    /// constant 0 on every production run because `listen()` exposed no
    /// live track count to its caller -- the manager's own count existed but
    /// was reachable only from inside the engine. This proves the observer
    /// handle tracks the real count during the run and settles at 0
    /// afterward (`TrackManager::finish()` closes every track). MAN-122: the
    /// number published here is `decoding_track_count()`, so an unconfirmed
    /// noise candidate can never lift it off 0.
    /// MAN-314: a 25 s scene holding one 30 dB, 170 Hz RTTY signal.
    fn rtty_scene() -> Vec<Complex32> {
        let fs = 96_000.0;
        let (mut iq, _) = manta_testkit::scene::render_scene(&[], fs, 25.0, Some(2)).unwrap();
        manta_testkit::fsk::add_fsk(
            &mut iq,
            fs,
            &manta_testkit::fsk::FskSpec {
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
        iq
    }

    #[test]
    fn rtty_observer_publishes_marks_and_clears_on_segment_restart() {
        let src = GapSource::new(rtty_scene(), 96_000.0, 14_070_000.0, 96_000);
        let gap_taken = src.gap_taken.clone();
        let obs = Arc::new(RttyObserver::default());
        let seen = obs.clone();
        let mut marked_before_gap = false;
        let mut first_after_gap: Option<(usize, u64)> = None;
        listen_with_observers(
            Box::new(src),
            &PipelineConfig::default(),
            Arc::new(AtomicBool::new(false)),
            ListenObservers {
                rtty: Some(obs.clone()),
                ..Default::default()
            },
            |_ev| {},
            |_spot| {},
            |_n| {
                let (snap, sample_ts) = seen.snapshot_with_sample_ts();
                if gap_taken.load(Ordering::Relaxed) {
                    first_after_gap.get_or_insert((snap.len(), sample_ts));
                } else if snap.len() == 1 && snap[0].shift_hz == 170.0 {
                    marked_before_gap = true;
                }
            },
        )
        .unwrap();
        assert!(marked_before_gap, "the RTTY signal was never published");
        // Emptied at the end of the pre-gap segment's 25 s of samples.
        assert_eq!(
            first_after_gap,
            Some((0, 2_400_000)),
            "the restart must empty the set, stamped where the segment ended"
        );
        assert!(obs.snapshot().is_empty());
    }

    /// Codex review on PR #272: `run` logs how long a signal was marked
    /// from these timestamps, so both must be stream sample times -- a
    /// file decodes far faster than real time.
    #[test]
    fn rtty_observer_stamps_marks_and_clears_in_stream_samples() {
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rtty_scene(),
            cursor: 0,
            fs: 96_000.0,
            center_freq_hz: 14_070_000.0,
        });
        let obs = Arc::new(RttyObserver::default());
        let seen = obs.clone();
        let mut marked: Option<(u64, u64)> = None;
        let mut cleared_at: Option<u64> = None;
        listen_with_observers(
            src,
            &PipelineConfig::default(),
            Arc::new(AtomicBool::new(false)),
            ListenObservers {
                rtty: Some(obs.clone()),
                ..Default::default()
            },
            |_ev| {},
            |_spot| {},
            |_n| {
                let (snap, sample_ts) = seen.snapshot_with_sample_ts();
                match (snap.first(), marked) {
                    (Some(s), None) => marked = Some((s.marked_sample_ts, sample_ts)),
                    (None, Some(_)) => {
                        cleared_at.get_or_insert(sample_ts);
                    }
                    _ => {}
                }
            },
        )
        .unwrap();
        let (marked_sample_ts, store_ts) = marked.expect("the RTTY signal was never published");
        assert_eq!(
            store_ts, marked_sample_ts,
            "the marking store carries its hop"
        );
        assert!(
            // 20 s dwell, less one 256-sample hop of slack.
            (20.0 * 96_000.0 - 256.0..=22.0 * 96_000.0).contains(&(marked_sample_ts as f64)),
            "{marked_sample_ts}"
        );
        // Still marked at the end of the 25 s file: cleared at its last sample.
        assert_eq!(cleared_at, Some(2_400_000));
    }

    #[test]
    fn rtty_observer_zeroed_on_drop() {
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rtty_scene(),
            cursor: 0,
            fs: 96_000.0,
            center_freq_hz: 14_070_000.0,
        });
        let obs = Arc::new(RttyObserver::default());
        let seen = obs.clone();
        let mut peak = 0;
        listen_with_observers(
            src,
            &PipelineConfig::default(),
            Arc::new(AtomicBool::new(false)),
            ListenObservers {
                rtty: Some(obs.clone()),
                ..Default::default()
            },
            |_ev| {},
            |_spot| {},
            |_n| peak = peak.max(seen.snapshot().len()),
        )
        .unwrap();
        assert_eq!(peak, 1);
        assert!(obs.generation() >= 2, "marked, then cleared");
        assert!(obs.snapshot().is_empty(), "cleared at exit");
    }

    /// MAN-313 test scene: noise plus a steady unkeyed carrier 10 kHz above
    /// centre at +20 dB in 2500 Hz -- confirmed by the detector within
    /// ~50 ms, never keyed CW.
    fn steady_carrier_scene(duration_s: f64) -> Vec<Complex32> {
        let fs = 96_000.0;
        let (mut iq, _) = manta_testkit::scene::render_scene(&[], fs, duration_s, Some(5)).unwrap();
        let noise_pow = iq.iter().map(|z| z.norm_sqr() as f64).sum::<f64>() / iq.len() as f64;
        let amp = (100.0 * noise_pow * 2500.0 / fs).sqrt() as f32;
        let w = 2.0 * std::f64::consts::PI * 10_000.0 / fs;
        for (n, z) in iq.iter_mut().enumerate() {
            let ph = w * n as f64;
            *z += Complex32::new(amp * ph.cos() as f32, amp * ph.sin() as f32);
        }
        iq
    }

    /// MAN-313: the detector-confirmation counter is published while the
    /// stream runs, summed across an outage restart (each segment's
    /// `TrackManager` counts from 0), and never goes back.
    #[test]
    fn listen_publishes_detector_confirmations() {
        let samples = steady_carrier_scene(6.0);
        let src = GapSource::new(samples, 96_000.0, 14_000_000.0, 96_000);
        let gap_taken = src.gap_taken.clone();
        let counter = Arc::new(AtomicU64::new(0));
        let seen = counter.clone();
        let mut last = 0u64;
        let mut before_gap = 0u64;
        listen_with_observers(
            Box::new(src),
            &PipelineConfig::default(),
            Arc::new(AtomicBool::new(false)),
            ListenObservers {
                detector_confirmations: Some(counter.clone()),
                ..Default::default()
            },
            |_ev| {},
            |_spot| {},
            |_n| {
                let now = seen.load(Ordering::Relaxed);
                assert!(now >= last, "the count went back from {last} to {now}");
                last = now;
                if !gap_taken.load(Ordering::Relaxed) {
                    before_gap = now;
                }
            },
        )
        .unwrap();
        let total = counter.load(Ordering::Relaxed);
        assert!(before_gap >= 1, "the carrier is confirmed before the gap");
        assert!(
            total > before_gap,
            "the post-gap segment's confirmation adds to the pre-gap count \
             ({before_gap} before the gap, {total} at the end)"
        );
    }

    #[test]
    fn listen_with_observers_publishes_a_live_active_track_count() {
        use std::sync::atomic::AtomicU64;

        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });

        let gauge = Arc::new(AtomicU64::new(0));
        let observed = gauge.clone();
        let mut peak = 0u64;
        listen_with_observers(
            src,
            &PipelineConfig::default(),
            Arc::new(AtomicBool::new(false)),
            ListenObservers {
                active_tracks: Some(gauge.clone()),
                ..Default::default()
            },
            |_ev| peak = peak.max(observed.load(Ordering::Relaxed)),
            |_spot| {},
            |_n| {},
        )
        .unwrap();

        assert!(
            peak >= 1,
            "V1's single strong signal must show as an active track mid-run"
        );
        assert_eq!(
            gauge.load(Ordering::Relaxed),
            0,
            "finish() closes every track"
        );
    }

    /// MAN-45 (PR #63 round-19 finding, P2): an `IqSource::read` error after
    /// tracks had become active used to return through `?` with the observer
    /// still holding that nonzero count, so a caller polling the handle kept
    /// reporting ghost tracks for a `TrackManager` that no longer existed.
    #[test]
    fn the_active_track_observer_is_cleared_when_listen_exits_with_an_error() {
        use std::sync::atomic::AtomicU64;

        /// Fails its `read` as soon as the observer reports at least one
        /// active track -- exactly the "device disconnected mid-run" shape
        /// the finding describes.
        struct FailsOnceTracksAreActive {
            inner: FixedFreqSource,
            observed: Arc<AtomicU64>,
        }

        impl manta_input::IqSource for FailsOnceTracksAreActive {
            fn sample_rate(&self) -> f64 {
                self.inner.sample_rate()
            }
            fn center_freq_hz(&self) -> f64 {
                self.inner.center_freq_hz()
            }
            fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
                if self.observed.load(Ordering::Relaxed) > 0 {
                    anyhow::bail!("simulated device disconnect");
                }
                self.inner.read(buf)
            }
        }

        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let gauge = Arc::new(AtomicU64::new(0));
        let src: Box<dyn manta_input::IqSource> = Box::new(FailsOnceTracksAreActive {
            inner: FixedFreqSource {
                samples: rendered.samples,
                cursor: 0,
                fs: spec.fs,
                center_freq_hz: spec.center_freq_hz,
            },
            observed: gauge.clone(),
        });

        let err = listen_with_observers(
            src,
            &PipelineConfig::default(),
            Arc::new(AtomicBool::new(false)),
            ListenObservers {
                active_tracks: Some(gauge.clone()),
                ..Default::default()
            },
            |_ev| {},
            |_spot| {},
            |_n| {},
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("simulated device disconnect"),
            "the read error must propagate, not be swallowed: {err}"
        );
        assert_eq!(
            gauge.load(Ordering::Relaxed),
            0,
            "the observer must not keep reporting tracks whose TrackManager is gone"
        );
    }

    /// The default path must stay exactly as cheap as before --
    /// `listen()`'s own signature and behavior are unchanged, and its four
    /// existing call sites (main.rs, soak.rs, two integration tests) do not
    /// move.
    #[test]
    fn plain_listen_still_runs_with_no_observers() {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });

        let stop = Arc::new(AtomicBool::new(false));
        let mut spots = Vec::new();
        listen(
            src,
            &PipelineConfig::default(),
            stop,
            |_ev| {},
            |spot| spots.push(spot.clone()),
        )
        .unwrap();

        assert!(!spots.is_empty(), "V1's repeated W1AW should have spotted");
    }

    #[test]
    fn listen_uses_the_sources_center_freq_hz_not_a_hardcoded_zero() {
        // A real V1-style golden signal (clean +20 dB tone), but fed through
        // a source that reports a nonzero center_freq_hz -- if listen() were
        // still hardcoding 0.0, every TrackMeta.freq_hz would come back as
        // just the +12.34 kHz baseband offset, not centered on 14 MHz.
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });

        let stop = Arc::new(AtomicBool::new(false));
        let mut last_freq_hz = None;
        listen(
            src,
            &PipelineConfig::default(),
            stop,
            |ev| {
                if let DecoderEvent::TrackMeta { freq_hz, .. } = ev {
                    last_freq_hz = Some(*freq_hz);
                }
            },
            |_spot| {},
        )
        .unwrap();

        let freq_hz = last_freq_hz.expect("expected at least one TrackMeta event");
        assert!(
            (freq_hz - (spec.center_freq_hz + 12_340.0)).abs() < 100.0,
            "freq_hz {freq_hz} should be near {} (center_freq_hz + V1's known offset), not near 12340 \
             (which is what a hardcoded center_freq_hz=0.0 would produce)",
            spec.center_freq_hz + 12_340.0
        );
    }

    /// MAN-122 review round 1: the track-count observer reports
    /// `TrackManager`'s promoted-track count, rises above zero while a real
    /// signal is being decoded, and is driven back to zero by `finish()` so
    /// the daemon's gauge doesn't stay stuck at the last live value after
    /// EOF or an SDR disconnect. Review round 2: it also fires once per
    /// processed batch, repeats included -- that per-batch edge is what the
    /// daemon's status line uses to tell a wedged decode loop from a quiet
    /// band.
    #[test]
    fn listen_reports_the_managers_track_count_and_clears_it_at_end_of_stream() {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });

        let stop = Arc::new(AtomicBool::new(false));
        let mut counts: Vec<usize> = Vec::new();
        listen_with_observers(
            src,
            &PipelineConfig::default(),
            stop,
            ListenObservers::default(),
            |_ev| {},
            |_spot| {},
            |n| counts.push(n),
        )
        .unwrap();

        assert!(
            counts.iter().any(|&n| n > 0),
            "V1 is a clean +20 dB tone -- the observer must see a promoted track at some point, got {counts:?}"
        );
        assert_eq!(
            counts.last().copied(),
            Some(0),
            "finish() drops every decoder, so the final reported count must be 0, got {counts:?}"
        );
        // One call per batch: the padding batch, the calibration batch, one
        // per CHUNK_SAMPLES-sized read, and one final zero from finish().
        // Far more calls than there are distinct values -- the point being
        // that a steady count still produces a steady stream of calls.
        assert!(
            counts.len()
                > counts
                    .iter()
                    .collect::<std::collections::HashSet<_>>()
                    .len(),
            "the observer must fire per batch, repeats included, got {counts:?}"
        );
    }

    /// A source that replays `samples` and then FAILS instead of reporting
    /// EOF -- an SDR disconnecting mid-stream, not a file running out.
    struct FailsAtEndSource {
        samples: Vec<Complex32>,
        cursor: usize,
        fs: f64,
        center_freq_hz: f64,
    }

    impl manta_input::IqSource for FailsAtEndSource {
        fn sample_rate(&self) -> f64 {
            self.fs
        }
        fn center_freq_hz(&self) -> f64 {
            self.center_freq_hz
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            if self.cursor >= self.samples.len() {
                anyhow::bail!("source disconnected");
            }
            let n = buf.len().min(self.samples.len() - self.cursor);
            buf[..n].copy_from_slice(&self.samples[self.cursor..self.cursor + n]);
            self.cursor += n;
            Ok(n)
        }
    }

    /// MAN-122 review round 3: a mid-stream `IqSource::read` failure exits
    /// `listen_with_observers` before the end-of-stream `on_tracks(0)`,
    /// so without an explicit zero on the error path the daemon's
    /// `manta_active_tracks` gauge (and the status line's `tracks=`) would
    /// stay frozen at its last nonzero value for the whole shutdown drain
    /// while the metrics listener still answers scrapes.
    #[test]
    fn listen_zeroes_the_track_count_when_a_read_fails_mid_stream() {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FailsAtEndSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });

        let stop = Arc::new(AtomicBool::new(false));
        let mut counts: Vec<usize> = Vec::new();
        let err = listen_with_observers(
            src,
            &PipelineConfig::default(),
            stop,
            ListenObservers::default(),
            |_ev| {},
            |_spot| {},
            |n| counts.push(n),
        )
        .expect_err("the source fails instead of reaching EOF, so listen must propagate the error");
        assert!(
            err.to_string().contains("source disconnected"),
            "the original read error must be propagated unchanged, got {err}"
        );

        assert!(
            counts.iter().any(|&n| n > 0),
            "V1 is a clean +20 dB tone -- a track must have been promoted before the failure, got {counts:?}"
        );
        assert_eq!(
            counts.last().copied(),
            Some(0),
            "a failed read must publish 0 before propagating, or the gauge stays frozen through shutdown drain, got {counts:?}"
        );
    }

    /// A live-shaped source: short reads that never reach EOF, and a Ctrl-C
    /// that lands during its first read -- i.e. while `listen` is still
    /// filling its startup calibration buffer.
    struct StopsDuringCalibrationSource {
        stop: Arc<AtomicBool>,
        reads: Arc<AtomicU64>,
    }

    impl manta_input::IqSource for StopsDuringCalibrationSource {
        fn sample_rate(&self) -> f64 {
            48_000.0
        }
        fn center_freq_hz(&self) -> f64 {
            14_000_000.0
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            self.stop.store(true, Ordering::Relaxed);
            let n = buf.len().min(64);
            buf[..n].fill(Complex32::new(0.0, 0.0));
            Ok(n)
        }
    }

    /// Runs `listen_with_observers` against `StopsDuringCalibrationSource`
    /// with `stop` initially `stop_before_start`, returning how many reads
    /// it issued and every `on_tracks` value.
    fn run_stopping_during_calibration(stop_before_start: bool) -> (u64, Vec<usize>) {
        let stop = Arc::new(AtomicBool::new(stop_before_start));
        let reads = Arc::new(AtomicU64::new(0));
        let src: Box<dyn manta_input::IqSource> = Box::new(StopsDuringCalibrationSource {
            stop: stop.clone(),
            reads: reads.clone(),
        });
        let mut counts = Vec::new();
        listen_with_observers(
            src,
            &PipelineConfig::default(),
            stop,
            ListenObservers::default(),
            |_ev| {},
            |_spot| {},
            |n| counts.push(n),
        )
        .expect("a stop request during calibration is a clean shutdown, not an error");
        (reads.load(Ordering::Relaxed), counts)
    }

    /// MAN-122 review round 7: the daemon installs its Ctrl-C handler
    /// before it logs the `listening:` banner, so a signal can land while
    /// `listen` is still filling the two-second calibration buffer. The
    /// calibration loop must check `stop` before each read, not keep
    /// reading until the buffer is full and only check `stop` once the
    /// decode loop starts. What was read is still processed and flushed,
    /// so the run ends through `finish()` and settles the count at 0.
    #[test]
    fn listen_stops_during_startup_calibration_without_filling_the_buffer() {
        let (reads, counts) = run_stopping_during_calibration(false);
        assert_eq!(
            reads, 1,
            "listen must not issue another read once stop is set during calibration"
        );
        assert_eq!(
            counts.last().copied(),
            Some(0),
            "the stopped run must still end through finish(), got {counts:?}"
        );
    }

    /// The same, for a signal that lands before `listen` starts at all --
    /// e.g. at the daemon's `listening:` banner: no read is issued, so a
    /// source that would block cannot hold up shutdown.
    #[test]
    fn listen_issues_no_read_when_stop_is_already_set() {
        let (reads, counts) = run_stopping_during_calibration(true);
        assert_eq!(reads, 0, "stop was set before listen started");
        assert_eq!(
            counts.last().copied(),
            Some(0),
            "the stopped run must still end through finish(), got {counts:?}"
        );
    }

    /// A live-shaped source that returns one short read, then EOF on its
    /// second -- setting `stop` first when `stops_at_eof`, as `manta-cli`'s
    /// reconnecting wrapper does when a stop request lands mid-outage.
    struct EndsDuringCalibrationSource {
        stop: Arc<AtomicBool>,
        stops_at_eof: bool,
        reads: u64,
    }

    impl manta_input::IqSource for EndsDuringCalibrationSource {
        fn sample_rate(&self) -> f64 {
            48_000.0
        }
        fn center_freq_hz(&self) -> f64 {
            14_000_000.0
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            self.reads += 1;
            if self.reads == 1 {
                let n = buf.len().min(64);
                buf[..n].fill(Complex32::new(0.0, 0.0));
                return Ok(n);
            }
            if self.stops_at_eof {
                self.stop.store(true, Ordering::Relaxed);
            }
            Ok(0)
        }
    }

    /// Runs `listen_with_observers` against `EndsDuringCalibrationSource`,
    /// returning its result and every `on_tracks` value.
    fn run_ending_during_calibration(stops_at_eof: bool) -> (Result<()>, Vec<usize>) {
        let stop = Arc::new(AtomicBool::new(false));
        let src: Box<dyn manta_input::IqSource> = Box::new(EndsDuringCalibrationSource {
            stop: stop.clone(),
            stops_at_eof,
            reads: 0,
        });
        let mut counts = Vec::new();
        let result = listen_with_observers(
            src,
            &PipelineConfig::default(),
            stop,
            ListenObservers::default(),
            |_ev| {},
            |_spot| {},
            |n| counts.push(n),
        );
        (result, counts)
    }

    /// MAN-252: `ReconnectingSource` reports a stop request mid-outage as
    /// EOF. During calibration that EOF is the stop, not a source that
    /// ended: the run ends cleanly through `finish()`.
    #[test]
    fn listen_treats_eof_after_a_stop_request_during_calibration_as_a_clean_end() {
        let (result, counts) = run_ending_during_calibration(true);
        result.expect("an EOF that follows a stop request is a clean shutdown, not an error");
        assert_eq!(
            counts.last().copied(),
            Some(0),
            "the stopped run must still end through finish(), got {counts:?}"
        );
    }

    /// The control: an EOF during calibration with no stop request is still
    /// the `ended during startup calibration` error (a too-short file).
    #[test]
    fn listen_still_fails_when_a_source_ends_during_calibration_unasked() {
        let (result, _counts) = run_ending_during_calibration(false);
        let err = result.expect_err("an unasked EOF during calibration is an error");
        assert!(
            err.to_string()
                .contains("audio source ended during startup calibration"),
            "got: {err}"
        );
    }

    /// MAN-29: `PipelineConfig::freq_correction_ppm` reaches the emitted
    /// spot's `freq_hz`, corrected by the configured ppm -- end-to-end
    /// through `listen()`, not just the `manta-spot::Validator` unit.
    #[test]
    fn listen_applies_freq_correction_ppm_to_emitted_spot_freq_hz() {
        const PPM: f64 = 10.0; // ~140 Hz at 14 MHz.
        let factor = 1.0 + PPM * 1e-6;

        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });

        let cfg = PipelineConfig {
            freq_correction_ppm: PPM,
            ..Default::default()
        };

        let stop = Arc::new(AtomicBool::new(false));
        let mut spots = Vec::new();
        listen(src, &cfg, stop, |_ev| {}, |spot| spots.push(spot.clone())).unwrap();

        assert!(!spots.is_empty(), "V1's repeated W1AW should have spotted");
        for spot in &spots {
            let uncorrected = spot.freq_hz / factor;
            assert!(
                (uncorrected - (spec.center_freq_hz + 12_340.0)).abs() < 100.0,
                "spot.freq_hz {} divided back by the calibration factor should land near the \
                 raw decoded frequency {}, proving the correction was applied once, multiplicatively",
                spot.freq_hz,
                spec.center_freq_hz + 12_340.0
            );
        }
    }

    /// MAN-29 review round 3: the `TrackMeta` events `listen()` passes to
    /// `on_event` (consumed directly by `listen --json`) must be
    /// calibrated too, not just the emitted spots.
    #[test]
    fn listen_calibrates_track_meta_events_passed_to_on_event() {
        const PPM: f64 = 10.0;
        let factor = 1.0 + PPM * 1e-6;

        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });
        let cfg = PipelineConfig {
            freq_correction_ppm: PPM,
            ..Default::default()
        };
        let stop = Arc::new(AtomicBool::new(false));
        let mut last_freq_hz = None;
        listen(
            src,
            &cfg,
            stop,
            |ev| {
                if let DecoderEvent::TrackMeta { freq_hz, .. } = ev {
                    last_freq_hz = Some(*freq_hz);
                }
            },
            |_spot| {},
        )
        .unwrap();

        let freq_hz = last_freq_hz.expect("expected at least one TrackMeta event");
        let uncorrected = freq_hz / factor;
        assert!(
            (uncorrected - (spec.center_freq_hz + 12_340.0)).abs() < 100.0,
            "on_event's TrackMeta.freq_hz {freq_hz} divided back by the calibration factor \
             should land near the raw decoded frequency {}",
            spec.center_freq_hz + 12_340.0
        );
    }

    /// MAN-29 review: an invalid `freq_correction_ppm` must fail `listen()`
    /// up front rather than silently poisoning spot output.
    #[test]
    fn listen_rejects_an_invalid_freq_correction_ppm() {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });
        let cfg = PipelineConfig {
            freq_correction_ppm: f64::NAN,
            ..Default::default()
        };
        let stop = Arc::new(AtomicBool::new(false));
        assert!(listen(src, &cfg, stop, |_ev| {}, |_spot| {}).is_err());
    }

    /// MAN-31: `listen()` is the other production call site that must
    /// apply an operator-supplied suppression list.
    #[test]
    fn listen_suppresses_a_blocklisted_callsign() {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });

        let cfg = PipelineConfig {
            blocklist: manta_spot::Blocklist::parse("W1AW\n"),
            ..Default::default()
        };
        let mut spots = Vec::new();
        listen(
            src,
            &cfg,
            Arc::new(AtomicBool::new(false)),
            |_ev| {},
            |spot| spots.push(spot.clone()),
        )
        .unwrap();

        assert!(
            spots.is_empty(),
            "blocklisted callsign must never be spotted, got {spots:?}"
        );
    }

    /// MAN-128: one latency sample per steady-state chunk, none for
    /// calibration/padding. `N = calib_n + 3*CHUNK_SAMPLES + 100` reads as
    /// 2048, 2048, 2048, 100 (then EOF) -- 4 steady-state chunks.
    #[test]
    fn listen_with_observers_records_one_latency_sample_per_steady_state_chunk() {
        use crate::latency::DecodeLatencyObserver;

        let fs = 48_000.0;
        let calib_n = (fs * CALIBRATION_SECONDS).round() as usize;
        let n_samples = calib_n + 3 * CHUNK_SAMPLES + 100;
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: vec![Complex32::new(0.0, 0.0); n_samples],
            cursor: 0,
            fs,
            center_freq_hz: 14_000_000.0,
        });

        let obs = Arc::new(DecodeLatencyObserver::new());
        listen_with_observers(
            src,
            &PipelineConfig::default(),
            Arc::new(AtomicBool::new(false)),
            ListenObservers {
                decode_latency: Some(obs.clone()),
                ..Default::default()
            },
            |_ev| {},
            |_spot| {},
            |_n| {},
        )
        .unwrap();

        assert_eq!(obs.snapshot().count, 4);
    }

    /// MAN-118: V1 through `listen_with_observers` with only the
    /// decode-quality observer set publishes every signal, and leaves the
    /// spot output byte-for-byte what a run without it produces.
    #[test]
    fn listen_with_observers_publishes_decode_quality_without_changing_spots() {
        use crate::quality::DecodeQualityObserver;

        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let run = |observers: ListenObservers| {
            let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
                samples: rendered.samples.clone(),
                cursor: 0,
                fs: spec.fs,
                center_freq_hz: spec.center_freq_hz,
            });
            let mut chars_seen = 0u64;
            let mut spots = Vec::new();
            listen_with_observers(
                src,
                &PipelineConfig::default(),
                Arc::new(AtomicBool::new(false)),
                observers,
                |ev| {
                    if matches!(ev, DecoderEvent::CharDecoded { .. }) {
                        chars_seen += 1;
                    }
                },
                |spot| spots.push(spot.clone()),
                |_n| {},
            )
            .unwrap();
            (chars_seen, spots)
        };

        let obs = Arc::new(DecodeQualityObserver::new());
        let (chars_seen, spots_observed) = run(ListenObservers {
            decode_quality: Some(obs.clone()),
            ..Default::default()
        });
        let snap = obs.snapshot();
        assert!(chars_seen > 0, "V1 must decode characters");
        assert_eq!(snap.chars_total, chars_seen);
        assert!(
            snap.rail_ratio_count >= 100,
            "V1's ~119 track-seconds give one rail sample each, got {}",
            snap.rail_ratio_count
        );
        assert_eq!(
            snap.rail_ratio_bucket_counts[0], 0,
            "V1's clean keying never sits at the collapse floor"
        );
        assert!(
            snap.repetition_bucket_counts.iter().sum::<u64>() >= 1,
            "V1's repeated call must reach the repetition gate"
        );

        let (_, spots_plain) = run(ListenObservers::default());
        assert!(!spots_plain.is_empty(), "V1 must spot");
        assert_eq!(
            spots_observed, spots_plain,
            "the observer must not change decode output"
        );
    }

    /// The default path must stay exactly as cheap as before: no observer
    /// means no `Instant::now()` call and no observation recorded.
    #[test]
    fn plain_listen_records_nothing() {
        let spec = manta_testkit::vectors::v1();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });

        listen(
            src,
            &PipelineConfig::default(),
            Arc::new(AtomicBool::new(false)),
            |_ev| {},
            |_spot| {},
        )
        .unwrap();
        // No observer was supplied -- nothing to assert beyond "this
        // compiles and runs with ListenObservers::default()", which is the
        // whole point: `listen()`'s existing callers pay nothing new.
    }
}
