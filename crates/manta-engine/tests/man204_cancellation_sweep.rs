//! MAN-204 measurement sweep: guarded two-stage cancellation across the
//! strata, seeds, weak-B levels and deliberately degraded reconstructions.
//! It asserts only that it ran and prints Markdown tables, sorted, so the
//! output is deterministic. Its numbers are in
//! docs/DECISIONS/2026-11-03-man204-guarded-two-stage-cancellation.md.
//!
//! cargo test -p manta-engine --test man204_cancellation_sweep --locked -- --ignored --nocapture

#[path = "support/man204.rs"]
mod support;

use manta_dsp::cancel::{
    estimate_strong, find_elements, fit_elements, residual_check, subtract, Hypothesis,
    StrongEstimate,
};
use manta_engine::cancel::{decode_with_cancellation, CancelOptions, CancellationReport};
use manta_engine::{decode_samples, NoSignal, PipelineConfig};
use num_complex::{Complex32, Complex64};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use support::*;

/// Run `jobs` on a fixed pool of scoped threads; results come back in job
/// order whatever order the threads finish in.
fn par_map<T: Sync, R: Send>(jobs: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get());
    let next = AtomicUsize::new(0);
    let out: Mutex<Vec<(usize, R)>> = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..workers.min(jobs.len()) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(job) = jobs.get(i) else { break };
                let r = f(job);
                out.lock().unwrap().push((i, r));
            });
        }
    });
    let mut out = out.into_inner().unwrap();
    out.sort_by_key(|(i, _)| *i);
    out.into_iter().map(|(_, r)| r).collect()
}

fn hyp(h: Option<Hypothesis>) -> String {
    match h {
        Some(Hypothesis::TwoSignalCw { offset_hz }) => format!("two {offset_hz:+.1}"),
        Some(h) => format!("{h:?}"),
        None => "-".into(),
    }
}

fn summary(r: &CancellationReport, true_a: f64) -> String {
    let c = r.candidates.first();
    let ev = c.and_then(|c| c.evidence);
    let check = c.and_then(|c| c.check);
    let recovered: Vec<_> = r
        .recovered
        .iter()
        .map(|i| format!("{}@{:+.1}", i.callsign, i.freq_hz - CENTER))
        .collect();
    let rejected: Vec<_> = r
        .rejected
        .iter()
        .map(|i| format!("{}:{:?}", i.callsign, i.reason))
        .collect();
    format!(
        "{} | {} | {} | {} | {} | {} | {} | {} | {} | {}",
        calls(&r.original.events, B_CALL),
        r.residual.as_ref().map_or(0, |b| calls(&b.events, B_CALL)),
        recovered.join(" "),
        rejected.join(" "),
        hyp(c.and_then(|c| c.hypothesis)),
        ev.map_or("-".into(), |e| format!("{:.3}", e.hot_fraction)),
        ev.map_or("-".into(), |e| format!("{:.2}", e.coherence)),
        check.map_or("-".into(), |k| format!(
            "{:.1} / {:.1}",
            k.edge_over_neighbour_db, k.interior_over_neighbour_db
        )),
        c.map_or("-".into(), |c| format!(
            "{:+.2}",
            c.carrier_hint_hz - true_a
        )),
        c.map_or("-".into(), |c| format!("{:+.3}", c.carrier_hz - true_a)),
    )
}

const SUMMARY_HEAD: &str = "baseline B | residual B | recovered | rejected | hypothesis | h | c | edge/nb / interior/nb dB | hint err | refined err";

fn decode(scene: &[Complex32], opts: &CancelOptions) -> CancellationReport {
    decode_with_cancellation(scene, FS, CENTER, &PipelineConfig::default(), opts).unwrap()
}

