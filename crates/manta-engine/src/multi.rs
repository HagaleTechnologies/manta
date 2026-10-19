//! MAN-13: one daemon, several inputs, one validator.
//!
//! Each input (an SDR, or one receiver channel of an SDR) runs its own front
//! end on its own OS thread -- source read, channelizer, `TrackManager`,
//! decoder-pool drain -- exactly as `listen_with_observers` does for one
//! input (`listen::run_input_front`). Every front end hands its decode
//! batches to one back end on the caller's thread, which owns the only
//! `Validator`, so the repetition gate and the 10-minute dedupe see every
//! input's reports and a station heard by two inputs is spotted once
//! (`manta_spot::dedupe`, D6).
//!
//! - **Common clock (D2).** The session runs at the highest input rate F;
//!   every channelizer rate is 93.75 Hz × 2^k, so input *i*'s `sample_ts`
//!   maps exactly to `offset_i + sample_ts × F / fs_i` ([`SourceClock`]).
//! - **Track ids (D3).** Input *i* of N numbers its tracks `1+i, 1+i+N, ...`
//!   ([`TrackIdSpace`]), so ids stay unique across inputs and the validator
//!   recovers a track's input as `(id − 1) mod N`.
//! - **Calibration (D4).** Each front end calibrates its own frequencies;
//!   the shared validator is built with factor 1.0.
//! - **Merge order (D5).** Replay merges whole batches in `(batch end time,
//!   input index)` order, a pure function of the inputs; live sessions take
//!   batches in arrival order so no input waits on another.
//! - **Failure isolation (D7).** An input that errors, panics or ends stops
//!   only its own thread; the others keep decoding.

use crate::latency::DecodeLatencyObserver;
use crate::listen::{run_input_front, Batch, FrontObservers, OperatorListsUpdate, RttyObserver};
use crate::live::LiveStateSink;
use crate::quality::DecodeQualityObserver;
use crate::PipelineConfig;
use anyhow::{anyhow, bail, Result};
use manta_decode::events::DecoderEvent;
use manta_input::IqSource;
use manta_spot::reference::ReferenceLists;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::mpsc::{self, Receiver, RecvError, SyncSender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// The most inputs one daemon runs (`[[input]]` entries).
pub const MAX_INPUTS: usize = 16;

/// Batches a replayed input may queue ahead of the merge.
const REPLAY_QUEUE_BATCHES: usize = 64;
/// Batches every live input together may queue ahead of the back end.
const LIVE_QUEUE_BATCHES: usize = 4096;

/// Maps one input's sample timestamps onto the session's common clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceClock {
    /// Common-clock samples per input sample (F / fs, a power of two).
    pub scale: u64,
    /// Common-clock timestamp of the input's sample 0.
    pub offset: u64,
}

impl SourceClock {
    /// One input: every timestamp maps to itself.
    pub const IDENTITY: Self = Self {
        scale: 1,
        offset: 0,
    };

    /// `local` on the common clock.
    pub fn to_common(self, local: u64) -> u64 {
        self.offset.saturating_add(local.saturating_mul(self.scale))
    }
}

/// The session's common rate: the highest input rate. Every other rate
/// must be that rate divided by a power of two, which holds for every rate
/// the channelizer accepts (93.75 Hz × 2^k).
pub fn common_rate_hz(rates: &[f64]) -> Result<f64> {
    let max = rates.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if rates.is_empty() || !max.is_finite() || max <= 0.0 {
        bail!("no input sample rate to build a common clock from");
    }
    for &rate in rates {
        let ratio = max / rate;
        let whole = ratio.round();
        if !(rate > 0.0
            && (ratio - whole).abs() < 1e-9
            && whole >= 1.0
            && (whole as u64).is_power_of_two())
        {
            bail!(
                "input sample rate {rate} Hz is not {max} Hz divided by a power of two, \
                 so its timestamps cannot share one clock with the other inputs"
            );
        }
    }
    Ok(max)
}

/// Input `index` of `count`: track ids `first`, `first + stride`, ...
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackIdSpace {
    pub first: u32,
    pub stride: u32,
}

impl TrackIdSpace {
    /// One input: ids 1, 2, 3, ... (today's numbering).
    pub const SINGLE: Self = Self {
        first: 1,
        stride: 1,
    };

