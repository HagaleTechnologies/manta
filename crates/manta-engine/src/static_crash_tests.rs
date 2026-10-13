//! MAN-312 (SPEC §2.7, §7.2): pipeline tests for the static-crash filter,
//! driving the real channelizer -> `TrackManager` -> decoder -> `Validator`
//! chain the way `decode_samples` does (zero pre-roll skipped, 4096-sample
//! chunks). In-crate because `TrackManager` and its crash count are not
//! public. Every crash test also checks that `"off"` behaves differently,
//! so none can pass vacuously.

use crate::track::{crash_live_channels, CrashFilter, TrackManager};
use crate::{primary_track_id, PipelineConfig};
use manta_decode::decoder::Engine;
use manta_decode::events::{ClosureKind, DecoderEvent};
use manta_decode::tree::Glyph;
use manta_dsp::channelizer::Channelizer;
use manta_testkit::crash::{
    add_crash, dense_scene, kiwi_like_96k, render_with_crashes, vc1_cases, vc1_crash, vc1_scene,
    vc2, Crash, KIWI_RESIDUAL_DB, KIWI_RESIDUAL_SEED,
};
use manta_testkit::vectors::{render, v1};
use num_complex::Complex32;

struct Run {
    events: Vec<DecoderEvent>,
    crashes: u64,
    unconfirmed: u64,
    spots: Vec<String>,
}

impl Run {
    fn promoted(&self) -> usize {
        self.events
            .iter()
            .filter(|e| matches!(e, DecoderEvent::TrackPromoted { .. }))
            .count()
    }

    fn primary(&self) -> u32 {
        primary_track_id(&self.events).expect("a track decoded characters")
    }

    /// Dits in the primary track's decoded glyphs (ITU Morse).
    fn dits(&self) -> usize {
        let id = self.primary();
        self.events
            .iter()
            .filter_map(|e| match e {
                DecoderEvent::CharDecoded {
                    track_id,
                    glyph: Glyph::Char(c),
                    ..
                } if *track_id == id => Some(morse(*c).chars().filter(|&m| m == '.').count()),
                _ => None,
            })
            .sum()
    }

    /// The primary track's `TrackClosed` closures, in order.
    fn primary_closes(&self) -> Vec<ClosureKind> {
        let id = self.primary();
        self.events
            .iter()
            .filter_map(|e| match e {
                DecoderEvent::TrackClosed {
                    track_id, closure, ..
                } if *track_id == id => Some(*closure),
                _ => None,
            })
            .collect()
    }

    fn spotted(&self, call: &str) -> bool {
        self.spots.iter().any(|s| s == call)
    }
}

fn morse(c: char) -> &'static str {
    match c.to_ascii_uppercase() {
        'A' => ".-",
        'B' => "-...",
        'C' => "-.-.",
        'D' => "-..",
        'E' => ".",
        'F' => "..-.",
        'G' => "--.",
        'H' => "....",
        'I' => "..",
        'J' => ".---",
        'K' => "-.-",
        'L' => ".-..",
        'M' => "--",
        'N' => "-.",
        'O' => "---",
        'P' => ".--.",
        'Q' => "--.-",
        'R' => ".-.",
        'S' => "...",
        'T' => "-",
        'U' => "..-",
        'V' => "...-",
        'W' => ".--",
        'X' => "-..-",
        'Y' => "-.--",
        'Z' => "--..",
        '0' => "-----",
        '1' => ".----",
        '2' => "..---",
        '3' => "...--",
        '4' => "....-",
        '5' => ".....",
        '6' => "-....",
        '7' => "--...",
        '8' => "---..",
        '9' => "----.",
        '=' => "-...-",
        '/' => "-..-.",
        '?' => "..--..",
        '.' => ".-.-.-",
        ',' => "--..--",
        _ => "",
    }
}

