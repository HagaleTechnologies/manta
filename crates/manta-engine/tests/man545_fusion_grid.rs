//! MAN-545 Phase 3: the measuring instrument behind the evidence-fusion
//! decision record. It decodes the `manta bench sensitivity` grid with
//! legacy (beam runner-ups on, refinement off), HSMM (refinement off) and
//! production legacy (refinement on), applies every pre-registered fuser
//! (plan D4) to each recording, replays each fused event stream through the
//! production spot validator, scores it exactly as the bench does, and
//! writes one bench-format report per engine and fuser into `MAN545_OUT`.
//!
//! Run with (defaults are the bench's grid over three trials):
//!
//! MAN545_OUT=<dir> cargo test --release -p manta-engine --locked \
//!   --test man545_fusion_grid -- --ignored --nocapture
//!
//! Its built-in assertions are the tests: it panics when replaying the
//! unmodified legacy events does not reproduce the decode's spots exactly,
//! or when a control fuser (`noop`, `dup-sub`, `dup-span`) scores
//! differently from legacy in any station field or bogus list.
#[path = "support/man545.rs"]
mod support;

use manta_decode::decoder::Engine as DecoderEngine;
use manta_decode::events::DecoderEvent;
use manta_engine::{decode_samples, DecodeReport, PipelineConfig};
use manta_spot::Spot;
use manta_testkit::sensitivity::{self, Condition};
use std::path::PathBuf;
use std::time::Instant;
use support::*;

/// The bench's defaults (`bench.rs`): 60 s recordings, seed 1.
const DURATION_S: u32 = 60;
const SEED: u64 = 1;
const CARGO_COMMAND: &str =
    "cargo test --release -p manta-engine --locked --test man545_fusion_grid -- --ignored --nocapture";

/// The grid, from the `MAN545_*` environment variables.
struct Grid {
    out: PathBuf,
    trial_start: u32,
    trials: u32,
    conditions: Vec<Condition>,
    wpm: Vec<u32>,
    snr_db: Vec<f64>,
    jobs: usize,
    /// `MAN545_CALIBRATION`: tables to use instead of cross-fitting. A
    /// run's `calibration.json` is fitted on every trial of that run, so it
    /// is only for a confirmation run on other trials (plan D8, for example
    /// `MAN545_TRIAL_START=3`); on the same trials it leaks their truth.
    calibration: Option<PathBuf>,
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn list<T: std::str::FromStr>(name: &str, default: &str) -> Vec<T> {
    env_or(name, default)
        .split(',')
        .map(|s| {
            s.trim()
                .parse()
                .unwrap_or_else(|_| panic!("{name}: cannot parse {s:?}"))
        })
        .collect()
}

impl Grid {
    fn from_env() -> Self {
        let out = std::env::var_os("MAN545_OUT")
            .map(PathBuf::from)
            .expect("set MAN545_OUT to the directory the reports are written to");
        let conditions = env_or("MAN545_CONDITIONS", "awgn,good,poor")
            .split(',')
            .map(|s| {
                Condition::ALL
                    .into_iter()
                    .find(|c| c.name() == s.trim())
                    .unwrap_or_else(|| panic!("MAN545_CONDITIONS: unknown condition {s:?}"))
            })
            .collect();
        let grid = Grid {
            out,
            trial_start: env_or("MAN545_TRIAL_START", "0")
                .parse()
                .expect("MAN545_TRIAL_START"),
            trials: env_or("MAN545_TRIALS", "3").parse().expect("MAN545_TRIALS"),
            conditions,
            wpm: list("MAN545_WPM", "15,25,35"),
            snr_db: list("MAN545_SNR", "0,3,6,9,12,15,20,25,30"),
            jobs: env_or("MAN545_JOBS", "2")
                .parse::<usize>()
                .expect("MAN545_JOBS")
                .max(1),
            calibration: std::env::var_os("MAN545_CALIBRATION").map(PathBuf::from),
        };
        assert!(grid.trials >= 1, "MAN545_TRIALS must be at least 1");
        // The bench sorts and de-duplicates its lists, so a grid in any
        // other order would pool and print points in a different order.
        let rank = |c: &Condition| Condition::ALL.iter().position(|a| a == c);
        assert!(
            grid.conditions
                .windows(2)
                .all(|w| rank(&w[0]) < rank(&w[1])),
            "MAN545_CONDITIONS must be distinct, in the order awgn,good,poor"
        );
        assert!(
            grid.wpm.windows(2).all(|w| w[0] < w[1]),
            "MAN545_WPM must be strictly ascending"
        );
        assert!(
            grid.snr_db.windows(2).all(|w| w[0] < w[1]),
            "MAN545_SNR must be strictly ascending"
        );
        grid
    }

