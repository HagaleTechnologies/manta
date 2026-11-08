//! MAN-544: speed steps. A sender changes speed mid-transmission; the decode
//! after the step must stay within 2 pp of CER of a steady control at the new
//! speed, on a window of at least 150 keyed characters.
//!
//! The scene: `key_text_steps("CQ TEST W1AW", ..)` at the first speed for
//! 16 s, a tight join (one character gap of the new segment's own timing),
//! then the second speed for `post_step_seconds(new content WPM)`; 5 kHz
//! complex NCO at 96 kS/s, `scene::mix` with AWGN, `decode_samples` with
//! `PipelineConfig::default()`. The control keys both segments at the new
//! speed. "Post-step text" is the busiest track's `CharDecoded` and
//! `WordBoundary` events at or after the step's first sample.
//!
//! Sweep: `cargo test --release -p manta-engine --test speed_step -- --ignored --nocapture | grep ^STEP`
use manta_decode::decoder::events_to_text;
use manta_decode::events::DecoderEvent;
use manta_engine::{decode_samples, PipelineConfig};
use manta_testkit::cer::cer;
use manta_testkit::keyer::{key_text_steps, KeyerSpec};
use manta_testkit::scene::{mix, place_envelope};

const FS: f64 = 96_000.0;
const OFFSET_HZ: f64 = 5_000.0;
const TEXT: &str = "CQ TEST W1AW";
const PRE_STEP_S: f64 = 16.0;
/// "Within 2 %": post-step CER minus control CER, absolute (plan D11).
const MAX_CER_EXCESS: f64 = 0.02;
const MIN_KEYED_CHARS: usize = 150;

#[derive(Debug, Clone, Copy)]
struct Step {
    from: KeyerSpec,
    to: KeyerSpec,
    snr_2500_db: f32,
    seed: u64,
}

#[derive(Debug, Clone)]
struct Outcome {
    keyed: String,
    post_text: String,
    control_text: String,
    post_cer: f64,
    control_cer: f64,
    lock_s: Option<f64>,
}

/// D11: the post-step segment keys at least 150 characters of `TEXT`.
fn post_step_seconds(to_content_wpm: f32) -> f64 {
    (1650.0 / to_content_wpm as f64).max(60.0)
}

fn content_wpm(spec: &KeyerSpec) -> f32 {
    spec.char_wpm.unwrap_or(spec.wpm)
}

fn plain(from_wpm: f32, to_wpm: f32, snr_2500_db: f32, seed: u64) -> Step {
    Step {
        from: KeyerSpec::new(from_wpm),
        to: KeyerSpec::new(to_wpm),
        snr_2500_db,
        seed,
    }
}

/// Ticket scenario 1 (plan D10): 20 WPM, then characters at 32 WPM with a
/// 40 ms gap unit ("40 ms Farnsworth gaps").
fn ticket_step(snr_2500_db: f32, seed: u64) -> Step {
    let to = KeyerSpec {
        char_wpm: Some(32.0),
        ..KeyerSpec::new((60_000.0 / (31.0 * 37.5 + 19.0 * 40.0)) as f32)
    };
    Step {
        from: KeyerSpec::new(20.0),
        to,
        snr_2500_db,
        seed,
    }
}

fn event_ts(e: &DecoderEvent) -> Option<u64> {
    match e {
        DecoderEvent::CharDecoded { sample_ts, .. }
        | DecoderEvent::WordBoundary { sample_ts, .. }
        | DecoderEvent::TrackMeta { sample_ts, .. } => Some(*sample_ts),
        _ => None,
    }
}

fn event_track(e: &DecoderEvent) -> Option<u32> {
    match e {
        DecoderEvent::CharDecoded { track_id, .. }
        | DecoderEvent::WordBoundary { track_id, .. }
        | DecoderEvent::SpeedUpdate { track_id, .. }
        | DecoderEvent::TrackMeta { track_id, .. } => Some(*track_id),
        _ => None,
    }
}