/// `decode_samples`' chain with the crash filter set to `mode`, the decode
/// engine to `engine`, and the detector's passband narrowed when `passband`
/// is given (as `listen` does from `IqSource::rf_passband_hz`).
fn run(
    iq: &[Complex32],
    fs: f64,
    center_freq_hz: f64,
    mode: CrashFilter,
    engine: Engine,
    passband: Option<(f64, f64)>,
    chunk: usize,
) -> Run {
    let mut cfg = PipelineConfig::default();
    cfg.detector.crash_filter = mode;
    cfg.decode.engine = engine;
    let mut ch = Channelizer::new(fs, center_freq_hz).unwrap();
    let hop = ch.hop() as u64;
    let pad = ch.filter_len();
    let pad_hops = (pad as u64).div_ceil(hop);
    let mut tm = TrackManager::new(
        ch.n_channels(),
        fs,
        center_freq_hz,
        cfg.detector,
        cfg.decode.clone(),
    );
    tm.skip_preroll_hops(pad_hops);
    if let Some((lo, hi)) = passband {
        tm.set_rf_passband_hz(lo, hi);
    }
    let mut padded = vec![Complex32::new(0.0, 0.0); pad];
    padded.extend_from_slice(iq);
    let mut events = Vec::new();
    for c in padded.chunks(chunk) {
        let hops = ch.process(c);
        events.extend(tm.process_hops(&hops, |m| m.saturating_sub(pad_hops) * hop));
    }
    events.extend(tm.finish());
    let mut validator = cfg.validator(fs).unwrap();
    let spots = events
        .iter()
        .flat_map(|e| validator.ingest(e))
        .map(|s| s.callsign)
        .collect();
    Run {
        crashes: tm.static_crash_count(),
        unconfirmed: tm.close_counts().unconfirmed,
        events,
        spots,
    }
}

fn run_default(iq: &[Complex32], fs: f64, mode: CrashFilter) -> Run {
    run(iq, fs, 0.0, mode, Engine::Legacy, None, 4096)
}

fn with_crash(base: &[Complex32], fs: f64, c: &Crash) -> Vec<Complex32> {
    let mut iq = base.to_vec();
    add_crash(&mut iq, fs, c);
    iq
}

/// VC2's samples with its 24 crashes at `level_db`, and without.
fn vc2_samples(level_db: f64, duration_s: Option<f64>) -> (Vec<Complex32>, Vec<Complex32>, f64) {
    let (mut spec, mut crashes) = vc2(level_db);
    if let Some(d) = duration_s {
        // A VC2 cut keeps VC2's own rule: crashes only while t < duration - 1.
        spec.duration_s = d;
        crashes.retain(|c| c.start_s < d - 1.0);
    }
    let clean = render(&spec).unwrap().samples;
    let crashed = render_with_crashes(&spec, &crashes).unwrap().samples;
    (clean, crashed, spec.fs)
}

/// Scenario 1: on a signal-free band, a crash starts no track under
/// `"full"` or `"starts-only"`, and each crash of +20 dB or more is
/// counted once in every mode.
#[test]
fn vc1_crashes_start_no_tracks() {
    let fs = 96_000.0;
    let base = vc1_scene(fs);
    let clean = run_default(&base, fs, CrashFilter::Full);
    assert_eq!(clean.promoted(), 0);
    assert_eq!(clean.crashes, 0);
    for (width_ms, level_db) in vc1_cases() {
        let iq = with_crash(&base, fs, &vc1_crash(fs, width_ms, level_db));
        for mode in [CrashFilter::Full, CrashFilter::StartsOnly, CrashFilter::Off] {
            let r = run_default(&iq, fs, mode);
            let case = format!("{width_ms} ms +{level_db} dB {mode:?}");
            assert_eq!(r.crashes, 1, "{case}: crash count");
            if mode != CrashFilter::Off {
                assert_eq!(r.promoted(), 0, "{case}: promotions");
                assert_eq!(r.unconfirmed, clean.unconfirmed, "{case}: candidates");
            } else if width_ms == 5.0 && level_db == 40.0 {
                // Today's behaviour: measured 177.
                assert!(r.promoted() > 0, "{case}: off should flood");
            }
        }
    }
}