    /// Input `index` (from 0) of `count` inputs.
    pub fn for_input(index: usize, count: usize) -> Self {
        Self {
            first: 1 + index as u32,
            stride: count.max(1) as u32,
        }
    }
}

/// What one input's front end publishes from its own thread.
#[derive(Clone, Default)]
pub struct InputObservers {
    /// Tracks holding a decoder on this input (the caller sums inputs).
    pub active_tracks: Option<Arc<AtomicU64>>,
    /// Static crashes on this input so far (the caller sums inputs).
    pub static_crashes: Option<Arc<AtomicU64>>,
    /// Input samples replaced as unusable on this input (MAN-234).
    pub bad_samples: Option<Arc<AtomicU64>>,
    /// This input's marked RTTY signals.
    pub rtty: Option<Arc<RttyObserver>>,
}

/// Opens one input's source on the input's own thread (`IqSource` is not
/// `Send`); returns the fully wrapped source.
pub type InputOpener = Box<dyn FnOnce() -> Result<Box<dyn IqSource>> + Send>;

/// One input to open.
pub struct InputSpec {
    /// The input's `[[input]]` name: thread name, log and error text.
    pub name: String,
    pub open: InputOpener,
    /// This input's receiver correction (D4).
    pub freq_correction_ppm: f64,
    pub observers: InputObservers,
}

/// What an input reported once open.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenedInput {
    pub name: String,
    pub sample_rate_hz: f64,
    pub center_freq_hz: f64,
    pub rf_passband_hz: (f64, f64),
    /// The source's trusted band, `None` when it is not decimated: an
    /// input whose `[[input]]` sets `capture_rate_hz` delivers a narrower
    /// usable passband than its rate suggests (MAN-196), and `run` says
    /// so per input.
    pub trusted_band_hz: Option<(f64, f64)>,
}

/// How a session's inputs share one clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionClock {
    /// File replay: every input starts at the session epoch, and batches
    /// merge deterministically.
    Replay,
    /// Live receivers: each input's open-completion time after the session
    /// epoch (one per input, in input order); batches merge in arrival
    /// order.
    Live { offsets: Vec<Duration> },
}

/// How one input ended.
#[derive(Debug)]
pub struct InputEnd {
    pub index: usize,
    pub name: String,
    pub result: Result<()>,
}

/// Observers every input shares, read on the back end's thread (the
/// decode-latency and decode-quality ones from every input's thread).
#[derive(Clone, Default)]
pub struct SharedObservers {
    pub decode_latency: Option<Arc<DecodeLatencyObserver>>,
    pub decode_quality: Option<Arc<DecodeQualityObserver>>,
    pub operator_lists: Option<Arc<OperatorListsUpdate>>,
    pub reference_lists: Option<Arc<ReferenceLists>>,
    pub live_state: Option<Arc<dyn LiveStateSink>>,
    /// MAN-14: the shared validator's per-input cross-input dedupe
    /// withholds, updated after each batch. `None` skips the copy.
    pub suppressions: Option<Arc<crate::SuppressionObserver>>,
}

enum Msg {
    Batch(Batch),
    End(Result<()>),
}

enum Outbox {
    Replay(SyncSender<Msg>),
    Live(usize, SyncSender<(usize, Msg)>),
}

impl Outbox {
    fn send(&self, msg: Msg) -> bool {
        match self {
            Outbox::Replay(tx) => tx.send(msg).is_ok(),
            Outbox::Live(index, tx) => tx.send((*index, msg)).is_ok(),
        }
    }
}

struct Go {
    cfg: PipelineConfig,
    clock: SourceClock,
    ids: TrackIdSpace,
    factor: f64,
    observers: FrontObservers,
    outbox: Outbox,
}

struct Parked {
    go: SyncSender<Go>,
    handle: JoinHandle<()>,
}

/// Every input opened and parked on its own thread, waiting for
/// [`listen_inputs`].
pub struct OpenedInputs {
    inputs: Vec<OpenedInput>,
    ppm: Vec<f64>,
    observers: Vec<InputObservers>,
    parked: Vec<Parked>,
    common_rate_hz: f64,
}

impl OpenedInputs {
    pub fn inputs(&self) -> &[OpenedInput] {
        &self.inputs
    }

    /// The session's common rate (the highest input rate): spot and event
    /// `sample_ts` from [`listen_inputs`] count samples at this rate.
    pub fn common_rate_hz(&self) -> f64 {
        self.common_rate_hz
    }
}

