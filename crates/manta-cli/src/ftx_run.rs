//! The FT8/FT4 join between input taps and the JSON spot server (MAN-485).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use manta_engine::ftx::tap::{
    self, CycleOutcome, DecodeService, FtxSink, Placement, SlotResult, TapSpec,
};
use manta_engine::ftx::{self, Coverage, FtxMode, FtxWindow};
use manta_input::IqSource;
use manta_server::band::{allocations, band_for_freq_hz};
use manta_server::bus::SpotBus;
use manta_server::metrics::{FtxCycleOutcome, Metrics};
use manta_server::relay::wsjtx::{cq_caller, CqCaller, Dedupe};
use manta_server::spot_message::{FtxMode as WireMode, FtxSpot};

/// Windows attempted by this input and one operator-facing line per mode/band.
pub(crate) fn plan(
    name: &str,
    modes: &[FtxMode],
    centre: f64,
    passband: (f64, f64),
    trusted: Option<(f64, f64)>,
    fs: f64,
) -> (Vec<FtxWindow>, Vec<String>) {
    let mut attempted = Vec::new();
    let mut lines = Vec::new();
    let (band_lo, band_hi) = ftx::usable_band_hz(centre, passband, trusted);
    for &mode in modes {
        let prefix = format!("input {name}: {}", mode.label());
        if centre == 0.0 {
            lines.push(format!("{prefix} not attempted: this stream has no RF reference (set input.center_freq_hz or --dial-freq-hz)"));
            continue;
        }
        let bands: Vec<_> = allocations()
            .iter()
            .filter(|(_, lo, hi)| band_lo <= *hi && band_hi >= *lo)
            .collect();
        if bands.is_empty() {
            lines.push(format!(
                "{prefix} not attempted: this stream is outside the supported amateur bands"
            ));
        }
        for &&(band, _, _) in &bands {
            let prefix = format!("{prefix} on {band}");
            let Some(window) =
                ftx::windows(mode).find(|w| band_for_freq_hz(f64::from(w.dial_hz)) == band)
            else {
                lines.push(format!(
                    "{prefix} not attempted: {band} has no {} window",
                    mode.label()
                ));
                continue;
            };
            let (lo, hi) = window.rf_span_hz();
            let span = format!("{:.1}-{:.1} kHz", lo / 1000.0, hi / 1000.0);
            match ftx::coverage(&window, centre, passband, trusted, fs) {
                Coverage::Inside => {
                    lines.push(format!(
                        "{prefix}: decoding {span} (dial {:.1} kHz)",
                        f64::from(window.dial_hz) / 1000.0
                    ));
                    attempted.push(window);
                }
                Coverage::Partial | Coverage::Outside => {
                    let relation = if lo < band_hi && hi > band_lo {
                        "only partly inside"
                    } else {
                        "outside"
                    };
                    lines.push(format!("{prefix} not attempted: {span} is {relation} this stream's {:.1}-{:.1} kHz", band_lo / 1000.0, band_hi / 1000.0));
                }
                Coverage::UnsupportedRate => lines.push(format!(
                    "{prefix} not attempted: its {fs} S/s rate is not a multiple of 24000"
                )),
                Coverage::NoRfReference => unreachable!("RF reference checked above"),
            }
        }
    }
    (attempted, lines)
}

pub(crate) fn clock_line(kernel: Option<bool>) -> Option<String> {
    match kernel {
        Some(true) => None,
        Some(false) => Some("warning: the system clock is not synchronised to network time; FT8/FT4 slots are cut on UTC boundaries and decode only within about 1 s: turn on network time: `sudo timedatectl set-ntp true`, or install and start chrony".into()),
        None => Some("note: manta cannot read this platform's clock-sync state; FT8/FT4 slots are cut on UTC boundaries, so check the clock with `manta doctor`".into()),
    }
}

fn wire_mode(mode: FtxMode) -> WireMode {
    match mode {
        FtxMode::Ft8 => WireMode::Ft8,
        FtxMode::Ft4 => WireMode::Ft4,
    }
}

type WindowKey = (usize, FtxWindow);
type ResultKey = (i64, usize, FtxWindow);