/// VC1 at 48 and 192 kHz: 5 ms +40 dB starts no track under `"full"` and
/// floods under `"off"` (measured 99 and 412).
#[test]
fn vc1_other_rates() {
    for fs in [48_000.0, 192_000.0] {
        let iq = with_crash(&vc1_scene(fs), fs, &vc1_crash(fs, 5.0, 40.0));
        let full = run_default(&iq, fs, CrashFilter::Full);
        assert_eq!(full.promoted(), 0, "{fs} Hz full");
        assert_eq!(full.crashes, 1, "{fs} Hz full");
        let off = run_default(&iq, fs, CrashFilter::Off);
        assert!(off.promoted() > 0, "{fs} Hz off");
    }
}

/// VC1 through a KiwiSDR-shaped ±5 kHz passband: judged over the passband
/// the crash is caught; judged over all 1024 channels it is missed, which
/// is why `set_rf_passband_hz` exists.
#[test]
fn vc1_kiwi_passband() {
    let fs12 = 12_000.0;
    let fs = 96_000.0;
    let kiwi = Some((-5000.0, 5000.0));
    let base12 = vc1_scene(fs12);
    for (width_ms, level_db) in [(5.0, 40.0), (1.0, 60.0), (500.0, 20.0), (5.0, 6.0)] {
        let iq12 = with_crash(&base12, fs12, &vc1_crash(fs12, width_ms, level_db));
        let iq = kiwi_like_96k(&iq12, KIWI_RESIDUAL_DB, KIWI_RESIDUAL_SEED);
        let case = format!("{width_ms} ms +{level_db} dB");
        let full = run(&iq, fs, 0.0, CrashFilter::Full, Engine::Legacy, kiwi, 4096);
        assert_eq!(full.promoted(), 0, "{case}: full");
        if level_db >= 20.0 {
            assert_eq!(full.crashes, 1, "{case}: full count");
            let off = run(&iq, fs, 0.0, CrashFilter::Off, Engine::Legacy, kiwi, 4096);
            assert!(off.promoted() > 0, "{case}: off should flood");
        }
        if width_ms == 5.0 && level_db == 40.0 {
            let all = run(&iq, fs, 0.0, CrashFilter::Full, Engine::Legacy, None, 4096);
            assert_eq!(all.crashes, 0, "{case}: all-channel statistic");
            assert!(all.promoted() > 0, "{case}: all-channel statistic");
        }
    }
}

fn assert_vc2_full(engine: Engine, level_db: f64) {
    let (clean, crashed, fs) = vc2_samples(level_db, None);
    let base = run(&clean, fs, 0.0, CrashFilter::Full, engine, None, 4096);
    let r = run(&crashed, fs, 0.0, CrashFilter::Full, engine, None, 4096);
    let engine_name = match engine {
        Engine::Legacy => "legacy",
        Engine::EdgeLegacy => "edge-legacy",
        Engine::Hsmm => "hsmm",
    };
    let case = format!("{engine_name} +{level_db} dB");
    assert_eq!(r.crashes, 24, "{case}: crash count");
    assert_eq!(r.promoted(), 1, "{case}: promotions");
    assert_eq!(
        r.primary_closes(),
        vec![ClosureKind::SignalEnded],
        "{case}: the track closes only at end of stream"
    );
    assert!(
        r.dits() <= base.dits(),
        "{case}: {} dits against {} clean",
        r.dits(),
        base.dits()
    );
    assert!(r.spotted("W1AW"), "{case}: spots {:?}", r.spots);
}

/// Scenario 2 on the legacy engine: the track stays open through 24
/// mid-character crashes and gains no dit. `"off"` floods and adds dits.
#[test]
fn vc2_full_keeps_one_track_and_adds_no_dit() {
    assert_vc2_full(Engine::Legacy, 40.0);
    assert_vc2_full(Engine::Legacy, 60.0);
    let (clean, crashed, fs) = vc2_samples(40.0, None);
    let base = run_default(&clean, fs, CrashFilter::Off);
    let off = run_default(&crashed, fs, CrashFilter::Off);
    assert!(off.promoted() > 1, "off promoted {}", off.promoted());
    assert!(
        off.dits() > base.dits(),
        "off: {} dits against {} clean",
        off.dits(),
        base.dits()
    );
}

