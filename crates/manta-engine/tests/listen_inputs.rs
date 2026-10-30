//! MAN-13: several inputs feeding one validator (`open_inputs` /
//! `listen_inputs`). Scenario 1 (two receivers, overlapping segments, the
//! shared station spotted once), failure isolation (scenario 3) and replay
//! determinism, against in-memory sources.
//!
//! The overlap fixture is the one measured in the MAN-13 plan: `seg_a` is
//! 96 kS/s at 14.000 MHz (W1AW at 14_012_340 Hz, JA1ABC at 13_980_000 Hz),
//! `seg_b` 192 kS/s at 14.060 MHz (W1AW again, ZL2XYZ at 14_085_000 Hz);
//! 120 s, 20 dB, 20 WPM.

use manta_decode::events::DecoderEvent;
use manta_engine::{
    listen_inputs, listen_with_observers, open_inputs, InputEnd, InputSpec, ListenObservers,
    OpenedInputs, PipelineConfig, SessionClock, SharedObservers, Spot, MAX_INPUTS,
};
use manta_testkit::scene::{render_scene, SignalSpec};
use num_complex::Complex32;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// How a test source behaves once `fail_at` samples have been read.
#[derive(Clone, Copy, Debug)]
enum Fault {
    None,
    Error,
    Panic,
}

struct SceneSource {
    samples: Arc<Vec<Complex32>>,
    cursor: usize,
    fs: f64,
    center_freq_hz: f64,
    fail_at: usize,
    fault: Fault,
}

impl manta_input::IqSource for SceneSource {
    fn sample_rate(&self) -> f64 {
        self.fs
    }
    fn center_freq_hz(&self) -> f64 {
        self.center_freq_hz
    }
    fn read(&mut self, buf: &mut [Complex32]) -> anyhow::Result<usize> {
        if self.cursor >= self.fail_at {
            match self.fault {
                Fault::None => {}
                Fault::Error => anyhow::bail!("receiver unplugged"),
                Fault::Panic => panic!("driver bug"),
            }
        }
        let end = self.samples.len().min(self.fail_at.max(self.cursor + 1));
        let n = buf.len().min(end.saturating_sub(self.cursor));
        buf[..n].copy_from_slice(&self.samples[self.cursor..self.cursor + n]);
        self.cursor += n;
        Ok(n)
    }
}

/// Blocks in `read` until `stop` is set, then ends.
struct StalledSource {
    stop: Arc<AtomicBool>,
}

impl manta_input::IqSource for StalledSource {
    fn sample_rate(&self) -> f64 {
        96_000.0
    }
    fn center_freq_hz(&self) -> f64 {
        21_000_000.0
    }
    fn read(&mut self, _buf: &mut [Complex32]) -> anyhow::Result<usize> {
        while !self.stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(0)
    }
}

fn station(text: &str, offset_hz: f64) -> SignalSpec {
    SignalSpec {
        dit_units: 1.0,
        element_gap_units: 1.0,
        text: text.into(),
        loop_text: true,
        wpm: 20.0,
        offset_hz,
        snr_2500_db: 20.0,
        jitter: None,
        qsb: None,
        watterson: None,
        char_wpm: None,
        weight: 3.0,
        char_gap_units: 3.0,
        word_gap_units: 7.0,
        rise_ms: 5.0,
    }
}

struct Scene {
    samples: Arc<Vec<Complex32>>,
    fs: f64,
    center_freq_hz: f64,
}

fn render(signals: &[SignalSpec], fs: f64, center: f64, seconds: f64, seed: u64) -> Scene {
    let (samples, _) = render_scene(signals, fs, seconds, Some(seed)).unwrap();
    Scene {
        samples: Arc::new(samples),
        fs,
        center_freq_hz: center,
    }
}

const SEG_A_CENTER: f64 = 14_000_000.0;
const SEG_B_CENTER: f64 = 14_060_000.0;
const W1AW_HZ: f64 = 14_012_340.0;
const JA1ABC_HZ: f64 = 13_980_000.0;
const ZL2XYZ_HZ: f64 = 14_085_000.0;

fn seg_a() -> &'static Scene {
    static S: OnceLock<Scene> = OnceLock::new();
    S.get_or_init(|| {
        render(
            &[
                station("CQ CQ DE W1AW W1AW K", W1AW_HZ - SEG_A_CENTER),
                station("CQ CQ DE JA1ABC JA1ABC K", JA1ABC_HZ - SEG_A_CENTER),
            ],
            96_000.0,
            SEG_A_CENTER,
            120.0,
            11,
        )
    })
}