#[derive(Default)]
struct State {
    registered: BTreeSet<WindowKey>,
    // A window's slots arrive in time order. Publish only when every replay
    // window has passed a slot's start, or its input has ended. Keeping a
    // watermark per window matters when FT4 and FT8 share an input.
    watermarks: BTreeMap<WindowKey, Option<i64>>,
    ended: BTreeSet<usize>,
    pending: BTreeMap<ResultKey, SlotResult>,
    dedupe: Dedupe,
}

pub(crate) struct Publisher {
    inputs: Vec<(String, f64)>,
    bound: OnceLock<(Arc<SpotBus>, Arc<Metrics>)>,
    state: Mutex<State>,
}

impl Publisher {
    fn new(inputs: Vec<(String, f64)>) -> Self {
        Self {
            inputs,
            bound: OnceLock::new(),
            state: Mutex::new(State::default()),
        }
    }

    fn register(&self, input: usize, windows: &[FtxWindow], replay: bool) {
        let mut st = self.state.lock().expect("FTx publisher lock");
        for &window in windows {
            st.registered.insert((input, window));
            if replay {
                st.watermarks.entry((input, window)).or_insert(None);
            }
            if let Some((_, metrics)) = self.bound.get() {
                metrics.register_ftx_window(&self.inputs[input].0, wire_mode(window.mode));
            }
        }
    }

    fn bind(&self, bus: Arc<SpotBus>, metrics: Arc<Metrics>) {
        let st = self.state.lock().expect("FTx publisher lock");
        for &(input, window) in &st.registered {
            metrics.register_ftx_window(&self.inputs[input].0, wire_mode(window.mode));
        }
        assert!(
            self.bound.set((bus, metrics)).is_ok(),
            "FTx server bound once"
        );
    }

    fn publish(&self, st: &mut State, result: SlotResult) {
        let (bus, metrics) = self
            .bound
            .get()
            .expect("FTx server bound before reading inputs");
        let (name, ppm) = &self.inputs[result.input];
        let mode = wire_mode(result.window.mode);
        let outcome = match result.outcome {
            CycleOutcome::Decoded => FtxCycleOutcome::Decoded,
            CycleOutcome::Incomplete => FtxCycleOutcome::Incomplete,
            CycleOutcome::Dropped => FtxCycleOutcome::Dropped,
        };
        metrics.record_ftx_cycle(name, mode, outcome);
        if result.outcome == CycleOutcome::Decoded {
            metrics.set_ftx_decode_seconds(name, mode, result.decode_s);
            metrics.record_ftx_decodes(mode, result.messages.len() as u64);
        }
        if let Some(error) = result.error {
            tracing::warn!("input {name}: {} decode failed: {error}", mode.label());
        }
        for message in result.messages {
            let CqCaller::Call(callsign) = cq_caller(&message.text) else {
                continue;
            };
            let freq_hz =
                (f64::from(result.window.dial_hz) + message.audio_hz) / (1.0 + ppm * 1e-6);
            let band = band_for_freq_hz(freq_hz.round());
            if !st.dedupe.admit(
                &callsign,
                mode.relay_mode(),
                band,
                result.slot_start_ms.div_euclid(1000),
            ) {
                continue;
            }
            let grid = message
                .text
                .split_ascii_whitespace()
                .last()
                .filter(|s| {
                    let b = s.as_bytes();
                    b.len() == 4
                        && (b'A'..=b'R').contains(&b[0])
                        && (b'A'..=b'R').contains(&b[1])
                        && b[2].is_ascii_digit()
                        && b[3].is_ascii_digit()
                        && *s != "RR73"
                })
                .map(str::to_string);
            bus.publish_ftx(FtxSpot {
                mode,
                callsign,
                grid,
                freq_hz,
                snr_db: message.snr_db.round() as i32,
                slot_start_ms: result.slot_start_ms,
            });
            metrics.record_ftx_spot(mode);
        }
    }

    fn flush(&self, st: &mut State) {
        while let Some((&key, _)) = st.pending.first_key_value() {
            if !st.watermarks.iter().all(|(&(input, _), at)| {
                st.ended.contains(&input) || at.is_some_and(|at| at > key.0)
            }) {
                break;
            }
            let result = st.pending.remove(&key).expect("first result exists");
            self.publish(st, result);
        }
    }
}

