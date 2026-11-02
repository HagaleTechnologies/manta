//! MAN-323 D8: the IQ tap that feeds FT8/FT4 windows from a stream the CW
//! pipeline is already reading.
//!
//! [`attach`] wraps an [`IqSource`]. Every `read()` hands the engine the
//! inner source's samples unchanged and also runs them through one
//! [`UsbDemod`] and [`CycleSlicer`] per attempted window. Each complete slot
//! is decoded:
//!
//! - [`Placement::Replay`]: inline, inside `read()`, so the decodes are a
//!   pure function of the file and its epoch;
//! - [`Placement::Live`]: posted to the window's mailbox in a
//!   [`DecodeService`]. A newer slot replaces an undecoded older one, which
//!   is reported [`CycleOutcome::Dropped`]. One `ftx-decode` thread takes
//!   the oldest pending slot across every mailbox.
//!
//! Every decode runs inside one process-wide one-thread rayon pool, so the
//! decoder's internal parallelism never competes with the CW decoder pool.

use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Instant, SystemTime};

use anyhow::Result;
use manta_input::{InputHealthCounters, IqSource};
use num_complex::Complex32;

use super::demod::UsbDemod;
use super::slicer::{CycleSlicer, Slot, SlotOutcome};
use super::{CycleDecode, FtxMessage, FtxMode, FtxWindow};

/// What became of one slot of one window (the `outcome` metric label).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CycleOutcome {
    /// Handed to the decoder.
    Decoded,
    /// Replaced by the next slot before the decoder reached it (live only).
    Dropped,
    /// Not delivered whole by the stream (startup, a gap, the end).
    Incomplete,
}

impl CycleOutcome {
    /// Every outcome, in the order `/metrics` renders them.
    pub const ALL: [CycleOutcome; 3] = [
        CycleOutcome::Decoded,
        CycleOutcome::Dropped,
        CycleOutcome::Incomplete,
    ];

    /// The `/metrics` label value.
    pub fn label(self) -> &'static str {
        match self {
            CycleOutcome::Decoded => "decoded",
            CycleOutcome::Dropped => "dropped",
            CycleOutcome::Incomplete => "incomplete",
        }
    }
}

/// One slot's result, for the sink.
#[derive(Clone, Debug, PartialEq)]
pub struct SlotResult {
    /// The input's index in the session.
    pub input: usize,
    pub window: FtxWindow,
    /// The slot's UTC start, milliseconds since the Unix epoch.
    pub slot_start_ms: i64,
    pub outcome: CycleOutcome,
    /// What the decoder returned, in its order (empty unless decoded).
    pub messages: Vec<FtxMessage>,
    /// Wall time of the decode, seconds (0 unless decoded).
    pub decode_s: f64,
    /// The decoder's error, when it returned one instead of messages; the
    /// sink logs it. The slot still counts as decoded (it was handed over).
    pub error: Option<String>,
}

/// Where slot results go: the CLI's publisher, or a test's recorder.
pub trait FtxSink: Send + Sync {
    /// One slot of one window ended.
    fn slot(&self, result: SlotResult);
    /// The input's stream ended; it reports no more slots.
    fn input_ended(&self, input: usize);
}

/// Builds the decoder for a window's mode; [`super::new_decoder`] in
/// production, a fake in tests.
pub type DecoderFactory = Arc<dyn Fn(FtxMode) -> Option<Box<dyn CycleDecode>> + Send + Sync>;

/// The production decoder factory: pancetta with the `ftx` feature.
pub fn pancetta_factory() -> DecoderFactory {
    Arc::new(super::new_decoder)
}

/// How a tap decodes complete slots.
#[derive(Clone)]
pub enum Placement {
    /// Inline in `read()`: file input not paced as live.
    Replay,
    /// Through the session's one decode thread.
    Live(Arc<DecodeService>),
}

/// Everything [`attach`] needs for one input.
pub struct TapSpec {
    /// The input's index in the session.
    pub input: usize,
    /// The UTC instant of the stream's sample 0.
    pub epoch: SystemTime,
    /// The windows this input attempts (see [`super::coverage`]).
    pub windows: Vec<FtxWindow>,
    pub placement: Placement,
    pub sink: Arc<dyn FtxSink>,
    pub decoders: DecoderFactory,
}