/// `ppm`: the receiver's oscillator error, so every station reads that many
/// ppm high.
fn seg_b(ppm: f64) -> Scene {
    let k = 1.0 + ppm * 1e-6;
    render(
        &[
            station("CQ CQ DE W1AW W1AW K", W1AW_HZ * k - SEG_B_CENTER),
            station("CQ CQ DE ZL2XYZ ZL2XYZ K", ZL2XYZ_HZ * k - SEG_B_CENTER),
        ],
        192_000.0,
        SEG_B_CENTER,
        120.0,
        22,
    )
}

fn seg_b_true() -> &'static Scene {
    static S: OnceLock<Scene> = OnceLock::new();
    S.get_or_init(|| seg_b(0.0))
}

/// 30 s of K5ARH at 14.012 MHz (V1-like), 96 kS/s.
fn short_scene() -> &'static Scene {
    static S: OnceLock<Scene> = OnceLock::new();
    S.get_or_init(|| {
        render(
            &[station("CQ CQ DE K5ARH K5ARH K", 12_340.0)],
            96_000.0,
            14_000_000.0,
            30.0,
            1,
        )
    })
}

/// 30 s of DL1ABC on 40 m, 96 kS/s.
fn forty_metre_scene() -> &'static Scene {
    static S: OnceLock<Scene> = OnceLock::new();
    S.get_or_init(|| {
        render(
            &[station("CQ CQ DE DL1ABC DL1ABC K", 10_000.0)],
            96_000.0,
            7_020_000.0,
            30.0,
            3,
        )
    })
}

fn spec(name: &str, scene: &Scene, ppm: f64, fail_at: usize, fault: Fault) -> InputSpec {
    let samples = scene.samples.clone();
    let (fs, center) = (scene.fs, scene.center_freq_hz);
    InputSpec {
        name: name.into(),
        open: Box::new(move || {
            Ok(Box::new(SceneSource {
                samples,
                cursor: 0,
                fs,
                center_freq_hz: center,
                fail_at,
                fault,
            }) as Box<dyn manta_input::IqSource>)
        }),
        freq_correction_ppm: ppm,
        observers: Default::default(),
        spawn_band_hz: None,
    }
}

fn plain(name: &str, scene: &Scene) -> InputSpec {
    spec(name, scene, 0.0, usize::MAX, Fault::None)
}

struct Run {
    spots: Vec<Spot>,
    events: Vec<DecoderEvent>,
    ends: Vec<InputEnd>,
}

fn run_opened(opened: OpenedInputs, clock: SessionClock, cfg: &PipelineConfig) -> Run {
    let mut spots = Vec::new();
    let mut events = Vec::new();
    let ends = listen_inputs(
        opened,
        cfg,
        clock,
        SharedObservers::default(),
        |e| events.push(e.clone()),
        |s| spots.push(s.clone()),
        |_input, _n| {},
        |_end| {},
    )
    .unwrap();
    Run {
        spots,
        events,
        ends,
    }
}

fn run(specs: Vec<InputSpec>) -> Run {
    let stop = Arc::new(AtomicBool::new(false));
    run_opened(
        open_inputs(specs, stop).unwrap(),
        SessionClock::Replay,
        &PipelineConfig::default(),
    )
}

fn call_counts(spots: &[Spot]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for s in spots {
        *counts.entry(s.callsign.clone()).or_insert(0) += 1;
    }
    counts
}

fn counts(pairs: &[(&str, usize)]) -> BTreeMap<String, usize> {
    pairs.iter().map(|(c, n)| (c.to_string(), *n)).collect()
}

fn overlap() -> Run {
    run(vec![plain("a", seg_a()), plain("b", seg_b_true())])
}