/// Scenario 2 on the HSMM engine.
#[test]
fn vc2_full_keeps_one_track_and_adds_no_dit_hsmm() {
    assert_vc2_full(Engine::Hsmm, 40.0);
    assert_vc2_full(Engine::Hsmm, 60.0);
}

/// Scenario 3: under `"starts-only"` no track starts on a crash, but the
/// crash reaches the running track's decoder (measured 91 dits against 86).
#[test]
fn vc2_starts_only_lets_the_crash_into_the_track() {
    let (clean, crashed, fs) = vc2_samples(40.0, None);
    let base = run_default(&clean, fs, CrashFilter::StartsOnly);
    let r = run_default(&crashed, fs, CrashFilter::StartsOnly);
    assert_eq!(r.crashes, 24);
    assert_eq!(r.promoted(), 1);
    assert!(
        r.dits() > base.dits(),
        "{} dits against {} clean",
        r.dits(),
        base.dits()
    );
}

/// A weak (+20 dB) crash train: no dit added, one track, W1AW spotted. Text
/// equality is not asserted (the legacy hold measured one `Q1AW`).
#[test]
fn vc2_plus_20_db_full_adds_no_dit() {
    for engine in [Engine::Legacy, Engine::Hsmm] {
        let (clean, crashed, fs) = vc2_samples(20.0, None);
        let base = run(&clean, fs, 0.0, CrashFilter::Full, engine, None, 4096);
        let r = run(&crashed, fs, 0.0, CrashFilter::Full, engine, None, 4096);
        assert!(r.dits() <= base.dits(), "{engine:?}");
        assert_eq!(r.promoted(), 1, "{engine:?}");
        assert!(r.spotted("W1AW"), "{engine:?}: {:?}", r.spots);
    }
}

/// `"off"` is today's behaviour: its event stream equals that of a
/// `"full"` manager whose detector is inactive (31 live channels).
#[test]
fn off_matches_an_inactive_detector() {
    let (_, crashed, fs) = vc2_samples(40.0, Some(30.0));
    assert!(crash_live_channels(fs, (-1500.0, 1500.0)) < crate::CRASH_MIN_LIVE_CHANNELS);
    let off = run_default(&crashed, fs, CrashFilter::Off);
    let inactive = run(
        &crashed,
        fs,
        0.0,
        CrashFilter::Full,
        Engine::Legacy,
        Some((-1500.0, 1500.0)),
        4096,
    );
    assert!(off.crashes > 0);
    assert_eq!(inactive.crashes, 0);
    assert!(off.events == inactive.events);
}

/// Dense bands joined mid-traffic never fire the detector, so `"full"`
/// changes nothing on them.
#[test]
fn dense_band_never_fires() {
    for (fs, spacing_hz) in [(12_000.0, 300.0), (12_000.0, 100.0), (24_000.0, 200.0)] {
        let iq = dense_scene(fs, spacing_hz, 30.0, 0x5eed);
        let full = run_default(&iq, fs, CrashFilter::Full);
        assert_eq!(full.crashes, 0, "{fs} Hz / {spacing_hz} Hz");
        let off = run_default(&iq, fs, CrashFilter::Off);
        assert!(full.events == off.events, "{fs} Hz / {spacing_hz} Hz");
    }
}

/// 60 s of 96 kHz noise and of KiwiSDR-shaped noise: no false crash.
#[test]
fn noise_never_fires() {
    let fs = 96_000.0;
    let (noise, _) = manta_testkit::scene::render_scene(&[], fs, 60.0, Some(0x77)).unwrap();
    assert_eq!(run_default(&noise, fs, CrashFilter::Full).crashes, 0);
    let (noise12, _) = manta_testkit::scene::render_scene(&[], 12_000.0, 60.0, Some(0x78)).unwrap();
    let kiwi = kiwi_like_96k(&noise12, KIWI_RESIDUAL_DB, KIWI_RESIDUAL_SEED);
    let r = run(
        &kiwi,
        fs,
        0.0,
        CrashFilter::Full,
        Engine::Legacy,
        Some((-5000.0, 5000.0)),
        4096,
    );
    assert_eq!(r.crashes, 0);
}