impl FtxSink for Publisher {
    fn slot(&self, result: SlotResult) {
        let mut st = self.state.lock().expect("FTx publisher lock");
        let key = (result.input, result.window);
        let several_replays = st
            .watermarks
            .keys()
            .map(|&(input, _)| input)
            .collect::<BTreeSet<_>>()
            .len()
            > 1;
        if several_replays && st.watermarks.contains_key(&key) {
            let at = st
                .watermarks
                .get_mut(&key)
                .expect("registered replay window");
            *at = Some(result.slot_start_ms);
            st.pending
                .insert((result.slot_start_ms, result.input, result.window), result);
            self.flush(&mut st);
        } else {
            self.publish(&mut st, result);
        }
    }
    fn input_ended(&self, input: usize) {
        let mut st = self.state.lock().expect("FTx publisher lock");
        st.ended.insert(input);
        self.flush(&mut st);
    }
}

/// One optional session per daemon. Without digital_modes the callers create
/// none, so the default CW path starts no tap, decode thread or metric series.
pub(crate) struct Session {
    publisher: Arc<Publisher>,
    service: OnceLock<Arc<DecodeService>>,
    lines: Mutex<BTreeMap<usize, Vec<String>>>,
    clock_noted: std::sync::atomic::AtomicBool,
}

impl Session {
    pub(crate) fn new(inputs: Vec<(String, f64)>) -> Self {
        Self {
            publisher: Arc::new(Publisher::new(inputs)),
            service: OnceLock::new(),
            lines: Mutex::new(BTreeMap::new()),
            clock_noted: std::sync::atomic::AtomicBool::new(false),
        }
    }
    pub(crate) fn bind(&self, bus: Arc<SpotBus>, metrics: Arc<Metrics>) {
        self.publisher.bind(bus, metrics);
    }