#[test]
fn one_input_through_listen_inputs_matches_listen_with_observers() {
    let scene = short_scene();
    let mut want_spots = Vec::new();
    let mut want_events = Vec::new();
    listen_with_observers(
        Box::new(SceneSource {
            samples: scene.samples.clone(),
            cursor: 0,
            fs: scene.fs,
            center_freq_hz: scene.center_freq_hz,
            fail_at: usize::MAX,
            fault: Fault::None,
        }),
        &PipelineConfig::default(),
        Arc::new(AtomicBool::new(false)),
        ListenObservers::default(),
        |e| want_events.push(e.clone()),
        |s| want_spots.push(s.clone()),
        |_| {},
    )
    .unwrap();
    let got = run(vec![plain("only", scene)]);
    assert!(!want_spots.is_empty());
    assert_eq!(format!("{:?}", got.spots), format!("{want_spots:?}"));
    assert_eq!(format!("{:?}", got.events), format!("{want_events:?}"));
    assert!(got.ends[0].result.is_ok());
}

#[test]
fn overlapping_inputs_spot_the_shared_station_once() {
    let got = overlap();
    assert_eq!(
        call_counts(&got.spots),
        counts(&[("JA1ABC", 1), ("W1AW", 1), ("ZL2XYZ", 1)]),
        "{:#?}",
        got.spots
    );
    assert!(got.ends.iter().all(|e| e.result.is_ok()));
}

#[test]
fn multi_input_replay_is_deterministic_across_three_runs() {
    let first = overlap();
    let text = |r: &Run| format!("{:?}\n{:?}", r.spots, r.events);
    let want = text(&first);
    for _ in 0..2 {
        assert_eq!(text(&overlap()), want);
    }
}

fn event_ts(e: &DecoderEvent) -> Option<u64> {
    match e {
        DecoderEvent::CharDecoded { sample_ts, .. }
        | DecoderEvent::WordBoundary { sample_ts, .. }
        | DecoderEvent::TrackMeta { sample_ts, .. }
        | DecoderEvent::ReadingsHeld { sample_ts, .. }
        | DecoderEvent::TimingUpdate { sample_ts, .. }
        | DecoderEvent::TrackPromoted { sample_ts, .. } => Some(*sample_ts),
        _ => None,
    }
}

fn event_track(e: &DecoderEvent) -> u32 {
    match e {
        DecoderEvent::CharDecoded { track_id, .. }
        | DecoderEvent::WordBoundary { track_id, .. }
        | DecoderEvent::SpeedUpdate { track_id, .. }
        | DecoderEvent::TrackMeta { track_id, .. }
        | DecoderEvent::ReadingsHeld { track_id, .. }
        | DecoderEvent::TimingUpdate { track_id, .. }
        | DecoderEvent::TrackPromoted { track_id, .. }
        | DecoderEvent::TrackClosed { track_id, .. } => *track_id,
    }
}

#[test]
fn spot_timestamps_share_one_clock_across_rates() {
    let got = overlap();
    let end = (120.0 * 192_000.0) as u64;
    // Input 0 (odd ids) runs at 96 kS/s on a 192 kS/s clock: every one of
    // its timestamps is a whole number of its own samples, so even.
    for e in &got.events {
        if let Some(ts) = event_ts(e) {
            assert!(ts <= end + 192_000, "{e:?} is past the session's end");
            if event_track(e) % 2 == 1 {
                assert_eq!(ts % 2, 0, "{e:?} is not on input a's 2x grid");
            }
        }
    }
    for s in &got.spots {
        let secs = s.sample_ts as f64 / 192_000.0;
        assert!(secs > 2.0 && secs <= 120.0, "{s:?} at {secs} s");
    }
}

#[test]
fn track_ids_are_unique_across_inputs() {
    let got = overlap();
    let promoted: Vec<(u32, f64)> = got
        .events
        .iter()
        .filter_map(|e| match e {
            DecoderEvent::TrackPromoted {
                track_id, freq_hz, ..
            } => Some((*track_id, *freq_hz)),
            _ => None,
        })
        .collect();
    let mut ids: Vec<u32> = promoted.iter().map(|p| p.0).collect();
    ids.sort_unstable();
    let n = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), n, "a track id was promoted twice");
    // JA1ABC is only in input a's segment (odd ids), ZL2XYZ only in b's.
    for (id, f) in &promoted {
        if (f - JA1ABC_HZ).abs() < 100.0 {
            assert_eq!(id % 2, 1, "JA1ABC track {id} is not input a's");
        }
        if (f - ZL2XYZ_HZ).abs() < 100.0 {
            assert_eq!(id % 2, 0, "ZL2XYZ track {id} is not input b's");
        }
    }
    let spot_ids: Vec<u32> = got.spots.iter().map(|s| s.track_id).collect();
    assert!(spot_ids.iter().all(|id| *id >= 1));
}