/// The process-wide one-thread pool every FT8/FT4 decode runs in.
fn decode_pool() -> &'static rayon::ThreadPool {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .thread_name(|_| "ftx-decode-pool".to_string())
            .build()
            .expect("a one-thread rayon pool builds")
    })
}

/// Decode one slot inside the shared pool, timing it. A decoder error is
/// a slot with no messages and the error's text.
fn decode_slot(
    decoder: &mut dyn CycleDecode,
    audio: &[f32],
) -> (Vec<FtxMessage>, f64, Option<String>) {
    let t0 = Instant::now();
    let decoded = decode_pool().install(|| decoder.decode(audio));
    let decode_s = t0.elapsed().as_secs_f64();
    match decoded {
        Ok(messages) => (messages, decode_s, None),
        Err(e) => (Vec::new(), decode_s, Some(format!("{e:#}"))),
    }
}

/// Wrap `src` with an FT8/FT4 tap for `spec.windows`. With no window to
/// attempt (or none whose demodulator and decoder build), `src` comes back
/// unchanged and nothing is registered.
pub fn attach(src: Box<dyn IqSource>, spec: TapSpec) -> Box<dyn IqSource> {
    let fs = src.sample_rate();
    let centre = src.center_freq_hz();
    let mut chains = Vec::new();
    for window in spec.windows {
        let Some(demod) = UsbDemod::new(fs, f64::from(window.dial_hz) - centre) else {
            continue;
        };
        let Some(decoder) = (spec.decoders)(window.mode) else {
            continue;
        };
        let slicer = CycleSlicer::new(window.mode, spec.epoch, demod.group_delay_s());
        let decoder = match &spec.placement {
            Placement::Replay => Some(decoder),
            Placement::Live(service) => {
                service.register(spec.input, window, decoder);
                None
            }
        };
        chains.push(Chain {
            window,
            demod,
            slicer,
            decoder,
        });
    }
    if chains.is_empty() {
        return src;
    }
    Box::new(FtxTap {
        inner: src,
        input: spec.input,
        chains,
        placement: spec.placement,
        sink: spec.sink,
        pending_gap: None,
        pending_missing: None,
        audio: Vec::new(),
        slots: Vec::new(),
        ended: false,
    })
}

/// One window's demodulator, slicer and (replay only) decoder.
struct Chain {
    window: FtxWindow,
    demod: UsbDemod,
    slicer: CycleSlicer,
    decoder: Option<Box<dyn CycleDecode>>,
}

struct FtxTap {
    inner: Box<dyn IqSource>,
    input: usize,
    chains: Vec<Chain>,
    placement: Placement,
    sink: Arc<dyn FtxSink>,
    pending_gap: Option<u64>,
    pending_missing: Option<u64>,
    audio: Vec<f32>,
    slots: Vec<Slot>,
    ended: bool,
}

impl FtxTap {
    fn dispatch(&mut self, chain: usize) {
        for slot in std::mem::take(&mut self.slots) {
            let window = self.chains[chain].window;
            match slot.outcome {
                SlotOutcome::Incomplete => self.sink.slot(SlotResult {
                    input: self.input,
                    window,
                    slot_start_ms: slot.start_ms,
                    outcome: CycleOutcome::Incomplete,
                    messages: Vec::new(),
                    decode_s: 0.0,
                    error: None,
                }),
                SlotOutcome::Complete => match &self.placement {
                    Placement::Replay => {
                        let decoder = self.chains[chain]
                            .decoder
                            .as_deref_mut()
                            .expect("a replay chain owns its decoder");
                        let (messages, decode_s, error) = decode_slot(decoder, &slot.audio);
                        self.sink.slot(SlotResult {
                            input: self.input,
                            window,
                            slot_start_ms: slot.start_ms,
                            outcome: CycleOutcome::Decoded,
                            messages,
                            decode_s,
                            error,
                        });
                    }
                    Placement::Live(service) => service.post(self.input, window, slot),
                },
            }
        }
    }
}

