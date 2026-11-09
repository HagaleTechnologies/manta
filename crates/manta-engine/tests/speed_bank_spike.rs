//! MAN-544 spike gate: the speed/timing hypothesis bank in observe-only mode
//! (`{ width: 3, switching: false }`) on `TrackDecoder` directly, fed 375 Hz
//! envelopes from `key_text_loop`/`key_text_steps`. It must never propose
//! leaving a correct lock on steady streams, and must propose the new speed
//! within 8 marks of a steep step. One `SPIKE` row per stream plus a
//! summary line:
//! `cargo test --release -p manta-engine --test speed_bank_spike -- --ignored --nocapture`
use manta_decode::decoder::{DecodeConfig, TrackDecoder};
use manta_decode::timing::{SpeedBankConfig, SwitchRecord};
use manta_testkit::keyer::{key_text_loop, key_text_steps, KeyerSpec};

const HOP_HZ: f64 = 375.0;
const OBSERVE: SpeedBankConfig = SpeedBankConfig {
    width: 3,
    switching: false,
};

const TEXTS: [&str; 28] = [
    "AU",
    "HH",
    "5NN",
    "K7X",
    "CQ",
    "TEST",
    "W1AW",
    "TU",
    "73",
    "EE",
    "SS",
    "IS",
    "CQ TEST W5AU",
    "599 001",
    "5NN 0042",
    "E 0000",
    "TEST 000",
    "OO",
    "MO",
    "T0M",
    "R 5NN TU",
    "CQ CQ DE K1ABC",
    "0000",
    "EIS 5",
    "EEE",
    "TTT",
    "TT",
    "EE EE",
];

fn within(x: f32, want: f32, frac: f32) -> bool {
    (x / want - 1.0).abs() <= frac
}

#[derive(Clone, Copy, Debug)]
enum Condition {
    Clean,
    Noise,
    Fade,
}

/// Feed `env` from `skip` on; returns the switch log and the bank's mark
/// count at hop `mark_at` (if reached).
fn run(env: &[f32], skip: usize, mark_at: usize) -> (Vec<SwitchRecord>, Option<u64>) {
    let cfg = DecodeConfig {
        speed_bank: OBSERVE,
        ..Default::default()
    };
    let mut d = TrackDecoder::new(1, cfg);
    let mut at = None;
    for (i, &a) in env.iter().enumerate().skip(skip) {
        if i == mark_at {
            at = Some(d.speed_bank().marks_seen());
        }
        let _ = d.push_envelope(a, i as u64 * 256);
    }
    let _ = d.finish();
    (d.speed_bank().switch_log().copied().collect(), at)
}

fn condition(env: &mut [f32], c: Condition, seed: u64) {
    let mut x = seed | 1;
    for (i, v) in env.iter_mut().enumerate() {
        match c {
            Condition::Clean => {}
            Condition::Noise => {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let u = (x >> 40) as f32 / (1u64 << 23) as f32 - 1.0;
                *v = (*v + 0.05 * u).max(0.0);
            }
            Condition::Fade => {
                let t = i as f32 / HOP_HZ as f32;
                *v *= 0.55 + 0.45 * (std::f32::consts::TAU * 0.5 * t).sin();
            }
        }
    }
}

#[test]
#[ignore]
fn speed_bank_spike() {
    let mut false_switches = Vec::new();
    let mut good = std::collections::BTreeMap::<&str, usize>::new();
    let mut streams = 0usize;
    for text in TEXTS {
        for wpm in [12.0f32, 16.0, 20.0, 25.0, 30.0, 35.0, 40.0] {
            let dit_ms = 1200.0 / wpm;
            let dit_hops = dit_ms as f64 * HOP_HZ / 1000.0;
            let (base, _) = key_text_loop(text, &KeyerSpec::new(wpm), HOP_HZ, 30.0).unwrap();
            for (oi, frac) in [0.0f64, 1.0 / 3.0, 0.5, 2.0 / 3.0].into_iter().enumerate() {
                let skip = (frac * dit_hops).round() as usize;
                for c in [Condition::Clean, Condition::Noise, Condition::Fade] {
                    let mut env = base.clone();
                    condition(&mut env, c, 0x9E37_79B9 + oi as u64);
                    let (log, _) = run(&env, skip, usize::MAX);
                    streams += 1;
                    let bad: Vec<_> = log
                        .iter()
                        .filter(|r| {
                            within(r.from_dit_ms, dit_ms, 0.2) && !within(r.to_dit_ms, dit_ms, 0.2)
                        })
                        .collect();
                    let ok = log
                        .iter()
                        .filter(|r| {
                            !within(r.from_dit_ms, dit_ms, 0.2) && within(r.to_dit_ms, dit_ms, 0.2)
                        })
                        .count();
                    *good.entry(text).or_default() += ok;
                    println!(
                        "SPIKE\tsteady\t{text}\t{wpm}\t{skip}\t{c:?}\t{}\t{ok}\t{}",
                        log.len(),
                        bad.len()
                    );
                    for r in bad {
                        false_switches.push(format!("{text} {wpm} WPM skip {skip} {c:?}: {r:?}"));
                    }
                }
            }
        }
    }
    let mut late = Vec::new();
    for (from, to) in [
        (20.0f32, 36.0f32),
        (20.0, 38.0),
        (20.0, 40.0),
        (15.0, 30.0),
        (20.0, 50.0),
        (40.0, 22.0),
        (40.0, 20.0),
        (32.0, 16.0),
    ] {
        let k = key_text_steps(
            "CQ TEST W1AW",
            &[(KeyerSpec::new(from), 16.0), (KeyerSpec::new(to), 30.0)],
            HOP_HZ,
        )
        .unwrap();
        let (log, at) = run(&k.env, 0, k.steps[0]);
        let at = at.unwrap_or(0);
        let new_dit = 1200.0 / to;
        let hit = log
            .iter()
            .find(|r| r.mark_index > at && within(r.to_dit_ms, new_dit, 0.2));
        streams += 1;
        println!(
            "SPIKE\tstep\t{from}->{to}\t{at}\t{}\t{:?}",
            log.len(),
            hit.map(|r| r.mark_index - at)
        );
        match hit {
            Some(r) if r.mark_index - at <= 8 => {}
            _ => late.push(format!("{from}->{to}: step at mark {at}, log {log:?}")),
        }
    }
    println!(
        "SPIKE-SUMMARY\tstreams {streams}\tfalse {}\tlate-steps {}\tgood {:?}",
        false_switches.len(),
        late.len(),
        good
    );
    assert!(
        false_switches.is_empty() && late.is_empty(),
        "false would-switches:\n{}\nsteps not proposed within 8 marks:\n{}",
        false_switches.join("\n"),
        late.join("\n")
    );
}