#[test]
fn each_inputs_ppm_applies_to_its_own_frequencies() {
    // Input b reads 20 ppm high: W1AW 280 Hz up, across a 300 Hz dedupe
    // bucket edge and more than one channel from input a's report. Its own
    // correction brings it back.
    let skewed = seg_b(20.0);
    let correction = -20.0 / (1.0 + 20e-6);
    let got = run(vec![
        plain("a", seg_a()),
        spec("b", &skewed, correction, usize::MAX, Fault::None),
    ]);
    assert_eq!(
        call_counts(&got.spots),
        counts(&[("JA1ABC", 1), ("W1AW", 1), ("ZL2XYZ", 1)]),
        "{:#?}",
        got.spots
    );
    let zl = got.spots.iter().find(|s| s.callsign == "ZL2XYZ").unwrap();
    assert!(
        (zl.freq_hz - ZL2XYZ_HZ).abs() < 60.0,
        "ZL2XYZ at {} Hz: input b's correction was not applied",
        zl.freq_hz
    );
    let ja = got.spots.iter().find(|s| s.callsign == "JA1ABC").unwrap();
    assert!((ja.freq_hz - JA1ABC_HZ).abs() < 60.0, "{ja:?}");
}

#[test]
fn a_failing_input_does_not_stop_the_others() {
    let twenty_s = 20 * 96_000;
    let got = run(vec![
        plain("a", short_scene()),
        spec("b", short_scene(), 0.0, twenty_s, Fault::Error),
        plain("c", forty_metre_scene()),
    ]);
    let calls = call_counts(&got.spots);
    assert!(calls.contains_key("K5ARH"), "{calls:?}");
    assert!(calls.contains_key("DL1ABC"), "{calls:?}");
    assert!(got.ends[0].result.is_ok());
    let err = format!("{:#}", got.ends[1].result.as_ref().unwrap_err());
    assert!(err.contains("receiver unplugged"), "{err}");
    assert!(got.ends[2].result.is_ok());
    assert_eq!(got.ends[1].name, "b");
}

#[test]
fn a_panicking_input_is_isolated() {
    let got = run(vec![
        plain("a", short_scene()),
        spec("b", short_scene(), 0.0, 10 * 96_000, Fault::Panic),
        plain("c", forty_metre_scene()),
    ]);
    let calls = call_counts(&got.spots);
    assert!(calls.contains_key("K5ARH"), "{calls:?}");
    assert!(calls.contains_key("DL1ABC"), "{calls:?}");
    let err = format!("{:#}", got.ends[1].result.as_ref().unwrap_err());
    assert_eq!(err, "input b: pipeline panicked");
    assert!(got.ends[0].result.is_ok() && got.ends[2].result.is_ok());
}

#[test]
fn live_merge_does_not_wait_for_a_stalled_input() {
    let stop = Arc::new(AtomicBool::new(false));
    let stalled_stop = stop.clone();
    let specs = vec![
        plain("a", short_scene()),
        InputSpec {
            name: "b".into(),
            open: Box::new(move || {
                Ok(Box::new(StalledSource { stop: stalled_stop })
                    as Box<dyn manta_input::IqSource>)
            }),
            freq_correction_ppm: 0.0,
            observers: Default::default(),
            spawn_band_hz: None,
        },
    ];
    let opened = open_inputs(specs, stop.clone()).unwrap();
    // A watchdog, so a merge that waits on the stalled input fails the test
    // instead of hanging it.
    let watchdog_stop = stop.clone();
    let watchdog_fired = Arc::new(AtomicBool::new(false));
    let fired = watchdog_fired.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(120));
        fired.store(true, Ordering::Relaxed);
        watchdog_stop.store(true, Ordering::Relaxed);
    });
    let mut spotted_before_stop = false;
    listen_inputs(
        opened,
        &PipelineConfig::default(),
        SessionClock::Live {
            offsets: vec![Duration::ZERO, Duration::from_millis(3)],
        },
        SharedObservers::default(),
        |_e| {},
        |s| {
            if s.callsign == "K5ARH" && !stop.load(Ordering::Relaxed) {
                spotted_before_stop = true;
                stop.store(true, Ordering::Relaxed);
            }
        },
        |_, _| {},
        |_| {},
    )
    .unwrap();
    assert!(spotted_before_stop);
    assert!(!watchdog_fired.load(Ordering::Relaxed));
}