    /// The reports' `command`: the cargo command and every grid variable
    /// that changes results. `MAN545_JOBS` and `MAN545_OUT` never do, so
    /// they are left out and the files do not depend on them.
    fn command(&self) -> String {
        let join = |v: Vec<String>| v.join(",");
        let mut cmd = format!(
            "MAN545_TRIAL_START={} MAN545_TRIALS={} MAN545_CONDITIONS={} MAN545_WPM={} MAN545_SNR={}",
            self.trial_start,
            self.trials,
            join(self.conditions.iter().map(|c| c.name().to_string()).collect()),
            join(self.wpm.iter().map(u32::to_string).collect()),
            join(self.snr_db.iter().map(f64::to_string).collect()),
        );
        if self.calibration.is_some() {
            cmd.push_str(" MAN545_CALIBRATION=<set>");
        }
        format!("{cmd} {CARGO_COMMAND}")
    }

    fn trials(&self) -> impl Iterator<Item = u32> {
        self.trial_start..self.trial_start + self.trials
    }
}

/// The engine reports the instrument writes besides the fusers.
const LEGACY: usize = 0;
const HSMM: usize = 1;
const LEGACY_REFINE: usize = 2;
const ENGINES: [(&str, &str); 3] = [
    ("legacy", "legacy.json"),
    ("hsmm", "hsmm.json"),
    ("legacy+refine", "legacy-refine.json"),
];

/// The pre-registered fusers (plan D4): report name, file, policy, and
/// whether the second branch is legacy itself (the duplicate controls).
const FUSERS: [(&str, &str, Policy, bool); 8] = [
    ("fused:noop", "fused-noop.json", Policy::Noop, false),
    ("fused:dup-sub", "fused-dup-sub.json", Policy::CalSub, true),
    (
        "fused:dup-span",
        "fused-dup-span.json",
        Policy::CalSpan,
        true,
    ),
    (
        "fused:oracle-legacy-alts",
        "fused-oracle-legacy-alts.json",
        Policy::OracleLegacyAlts,
        false,
    ),
    (
        "fused:oracle-sub",
        "fused-oracle-sub.json",
        Policy::OracleSub,
        false,
    ),
    (
        "fused:oracle-span",
        "fused-oracle-span.json",
        Policy::OracleSpan,
        false,
    ),
    ("fused:cal-sub", "fused-cal-sub.json", Policy::CalSub, false),
    (
        "fused:cal-span",
        "fused-cal-span.json",
        Policy::CalSpan,
        false,
    ),
];
/// Fusers that must equal legacy exactly.
const CONTROLS: [&str; 3] = ["fused:noop", "fused:dup-sub", "fused:dup-span"];
/// Fusers that need calibration tables fitted on other trials (or loaded).
const REALIZABLE: [&str; 2] = ["fused:cal-sub", "fused:cal-span"];

fn configs() -> [PipelineConfig; 3] {
    let mut legacy = PipelineConfig::default();
    legacy.decode.engine = DecoderEngine::Legacy;
    legacy.decode.refine.enabled = false;
    legacy.decode.legacy_alternatives = true;
    let mut hsmm = PipelineConfig::default();
    hsmm.decode.engine = DecoderEngine::Hsmm;
    hsmm.decode.refine.enabled = false;
    let mut production = PipelineConfig::default();
    production.decode.engine = DecoderEngine::Legacy;
    [legacy, hsmm, production]
}

/// `None` when the engine finds no signal (the bench's empty decode).
fn decode(iq: &[num_complex::Complex32], cfg: &PipelineConfig) -> Option<DecodeReport> {
    match decode_samples(
        iq,
        sensitivity::SAMPLE_RATE_HZ,
        sensitivity::CENTER_FREQ_HZ,
        cfg,
    ) {
        Ok(r) => Some(r),
        Err(e) if e.to_string().starts_with("no signal found") => None,
        Err(e) => panic!("decode failed: {e}"),
    }
}

/// What pass 2 needs of one recording; the samples are dropped.
struct Recording {
    trial: u32,
    point: usize,
    condition: Condition,
    wpm: u32,
    snr_db: f64,
    stations: Vec<Station>,
    /// Legacy (runner-ups on, refinement off): events and spots.
    legacy: Option<(Vec<DecoderEvent>, Vec<Spot>)>,
    legacy_items: Vec<Vec<Item>>,
    hsmm_items: Vec<Vec<Item>>,
    truth: Vec<Vec<char>>,
    scores: [PointScore; 3],
    wall_s: [f64; 3],
}

fn decode_recording(
    iq: &[num_complex::Complex32],
    cfgs: &[PipelineConfig; 3],
    stations: &[Station],
) -> (
    Option<DecodeReport>,
    Option<DecodeReport>,
    PointScore,
    [f64; 3],
) {
    let mut wall_s = [0.0; 3];
    let mut timed = |i: usize| {
        let t = Instant::now();
        let r = decode(iq, &cfgs[i]);
        wall_s[i] = t.elapsed().as_secs_f64();
        r
    };
    let legacy = timed(LEGACY);
    let hsmm = timed(HSMM);
    let production = timed(LEGACY_REFINE);
    let score = score_point(
        stations,
        production
            .as_ref()
            .map(|r| (r.events.as_slice(), r.spots.as_slice())),
    );
    (legacy, hsmm, score, wall_s)
}

fn items_per_station(report: Option<&DecodeReport>, stations: &[Station]) -> Vec<Vec<Item>> {
    let Some(r) = report else {
        return vec![Vec::new(); stations.len()];
    };
    let freqs = track_freqs(&r.events);
    stations
        .iter()
        .map(|st| station_items(&r.events, &freqs, st.freq_hz))
        .collect()
}

/// Pass 1: decode every recording three times, in the bench's order.
fn pass1(grid: &Grid, cfgs: &[PipelineConfig; 3]) -> Vec<Recording> {
    let duration_s = DURATION_S as f64;
    let per_trial = grid.conditions.len() * grid.wpm.len() * grid.snr_db.len();
    let mut out = Vec::new();
    for trial in grid.trials() {
        let noise = sensitivity::render_noise(duration_s, SEED, trial).expect("noise");
        let calls = sensitivity::series_calls(trial);
        let mut point = 0usize;
        for &condition in &grid.conditions {
            for &wpm in &grid.wpm {
                let series = sensitivity::series_signals(condition, wpm as f32, trial, SEED);
                let (clean, texts) =
                    sensitivity::render_series_chunked(&series, duration_s, grid.jobs)
                        .expect("series");
                let stations: Vec<Station> = calls
                    .iter()
                    .zip(texts)
                    .enumerate()
                    .map(|(j, (call, keyed_text))| {
                        let offset_hz = sensitivity::station_offset_hz(j);
                        Station {
                            trial,
                            call: call.clone(),
                            offset_hz,
                            freq_hz: sensitivity::CENTER_FREQ_HZ + offset_hz,
                            keyed_text,
                        }
                    })
                    .collect();
                for chunk in grid.snr_db.chunks(grid.jobs) {
                    let (clean, noise, stations) = (&clean, &noise, &stations);
                    let decoded: Vec<_> = std::thread::scope(|s| {
                        let handles: Vec<_> = chunk
                            .iter()
                            .map(|&snr_db| {
                                s.spawn(move || {
                                    let gain = sensitivity::gain_for_snr_2500(snr_db - RBN_CORR_DB);
                                    let iq = sensitivity::compose(clean, noise, gain);
                                    decode_recording(&iq, cfgs, stations)
                                })
                            })
                            .collect();
                        handles.into_iter().map(|h| h.join().unwrap()).collect()
                    });
                    for (&snr_db, (legacy, hsmm, production, wall_s)) in chunk.iter().zip(decoded) {
                        let legacy_items = items_per_station(legacy.as_ref(), stations);
                        let hsmm_items = items_per_station(hsmm.as_ref(), stations);
                        let score = |r: &Option<DecodeReport>| {
                            score_point(
                                stations,
                                r.as_ref()
                                    .map(|r| (r.events.as_slice(), r.spots.as_slice())),
                            )
                        };
                        let scores = [score(&legacy), score(&hsmm), production];
                        out.push(Recording {
                            trial,
                            point,
                            condition,
                            wpm,
                            snr_db,
                            stations: stations.clone(),
                            legacy: legacy.map(|r| (r.events, r.spots)),
                            legacy_items,
                            hsmm_items,
                            truth: stations
                                .iter()
                                .map(|st| normalize(&st.keyed_text))
                                .collect(),
                            scores,
                            wall_s,
                        });
                        point += 1;
                    }
                }
            }
        }
        assert_eq!(point, per_trial);
    }
    out
}

/// Every truth label of every recording, tagged by trial and engine.
fn labels(recs: &[Recording]) -> Vec<(u32, Engine, Label)> {
    let mut out = Vec::new();
    for r in recs {
        for (j, truth) in r.truth.iter().enumerate() {
            for (engine, items) in [
                (Engine::Legacy, &r.legacy_items[j]),
                (Engine::Hsmm, &r.hsmm_items[j]),
            ] {
                out.extend(
                    truth_labels(truth, items)
                        .into_iter()
                        .map(|l| (r.trial, engine, l)),
                );
            }
        }
    }
    out
}

/// One fuser applied to one recording, replayed through the validator and
/// scored like the bench.
fn fuse_and_score(
    r: &Recording,
    policy: Policy,
    duplicate: bool,
    cal: Option<&Calibration>,
    cfg: &PipelineConfig,
) -> PointScore {
    let Some((events, _)) = &r.legacy else {
        return score_point(&r.stations, None);
    };
    let inputs: Vec<StationInput> = (0..r.stations.len())
        .map(|j| StationInput {
            legacy: &r.legacy_items[j],
            hsmm: if duplicate {
                &r.legacy_items[j]
            } else {
                &r.hsmm_items[j]
            },
            truth: &r.truth[j],
        })
        .collect();
    let fused = fuse(policy, events, &inputs, cal);
    let spots = replay(&fused, cfg);
    score_point(&r.stations, Some((&fused, &spots)))
}

fn replay(events: &[DecoderEvent], cfg: &PipelineConfig) -> Vec<Spot> {
    let mut v = cfg
        .validator(sensitivity::SAMPLE_RATE_HZ)
        .expect("validator");
    events.iter().flat_map(|e| v.ingest(e)).collect()
}

/// Legacy's character errors against the keyed truth and what each
/// engine's evidence held at them (the plan-phase probe's counts).
#[derive(Default)]
struct Chars {
    /// Truth items: characters and word spaces.
    truth: usize,
    /// Legacy characters, spaces and separators left out.
    legacy: usize,
    with_runner_up: usize,
    sub: usize,
    sub_hsmm_right: usize,
    sub_in_hsmm_alts: usize,
    sub_in_legacy_alts: usize,
    del: usize,
    del_hsmm_right: usize,
    ins: usize,
}

/// Timing of characters both engines decoded right, |Δ sample_ts|.
#[derive(Default)]
struct Timing {
    pairs: usize,
    under_25ms: usize,
    under_100ms: usize,
    under_250ms: usize,
    at_least_250ms: usize,
}

fn tally(r: &Recording, chars: &mut Chars, timing: &mut Timing) {
    for (j, truth) in r.truth.iter().enumerate() {
        let (l, h) = (&r.legacy_items[j], &r.hsmm_items[j]);
        // Truth position -> the HSMM item aligned to it, and whether it matches.
        let mut hsmm_at: Vec<Option<(usize, bool)>> = vec![None; truth.len()];
        for (op, hi, ti) in align(&support::chars(h), truth) {
            if let (Some(hi), Some(ti)) = (hi, ti) {
                hsmm_at[ti] = Some((hi, op == Op::Match));
            }
        }
        chars.truth += truth.len();
        for it in l.iter().filter(|it| it.c != SEPARATOR && it.c != ' ') {
            chars.legacy += 1;
            chars.with_runner_up += usize::from(!it.alts.is_empty());
        }
        for (op, li, ti) in align(&support::chars(l), truth) {
            match op {
                Op::Match => {
                    let (li, ti) = (li.unwrap(), ti.unwrap());
                    if let Some((hi, true)) = hsmm_at[ti] {
                        let dt = l[li].ts.abs_diff(h[hi].ts) as f64 / sensitivity::SAMPLE_RATE_HZ;
                        timing.pairs += 1;
                        match dt {
                            d if d < 0.025 => timing.under_25ms += 1,
                            d if d < 0.100 => timing.under_100ms += 1,
                            d if d < 0.250 => timing.under_250ms += 1,
                            _ => timing.at_least_250ms += 1,
                        }
                    }
                }
                Op::Sub => {
                    let (li, ti) = (li.unwrap(), ti.unwrap());
                    if l[li].c == SEPARATOR {
                        continue;
                    }
                    chars.sub += 1;
                    let t = truth[ti];
                    match hsmm_at[ti] {
                        Some((_, true)) => chars.sub_hsmm_right += 1,
                        Some((hi, false)) if h[hi].alts.iter().any(|&(a, _)| a == t) => {
                            chars.sub_in_hsmm_alts += 1
                        }
                        _ => {}
                    }
                    chars.sub_in_legacy_alts +=
                        usize::from(l[li].alts.iter().any(|&(a, _)| a == t));
                }
                Op::RightOnly => {
                    chars.del += 1;
                    chars.del_hsmm_right +=
                        usize::from(matches!(hsmm_at[ti.unwrap()], Some((_, true))));
                }
                Op::LeftOnly => {
                    chars.ins += usize::from(l[li.unwrap()].c != SEPARATOR);
                }
            }
        }
    }
}

fn pooled(grid: &Grid, recs: &[Recording], scores: &[PointScore]) -> Vec<PointResult> {
    let mut points: Vec<PointResult> = Vec::new();
    for &condition in &grid.conditions {
        for &wpm in &grid.wpm {
            for &snr_db in &grid.snr_db {
                points.push(PointResult::new(condition, wpm, snr_db));
            }
        }
    }
    // Recordings are in trial order, so each point pools trial by trial.
    for (r, s) in recs.iter().zip(scores) {
        points[r.point].add(s.clone());
    }
    points
}

fn spotted(s: &PointScore) -> usize {
    s.stations.iter().filter(|st| st.spotted).count()
}

/// The MAN-545 measuring instrument. About 80 minutes release on 2 CPUs
/// for the default grid; see the module doc for the command.
#[test]
#[ignore = "MAN-545 measuring instrument, about 80 minutes release on 2 CPUs for the default grid"]
fn man545_fusion_grid() {
    let grid = Grid::from_env();
    std::fs::create_dir_all(&grid.out).expect("create MAN545_OUT");
    let cfgs = configs();
    let recs = pass1(&grid, &cfgs);

    // Item 16's replay check: the unmodified legacy events reproduce the
    // decode's spots exactly, so a fused stream's spots are the fuser's.
    for r in &recs {
        if let Some((events, spots)) = &r.legacy {
            assert_eq!(
                &replay(events, &cfgs[LEGACY]),
                spots,
                "legacy replay is not faithful at {} {} WPM {} dB trial {}",
                r.condition.name(),
                r.wpm,
                r.snr_db,
                r.trial
            );
        }
    }

    // Calibration: loaded, or cross-fitted per held-out trial (plan D6).
    let all_labels = labels(&recs);
    let full = Calibration::fit(&all_labels, None);
    let loaded = grid.calibration.as_ref().map(|p| {
        calibration_from_json(&std::fs::read_to_string(p).expect("read MAN545_CALIBRATION"))
            .unwrap_or_else(|e| panic!("MAN545_CALIBRATION: {e}"))
    });
    let held_out: std::collections::BTreeMap<u32, Calibration> = grid
        .trials()
        .map(|t| {
            let cal = loaded
                .clone()
                .unwrap_or_else(|| Calibration::fit(&all_labels, Some(t)));
            (t, cal)
        })
        .collect();
    let realizable = loaded.is_some() || grid.trials >= 2;

    // Pass 2: every fuser on every recording.
    let mut fused: Vec<(&str, &str, Vec<PointScore>)> = Vec::new();
    for &(name, file, policy, duplicate) in &FUSERS {
        if REALIZABLE.contains(&name) && !realizable {
            continue;
        }
        let scores: Vec<PointScore> = recs
            .iter()
            .map(|r| {
                let cal =
                    matches!(policy, Policy::CalSub | Policy::CalSpan).then(|| &held_out[&r.trial]);
                fuse_and_score(r, policy, duplicate, cal, &cfgs[LEGACY])
            })
            .collect();
        fused.push((name, file, scores));
    }

    for (name, _, scores) in &fused {
        if !CONTROLS.contains(name) {
            continue;
        }
        for (r, s) in recs.iter().zip(scores) {
            assert_eq!(
                s,
                &r.scores[LEGACY],
                "{name} differs from legacy at {} {} WPM {} dB trial {}",
                r.condition.name(),
                r.wpm,
                r.snr_db,
                r.trial
            );
        }
    }

    // The reports.
    let command = grid.command();
    let write = |file: &str, text: String| {
        std::fs::write(grid.out.join(file), text).unwrap_or_else(|e| panic!("write {file}: {e}"));
    };
    let render = |engine: &str, scores: &[PointScore]| {
        render_bench_json(
            engine,
            &command,
            DURATION_S,
            grid.trials,
            SEED,
            &pooled(&grid, &recs, scores),
        ) + "\n"
    };
    for (i, (engine, file)) in ENGINES.iter().enumerate() {
        let scores: Vec<PointScore> = recs.iter().map(|r| r.scores[i].clone()).collect();
        write(file, render(engine, &scores));
    }
    for (name, file, scores) in &fused {
        write(file, render(name, scores));
    }
    let cal_json = calibration_to_json(&full);
    assert_eq!(calibration_from_json(&cal_json).as_ref(), Ok(&full));
    write("calibration.json", cal_json + "\n");

    // stdout, in the plan's order.
    let names: Vec<&str> = ENGINES
        .iter()
        .map(|(e, _)| *e)
        .chain(fused.iter().map(|(n, _, _)| *n))
        .collect();
    println!(
        "MAN545-POINT\tcondition\twpm\tsnr_db\ttrial\t{}",
        names.join("\t")
    );
    for (k, r) in recs.iter().enumerate() {
        let counts: Vec<String> = r
            .scores
            .iter()
            .chain(fused.iter().map(|(_, _, s)| &s[k]))
            .map(|s| spotted(s).to_string())
            .collect();
        println!(
            "MAN545-POINT\t{}\t{}\t{}\t{}\t{}",
            r.condition.name(),
            r.wpm,
            r.snr_db,
            r.trial,
            counts.join("\t")
        );
    }
    let (mut chars, mut timing) = (Chars::default(), Timing::default());
    for r in &recs {
        tally(r, &mut chars, &mut timing);
    }
    println!(
        "MAN545-CHARS\ttruth_items={}\tlegacy_chars={}\twith_runner_up={}",
        chars.truth, chars.legacy, chars.with_runner_up
    );
    println!(
        "MAN545-CHARS\tsub={}\thsmm_right={}\tin_hsmm_alts={}\tin_legacy_alts={}",
        chars.sub, chars.sub_hsmm_right, chars.sub_in_hsmm_alts, chars.sub_in_legacy_alts
    );
    println!(
        "MAN545-CHARS\tdel={}\thsmm_right={}",
        chars.del, chars.del_hsmm_right
    );
    println!("MAN545-CHARS\tins={}", chars.ins);
    println!(
        "MAN545-TIMING\tpairs={}\tunder_25ms={}\tunder_100ms={}\tunder_250ms={}\tat_least_250ms={}",
        timing.pairs,
        timing.under_25ms,
        timing.under_100ms,
        timing.under_250ms,
        timing.at_least_250ms
    );
    println!("MAN545-CHECK replay=faithful noop=identical dup-sub=identical dup-span=identical");
    let wall = |i: usize| recs.iter().map(|r| r.wall_s[i]).sum::<f64>();
    println!(
        "MAN545-WALL\tlegacy={:.1}\thsmm={:.1}\tlegacy+refine={:.1}",
        wall(LEGACY),
        wall(HSMM),
        wall(LEGACY_REFINE)
    );
    if !realizable {
        println!("MAN545-SKIP cal-sub cal-span: need at least 2 trials or MAN545_CALIBRATION");
    }
}