/// A sustained +10 dB band-wide step counts once, releases after
/// `CRASH_MAX_HOPS`, and keeps W1AW's track.
#[test]
fn level_step_releases_after_one_second() {
    let mut spec = v1();
    spec.duration_s = 40.0;
    spec.signals[0].snr_2500_db = 25.0;
    let step = Crash {
        start_s: 10.0,
        width_ms: 15_000.0,
        level_db: 10.0 * 9f64.log10(),
        seed: 9,
    };
    let iq = render_with_crashes(&spec, &[step]).unwrap().samples;
    let r = run_default(&iq, spec.fs, CrashFilter::Full);
    assert_eq!(r.crashes, 1);
    assert_eq!(r.primary_closes(), vec![ClosureKind::SignalEnded]);
    assert!(r.spotted("W1AW"), "{:?}", r.spots);
}

/// Crash state depends only on the hop sequence, not on chunking: the
/// count, the promotions and the primary track's decoded characters
/// (glyph and timestamp) are identical at every chunk size. (`process_hops`
/// batch boundaries already move non-character events such as `TrackMeta`
/// before MAN-312, so whole event vectors are not compared.)
#[test]
fn crash_state_is_chunk_invariant() {
    let (_, crashed, fs) = vc2_samples(40.0, Some(15.0));
    let runs: Vec<Run> = [256, 4096, 65536]
        .iter()
        .map(|&c| {
            run(
                &crashed,
                fs,
                0.0,
                CrashFilter::Full,
                Engine::Legacy,
                None,
                c,
            )
        })
        .collect();
    let chars = |r: &Run| -> Vec<(u64, Glyph)> {
        let id = r.primary();
        r.events
            .iter()
            .filter_map(|e| match e {
                DecoderEvent::CharDecoded {
                    track_id,
                    sample_ts,
                    glyph,
                    ..
                } if *track_id == id => Some((*sample_ts, *glyph)),
                _ => None,
            })
            .collect()
    };
    for r in &runs {
        assert_eq!(r.crashes, 5);
        assert_eq!(r.promoted(), 1);
        assert_eq!(chars(r), chars(&runs[0]));
    }
}

/// Signal arriving after 2 s of digital silence is one crash; `"full"`
/// starts one track, where `"off"` floods (measured 52).
#[test]
fn silence_lead_in_is_one_crash() {
    let mut spec = v1();
    spec.duration_s = 30.0;
    let mut iq = render(&spec).unwrap().samples;
    let lead = (2.0 * spec.fs) as usize;
    for s in &mut iq[..lead] {
        *s = Complex32::new(0.0, 0.0);
    }
    let full = run_default(&iq, spec.fs, CrashFilter::Full);
    assert_eq!(full.promoted(), 1);
    assert_eq!(full.crashes, 1);
    let off = run_default(&iq, spec.fs, CrashFilter::Off);
    assert!(off.promoted() > 1, "off promoted {}", off.promoted());
}

/// MAN-312 Phase 6: proves the VC3 gate discriminates -- under `"off"` the
/// crash train floods tracks and loses calls (measured 1694 promoted, 16
/// validated). Ignored: a full 120 s V8w pileup is slow in debug builds;
/// run with `cargo test --release -p manta-engine --lib -- --ignored
/// vc3_off_floods_and_loses_calls`.
#[test]
#[ignore]
fn vc3_off_floods_and_loses_calls() {
    let (spec, crashes) = manta_testkit::crash::vc3();
    let rendered = render_with_crashes(&spec, &crashes).unwrap();
    let off = run_default(&rendered.samples, spec.fs, CrashFilter::Off);
    let truth: std::collections::BTreeSet<String> = rendered
        .keyed_texts
        .iter()
        .filter_map(|t| t.split_whitespace().nth(3).map(str::to_string))
        .collect();
    let validated = off
        .spots
        .iter()
        .filter(|s| truth.contains(*s))
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    eprintln!("vc3 off: promoted {} validated {validated}", off.promoted());
    assert!(off.promoted() > 500);
    assert!(validated < 20);
}