#[test]
fn open_inputs_fails_fast_naming_the_input() {
    let specs = vec![
        plain("a", short_scene()),
        InputSpec {
            name: "b".into(),
            open: Box::new(|| anyhow::bail!("boom")),
            freq_correction_ppm: 0.0,
            observers: Default::default(),
            spawn_band_hz: None,
        },
        plain("c", forty_metre_scene()),
    ];
    let err = match open_inputs(specs, Arc::new(AtomicBool::new(false))) {
        Ok(_) => panic!("an input failed to open, so startup must fail"),
        Err(e) => format!("{e:#}"),
    };
    assert_eq!(err, "input \"b\" failed to open: boom");
}

#[test]
fn open_inputs_does_not_wait_for_a_hung_opener_after_a_failure() {
    // Input "a" (first in config order) stays inside its open until the
    // test releases it; input "b" fails at once. open_inputs must return
    // b's error without waiting for "a": collecting the results in config
    // order would hide the failure behind the hung opener indefinitely.
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let specs = vec![
        InputSpec {
            name: "a".into(),
            open: Box::new(move || {
                // Stands in for an opener blocked in a connect: it returns
                // only once the test lets it.
                let _ = release_rx.recv();
                anyhow::bail!("a was never opened")
            }),
            freq_correction_ppm: 0.0,
            observers: Default::default(),
            spawn_band_hz: None,
        },
        InputSpec {
            name: "b".into(),
            open: Box::new(|| anyhow::bail!("boom")),
            freq_correction_ppm: 0.0,
            observers: Default::default(),
            spawn_band_hz: None,
        },
    ];
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = result_tx.send(open_inputs(specs, Arc::new(AtomicBool::new(false))));
    });
    // A bound, so the pre-fix behaviour fails the test instead of
    // hanging it.
    let err = match result_rx.recv_timeout(Duration::from_secs(30)) {
        Ok(Ok(_)) => panic!("an input failed to open, so startup must fail"),
        Ok(Err(e)) => format!("{e:#}"),
        Err(_) => panic!("open_inputs still waits for the hung opener \"a\" after \"b\" failed"),
    };
    assert_eq!(err, "input \"b\" failed to open: boom");
    // Let the detached "a" thread finish and exit.
    drop(release_tx);
}

/// Opens fine, then panics when asked for its metadata.
struct PanickingMetadataSource;

impl manta_input::IqSource for PanickingMetadataSource {
    fn sample_rate(&self) -> f64 {
        panic!("driver bug reading the sample rate")
    }
    fn center_freq_hz(&self) -> f64 {
        14_000_000.0
    }
    fn read(&mut self, _buf: &mut [Complex32]) -> anyhow::Result<usize> {
        Ok(0)
    }
}

/// Codex (PR 313): an input whose opener succeeds but whose source panics
/// while its metadata is read must fail startup naming the input, not
/// leave `open_inputs` waiting forever for a completion that thread will
/// never send.
#[test]
fn open_inputs_fails_when_a_source_panics_reading_its_metadata() {
    let specs = vec![
        plain("a", short_scene()),
        InputSpec {
            name: "b".into(),
            open: Box::new(|| {
                Ok(Box::new(PanickingMetadataSource) as Box<dyn manta_input::IqSource>)
            }),
            freq_correction_ppm: 0.0,
            observers: Default::default(),
            spawn_band_hz: None,
        },
    ];
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = result_tx.send(open_inputs(specs, Arc::new(AtomicBool::new(false))));
    });
    // A bound, so the pre-fix hang fails the test instead of hanging it.
    let err = match result_rx.recv_timeout(Duration::from_secs(30)) {
        Ok(Ok(_)) => panic!("input b panicked while opening, so startup must fail"),
        Ok(Err(e)) => format!("{e:#}"),
        Err(_) => panic!("open_inputs hangs after input b's metadata read panicked"),
    };
    assert_eq!(
        err,
        "input \"b\" failed to open: the source panicked while opening"
    );
}

