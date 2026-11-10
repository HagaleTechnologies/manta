//! MAN-515: the MAN-208 late-rescue scene, parameterised by the base
//! decode engine, shared via `#[path]`. Synthetic at the event/hop
//! boundary: fabricated causal decoder events plus a true waveform
//! retained for replay. `man208_late_rescue.rs` keeps its own copy, so
//! its pinned counts never depend on this module.

use manta_decode::decoder::{DecodeConfig, Engine, RefineConfig};
use manta_decode::events::DecoderEvent;
use manta_decode::tree::Glyph;
use manta_engine::refine::{RefineBuffers, RefineScheduler};
use manta_spot::{ObservationPolicy, Spot, Validator};

pub const FS: f64 = 96_000.0;
pub const CTY: &str = "\
United States:    5:  8: NA:  40.0:  75.0:  5.0:  K:
    K,W,N,AA,AB,AC;
";
const DIT_HOPS: u64 = 18; // 25 WPM at 375 hops/s

fn pattern(c: char) -> &'static str {
    match c {
        'K' => "-.-",
        '3' => "...--",
        'A' => ".-",
        'B' => "-...",
        'C' => "-.-.",
        'D' => "-..",
        'E' => ".",
        'T' => "-",
        _ => panic!("no pattern for {c}"),
    }
}

/// The retained waveform: the hops, where the word under refinement
/// starts, and where its decoder flushes the word boundary (7 dits of
/// trailing space, the default `flush_gap_dits`).
struct Wave {
    hops: Vec<(f32, f32, Option<f32>, u64, f32, f64)>,
    word_start: u64,
    boundary: u64,
}

/// Envelope hops for `text` at `base`, preceded by five spaced warm-up
/// dits so a replay's decoder bootstraps its speed tracker before the
/// word under refinement starts.
fn waveform(text: &str, base: u64) -> Wave {
    let mut hops = Vec::new();
    let mut ts = base;
    let push = |level: f32, n: u64, hops: &mut Vec<_>, ts: &mut u64| {
        for _ in 0..n {
            hops.push((level, level * level, None, *ts, 20.0, 14_000_000.0));
            *ts += 256;
        }
    };
    for _ in 0..5 {
        push(1.0, DIT_HOPS, &mut hops, &mut ts);
        push(0.0, DIT_HOPS, &mut hops, &mut ts);
    }
    push(0.0, 7 * DIT_HOPS, &mut hops, &mut ts); // word gap
    let word_start = ts;
    let chars: Vec<char> = text.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        let els: Vec<char> = pattern(*c).chars().collect();
        for (j, e) in els.iter().enumerate() {
            let n = if *e == '.' { DIT_HOPS } else { 3 * DIT_HOPS };
            push(1.0, n, &mut hops, &mut ts);
            if j < els.len() - 1 {
                push(0.0, DIT_HOPS, &mut hops, &mut ts);
            }
        }
        if i < chars.len() - 1 {
            push(0.0, 3 * DIT_HOPS, &mut hops, &mut ts);
        }
    }
    let boundary = ts + 7 * DIT_HOPS * 256;
    push(0.0, 8 * DIT_HOPS, &mut hops, &mut ts); // flush tail
    Wave {
        hops,
        word_start,
        boundary,
    }
}

/// `causal_events` with the call word's boundary pinned to the true RF
/// flush point.
fn causal_events_boundary(
    track_id: u32,
    call: &str,
    base: u64,
    call_base: u64,
    call_boundary: u64,
    ambiguous: bool,
) -> Vec<DecoderEvent> {
    let (events, _) = causal_events(track_id, call, base, call_base, ambiguous);
    events
        .into_iter()
        .map(|ev| match ev {
            DecoderEvent::WordBoundary {
                track_id,
                sample_ts,
                ..
            } if sample_ts > call_base && sample_ts <= call_base + 5_000 => {
                DecoderEvent::word_boundary(track_id, call_boundary)
            }
            other => other,
        })
        .collect()
}