    pub(crate) fn wrap(
        &self,
        input: usize,
        src: Box<dyn IqSource>,
        epoch: SystemTime,
        modes: &[FtxMode],
        live: bool,
    ) -> Box<dyn IqSource> {
        let (windows, mut lines) = plan(
            &self.publisher.inputs[input].0,
            modes,
            src.center_freq_hz(),
            src.rf_passband_hz(),
            src.trusted_band_hz(),
            src.sample_rate(),
        );
        if live
            && !windows.is_empty()
            && !self
                .clock_noted
                .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            if let Some(line) = clock_line(crate::clock_check::kernel_synced()) {
                lines.push(line);
            }
        }
        self.lines
            .lock()
            .expect("FTx startup lines")
            .insert(input, lines);
        if windows.is_empty() {
            return src;
        }
        self.publisher.register(input, &windows, !live);
        let placement = if live {
            Placement::Live(
                self.service
                    .get_or_init(|| DecodeService::start(self.publisher.clone()))
                    .clone(),
            )
        } else {
            Placement::Replay
        };
        tap::attach(
            src,
            TapSpec {
                input,
                epoch,
                windows,
                placement,
                sink: self.publisher.clone(),
                decoders: tap::pancetta_factory(),
            },
        )
    }
    pub(crate) fn lines(&self, input: usize) -> Vec<String> {
        self.lines
            .lock()
            .expect("FTx startup lines")
            .remove(&input)
            .unwrap_or_default()
    }
    /// Release replay watermarks even when a read error bypassed the tap's EOF.
    pub(crate) fn input_ended(&self, input: usize) {
        self.publisher.input_ended(input);
    }

    pub(crate) fn stop(&self) {
        if let Some(service) = self.service.get() {
            service.stop();
        }
        // A signal may stop a replay before EOF. Once all readers are joined,
        // publish the complete slots already decoded in their deterministic order.
        for input in 0..self.publisher.inputs.len() {
            self.publisher.input_ended(input);
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use manta_engine::ftx::FtxMessage;
    use manta_server::bus::JsonItem;
    use std::time::UNIX_EPOCH;

    const FT8: FtxWindow = FtxWindow {
        mode: FtxMode::Ft8,
        dial_hz: 14_074_000,
    };
    fn result(input: usize, start: i64, text: &str) -> SlotResult {
        SlotResult {
            input,
            window: FT8,
            slot_start_ms: start,
            outcome: CycleOutcome::Decoded,
            messages: vec![FtxMessage {
                text: text.into(),
                audio_hz: 1234.0,
                snr_db: -10.0,
                dt_s: 0.5,
            }],
            decode_s: 0.1,
            error: None,
        }
    }
    fn publisher(ppm: f64) -> (Publisher, Arc<SpotBus>, Arc<Metrics>) {
        let p = Publisher::new(vec![("file".into(), ppm), ("other".into(), 0.0)]);
        p.register(0, &[FT8], false);
        p.register(1, &[FT8], false);
        let b = Arc::new(SpotBus::new(96_000.0, UNIX_EPOCH, 0));
        let m = Arc::new(Metrics::new());
        p.bind(b.clone(), m.clone());
        (p, b, m)
    }
    fn spots(rx: &mut tokio::sync::broadcast::Receiver<JsonItem>) -> Vec<FtxSpot> {
        let mut out = Vec::new();
        while let Ok(item) = rx.try_recv() {
            if let JsonItem::Ftx { spot, .. } = item {
                out.push(spot);
            }
        }
        out
    }
    #[test]
    fn only_cqs_become_spots_and_repeats_are_held_600_s() {
        let (p, b, m) = publisher(0.0);
        let mut rx = b.subscribe_json();
        p.slot(result(0, 0, "CQ W1AW FN31"));
        p.slot(result(0, 15_000, "K1ABC W1AW -12"));
        p.slot(result(0, 30_000, "CQ W1AW FN31"));
        p.slot(result(0, 601_000, "CQ W1AW FN31"));
        assert_eq!(spots(&mut rx).len(), 2);
        assert!(m
            .render_prometheus_text()
            .contains("manta_ftx_decodes_total{mode=\"FT8\"} 4"));
        assert!(b.recent(10).is_empty());
    }
    #[test]
    fn the_grid_comes_from_the_cq_and_rr73_is_not_a_grid() {
        let (p, b, _) = publisher(0.0);
        let mut rx = b.subscribe_json();
        p.slot(result(0, 0, "CQ DX W1AW FN31"));
        p.slot(result(0, 15_000, "CQ K1ABC RR73"));
        let got = spots(&mut rx);
        assert_eq!(got[0].grid.as_deref(), Some("FN31"));
        assert_eq!(got[1].grid, None);
    }
    #[test]
    fn frequency_applies_the_input_calibration() {
        let (p, b, _) = publisher(2.0);
        let mut rx = b.subscribe_json();
        p.slot(result(0, 0, "CQ W1AW FN31"));
        assert_eq!(spots(&mut rx)[0].freq_hz, 14_075_234.0 / 1.000002);
    }
    #[test]
    fn replay_order_is_a_pure_function_of_the_slots() {
        let run = |order: &[usize]| {
            let (p, b, _) = publisher(0.0);
            p.register(0, &[FT8], true);
            p.register(1, &[FT8], true);
            let mut rx = b.subscribe_json();
            let rows = [
                result(0, 0, "CQ W1AW FN31"),
                result(1, 0, "CQ K1ABC FN31"),
                result(0, 15_000, "CQ W2XYZ FN31"),
                result(1, 15_000, "CQ K2XYZ FN31"),
            ];
            for &i in order {
                p.slot(rows[i].clone());
            }
            p.input_ended(0);
            p.input_ended(1);
            spots(&mut rx)
        };
        let expected = run(&[0, 1, 2, 3]);
        assert_eq!(
            expected
                .iter()
                .map(|s| s.callsign.as_str())
                .collect::<Vec<_>>(),
            ["W1AW", "K1ABC", "W2XYZ", "K2XYZ"]
        );
        assert_eq!(expected, run(&[1, 3, 0, 2]));
    }
    #[test]
    fn an_input_ending_without_eof_releases_its_peers_results() {
        let (p, b, _) = publisher(0.0);
        p.register(0, &[FT8], true);
        p.register(1, &[FT8], true);
        let mut rx = b.subscribe_json();
        p.slot(result(0, 0, "CQ W1AW FN31"));
        p.slot(result(0, 15_000, "CQ K1ABC FN31"));
        assert!(spots(&mut rx).is_empty());
        // An I/O failure produces no tap EOF. The engine's end callback
        // releases that input while its sibling is still decoding.
        p.input_ended(1);
        assert_eq!(spots(&mut rx)[0].callsign, "W1AW");
        p.slot(result(0, 30_000, "CQ W2XYZ FN31"));
        assert_eq!(spots(&mut rx)[0].callsign, "K1ABC");
        p.input_ended(0);
        assert_eq!(spots(&mut rx)[0].callsign, "W2XYZ");
    }

    #[test]
    fn rig_audio_without_a_dial_has_no_rf_reference() {
        assert_eq!(plan("rig", &[FtxMode::Ft8],0.0,(300.0,3000.0),None,48_000.0).1,
            ["input rig: FT8 not attempted: this stream has no RF reference (set input.center_freq_hz or --dial-freq-hz)"]);
    }
    #[test]
    fn an_unsupported_rate_is_named() {
        assert_eq!(plan("sdr", &[FtxMode::Ft8],14_050_000.0,(-125_000.0,125_000.0),None,250_000.0).1,
            ["input sdr: FT8 on 20m not attempted: its 250000 S/s rate is not a multiple of 24000"]);
    }
    #[test]
    fn band_160m_has_no_ft4_window() {
        assert_eq!(
            plan(
                "hf160",
                &[FtxMode::Ft4],
                1_850_000.0,
                (-48_000.0, 48_000.0),
                None,
                96_000.0
            )
            .1,
            ["input hf160: FT4 on 160m not attempted: 160m has no FT4 window"]
        );
    }
    #[test]
    fn kiwi_20m_streams_report_each_window() {
        let band = manta_server::band_plan::parse_item("20m").unwrap();
        let streams = manta_server::band_plan::plan(
            &[band],
            (0.0, 30_000_000.0),
            crate::band_streams::usable_width_hz(10_000.0),
        )
        .unwrap();
        assert_eq!(streams.streams.len(), 12);
        for (i, stream) in streams.streams.iter().enumerate() {
            let (windows, lines) = plan(
                &stream.label,
                &FtxMode::ALL,
                stream.centre_hz,
                (-5000.0, 5000.0),
                None,
                96_000.0,
            );
            assert_eq!(lines.len(), 2);
            if i == 9 {
                assert_eq!(windows.len(), 2, "stream {i}: {lines:?}");
            } else {
                assert!(windows.is_empty(), "stream {i}: {lines:?}");
                assert!(lines.iter().all(|line| line.contains("not attempted")));
            }
            if i == 8 {
                assert_eq!(lines, [
                    "input 20m-9: FT8 on 20m not attempted: 14074.3-14077.0 kHz is only partly inside this stream's 14065.8-14075.8 kHz",
                    "input 20m-9: FT4 on 20m not attempted: 14080.3-14083.0 kHz is outside this stream's 14065.8-14075.8 kHz"
                ]);
            }
        }
        let (_, lines) = plan(
            "kiwi-20m-10",
            &FtxMode::ALL,
            14_079_166.666_7,
            (-5000.0, 5000.0),
            None,
            96_000.0,
        );
        assert_eq!(
            lines,
            [
                "input kiwi-20m-10: FT8 on 20m: decoding 14074.3-14077.0 kHz (dial 14074.0 kHz)",
                "input kiwi-20m-10: FT4 on 20m: decoding 14080.3-14083.0 kHz (dial 14080.0 kHz)"
            ]
        );
        let (_, lines) = plan(
            "kiwi-20m-11",
            &FtxMode::ALL,
            14_087_500.0,
            (-5000.0, 5000.0),
            None,
            96_000.0,
        );
        assert_eq!(lines,["input kiwi-20m-11: FT8 on 20m not attempted: 14074.3-14077.0 kHz is outside this stream's 14082.5-14092.5 kHz",
            "input kiwi-20m-11: FT4 on 20m not attempted: 14080.3-14083.0 kHz is only partly inside this stream's 14082.5-14092.5 kHz"]);
    }
    #[test]
    fn the_clock_line_follows_kernel_sync_state() {
        assert!(clock_line(Some(true)).is_none());
        assert_eq!(clock_line(Some(false)).as_deref(), Some("warning: the system clock is not synchronised to network time; FT8/FT4 slots are cut on UTC boundaries and decode only within about 1 s: turn on network time: `sudo timedatectl set-ntp true`, or install and start chrony"));
        assert_eq!(clock_line(None).as_deref(), Some("note: manta cannot read this platform's clock-sync state; FT8/FT4 slots are cut on UTC boundaries, so check the clock with `manta doctor`"));
    }
}