#[test]
fn stop_ends_every_input() {
    let stop = Arc::new(AtomicBool::new(false));
    let opened = open_inputs(
        vec![plain("a", short_scene()), plain("b", forty_metre_scene())],
        stop.clone(),
    )
    .unwrap();
    stop.store(true, Ordering::Relaxed);
    let got = run_opened(opened, SessionClock::Replay, &PipelineConfig::default());
    assert_eq!(got.ends.len(), 2);
    assert!(got.ends.iter().all(|e| e.result.is_ok()));
    assert!(got.spots.is_empty());
}

#[test]
fn more_than_max_inputs_is_refused() {
    let specs = (0..=MAX_INPUTS)
        .map(|i| plain(&format!("in{i}"), short_scene()))
        .collect();
    let err = match open_inputs(specs, Arc::new(AtomicBool::new(false))) {
        Ok(_) => panic!("17 inputs must be refused"),
        Err(e) => e.to_string(),
    };
    assert_eq!(err, "at most 16 inputs are supported, found 17");
}

/// Records every live-state record.
#[derive(Default)]
struct Recorder(std::sync::Mutex<Vec<manta_engine::LiveRecord>>);

impl manta_engine::LiveStateSink for Recorder {
    fn wants_records(&self) -> bool {
        true
    }
    fn publish(&self, record: manta_engine::LiveRecord) {
        self.0.lock().unwrap().push(record);
    }
}

#[test]
fn each_input_publishes_its_own_band_sample_once_per_stream_second() {
    let stop = Arc::new(AtomicBool::new(false));
    let opened = open_inputs(
        vec![
            plain("20m", short_scene()),
            plain("40m", forty_metre_scene()),
        ],
        stop,
    )
    .unwrap();
    let recorder = Arc::new(Recorder::default());
    let mut spots = Vec::new();
    listen_inputs(
        opened,
        &PipelineConfig::default(),
        SessionClock::Replay,
        SharedObservers {
            live_state: Some(recorder.clone()),
            ..Default::default()
        },
        |_| {},
        |s| spots.push(s.clone()),
        |_input, _n| {},
        |_end| {},
    )
    .unwrap();
    let records = recorder.0.lock().unwrap();
    let bands: Vec<&manta_engine::BandSample> = records
        .iter()
        .filter_map(|r| match r {
            manta_engine::LiveRecord::Bands(b) => Some(b),
            _ => None,
        })
        .collect();
    for (input, center) in [(0, 14_000_000.0), (1, 7_020_000.0)] {
        let mine: Vec<_> = bands.iter().filter(|b| b.input == input).collect();
        // 30 s of stream: one sample per second after the calibration fill.
        assert!(
            (28..=30).contains(&mine.len()),
            "input {input}: {} band samples",
            mine.len()
        );
        assert!(mine.windows(2).all(|w| w[0].sample_ts < w[1].sample_ts));
        // Each input's keyed station occupies its channel (MAN-324's window).
        assert!(mine.iter().flat_map(|b| &b.channels).any(|c| c.occupied));
        assert!(mine
            .iter()
            .flat_map(|b| &b.channels)
            .all(|c| (c.freq_hz - center).abs() <= 48_000.0));
    }
    assert_eq!(bands.len(), bands.iter().filter(|b| b.input < 2).count());
    // The tap changes no spot.
    assert_eq!(
        call_counts(&spots),
        call_counts(
            &run(vec![
                plain("20m", short_scene()),
                plain("40m", forty_metre_scene())
            ])
            .spots
        )
    );
}