impl Drop for OpenedInputs {
    fn drop(&mut self) {
        // Never started: dropping each go-sender makes its thread drop the
        // source and exit.
        for p in self.parked.drain(..) {
            drop(p.go);
            let _ = p.handle.join();
        }
    }
}

/// Opens every input in parallel, each on its own thread named
/// `manta-input-<name>`, and fails fast if any open fails: the first
/// failure to arrive is returned naming the input, without waiting for
/// any still-running opener. Every other input's thread is released
/// before returning -- one that has finished opening is joined; one still
/// inside its open is detached, because an opener blocked in a connect
/// cannot be cancelled from outside and must not hold the error hostage.
pub fn open_inputs(specs: Vec<InputSpec>, stop: Arc<AtomicBool>) -> Result<OpenedInputs> {
    if specs.is_empty() {
        bail!("no input to open");
    }
    if specs.len() > MAX_INPUTS {
        bail!(
            "at most {MAX_INPUTS} inputs are supported, found {}",
            specs.len()
        );
    }
    let n = specs.len();
    let names: Vec<String> = specs.iter().map(|s| s.name.clone()).collect();
    let mut pending = Vec::with_capacity(n);
    let mut ppm = Vec::with_capacity(n);
    let mut observers = Vec::with_capacity(n);
    // One shared completion channel, so the first failure is observed the
    // moment it arrives instead of after every earlier opener has returned
    // (a per-input `recv()` in config order would hide it behind a hung
    // opener indefinitely).
    let (done_tx, done_rx) = mpsc::channel::<(usize, Result<OpenedInput>)>();
    for (index, spec) in specs.into_iter().enumerate() {
        let (go_tx, go_rx) = mpsc::sync_channel::<Go>(1);
        let done_tx = done_tx.clone();
        let name = spec.name.clone();
        let open = spec.open;
        let stop = stop.clone();
        let handle = std::thread::Builder::new()
            .name(format!("manta-input-{name}"))
            .spawn(move || input_thread(name, open, index, done_tx, go_rx, stop))
            .map_err(|e| anyhow!("input \"{}\": cannot start its thread: {e}", spec.name))?;
        ppm.push(spec.freq_correction_ppm);
        observers.push(spec.observers);
        pending.push(Parked { go: go_tx, handle });
    }
    // Only the input threads hold senders now, so a thread that ends
    // without reporting disconnects the channel instead of leaving the
    // `recv()` below waiting forever.
    drop(done_tx);
    let mut opened = OpenedInputs {
        inputs: Vec::new(),
        ppm,
        observers,
        parked: pending,
        common_rate_hz: 0.0,
    };
    let mut slots: Vec<Option<OpenedInput>> = (0..n).map(|_| None).collect();
    let mut reported = vec![false; n];
    let mut first_error = None;
    for _ in 0..n {
        let (index, result) = match done_rx.recv() {
            Ok(done) => done,
            Err(_) => {
                first_error = Some(anyhow!("an input thread ended while opening"));
                break;
            }
        };
        reported[index] = true;
        match result {
            Ok(input) => slots[index] = Some(input),
            Err(e) => {
                first_error = Some(anyhow!("input \"{}\" failed to open: {e:#}", names[index]));
                break;
            }
        }
    }
    if let Some(e) = first_error {
        // Release every thread: dropping its go-sender makes one that
        // finished opening drop its source and exit, so it can be joined.
        // One still inside `open` is detached -- joining it would wait on
        // the very opener the first failure must not sit behind.
        for (reported, p) in reported.into_iter().zip(opened.parked.drain(..)) {
            drop(p.go);
            if reported {
                let _ = p.handle.join();
            }
        }
        return Err(e);
    }
    opened.inputs = slots
        .into_iter()
        .map(|s| s.expect("every input reported its open"))
        .collect();
    let rates: Vec<f64> = opened.inputs.iter().map(|i| i.sample_rate_hz).collect();
    opened.common_rate_hz = common_rate_hz(&rates)?;
    Ok(opened)
}