/// Fabricated causal decoder events for one `DE <call> K` transmission at
/// `base`; an ambiguous decode carries an unresolved `D` alternative on
/// its last character. Returns the events and the call word's boundary.
pub fn causal_events(
    track_id: u32,
    call: &str,
    base: u64,
    call_base: u64,
    ambiguous: bool,
) -> (Vec<DecoderEvent>, u64) {
    let mut events = Vec::new();
    let mut ts = base;
    let word = |text: &str, events: &mut Vec<DecoderEvent>, ts: &mut u64, alt: bool| {
        let n = text.chars().count();
        for (i, c) in text.chars().enumerate() {
            let alternatives = if alt && i == n - 1 {
                vec![(Glyph::Char('D'), 0.4)]
            } else {
                Vec::new()
            };
            events.push(DecoderEvent::CharDecoded {
                track_id,
                sample_ts: *ts,
                glyph: Glyph::Char(c),
                confidence: 0.95,
                alternatives,
            });
            *ts += 100;
        }
        events.push(DecoderEvent::word_boundary(track_id, *ts));
        *ts += 100;
    };
    word("DE", &mut events, &mut ts, false);
    ts = call_base;
    let boundary = call_base + call.chars().count() as u64 * 100;
    word(call, &mut events, &mut ts, ambiguous);
    word("K", &mut events, &mut ts, false);
    (events, boundary)
}

pub struct Scene {
    pub validator: Validator,
    pub scheduler: RefineScheduler,
    pub buffers: RefineBuffers,
    pub decode_cfg: DecodeConfig,
    /// The ambiguous occurrence's span, as the validator holds it.
    pub span: (u64, u64),
    /// The clean repetition word's boundary sample ts (its RF time), or
    /// the retained word's boundary without a repetition.
    pub rep_boundary: u64,
    /// Spots the causal events alone produced.
    pub causal_spots: Vec<Spot>,
}

impl Scene {
    /// One scheduler pass at `now`.
    pub fn step(&mut self, now: u64) -> Vec<Spot> {
        self.scheduler.step(
            &mut self.buffers,
            &mut self.validator,
            &self.decode_cfg,
            now,
        )
    }

    /// Feeds whole `words` on track 1 starting at `ts`, so the track's
    /// latest word sequence advances (the `later_word` signal) and a
    /// preceding `SpeedUpdate` becomes stable. Returns the next free ts.
    pub fn later_words(&mut self, words: &[&str], mut ts: u64) -> u64 {
        for word in words {
            for c in word.chars() {
                self.validator
                    .ingest(&DecoderEvent::char_decoded(1, ts, Glyph::Char(c), 0.95));
                ts += 100;
            }
            self.validator.ingest(&DecoderEvent::word_boundary(1, ts));
            ts += 100;
        }
        ts
    }
}

/// Track 1 decodes `causal` (ambiguous, with an unresolved alternative)
/// under `engine`; the retained waveform carries `truth`; with
/// `with_repetition`, a clean repetition of `truth` arrives 8 s later.
/// Records keep the text trace, so a `Refined` record names its
/// hypothesis.
pub fn scene(
    truth: &str,
    causal: &str,
    engine: Engine,
    refine_enabled: bool,
    with_repetition: bool,
) -> Scene {
    let base = 1_000_000u64;
    let decode_cfg = DecodeConfig {
        engine,
        refine: RefineConfig {
            enabled: refine_enabled,
            ..Default::default()
        },
        ..DecodeConfig::default()
    };
    let mut validator = Validator::new(FS, CTY, None).with_observations(ObservationPolicy::text());
    validator.ingest(&DecoderEvent::TrackMeta {
        track_id: 1,
        sample_ts: base,
        snr_2500_db: 20.0,
        freq_hz: 14_000_000.0,
        last_char_ts: base,
    });
    validator.ingest(&DecoderEvent::SpeedUpdate {
        track_id: 1,
        wpm: 25.0,
    });
    let wave = waveform(truth, base);
    let mut buffers = RefineBuffers::new(&decode_cfg.refine);
    for h in &wave.hops {
        buffers.record(1, *h);
    }
    let first = causal_events_boundary(1, causal, base, wave.word_start + 500, wave.boundary, true);
    let mut causal_spots = Vec::new();
    for ev in &first {
        causal_spots.extend(validator.ingest(ev));
    }
    let rep_base = base + 768_000;
    let rep_boundary = if with_repetition {
        let (rep, rep_boundary) = causal_events(1, truth, rep_base, rep_base, false);
        for ev in &rep {
            causal_spots.extend(validator.ingest(ev));
        }
        rep_boundary
    } else {
        wave.boundary
    };
    let span = validator
        .refine_candidates(rep_boundary, 10 * FS as u64)
        .into_iter()
        .find(|c| c.call == causal)
        .map(|c| c.span)
        .expect("the ambiguous word is a refine candidate");
    Scene {
        validator,
        scheduler: RefineScheduler::new(&decode_cfg, FS),
        buffers,
        decode_cfg,
        span,
        rep_boundary,
        causal_spots,
    }
}