impl IqSource for FtxTap {
    fn sample_rate(&self) -> f64 {
        self.inner.sample_rate()
    }

    fn center_freq_hz(&self) -> f64 {
        self.inner.center_freq_hz()
    }

    fn rf_passband_hz(&self) -> (f64, f64) {
        self.inner.rf_passband_hz()
    }

    fn trusted_band_hz(&self) -> Option<(f64, f64)> {
        self.inner.trusted_band_hz()
    }

    fn analytic_guard_hz(&self) -> f64 {
        self.inner.analytic_guard_hz()
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        let n = self.inner.read(buf)?;
        // Consumed here, applied before the block they precede, and handed
        // to the engine unchanged (as RecordingSource does).
        let gap = self.inner.take_discontinuity();
        let missing = self.inner.take_missing_samples();
        self.pending_gap = gap;
        self.pending_missing = missing;
        if self.ended {
            return Ok(n);
        }
        let fs = self.inner.sample_rate();
        for c in 0..self.chains.len() {
            let mut slots = std::mem::take(&mut self.slots);
            let chain = &mut self.chains[c];
            if let Some(g) = gap {
                chain.demod.reset();
                chain.slicer.gap(g, fs, |s| slots.push(s));
            }
            if let Some(m) = missing {
                // Zero IQ through the demodulator, so its decimation phase
                // and filter history stay in step with the slicer's clock.
                static ZEROS: [Complex32; 4096] = [Complex32::new(0.0, 0.0); 4096];
                let mut left = m as usize;
                while left > 0 {
                    let k = left.min(ZEROS.len());
                    self.audio.clear();
                    chain.demod.process(&ZEROS[..k], &mut self.audio);
                    chain.slicer.push(&self.audio, |s| slots.push(s));
                    left -= k;
                }
            }
            if n > 0 {
                self.audio.clear();
                chain.demod.process(&buf[..n], &mut self.audio);
                chain.slicer.push(&self.audio, |s| slots.push(s));
            } else {
                chain.slicer.finish(|s| slots.push(s));
            }
            self.slots = slots;
            self.dispatch(c);
        }
        if n == 0 {
            self.ended = true;
            self.sink.input_ended(self.input);
        }
        Ok(n)
    }

    fn confirmed_live_handle(&self) -> Option<Arc<AtomicBool>> {
        self.inner.confirmed_live_handle()
    }

    fn take_discontinuity(&mut self) -> Option<u64> {
        self.pending_gap.take()
    }

    fn take_missing_samples(&mut self) -> Option<u64> {
        self.pending_missing.take()
    }

    fn acquisition_start_offset(&self) -> std::time::Duration {
        self.inner.acquisition_start_offset()
    }

    fn health_counters(&self) -> Option<Arc<InputHealthCounters>> {
        self.inner.health_counters()
    }
}

type Key = (usize, FtxWindow);

#[derive(Default)]
struct ServiceState {
    pending: BTreeMap<Key, Slot>,
    decoders: BTreeMap<Key, Box<dyn CycleDecode>>,
    stopped: bool,
}

/// What the decode thread shares with the service handle. The thread holds
/// only this, so dropping the last [`DecodeService`] handle stops it.
struct ServiceShared {
    state: Mutex<ServiceState>,
    wake: Condvar,
    sink: Arc<dyn FtxSink>,
}