fn input_thread(
    name: String,
    open: InputOpener,
    index: usize,
    done: mpsc::Sender<(usize, Result<OpenedInput>)>,
    go_rx: Receiver<Go>,
    stop: Arc<AtomicBool>,
) {
    // The metadata reads are part of opening: a source that panics in one
    // fails startup naming the input, like a panicking opener (Codex, PR
    // 313), instead of ending this thread without a completion.
    let opened = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let src = open()?;
        let info = OpenedInput {
            name: name.clone(),
            sample_rate_hz: src.sample_rate(),
            center_freq_hz: src.center_freq_hz(),
            rf_passband_hz: src.rf_passband_hz(),
            trusted_band_hz: src.trusted_band_hz(),
        };
        Ok((src, info))
    }))
    .unwrap_or_else(|_| Err(anyhow!("the source panicked while opening")));
    let (mut src, info) = match opened {
        Ok(opened) => opened,
        Err(e) => {
            let _ = done.send((index, Err(e)));
            return;
        }
    };
    if done.send((index, Ok(info))).is_err() {
        return;
    }
    let Ok(go) = go_rx.recv() else {
        return; // startup failed elsewhere: drop the source and exit
    };
    let Go {
        cfg,
        clock,
        ids,
        factor,
        observers,
        outbox,
    } = go;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_input_front(
            src.as_mut(),
            &cfg,
            &stop,
            clock,
            ids,
            factor,
            &observers,
            &mut |b| outbox.send(Msg::Batch(b)),
        )
    }))
    .unwrap_or_else(|_| Err(anyhow!("input {name}: pipeline panicked")));
    outbox.send(Msg::End(result));
}

/// Runs every opened input until each has ended (EOF, `stop`, an error or a
/// panic), feeding one shared validator. `on_event` sees every input's
/// events with common-clock timestamps and calibrated frequencies;
/// `on_spot` every spot; `on_batch(input, n_tracks)` fires after each
/// batch the back end ingests; `on_input_end` fires as each input ends,
/// while the others keep running. Returns every input's end, in input
/// order.
#[allow(clippy::too_many_arguments)]
pub fn listen_inputs(
    mut opened: OpenedInputs,
    cfg: &PipelineConfig,
    clock: SessionClock,
    shared: SharedObservers,
    mut on_event: impl FnMut(&DecoderEvent),
    mut on_spot: impl FnMut(&crate::Spot),
    mut on_batch: impl FnMut(usize, usize),
    mut on_input_end: impl FnMut(&InputEnd),
) -> Result<Vec<InputEnd>> {
    let n = opened.inputs.len();
    let rate = opened.common_rate_hz;
    let offsets: Vec<u64> = match &clock {
        SessionClock::Replay => vec![0; n],
        SessionClock::Live { offsets } => {
            if offsets.len() != n {
                bail!("{} live clock offsets for {n} inputs", offsets.len());
            }
            offsets
                .iter()
                .map(|d| (d.as_secs_f64() * rate).round() as u64)
                .collect()
        }
    };
    let factors = opened
        .ppm
        .iter()
        .map(|&ppm| manta_spot::calibration_factor_from_ppm(ppm).map_err(|e| anyhow!(e)))
        .collect::<Result<Vec<f64>>>()?;
    let validator = cfg.shared_validator(rate, n as u32)?;
    let mut back = crate::listen::Back::new(&shared, validator, rate);

    let live = matches!(clock, SessionClock::Live { .. });
    let (live_tx, live_rx) = mpsc::sync_channel::<(usize, Msg)>(LIVE_QUEUE_BATCHES);
    let mut replay_rx = Vec::new();
    let parked = std::mem::take(&mut opened.parked);
    let mut handles = Vec::with_capacity(n);
    for (index, p) in parked.into_iter().enumerate() {
        let outbox = if live {
            Outbox::Live(index, live_tx.clone())
        } else {
            let (tx, rx) = mpsc::sync_channel(REPLAY_QUEUE_BATCHES);
            replay_rx.push(rx);
            Outbox::Replay(tx)
        };
        let fs = opened.inputs[index].sample_rate_hz;
        let go = Go {
            cfg: cfg.clone(),
            clock: SourceClock {
                scale: (rate / fs).round() as u64,
                offset: offsets[index],
            },
            ids: TrackIdSpace::for_input(index, n),
            factor: factors[index],
            observers: FrontObservers {
                active_tracks: opened.observers[index].active_tracks.clone(),
                static_crashes: opened.observers[index].static_crashes.clone(),
                bad_samples: opened.observers[index].bad_samples.clone(),
                rtty: opened.observers[index].rtty.clone(),
                decode_latency: shared.decode_latency.clone(),
                decode_quality: shared.decode_quality.clone(),
                live_ids: shared.live_state.is_some(),
                input: index,
            },
            outbox,
        };
        // A thread that already died leaves its go-channel closed; the
        // merge below sees its queue disconnect and reports it.
        let _ = p.go.send(go);
        handles.push(p.handle);
    }
    drop(live_tx);

    let names: Vec<String> = opened.inputs.iter().map(|i| i.name.clone()).collect();
    let mut ends: Vec<Option<InputEnd>> = (0..n).map(|_| None).collect();
    let mut handle = |index: usize, msg: Msg, ends: &mut Vec<Option<InputEnd>>| match msg {
        Msg::Batch(batch) => {
            let n_tracks = batch.n_tracks;
            back.ingest(index, n, batch, &mut on_event, &mut on_spot);
            on_batch(index, n_tracks);
        }
        Msg::End(result) => {
            // An input that panicked sent no closing batch: its tracks are
            // retired before its end is reported (`on_input_end` retires
            // its gauges). After a closing batch this changes nothing.
            back.close_input(index, n, &mut on_event, &mut on_spot);
            let end = InputEnd {
                index,
                name: names[index].clone(),
                result,
            };
            on_input_end(&end);
            ends[index] = Some(end);
        }
    };
    let panicked = |index: usize| Err(anyhow!("input {}: pipeline panicked", names[index]));

    if live {
        let mut open = n;
        while open > 0 {
            match live_rx.recv() {
                Ok((index, msg)) => {
                    let ended = matches!(msg, Msg::End(_));
                    handle(index, msg, &mut ends);
                    if ended {
                        open -= 1;
                    }
                }
                Err(RecvError) => {
                    for index in 0..n {
                        if ends[index].is_none() {
                            handle(index, Msg::End(panicked(index)), &mut ends);
                        }
                    }
                    open = 0;
                }
            }
        }
    } else {
        let mut merge = ReplayMerge::new(n);
        while let Some(step) = merge.next(|index| match replay_rx[index].recv() {
            Ok(Msg::Batch(b)) => Pulled::Batch(b.end_ts, b),
            Ok(Msg::End(result)) => Pulled::End(result),
            Err(RecvError) => Pulled::End(panicked(index)),
        }) {
            match step {
                Step::Batch(index, b) => handle(index, Msg::Batch(b), &mut ends),
                Step::End(index, result) => handle(index, Msg::End(result), &mut ends),
            }
        }
    }
    for h in handles {
        let _ = h.join();
    }
    Ok(ends
        .into_iter()
        .enumerate()
        .map(|(index, e)| {
            e.unwrap_or_else(|| InputEnd {
                index,
                name: opened.inputs[index].name.clone(),
                result: Err(anyhow!(
                    "input {}: ended without a result",
                    opened.inputs[index].name
                )),
            })
        })
        .collect())
}