/// An input that errors or panics mid-transmission still closes every track
/// it reported (Codex, PR 313): the shared validator frees their state and
/// the live tap publishes their closure, while the other inputs run on.
#[test]
fn a_failing_input_closes_every_track_it_reported() {
    for fault in [Fault::Error, Fault::Panic] {
        let stop = Arc::new(AtomicBool::new(false));
        let opened = open_inputs(
            vec![
                plain("a", forty_metre_scene()),
                spec("b", short_scene(), 0.0, 20 * 96_000, fault),
            ],
            stop,
        )
        .unwrap();
        let recorder = Arc::new(Recorder::default());
        let mut events = Vec::new();
        listen_inputs(
            opened,
            &PipelineConfig::default(),
            SessionClock::Replay,
            SharedObservers {
                live_state: Some(recorder.clone()),
                ..Default::default()
            },
            |e| events.push(e.clone()),
            |_| {},
            |_input, _n| {},
            |_end| {},
        )
        .unwrap();
        // Input b, the second of two, numbers its tracks 2, 4, 6, ...
        let reported: BTreeSet<u32> = events
            .iter()
            .filter(|e| {
                !matches!(
                    e,
                    DecoderEvent::TrackPromoted { .. } | DecoderEvent::TrackClosed { .. }
                )
            })
            .map(event_track)
            .filter(|id| id % 2 == 0)
            .collect();
        assert!(!reported.is_empty(), "{fault:?}: input b reported nothing");
        let closed: BTreeSet<u32> = events
            .iter()
            .filter_map(|e| match e {
                DecoderEvent::TrackClosed { track_id, .. } => Some(*track_id),
                _ => None,
            })
            .collect();
        let open: Vec<&u32> = reported.difference(&closed).collect();
        assert!(open.is_empty(), "{fault:?}: tracks {open:?} never closed");
        let tapped: BTreeSet<u32> = recorder
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|r| match r {
                manta_engine::LiveRecord::TrackClosed { track_id, .. } => Some(*track_id),
                _ => None,
            })
            .collect();
        assert!(
            reported.is_subset(&tapped),
            "{fault:?}: the live tap kept {:?}",
            reported.difference(&tapped).collect::<Vec<_>>()
        );
    }
}