#[test]
#[ignore = "MAN-204 measurement sweep (about 4 min on 32 cores, dev profile); run before handover"]
fn man204_cancellation_sweep() {
    // 1. Strata, seed 204.
    let mut cells = Vec::new();
    for a_off in [A_OFF, A_OFF + MID] {
        for a_snr in [20.0f32, 30.0, 40.0] {
            for db in [-80.0, -40.0, 20.0, 30.0, 40.0, 60.0, 80.0, 120.0] {
                cells.push((a_off, a_snr, db));
            }
        }
    }
    let rows = par_map(&cells, |&(a_off, a_snr, db)| {
        let r = decode(
            &pair(a_off, a_snr, a_off + db, 10.0, 204),
            &CancelOptions::default(),
        );
        format!(
            "| {} | {:+} | {} | {} |",
            if a_off == A_OFF { "on-bin" } else { "mid" },
            db,
            a_snr - 10.0,
            summary(&r, a_off)
        )
    });
    println!("\n### Strata (seed 204, B +10 dB)\n");
    println!("| A | B rel. A Hz | diff dB | {SUMMARY_HEAD} |");
    println!("{}", "|---".repeat(13) + "|");
    for r in rows {
        println!("{r}");
    }

    // 2. Reference cell on six seeds; A-alone controls, gate on and off.
    let mut jobs: Vec<(String, u64, f64, f32, bool, bool)> = Vec::new();
    for seed in [204u64, 2, 3, 5, 6, 7] {
        jobs.push(("pair".into(), seed, A_OFF, 30.0, true, true));
    }
    for seed in [204u64, 2, 3] {
        for a_off in [A_OFF, A_OFF + MID] {
            for a_snr in [20.0f32, 30.0, 40.0] {
                for gate in [true, false] {
                    jobs.push(("alone".into(), seed, a_off, a_snr, gate, false));
                }
            }
        }
    }
    let rows = par_map(&jobs, |(kind, seed, a_off, a_snr, gate, is_pair)| {
        let scene = if *is_pair {
            pair(*a_off, *a_snr, a_off + 40.0, 10.0, *seed)
        } else {
            alone(*a_off, *a_snr, *seed)
        };
        let r = decode(
            &scene,
            &CancelOptions {
                evidence_gate: *gate,
                ..Default::default()
            },
        );
        let spots = r.residual.as_ref().map_or(0, |b| b.spots.len());
        format!(
            "| {kind} | {seed} | {} | {a_snr} | {gate} | {spots} | {} |",
            if *a_off == A_OFF { "on-bin" } else { "mid" },
            summary(&r, *a_off)
        )
    });
    println!("\n### Seeds and single-signal controls\n");
    println!("| scene | seed | A | A dB | gate | residual spots | {SUMMARY_HEAD} |");
    println!("{}", "|---".repeat(16) + "|");
    for r in rows {
        println!("{r}");
    }

    // 3. Weak B.
    let rows = par_map(&[0.0f32, -5.0], |&b_snr| {
        let r = decode(
            &pair(A_OFF, 30.0, A_OFF + 40.0, b_snr, 204),
            &CancelOptions::default(),
        );
        format!("| {b_snr} | {} |", summary(&r, A_OFF))
    });
    println!("\n### Weak B (+40 Hz, seed 204)\n");
    println!("| B dB | {SUMMARY_HEAD} |");
    println!("{}", "|---".repeat(11) + "|");
    for r in rows {
        println!("{r}");
    }

    // 4. Degraded reconstructions, forced subtraction.
    let mut jobs = Vec::new();
    for seed in [204u64, 2, 3] {
        for is_pair in [true, false] {
            for d in DEGRADATIONS {
                jobs.push((seed, is_pair, *d));
            }
        }
    }
    let rows = par_map(&jobs, |&(seed, is_pair, d)| degraded_row(seed, is_pair, d));
    println!("\n### Degraded reconstructions (forced subtraction)\n");
    println!("| scene | seed | degradation | edge/nb dB | interior/nb dB | verdict | weak calls | residual spots | tracks |");
    println!("{}", "|---".repeat(9) + "|");
    for r in rows {
        println!("{r}");
    }
}

#[derive(Debug, Clone, Copy)]
enum Degradation {
    None,
    Gain(f64),
    PhaseDeg(f64),
    ShiftMs(f64),
    JitterMs(f64),
    CarrierHz(f64),
}