/// What a replay merge pulled from one input's queue.
enum Pulled<B> {
    Batch(u64, B),
    End(Result<()>),
}

/// One step of the replay merge.
enum Step<B> {
    Batch(usize, B),
    End(usize, Result<()>),
}

/// MAN-13 (D5): a k-way merge of per-input batch queues in `(batch end
/// time, input index)` order. It pulls only from inputs with no batch
/// queued, so the order it yields is a pure function of each input's batch
/// sequence, never of thread timing. An ended input leaves the minimum.
struct ReplayMerge<B> {
    heads: Vec<Option<(u64, B)>>,
    ended: Vec<bool>,
}

impl<B> ReplayMerge<B> {
    fn new(n: usize) -> Self {
        Self {
            heads: (0..n).map(|_| None).collect(),
            ended: vec![false; n],
        }
    }

    fn next(&mut self, mut pull: impl FnMut(usize) -> Pulled<B>) -> Option<Step<B>> {
        for index in 0..self.heads.len() {
            if self.heads[index].is_none() && !self.ended[index] {
                match pull(index) {
                    Pulled::Batch(ts, b) => self.heads[index] = Some((ts, b)),
                    Pulled::End(result) => {
                        self.ended[index] = true;
                        return Some(Step::End(index, result));
                    }
                }
            }
        }
        let index = self
            .heads
            .iter()
            .enumerate()
            .filter_map(|(i, h)| h.as_ref().map(|(ts, _)| (*ts, i)))
            .min()?
            .1;
        let (_, b) = self.heads[index].take()?;
        Some(Step::Batch(index, b))
    }
}