/// The live session's FT8/FT4 decode thread and its per-window mailboxes.
pub struct DecodeService {
    shared: Arc<ServiceShared>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl DecodeService {
    /// Start the `ftx-decode` thread, reporting to `sink`.
    pub fn start(sink: Arc<dyn FtxSink>) -> Arc<Self> {
        let shared = Arc::new(ServiceShared {
            state: Mutex::new(ServiceState::default()),
            wake: Condvar::new(),
            sink,
        });
        let worker = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("ftx-decode".to_string())
            .spawn(move || worker.run())
            .expect("spawn the ftx-decode thread");
        Arc::new(DecodeService {
            shared,
            thread: Mutex::new(Some(handle)),
        })
    }

    fn register(&self, input: usize, window: FtxWindow, decoder: Box<dyn CycleDecode>) {
        let mut st = self.shared.lock();
        st.decoders.insert((input, window), decoder);
    }

    /// Post a complete slot; an undecoded older slot of the same window is
    /// reported dropped.
    fn post(&self, input: usize, window: FtxWindow, slot: Slot) {
        let replaced = {
            let mut st = self.shared.lock();
            if st.stopped {
                return;
            }
            st.pending.insert((input, window), slot)
        };
        self.shared.wake.notify_all();
        if let Some(old) = replaced {
            self.shared.dropped((input, window), old.start_ms);
        }
    }

    /// End the decode thread (at shutdown). A slot it is decoding finishes;
    /// slots still waiting are reported dropped.
    pub fn stop(&self) {
        let abandoned = {
            let mut st = self.shared.lock();
            st.stopped = true;
            std::mem::take(&mut st.pending)
        };
        self.shared.wake.notify_all();
        for (key, slot) in abandoned {
            self.shared.dropped(key, slot.start_ms);
        }
        let handle = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(h) = handle {
            let _ = h.join();
        }
    }
}

impl Drop for DecodeService {
    fn drop(&mut self) {
        self.stop();
    }
}

impl ServiceShared {
    fn lock(&self) -> std::sync::MutexGuard<'_, ServiceState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn dropped(&self, key: Key, slot_start_ms: i64) {
        self.sink.slot(SlotResult {
            input: key.0,
            window: key.1,
            slot_start_ms,
            outcome: CycleOutcome::Dropped,
            messages: Vec::new(),
            decode_s: 0.0,
            error: None,
        });
    }