const DEGRADATIONS: &[Degradation] = &[
    Degradation::None,
    Degradation::Gain(1.01),
    Degradation::Gain(1.03),
    Degradation::Gain(1.10),
    Degradation::PhaseDeg(3.0),
    Degradation::PhaseDeg(10.0),
    Degradation::ShiftMs(0.33),
    Degradation::ShiftMs(0.67),
    Degradation::ShiftMs(1.33),
    Degradation::JitterMs(0.2),
    Degradation::JitterMs(0.4),
    Degradation::JitterMs(0.8),
    Degradation::CarrierHz(0.2),
    Degradation::CarrierHz(1.0),
    Degradation::CarrierHz(3.0),
];

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Standard normal keyed on `i` (splitmix64 + Box-Muller).
fn gauss(i: u64) -> f64 {
    let u1 = ((splitmix64(2 * i) >> 11) as f64 + 1.0) / (1u64 << 53) as f64;
    let u2 = ((splitmix64(2 * i + 1) >> 11) as f64) / (1u64 << 53) as f64;
    (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
}

fn degrade(scene: &[Complex32], hint: f64, d: Degradation) -> StrongEstimate {
    if let Degradation::CarrierHz(err) = d {
        let carrier = hint + err;
        let mut fits = fit_elements(scene, FS, carrier, &find_elements(scene, FS, carrier));
        for f in &mut fits {
            f.slope_rad_per_sample = 0.0;
        }
        return StrongEstimate {
            carrier_hz: carrier,
            carrier_step_hz: 0.0,
            fits,
            elements: Vec::new(),
        };
    }
    let mut est = estimate_strong(scene, FS, hint);
    for (i, f) in est.fits.iter_mut().enumerate() {
        match d {
            Degradation::Gain(g) => f.gain *= g,
            Degradation::PhaseDeg(p) => f.gain *= Complex64::from_polar(1.0, p.to_radians()),
            Degradation::ShiftMs(ms) => {
                f.element.start += ms * 1e-3 * FS;
                f.element.end += ms * 1e-3 * FS;
            }
            Degradation::JitterMs(ms) => {
                f.element.start += ms * 1e-3 * FS * gauss(2 * i as u64);
                f.element.end += ms * 1e-3 * FS * gauss(2 * i as u64 + 1);
            }
            Degradation::None | Degradation::CarrierHz(_) => {}
        }
    }
    est
}

fn degraded_row(seed: u64, is_pair: bool, d: Degradation) -> String {
    let scene = if is_pair {
        pair(A_OFF, 30.0, A_OFF + 40.0, 10.0, seed)
    } else {
        alone(A_OFF, 30.0, seed)
    };
    // The hint the experiment would take: A's carrier plus the 0.3 Hz a
    // spot typically misses by (plan table 1).
    let est = degrade(&scene, A_OFF + 0.3, d);
    let check = residual_check(&scene, FS, &est);
    let mut residual = scene.clone();
    subtract(&mut residual, FS, &est);
    let (weak, spots, tracks) =
        match decode_samples(&residual, FS, CENTER, &PipelineConfig::default()) {
            Ok(r) => (
                calls(&r.events, B_CALL),
                r.spots
                    .iter()
                    .map(|s| format!("{}@{:+.1}", s.callsign, s.freq_hz - CENTER))
                    .collect::<Vec<_>>()
                    .join(" "),
                texts(&r.events).len(),
            ),
            Err(e) if e.downcast_ref::<NoSignal>().is_some() => (0, String::new(), 0),
            Err(e) => panic!("{e}"),
        };
    format!(
        "| {} | {seed} | {d:?} | {:.1} | {:.1} | {} | {weak} | {spots} | {tracks} |",
        if is_pair { "pair" } else { "alone" },
        check.edge_over_neighbour_db,
        check.interior_over_neighbour_db,
        if check.clean() { "Clean" } else { "Dirty" },
    )
}

/// A MAN-309 `cf32` recording, replayed into memory, through the experiment.
/// Skips unless `MANTA_MAN204_RECORDING` names one.
#[test]
#[ignore = "MAN-204 probe: needs MANTA_MAN204_RECORDING"]
fn man204_recording_probe() {
    let Some(path) = std::env::var_os("MANTA_MAN204_RECORDING") else {
        eprintln!("skipping: MANTA_MAN204_RECORDING is not set");
        return;
    };
    let mut src = manta_input::open_iq_file(
        std::path::Path::new(&path),
        manta_input::IqFileOptions::default(),
    )
    .unwrap();
    let (fs, center) = (src.sample_rate(), src.center_freq_hz());
    let mut iq = Vec::new();
    let mut buf = vec![Complex32::new(0.0, 0.0); 1 << 16];
    loop {
        let n = src.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        iq.extend_from_slice(&buf[..n]);
    }
    let r = decode_with_cancellation(
        &iq,
        fs,
        center,
        &PipelineConfig::default(),
        &CancelOptions::default(),
    )
    .unwrap();
    println!("original spots: {}", r.original.spots.len());
    for c in &r.candidates {
        println!(
            "candidate {} track {} carrier {:+.2} fits {} {} check {:?} -> {:?}",
            c.callsign,
            c.track_id,
            c.carrier_hz,
            c.fits,
            hyp(c.hypothesis),
            c.check,
            c.outcome
        );
    }
    println!(
        "residual spots: {:?}",
        r.residual.as_ref().map(|b| b.spots.len())
    );
    println!("recovered: {:?}", r.recovered);
    println!("rejected: {:?}", r.rejected);
}