/// The track with the most `CharDecoded` events at or after `step_sample`.
fn busiest_track(events: &[DecoderEvent], step_sample: u64) -> Option<u32> {
    let mut counts: Vec<(u32, usize)> = Vec::new();
    for e in events {
        if let DecoderEvent::CharDecoded {
            track_id,
            sample_ts,
            ..
        } = e
        {
            if *sample_ts < step_sample {
                continue;
            }
            match counts.iter_mut().find(|(t, _)| t == track_id) {
                Some((_, n)) => *n += 1,
                None => counts.push((*track_id, 1)),
            }
        }
    }
    // Most characters; ties to the lowest track id (deterministic).
    counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))
        .map(|(t, _)| t)
}

/// The busiest track's text from `step_sample` on.
fn post_text(events: &[DecoderEvent], step_sample: u64) -> String {
    let Some(track) = busiest_track(events, step_sample) else {
        return String::new();
    };
    let post: Vec<DecoderEvent> = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                DecoderEvent::CharDecoded { .. } | DecoderEvent::WordBoundary { .. }
            ) && event_track(e) == Some(track)
                && event_ts(e).is_some_and(|ts| ts >= step_sample)
        })
        .cloned()
        .collect();
    events_to_text(&post).trim().to_string()
}

/// Seconds from the step to the first `SpeedUpdate` on the busiest track
/// that is within 15 % of `to_wpm` and that no later update leaves.
/// `SpeedUpdate` carries no timestamp: it takes the last timestamp seen on
/// that track before it. Report-only.
fn lock_s(events: &[DecoderEvent], step_sample: u64, to_wpm: f32) -> Option<f64> {
    let track = busiest_track(events, step_sample)?;
    let mut last_ts = 0u64;
    let mut lock: Option<u64> = None;
    for e in events {
        if event_track(e) != Some(track) {
            continue;
        }
        if let Some(ts) = event_ts(e) {
            last_ts = ts;
        }
        if let DecoderEvent::SpeedUpdate { wpm, .. } = e {
            let inside = (wpm / to_wpm - 1.0).abs() <= 0.15;
            match (inside, lock) {
                (true, None) => lock = Some(last_ts.max(step_sample)),
                (false, _) => lock = None,
                _ => {}
            }
        }
    }
    lock.map(|ts| (ts - step_sample) as f64 / FS)
}

/// Decode one keyed two-segment transmission; returns the post-step text,
/// the post-step keyed text and the lock time.
fn decode_steps(
    from: KeyerSpec,
    to: KeyerSpec,
    snr: f32,
    seed: u64,
) -> (String, String, Option<f64>) {
    let post_s = post_step_seconds(content_wpm(&to));
    let k = key_text_steps(TEXT, &[(from, PRE_STEP_S), (to, post_s)], FS).unwrap();
    let sig = place_envelope(&k.env, OFFSET_HZ, snr, FS);
    let iq = mix(&[&sig], sig.len(), Some(seed));
    let report = decode_samples(&iq, FS, 0.0, &PipelineConfig::default()).unwrap();
    let step = k.steps[0] as u64;
    (
        post_text(&report.events, step),
        k.texts[1].clone(),
        lock_s(&report.events, step, content_wpm(&to)),
    )
}

fn run(step: Step) -> Outcome {
    let (post, keyed, lock) = decode_steps(step.from, step.to, step.snr_2500_db, step.seed);
    let (control, control_keyed, _) = decode_steps(step.to, step.to, step.snr_2500_db, step.seed);
    assert_eq!(keyed, control_keyed, "step and control key the same text");
    Outcome {
        post_cer: cer(&keyed, &post),
        control_cer: cer(&keyed, &control),
        keyed,
        post_text: post,
        control_text: control,
        lock_s: lock,
    }
}

fn assert_follows(step: Step) {
    let o = run(step);
    assert!(
        o.keyed.chars().count() >= MIN_KEYED_CHARS,
        "post-step window keys {} characters, under {MIN_KEYED_CHARS}",
        o.keyed.chars().count()
    );
    assert!(
        o.post_cer - o.control_cer <= MAX_CER_EXCESS,
        "{:.1} -> {:.1} WPM: post-step CER {:.4} vs control {:.4}\nkeyed:   {}\nstep:    {}\ncontrol: {}",
        content_wpm(&step.from),
        content_wpm(&step.to),
        o.post_cer,
        o.control_cer,
        o.keyed,
        o.post_text,
        o.control_text
    );
}