    fn run(&self) {
        loop {
            let (key, slot, mut decoder) = {
                let mut st = self.lock();
                loop {
                    if st.stopped {
                        return;
                    }
                    // The oldest pending slot; ties by (input, window).
                    let next = st
                        .pending
                        .iter()
                        .min_by_key(|(key, slot)| (slot.start_ms, **key))
                        .map(|(key, _)| *key);
                    if let Some(key) = next {
                        let slot = st.pending.remove(&key).expect("key just found");
                        let decoder = st.decoders.remove(&key);
                        break (key, slot, decoder);
                    }
                    st = self.wake.wait(st).unwrap_or_else(|e| e.into_inner());
                }
            };
            let (messages, decode_s, error) = match decoder.as_deref_mut() {
                Some(d) => decode_slot(d, &slot.audio),
                None => (Vec::new(), 0.0, Some("no decoder registered".to_string())),
            };
            if let Some(d) = decoder {
                self.lock().decoders.insert(key, d);
            }
            self.sink.slot(SlotResult {
                input: key.0,
                window: key.1,
                slot_start_ms: slot.start_ms,
                outcome: CycleOutcome::Decoded,
                messages,
                decode_s,
                error,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::mpsc;
    use std::time::{Duration, UNIX_EPOCH};

    const BOUNDARY_MS: u64 = 1_730_462_400_000;
    const FT8_20M: FtxWindow = FtxWindow {
        mode: FtxMode::Ft8,
        dial_hz: 14_074_000,
    };
    const FT4_20M: FtxWindow = FtxWindow {
        mode: FtxMode::Ft4,
        dial_hz: 14_080_000,
    };

    #[derive(Default)]
    struct Recorder {
        slots: Mutex<Vec<SlotResult>>,
        ended: Mutex<Vec<usize>>,
    }

    impl FtxSink for Recorder {
        fn slot(&self, result: SlotResult) {
            self.slots.lock().unwrap().push(result);
        }
        fn input_ended(&self, input: usize) {
            self.ended.lock().unwrap().push(input);
        }
    }

    impl Recorder {
        fn outcomes(&self, outcome: CycleOutcome) -> Vec<i64> {
            self.slots
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.outcome == outcome)
                .map(|r| r.slot_start_ms)
                .collect()
        }
    }

    /// A fake decoder that records each slot's length and returns one message.
    struct Fake {
        calls: Arc<Mutex<Vec<usize>>>,
    }

    impl CycleDecode for Fake {
        fn decode(&mut self, audio: &[f32]) -> Result<Vec<FtxMessage>> {
            self.calls.lock().unwrap().push(audio.len());
            Ok(vec![FtxMessage {
                text: "CQ W1AW FN31".into(),
                audio_hz: 1234.0,
                snr_db: -10.0,
                dt_s: 0.5,
            }])
        }
    }

    fn fake_factory(calls: Arc<Mutex<Vec<usize>>>) -> DecoderFactory {
        Arc::new(move |_| {
            Some(Box::new(Fake {
                calls: Arc::clone(&calls),
            }) as Box<dyn CycleDecode>)
        })
    }

    /// A scripted source: blocks of zeros, each with an optional preceding
    /// gap or loss, then EOF.
    struct Scripted {
        fs: f64,
        blocks: VecDeque<(usize, Option<u64>, Option<u64>)>,
        gap: Option<u64>,
        missing: Option<u64>,
        live: Arc<AtomicBool>,
        health: Arc<InputHealthCounters>,
    }

    impl Scripted {
        fn zeros(fs: f64, seconds: f64, block: usize) -> Self {
            let total = (fs * seconds) as usize;
            let blocks = (0..total.div_ceil(block))
                .map(|i| (block.min(total - i * block), None, None))
                .collect();
            Scripted {
                fs,
                blocks,
                gap: None,
                missing: None,
                live: Arc::new(AtomicBool::new(true)),
                health: Arc::new(InputHealthCounters::new()),
            }
        }
    }

    impl IqSource for Scripted {
        fn sample_rate(&self) -> f64 {
            self.fs
        }
        fn center_freq_hz(&self) -> f64 {
            14_050_000.0
        }
        fn rf_passband_hz(&self) -> (f64, f64) {
            (-30_000.0, 31_000.0)
        }
        fn trusted_band_hz(&self) -> Option<(f64, f64)> {
            Some((-29_000.0, 29_500.0))
        }
        fn analytic_guard_hz(&self) -> f64 {
            7.0
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            let Some((n, gap, missing)) = self.blocks.pop_front() else {
                return Ok(0);
            };
            let n = n.min(buf.len());
            for (i, s) in buf[..n].iter_mut().enumerate() {
                *s = Complex32::new(i as f32, -(i as f32));
            }
            self.gap = gap;
            self.missing = missing;
            Ok(n)
        }
        fn confirmed_live_handle(&self) -> Option<Arc<AtomicBool>> {
            Some(Arc::clone(&self.live))
        }
        fn take_discontinuity(&mut self) -> Option<u64> {
            self.gap.take()
        }
        fn take_missing_samples(&mut self) -> Option<u64> {
            self.missing.take()
        }
        fn acquisition_start_offset(&self) -> Duration {
            Duration::from_millis(321)
        }
        fn health_counters(&self) -> Option<Arc<InputHealthCounters>> {
            Some(Arc::clone(&self.health))
        }
    }

    fn spec(
        windows: Vec<FtxWindow>,
        placement: Placement,
        sink: Arc<Recorder>,
        decoders: DecoderFactory,
        epoch_ms: u64,
    ) -> TapSpec {
        TapSpec {
            input: 0,
            epoch: UNIX_EPOCH + Duration::from_millis(epoch_ms),
            windows,
            placement,
            sink,
            decoders,
        }
    }

    #[test]
    fn forwards_every_iq_source_hook() {
        let fs = 48_000.0;
        let mut inner = Scripted::zeros(fs, 1.0, 4800);
        inner.blocks[2].1 = Some(9_600);
        inner.blocks[4].2 = Some(480);
        let live = Arc::clone(&inner.live);
        let health = Arc::clone(&inner.health);
        let sink = Arc::new(Recorder::default());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut tap = attach(
            Box::new(inner),
            spec(
                vec![FT8_20M],
                Placement::Replay,
                sink.clone(),
                fake_factory(calls),
                BOUNDARY_MS,
            ),
        );
        assert_eq!(tap.sample_rate(), fs);
        assert_eq!(tap.center_freq_hz(), 14_050_000.0);
        assert_eq!(tap.rf_passband_hz(), (-30_000.0, 31_000.0));
        assert_eq!(tap.trusted_band_hz(), Some((-29_000.0, 29_500.0)));
        assert_eq!(tap.analytic_guard_hz(), 7.0);
        assert_eq!(tap.acquisition_start_offset(), Duration::from_millis(321));
        assert!(Arc::ptr_eq(&tap.confirmed_live_handle().unwrap(), &live));
        assert!(Arc::ptr_eq(&tap.health_counters().unwrap(), &health));
        let mut buf = vec![Complex32::new(0.0, 0.0); 8192];
        let mut gaps = Vec::new();
        let mut losses = Vec::new();
        for _ in 0..10 {
            let n = tap.read(&mut buf).unwrap();
            assert_eq!(n, 4800);
            // The engine sees the inner source's samples unchanged.
            assert_eq!(buf[7], Complex32::new(7.0, -7.0));
            gaps.push(tap.take_discontinuity());
            losses.push(tap.take_missing_samples());
            assert_eq!(tap.take_discontinuity(), None, "returned once");
            assert_eq!(tap.take_missing_samples(), None, "returned once");
        }
        assert_eq!(gaps.iter().flatten().collect::<Vec<_>>(), vec![&9_600]);
        assert_eq!(gaps[2], Some(9_600));
        assert_eq!(losses[4], Some(480));
        assert_eq!(losses.iter().flatten().count(), 1);
        assert_eq!(tap.read(&mut buf).unwrap(), 0);
        assert_eq!(*sink.ended.lock().unwrap(), vec![0]);
    }

    #[test]
    fn replay_decodes_inline_with_slot_aligned_audio() {
        let fs = 96_000.0;
        let sink = Arc::new(Recorder::default());
        let calls = Arc::new(Mutex::new(Vec::new()));
        // Epoch 3.7 s before a boundary: 3.7 + 3 * 15 = 48.7 s < 50 s.
        let mut tap = attach(
            Box::new(Scripted::zeros(fs, 50.0, 8192)),
            spec(
                vec![FT8_20M],
                Placement::Replay,
                sink.clone(),
                fake_factory(Arc::clone(&calls)),
                BOUNDARY_MS - 3_700,
            ),
        );
        let mut buf = vec![Complex32::new(0.0, 0.0); 8192];
        let mut decoded_after_read = Vec::new();
        while tap.read(&mut buf).unwrap() > 0 {
            decoded_after_read.push(calls.lock().unwrap().len());
        }
        assert_eq!(*calls.lock().unwrap(), vec![180_000; 3]);
        let decoded = sink.outcomes(CycleOutcome::Decoded);
        assert_eq!(decoded.len(), 3);
        for (k, start) in decoded.iter().enumerate() {
            assert_eq!(start % 15_000, 0);
            assert_eq!(*start, (BOUNDARY_MS + 15_000 * k as u64) as i64);
        }
        // Each slot was decoded inside the read() that completed it: the
        // count rises one at a time across reads, and the read that ends
        // slot 1 (3.7 s + 15 s + the filter delay in) already shows it.
        let read_ending_slot_1 = ((18.7037 * fs) / 8192.0).ceil() as usize - 1;
        assert_eq!(decoded_after_read[read_ending_slot_1 - 1], 0);
        assert_eq!(decoded_after_read[read_ending_slot_1], 1);
        // The startup slot and the trailing partial one are incomplete.
        assert_eq!(
            sink.outcomes(CycleOutcome::Incomplete),
            vec![(BOUNDARY_MS - 15_000) as i64, (BOUNDARY_MS + 45_000) as i64]
        );
        let slots = sink.slots.lock().unwrap();
        assert!(slots.iter().all(|r| r.window == FT8_20M && r.input == 0));
        assert!(slots
            .iter()
            .filter(|r| r.outcome == CycleOutcome::Decoded)
            .all(|r| r.messages.len() == 1));
    }

    /// A decoder that blocks until the test releases it.
    struct Gate {
        entered: mpsc::Sender<usize>,
        release: Arc<Mutex<mpsc::Receiver<()>>>,
        running: Arc<std::sync::atomic::AtomicUsize>,
        max_running: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl CycleDecode for Gate {
        fn decode(&mut self, audio: &[f32]) -> Result<Vec<FtxMessage>> {
            use std::sync::atomic::Ordering;
            let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_running.fetch_max(now, Ordering::SeqCst);
            self.entered.send(audio.len()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
            self.running.fetch_sub(1, Ordering::SeqCst);
            Ok(Vec::new())
        }
    }

    fn slot_at(start_ms: i64, mode: FtxMode) -> Slot {
        Slot {
            start_ms,
            audio: vec![0.0; mode.slot_samples()],
            outcome: SlotOutcome::Complete,
        }
    }

    fn wait_for(sink: &Recorder, decoded: usize) {
        for _ in 0..500 {
            if sink.outcomes(CycleOutcome::Decoded).len() >= decoded {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("decode thread did not report {decoded} slots");
    }

    #[test]
    fn live_mailbox_keeps_the_newest_slot_and_counts_the_replaced_one() {
        let sink = Arc::new(Recorder::default());
        let service = DecodeService::start(sink.clone());
        let (entered_tx, entered) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let gate = Gate {
            entered: entered_tx,
            release: Arc::new(Mutex::new(release_rx)),
            running: Arc::default(),
            max_running: Arc::default(),
        };
        service.register(0, FT8_20M, Box::new(gate));
        service.post(0, FT8_20M, slot_at(15_000, FtxMode::Ft8));
        // The decoder is now busy with slot 1.
        assert_eq!(
            entered.recv_timeout(Duration::from_secs(5)).unwrap(),
            180_000
        );
        service.post(0, FT8_20M, slot_at(30_000, FtxMode::Ft8));
        service.post(0, FT8_20M, slot_at(45_000, FtxMode::Ft8));
        assert_eq!(sink.outcomes(CycleOutcome::Dropped), vec![30_000]);
        release_tx.send(()).unwrap();
        assert_eq!(
            entered.recv_timeout(Duration::from_secs(5)).unwrap(),
            180_000
        );
        release_tx.send(()).unwrap();
        wait_for(&sink, 2);
        assert_eq!(sink.outcomes(CycleOutcome::Decoded), vec![15_000, 45_000]);
        service.stop();
    }

    #[test]
    fn stop_reports_waiting_slots_dropped_and_ends_the_thread() {
        let sink = Arc::new(Recorder::default());
        let service = DecodeService::start(sink.clone());
        let (entered_tx, entered) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        service.register(
            0,
            FT8_20M,
            Box::new(Gate {
                entered: entered_tx,
                release: Arc::new(Mutex::new(release_rx)),
                running: Arc::default(),
                max_running: Arc::default(),
            }),
        );
        service.post(0, FT8_20M, slot_at(15_000, FtxMode::Ft8));
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        service.post(0, FT8_20M, slot_at(30_000, FtxMode::Ft8));
        // Release the in-flight decode only once stop() has taken the
        // waiting slot (it does so before it joins the thread).
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            release_tx.send(()).unwrap();
        });
        service.stop();
        releaser.join().unwrap();
        assert_eq!(sink.outcomes(CycleOutcome::Dropped), vec![30_000]);
        assert_eq!(sink.outcomes(CycleOutcome::Decoded), vec![15_000]);
        // Posting after stop is ignored.
        service.post(0, FT8_20M, slot_at(45_000, FtxMode::Ft8));
        assert_eq!(sink.slots.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_decoder_error_reaches_the_sink() {
        struct Broken;
        impl CycleDecode for Broken {
            fn decode(&mut self, _audio: &[f32]) -> Result<Vec<FtxMessage>> {
                anyhow::bail!("insufficient data")
            }
        }
        let (messages, _, error) = decode_slot(&mut Broken, &[0.0; 16]);
        assert!(messages.is_empty());
        assert_eq!(error.as_deref(), Some("insufficient data"));
    }

    #[test]
    fn windows_share_one_decode_thread_and_never_run_concurrently() {
        let sink = Arc::new(Recorder::default());
        let service = DecodeService::start(sink.clone());
        let running = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let max_running = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (entered_tx, entered) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release = Arc::new(Mutex::new(release_rx));
        let keys = [(0, FT8_20M), (0, FT4_20M), (1, FT8_20M)];
        for (input, window) in keys {
            service.register(
                input,
                window,
                Box::new(Gate {
                    entered: entered_tx.clone(),
                    release: Arc::clone(&release),
                    running: Arc::clone(&running),
                    max_running: Arc::clone(&max_running),
                }),
            );
        }
        for (input, window) in keys {
            service.post(input, window, slot_at(15_000, window.mode));
        }
        let mut lens = Vec::new();
        for _ in keys {
            lens.push(entered.recv_timeout(Duration::from_secs(5)).unwrap());
            release_tx.send(()).unwrap();
        }
        wait_for(&sink, 3);
        assert_eq!(max_running.load(std::sync::atomic::Ordering::SeqCst), 1);
        // Equal starts are taken by (input, window): FT8 sorts before FT4.
        let order: Vec<(usize, FtxWindow)> = sink
            .slots
            .lock()
            .unwrap()
            .iter()
            .map(|r| (r.input, r.window))
            .collect();
        assert_eq!(order, vec![(0, FT8_20M), (0, FT4_20M), (1, FT8_20M)]);
        assert_eq!(lens, vec![180_000, 90_000, 180_000]);
        service.stop();
    }

    #[test]
    fn no_attempted_window_means_no_wrapper_and_no_thread() {
        let sink = Arc::new(Recorder::default());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut src = attach(
            Box::new(Scripted::zeros(48_000.0, 1.0, 4800)),
            spec(
                Vec::new(),
                Placement::Replay,
                sink.clone(),
                fake_factory(calls),
                BOUNDARY_MS,
            ),
        );
        let mut buf = vec![Complex32::new(0.0, 0.0); 8192];
        while src.read(&mut buf).unwrap() > 0 {}
        // No tap: nothing reached the sink, not even the end of input.
        assert!(sink.slots.lock().unwrap().is_empty());
        assert!(sink.ended.lock().unwrap().is_empty());
        // Nor when no decoder can be built (a build without `ftx`).
        let none: DecoderFactory = Arc::new(|_| None);
        let mut src = attach(
            Box::new(Scripted::zeros(48_000.0, 1.0, 4800)),
            spec(
                vec![FT8_20M],
                Placement::Replay,
                sink.clone(),
                none,
                BOUNDARY_MS,
            ),
        );
        while src.read(&mut buf).unwrap() > 0 {}
        assert!(sink.ended.lock().unwrap().is_empty());
    }

    #[test]
    fn a_gap_reports_the_broken_slot_incomplete() {
        let fs = 48_000.0;
        let mut inner = Scripted::zeros(fs, 40.0, 4800);
        // 5 s in, a 4.2 s outage.
        inner.blocks[50].1 = Some((4.2 * fs) as u64);
        let sink = Arc::new(Recorder::default());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut tap = attach(
            Box::new(inner),
            spec(
                vec![FT8_20M],
                Placement::Replay,
                sink.clone(),
                fake_factory(calls),
                BOUNDARY_MS,
            ),
        );
        let mut buf = vec![Complex32::new(0.0, 0.0); 8192];
        while tap.read(&mut buf).unwrap() > 0 {}
        let base = BOUNDARY_MS as i64;
        // 40 s of samples plus 4.2 s of gap end at 44.2 s: the slot the gap
        // broke and the one the stream ended in are incomplete, and the
        // filter delay puts the stream's first samples in the slot before.
        assert_eq!(sink.outcomes(CycleOutcome::Decoded), vec![base + 15_000]);
        assert_eq!(
            sink.outcomes(CycleOutcome::Incomplete),
            vec![base - 15_000, base, base + 30_000]
        );
    }
}