impl PipelineConfig {
    /// MAN-13: the one validator several inputs share: built at the common
    /// rate, with calibration factor 1.0 (each input calibrates its own
    /// frequencies, D4) and the input count its track ids interleave by.
    pub fn shared_validator(
        &self,
        rate_hz: f64,
        input_count: u32,
    ) -> Result<manta_spot::Validator> {
        let uncalibrated = PipelineConfig {
            freq_correction_ppm: 0.0,
            ..self.clone()
        };
        Ok(uncalibrated
            .validator(rate_hz)?
            .with_input_count(input_count))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_clock_identity_maps_every_ts_to_itself() {
        for ts in [0, 1, 96_000, u64::from(u32::MAX) + 7] {
            assert_eq!(SourceClock::IDENTITY.to_common(ts), ts);
        }
    }

    #[test]
    fn source_clock_scales_by_the_rate_ratio_and_adds_the_offset() {
        let clock = SourceClock {
            scale: (192_000.0_f64 / 48_000.0) as u64,
            offset: 500,
        };
        assert_eq!(clock.to_common(1000), 500 + 4000);
    }

    #[test]
    fn common_rate_is_the_highest_rate() {
        assert_eq!(
            common_rate_hz(&[96_000.0, 192_000.0, 48_000.0]).unwrap(),
            192_000.0
        );
    }

    #[test]
    fn common_rate_rejects_a_rate_that_is_not_a_power_of_two_multiple() {
        let err = common_rate_hz(&[96_000.0, 100_000.0])
            .unwrap_err()
            .to_string();
        assert!(err.contains("96000"), "{err}");
        let err = common_rate_hz(&[48_000.0, 144_000.0])
            .unwrap_err()
            .to_string();
        assert!(err.contains("48000"), "{err}");
    }

    fn drain<B>(merge: &mut ReplayMerge<B>, queues: &mut [Vec<Pulled<B>>]) -> Vec<Step<B>> {
        let mut out = Vec::new();
        while let Some(step) = merge.next(|i| queues[i].remove(0)) {
            out.push(step);
        }
        out
    }

    fn order(steps: &[Step<&'static str>]) -> Vec<String> {
        steps
            .iter()
            .map(|s| match s {
                Step::Batch(i, b) => format!("{i}:{b}"),
                Step::End(i, r) => format!("{i}:end{}", if r.is_ok() { "" } else { "!" }),
            })
            .collect()
    }

    #[test]
    fn replay_merge_orders_batches_by_end_time_then_input_index() {
        let mut merge = ReplayMerge::new(2);
        let mut queues = vec![
            vec![
                Pulled::Batch(10, "a1"),
                Pulled::Batch(20, "a2"),
                Pulled::End(Ok(())),
            ],
            vec![
                Pulled::Batch(10, "b1"),
                Pulled::Batch(15, "b2"),
                Pulled::End(Ok(())),
            ],
        ];
        let steps = drain(&mut merge, &mut queues);
        assert_eq!(
            order(&steps),
            vec!["0:a1", "1:b1", "1:b2", "1:end", "0:a2", "0:end"]
        );
    }

    #[test]
    fn replay_merge_drops_an_ended_input_from_the_minimum() {
        let mut merge = ReplayMerge::new(3);
        let mut queues = vec![
            vec![
                Pulled::Batch(5, "a1"),
                Pulled::Batch(30, "a2"),
                Pulled::End(Ok(())),
            ],
            vec![Pulled::End(Err(anyhow!("boom")))],
            vec![Pulled::Batch(10, "c1"), Pulled::End(Ok(()))],
        ];
        let steps = drain(&mut merge, &mut queues);
        assert_eq!(
            order(&steps),
            vec!["1:end!", "0:a1", "2:c1", "2:end", "0:a2", "0:end"]
        );
    }

    #[test]
    fn replay_merge_waits_only_on_the_input_without_a_queued_batch() {
        let mut merge = ReplayMerge::new(2);
        let mut pulls = Vec::new();
        let mut queues = [
            vec![Pulled::Batch(1, "a1"), Pulled::Batch(2, "a2")],
            vec![Pulled::Batch(9, "b1")],
        ];
        for _ in 0..2 {
            merge.next(|i| {
                pulls.push(i);
                queues[i].remove(0)
            });
        }
        // First step pulls both; the second only input 0, whose head went.
        assert_eq!(pulls, vec![0, 1, 0]);
    }
}