/// Ticket scenario 1: 20 -> 32 WPM, character speed 32 WPM, 40 ms gap unit.
#[test]
fn ticket_20_to_32_with_40ms_gap_unit_follows_the_step() {
    assert_follows(ticket_step(25.0, 1));
}

#[test]
fn plain_20_to_32_follows_the_step() {
    assert_follows(plain(20.0, 32.0, 25.0, 1));
}

#[test]
fn post_step_window_keys_at_least_150_characters() {
    for wpm in [16.0f32, 20.0, 25.0, 32.0, 40.0, 50.0] {
        let spec = KeyerSpec::new(wpm);
        let k = key_text_steps(
            TEXT,
            &[(KeyerSpec::new(20.0), 2.0), (spec, post_step_seconds(wpm))],
            FS,
        )
        .unwrap();
        assert!(k.texts[1].chars().count() >= MIN_KEYED_CHARS, "{wpm} WPM");
    }
}

/// Plan D12 rungs: (from, to, gated). Gated rungs are acceptance rungs;
/// the rest are report-only.
const RUNGS: &[(f32, f32, bool)] = &[
    (20.0, 24.0, true),
    (20.0, 30.0, true),
    (20.0, 32.0, true),
    (20.0, 36.0, true),
    (20.0, 38.0, true),
    (20.0, 40.0, true),
    (15.0, 30.0, true),
    (20.0, 50.0, true),
    (40.0, 25.0, true),
    (40.0, 22.0, true),
    (40.0, 20.0, true),
    (32.0, 16.0, true),
    (20.0, 34.0, false),
    (20.0, 35.0, false),
    (30.0, 52.0, false),
    (30.0, 60.0, false),
    (40.0, 18.0, false),
    (40.0, 16.0, false),
    (24.0, 12.0, false),
    (20.0, 10.0, false),
];

/// Every D12 rung plus the ticket's Farnsworth scene, seeds 1-3, 15 and
/// 25 dB: one tab-separated STEP row each (from, to, char_wpm, snr, seed,
/// gated, keyed_len, post_cer, control_cer, diff_pp, lock_s, post-step
/// text). Asserts nothing; it records the baselines the bank is measured
/// against.
#[test]
#[ignore]
fn speed_step_sweep() {
    let mut steps: Vec<(Step, bool)> = Vec::new();
    for &(from, to, gated) in RUNGS {
        for snr in [15.0f32, 25.0] {
            for seed in 1..=3u64 {
                steps.push((plain(from, to, snr, seed), gated));
            }
        }
    }
    for snr in [15.0f32, 25.0] {
        for seed in 1..=3u64 {
            steps.push((ticket_step(snr, seed), true));
        }
    }
    let jobs = std::thread::available_parallelism().map_or(1, |n| n.get());
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: Vec<std::sync::Mutex<Option<Outcome>>> =
        steps.iter().map(|_| std::sync::Mutex::new(None)).collect();
    std::thread::scope(|s| {
        for _ in 0..jobs {
            s.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if i >= steps.len() {
                    break;
                }
                let o = run(steps[i].0);
                *results[i].lock().unwrap() = Some(o);
            });
        }
    });
    for ((step, gated), o) in steps.iter().zip(results) {
        let o = o.into_inner().unwrap().unwrap();
        println!(
            "STEP\t{:.2}\t{:.2}\t{:?}\t{:?}\t{}\t{}\t{}\t{:.4}\t{:.4}\t{:+.2}\t{}\t{:?}",
            step.from.wpm,
            step.to.wpm,
            step.to.char_wpm,
            step.snr_2500_db,
            step.seed,
            gated,
            o.keyed.chars().count(),
            o.post_cer,
            o.control_cer,
            100.0 * (o.post_cer - o.control_cer),
            o.lock_s.map_or("-".to_string(), |s| format!("{s:.2}")),
            o.post_text
        );
    }
}