/// Every way an input ends finishes its tracks (validation, PR 313): an
/// input whose read errors mid-transmission finishes them on its own front
/// end and sends a closing batch, so its track count settles to 0 in
/// `on_batch`, as a clean end does. (A panic can send nothing; the back
/// end retires its tracks and `on_input_end` its gauges.)
#[test]
fn an_input_whose_read_fails_settles_its_track_count_to_zero() {
    {
        let fault = Fault::Error;
        let opened = open_inputs(
            vec![
                plain("a", forty_metre_scene()),
                spec("b", short_scene(), 0.0, 20 * 96_000, fault),
            ],
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let mut counts: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        listen_inputs(
            opened,
            &PipelineConfig::default(),
            SessionClock::Replay,
            SharedObservers::default(),
            |_| {},
            |_| {},
            |input, n| counts.entry(input).or_default().push(n),
            |_end| {},
        )
        .unwrap();
        let b = &counts[&1];
        assert!(
            b.iter().any(|&n| n > 0),
            "{fault:?}: test setup: input b must be decoding when it fails"
        );
        assert_eq!(
            b.last(),
            Some(&0),
            "{fault:?}: input b's last track count must be 0"
        );
    }
}

/// MAN-234 (Codex, PR 313): each input's front end counts the samples its
/// own channelizer replaced, so a multi-input run can warn and publish per
/// source as the one-input path does: a poisoned input raises only its own
/// counter, a clean input beside it stays at 0.
#[test]
fn an_inputs_replaced_samples_reach_only_its_own_counter() {
    let scene = forty_metre_scene();
    let mut poisoned: Vec<Complex32> = scene.samples.as_ref().clone();
    poisoned[1_000_000].re = f32::NAN;
    poisoned[2_000_000].im = f32::INFINITY;
    let bad: Vec<Arc<AtomicU64>> = (0..2).map(|_| Arc::new(AtomicU64::new(0))).collect();
    let mk = |name: &str, samples: Arc<Vec<Complex32>>, counter: Arc<AtomicU64>| {
        let (fs, center) = (scene.fs, scene.center_freq_hz);
        InputSpec {
            name: name.into(),
            open: Box::new(move || {
                Ok(Box::new(SceneSource {
                    samples,
                    cursor: 0,
                    fs,
                    center_freq_hz: center,
                    fail_at: usize::MAX,
                    fault: Fault::None,
                }) as Box<dyn manta_input::IqSource>)
            }),
            freq_correction_ppm: 0.0,
            observers: manta_engine::InputObservers {
                bad_samples: Some(counter),
                ..Default::default()
            },
            spawn_band_hz: None,
        }
    };
    let specs = vec![
        mk("a", scene.samples.clone(), bad[0].clone()),
        mk("b", Arc::new(poisoned), bad[1].clone()),
    ];
    let stop = Arc::new(AtomicBool::new(false));
    let opened = open_inputs(specs, stop).unwrap();
    listen_inputs(
        opened,
        &PipelineConfig::default(),
        SessionClock::Replay,
        SharedObservers::default(),
        |_e| {},
        |_s| {},
        |_input, _n| {},
        |_end| {},
    )
    .unwrap();
    assert_eq!(bad[0].load(Ordering::Relaxed), 0);
    assert!(
        bad[1].load(Ordering::Relaxed) >= 2,
        "poisoned input counted {} bad samples",
        bad[1].load(Ordering::Relaxed)
    );
}

/// MAN-310: the planned 15 m split at 96 kS/s (`band_plan::plan`).
const M310_LO_CENTER: f64 = 21_037_500.0;
const M310_HI_CENTER: f64 = 21_112_500.0;
const M310_BOUND_HZ: f64 = 37_593.75;

fn split_spec(name: &str, scene: &Scene, spawn_band_hz: (f64, f64)) -> InputSpec {
    let mut s = plain(name, scene);
    s.spawn_band_hz = Some(spawn_band_hz);
    s
}

fn split_pair(signals: &[(&str, f64)], seed: u64) -> Run {
    let scene = |center: f64, seed: u64| {
        let stations: Vec<SignalSpec> = signals
            .iter()
            .map(|(text, hz)| station(text, hz - center))
            .collect();
        render(&stations, 96_000.0, center, 120.0, seed)
    };
    // MAN-311: these split fixtures sit at 21 075 kHz (15 m FT8 window) and
    // 21 080 kHz (15 m JS8 window), and both frequencies are load-bearing
    // for the split geometry under test, so they opt out of the
    // digital-window spawn veto; the veto itself is covered by
    // crates/manta-engine/tests/digital_windows.rs.
    let mut cfg = PipelineConfig::default();
    cfg.detector.digital_window_filter = false;
    let stop = Arc::new(AtomicBool::new(false));
    run_opened(
        open_inputs(
            vec![
                split_spec(
                    "15m-1",
                    &scene(M310_LO_CENTER, seed),
                    (f64::NEG_INFINITY, M310_BOUND_HZ),
                ),
                split_spec(
                    "15m-2",
                    &scene(M310_HI_CENTER, seed + 1),
                    (-M310_BOUND_HZ, f64::INFINITY),
                ),
            ],
            stop,
        )
        .unwrap(),
        SessionClock::Replay,
        &cfg,
    )
}

/// MAN-310 scenario 3: a signal in a split band's overlap is tracked only
/// by the stream whose centre is nearer, and spotted once.
#[test]
fn a_split_bands_overlap_signal_is_decoded_once_by_the_nearer_stream() {
    const DL1ABC_HZ: f64 = 21_070_000.0;
    const JA1XYZ_HZ: f64 = 21_080_000.0;
    let got = split_pair(
        &[
            ("CQ CQ DE DL1ABC DL1ABC K", DL1ABC_HZ),
            ("CQ CQ DE JA1XYZ JA1XYZ K", JA1XYZ_HZ),
        ],
        31,
    );
    assert_eq!(
        call_counts(&got.spots),
        counts(&[("DL1ABC", 1), ("JA1XYZ", 1)]),
        "{:#?}",
        got.spots
    );
    // D3: input i numbers its tracks i+1, i+1+n, ...
    let input_of = |track_id: u32| (track_id - 1) % 2;
    for s in &got.spots {
        let want = if s.callsign == "DL1ABC" { 0 } else { 1 };
        assert_eq!(input_of(s.track_id), want, "{s:?}");
    }
    for e in &got.events {
        if let DecoderEvent::TrackPromoted {
            track_id, freq_hz, ..
        } = e
        {
            let input = input_of(*track_id);
            assert!(
                !(input == 1 && (freq_hz - DL1ABC_HZ).abs() <= 93.75),
                "the upper stream promoted DL1ABC: {e:?}"
            );
            assert!(
                !(input == 0 && (freq_hz - JA1XYZ_HZ).abs() <= 93.75),
                "the lower stream promoted JA1XYZ: {e:?}"
            );
        }
    }
    assert!(got.ends.iter().all(|e| e.result.is_ok()));
}

/// MAN-310: the one-channel guard leaves no gap at the split point.
#[test]
fn a_signal_on_the_split_point_is_spotted_once() {
    let got = split_pair(&[("CQ CQ DE OH2XX OH2XX K", 21_075_000.0)], 41);
    assert_eq!(
        call_counts(&got.spots),
        counts(&[("OH2XX", 1)]),
        "{:#?}",
        got.spots
    );
}
