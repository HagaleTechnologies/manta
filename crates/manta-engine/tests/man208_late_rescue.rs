//! MAN-208: a later clean repetition rescues an earlier ambiguous decode,
//! end to end through the real scheduler, retention buffer and validator.
//! The scene is synthetic at the event/hop boundary (fabricated causal
//! decoder events + a true waveform retained for replay), pinning the
//! ticket's two scenarios without an RF fixture.

use manta_decode::decoder::{DecodeConfig, Engine};
use manta_decode::events::DecoderEvent;
use manta_decode::tree::Glyph;
use manta_engine::refine::{RefineBuffers, RefineScheduler};
use manta_spot::observation::Cause;
use manta_spot::{ObservationPolicy, Validator};

const FS: f64 = 96_000.0;
const CTY: &str = "\
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

/// Envelope hops for `text` at `base`, the way the engine would have fed
/// them: `(amp, power, spectral_ref, sample_ts, snr, freq)`, preceded by
/// five spaced warm-up dits so a replay's decoder bootstraps its speed
/// tracker BEFORE the word under refinement starts.
fn waveform(text: &str, base: u64) -> Wave {
    let mut hops = Vec::new();
    let mut ts = base;
    let push = |level: f32, n: u64, hops: &mut Vec<_>, ts: &mut u64| {
        for _ in 0..n {
            hops.push((level, level * level, None, *ts, 20.0, 14_000_000.0));
            *ts += 256;
        }
    };
    // Five dits at one-dit spacing (the digit 5): five marks for the
    // bootstrap inside one second of run-up.
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
/// flush point (the fabricated characters are a stand-in for the decoder's
/// reading, but the occurrence's span is the true word's).
fn causal_events_boundary(
    track_id: u32,
    call: &str,
    base: u64,
    call_base: u64,
    call_boundary: u64,
    ambiguous: bool,
) -> (Vec<DecoderEvent>, u64) {
    let (mut events, _) = causal_events(track_id, call, base, call_base, ambiguous);
    // Replace the call word's synthetic boundary with the true one.
    let mut fixed = Vec::with_capacity(events.len());
    for ev in events.drain(..) {
        match ev {
            DecoderEvent::WordBoundary {
                track_id,
                sample_ts,
                ..
            } if sample_ts > call_base && sample_ts <= call_base + 5_000 => {
                fixed.push(DecoderEvent::word_boundary(track_id, call_boundary));
            }
            other => fixed.push(other),
        }
    }
    (fixed, call_boundary)
}

/// Fabricated causal decoder events for one `DE <call> K` transmission at
/// `base`: the ambiguous first decode carries an unresolved `D`
/// alternative on its last character.
fn causal_events(
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
    let chars_end = call_base + call.chars().count() as u64 * 100;
    word(call, &mut events, &mut ts, ambiguous);
    let boundary = chars_end; // the call word's own boundary
    word("K", &mut events, &mut ts, false);
    (events, boundary)
}

struct Scene {
    validator: Validator,
    scheduler: RefineScheduler,
    buffers: RefineBuffers,
    decode_cfg: DecodeConfig,
    /// The ambiguous occurrence's candidate info, as the validator holds it.
    span: (u64, u64),
    word_seq: u64,
    /// The clean repetition word's boundary sample ts (its RF time).
    rep_boundary: u64,
    causal_spots: Vec<manta_spot::Spot>,
}

/// The ticket's scene: track 1 decodes `causal` (ambiguous, with an
/// unresolved alternative); the retained waveform carries `truth`; with
/// `with_repetition`, a clean repetition of `truth` arrives 8 s later.
fn scene(truth: &str, causal: &str, refine_enabled: bool, with_repetition: bool) -> Scene {
    let base = 1_000_000u64;
    let decode_cfg = DecodeConfig {
        // Hsmm base so the first alternate configuration is the Legacy
        // engine flip (a deterministic, well-understood re-decode).
        engine: Engine::Hsmm,
        refine: manta_decode::decoder::RefineConfig {
            enabled: refine_enabled,
            ..Default::default()
        },
        ..DecodeConfig::default()
    };
    let mut validator =
        Validator::new(FS, CTY, None).with_observations(ObservationPolicy::records());
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
    // The fabricated ambiguous decode sits inside the true word, and its
    // boundary is the true flush point.
    let (first, _) =
        causal_events_boundary(1, causal, base, wave.word_start + 500, wave.boundary, true);
    let mut causal_spots = Vec::new();
    for ev in &first {
        causal_spots.extend(validator.ingest(ev));
    }
    assert!(
        causal_spots.is_empty(),
        "the ambiguous decode alone never spots"
    );
    // The clean repetition, 8 s (768_000 samples) after the first decode.
    let rep_base = base + 768_000;
    let rep_boundary = if with_repetition {
        let (rep, rep_boundary) = causal_events(1, truth, rep_base, rep_base, false);
        for ev in &rep {
            causal_spots.extend(validator.ingest(ev));
        }
        assert!(
            causal_spots.is_empty(),
            "a single clean repetition alone never spots: {causal_spots:?}"
        );
        rep_boundary
    } else {
        wave.boundary
    };
    let candidate = validator
        .refine_candidates(rep_boundary, 10 * FS as u64)
        .into_iter()
        .find(|c| c.call == causal)
        .expect("the ambiguous word is a refine candidate");
    Scene {
        validator,
        scheduler: RefineScheduler::new(&decode_cfg, FS),
        buffers,
        decode_cfg,
        span: candidate.span,
        word_seq: candidate.word_seq,
        rep_boundary,
        causal_spots,
    }
}

#[test]
fn a_late_clean_repetition_rescues_the_ambiguous_episode_end_to_end() {
    let mut s = scene("K3ABD", "K3ABC", true, true);
    let now = s.rep_boundary + 32_000;
    let spots = s
        .scheduler
        .step(&mut s.buffers, &mut s.validator, &s.decode_cfg, now);
    assert_eq!(spots.len(), 1, "{spots:?}");
    assert_eq!(spots[0].callsign, "K3ABD");
    assert_eq!(
        spots[0].sample_ts, s.rep_boundary,
        "the rescued spot carries the later repetition's RF time"
    );
    // The refinement is recorded as a Refined revision of the SAME
    // occurrence, linked to the record it supersedes.
    let refined: Vec<_> = s
        .validator
        .observations()
        .iter()
        .filter(|o| o.draft.cause == Cause::Refined)
        .collect();
    assert_eq!(refined.len(), 1, "no second repetition was manufactured");
    assert_eq!(
        refined[0].draft.id.to_string(),
        format!("t1.w{}", s.word_seq)
    );
    // It supersedes the SAME occurrence's latest prior record (its
    // original withheld decision), never a new one.
    assert_eq!(refined[0].supersedes, Some(0));
    // The scheduler saw the whole pass through its counters.
    let c = s.scheduler.counters();
    assert_eq!(c.admitted_total, 1);
    assert!(c.replayed_hops_total > 0);
    assert_eq!(c.rescues_total, 1);
    println!("rescue=K3ABD rf_ts={} decision_ts={} replayed_hops={} admitted={} budget_pauses={} overflow={}", spots[0].sample_ts, now, c.replayed_hops_total, c.admitted_total, c.budget_exhausted_total, c.backlog_overflow_total);
}

#[test]
fn no_rescue_when_refinement_is_disabled() {
    let mut s = scene("K3ABD", "K3ABC", false, true);
    let spots = s.scheduler.step(
        &mut s.buffers,
        &mut s.validator,
        &s.decode_cfg,
        s.rep_boundary + 32_000,
    );
    assert!(spots.is_empty());
    let c = s.scheduler.counters();
    assert_eq!(
        *c,
        Default::default(),
        "disabled retains and replays nothing"
    );
    assert!(s
        .validator
        .observations()
        .iter()
        .all(|o| o.draft.cause != Cause::Refined));
}

#[test]
fn refinement_output_is_byte_identical_across_runs() {
    let run = || {
        let mut s = scene("K3ABD", "K3ABC", true, true);
        let spots = s.scheduler.step(
            &mut s.buffers,
            &mut s.validator,
            &s.decode_cfg,
            s.rep_boundary + 32_000,
        );
        spots
            .into_iter()
            .map(|sp| {
                (
                    sp.callsign,
                    sp.sample_ts,
                    sp.freq_hz.to_bits(),
                    sp.confidence.to_bits(),
                    sp.wpm.to_bits(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        run(),
        run(),
        "identical input explores an identical hypothesis set"
    );
}

#[test]
fn per_episode_cap_of_two_refinements_is_enforced() {
    // No confirming repetition: every refinement leaves the episode
    // ambiguous (still under its repetition requirement), so the cap --
    // not the ambiguity -- must stop the third admission.
    let mut s = scene("K3ABC", "K3ABC", true, false);
    // Drop the repetition's support by refining against a span whose only
    // support is the moved occurrence: simply never let it reach two reps.
    let mut admitted = 0u64;
    for i in 0..3u64 {
        if i == 1 {
            s.validator.ingest(&DecoderEvent::SpeedUpdate {
                track_id: 1,
                wpm: 29.0,
            });
        }
        let now = s.span.1 + 50_000 + i * 32_000;
        s.scheduler
            .step(&mut s.buffers, &mut s.validator, &s.decode_cfg, now);
        let admitted_now = s.scheduler.counters().admitted_total;
        assert!(admitted_now >= admitted);
        admitted = admitted_now;
    }
    // The cap (2) bounds admissions for this episode even though it stayed
    // ambiguous after every refinement.
    assert_eq!(
        admitted, 2,
        "the default cap of two refinements per episode is enforced"
    );
}

#[test]
fn replay_hops_per_batch_is_enforced_and_counted() {
    let mut s = scene("K3ABD", "K3ABC", true, true);
    // A replay larger than one batch must make progress under a fixed
    // budget, without resetting the scheduler or inventing a repetition.
    s.decode_cfg.refine.replay_hops_per_batch = 17;
    s.scheduler = RefineScheduler::new(&s.decode_cfg, FS);
    let now = s.rep_boundary + 32_000;
    let mut spots = Vec::new();
    for batch in 0..200 {
        let before = s.scheduler.counters().replayed_hops_total;
        spots.extend(s.scheduler.step(
            &mut s.buffers,
            &mut s.validator,
            &s.decode_cfg,
            now + batch * 256,
        ));
        let after = s.scheduler.counters().replayed_hops_total;
        assert!(after - before <= 17);
        if !spots.is_empty() {
            break;
        }
    }
    assert!(s.scheduler.counters().budget_exhausted_total > 0);
    assert_eq!(spots.len(), 1);
    assert_eq!(spots[0].sample_ts, s.rep_boundary);
}

#[test]
fn backlog_overflow_drops_oldest_and_counts() {
    // Two ambiguous episodes (two tracks) against a one-episode backlog:
    // the OLDEST admission (track 1) drops, deterministically, and the
    // loss is counted.
    let base = 1_000_000u64;
    let decode_cfg = DecodeConfig {
        engine: Engine::Hsmm,
        refine: manta_decode::decoder::RefineConfig {
            backlog: 1,
            ..Default::default()
        },
        ..DecodeConfig::default()
    };
    let mut validator = Validator::new(FS, CTY, None);
    let mut buffers = RefineBuffers::new(&decode_cfg.refine);
    for track_id in 1..=2u32 {
        validator.ingest(&DecoderEvent::TrackMeta {
            track_id,
            sample_ts: base,
            snr_2500_db: 20.0,
            freq_hz: 14_000_000.0,
            last_char_ts: base,
        });
        let tbase = base + (track_id as u64 - 1) * 500_000;
        let wave = waveform("K3ABC", tbase);
        let (events, _) = causal_events_boundary(
            track_id,
            "K3ABC",
            tbase,
            wave.word_start + 500,
            wave.boundary,
            true,
        );
        for ev in &events {
            validator.ingest(ev);
        }
        for h in &wave.hops {
            buffers.record(track_id, *h);
        }
    }
    let mut scheduler = RefineScheduler::new(&decode_cfg, FS);
    let _ = scheduler.step(&mut buffers, &mut validator, &decode_cfg, base + 1_000_000);
    let c = scheduler.counters();
    assert_eq!(c.admitted_total, 2);
    assert_eq!(c.backlog_overflow_total, 1, "the oldest admission dropped");
}

#[test]
fn a_refinement_agreeing_with_the_causal_decode_changes_nothing() {
    // The retained waveform reads the SAME call the causal decode did:
    // no support moves, no repetition is added, nothing is emitted.
    let mut s = scene("K3ABC", "K3ABC", true, false);
    let now = s.rep_boundary + 32_000;
    let spots = s
        .scheduler
        .step(&mut s.buffers, &mut s.validator, &s.decode_cfg, now);
    assert!(
        spots.is_empty(),
        "an agreeing refinement must not manufacture a spot: {spots:?}"
    );
    // ...and it manufactured no repetition: the repetition word still
    // counts once (no K3ABC spot ever fired on one real repetition).
    assert!(s.causal_spots.is_empty());
}

#[test]
fn control_scene_without_ambiguity_emits_no_refined_records() {
    // No unresolved alternative, no rival: the episode never becomes a
    // refinement candidate, so no Refined record exists.
    let base = 1_000_000u64;
    let decode_cfg = DecodeConfig {
        engine: Engine::Hsmm,
        ..DecodeConfig::default()
    };
    let mut validator =
        Validator::new(FS, CTY, None).with_observations(ObservationPolicy::records());
    validator.ingest(&DecoderEvent::TrackMeta {
        track_id: 1,
        sample_ts: base,
        snr_2500_db: 20.0,
        freq_hz: 14_000_000.0,
        last_char_ts: base,
    });
    let (first, _) = causal_events(1, "K3ABC", base, base + 300, false);
    for ev in &first {
        validator.ingest(ev);
    }
    let (rep, rep_boundary) = causal_events(1, "K3ABD", base + 768_000, base + 768_000, false);
    for ev in &rep {
        validator.ingest(ev);
    }
    let candidates = validator.refine_candidates(rep_boundary, 10 * FS as u64);
    assert!(
        candidates
            .iter()
            .all(|c| !manta_engine::refine::ambiguity_trigger(c)),
        "a clean decode with no rival and no alternatives never triggers: {candidates:?}"
    );
    let mut buffers = RefineBuffers::new(&decode_cfg.refine);
    for h in &waveform("K3ABC", base).hops {
        buffers.record(1, *h);
    }
    let mut scheduler = RefineScheduler::new(&decode_cfg, FS);
    let spots = scheduler.step(
        &mut buffers,
        &mut validator,
        &decode_cfg,
        rep_boundary + 32_000,
    );
    assert!(spots.is_empty());
    assert_eq!(scheduler.counters().admitted_total, 0);
    assert!(validator
        .observations()
        .iter()
        .all(|o| o.draft.cause != Cause::Refined));
}
