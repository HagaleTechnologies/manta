//! MAN-515 spot-level A/B: the 126 standing sensitivity recordings (10
//! stations keying `CQ CQ DE <call> <call> K` for 60 s at 96 kS/s, seed
//! 1; awgn, good and poor; 15, 25 and 35 WPM; trials 0 and 1; 500 Hz SNR
//! 0, 3, 6, 9, 12, 18 and 24 dB), decoded on `MAN515_AB_ENGINE`
//! (`legacy`, the default, or `hsmm`). One tab-separated `AB` row per
//! recording: its key, the sorted correct callsigns spotted (in the
//! trial's `series_calls`) and the sorted bogus ones, `-` for none.
//! Calls, not `call@freq`: refinement moves emission timing, and with it
//! a spot's frequency by 1 to 2 Hz.
//!
//! Uses only APIs present on main, so the same file produces main's
//! baseline:
//! `cargo test --release -p manta-engine --test man515_refine_ab -- --ignored --nocapture | grep ^AB`
//!
//! `MAN515_AB_ONLY=<prefix>[,<prefix>...]` keeps the series whose key
//! starts with one of them (`awgn`, `good/25wpm`, ...); `MAN515_AB_JOBS`
//! (default 3) bounds concurrent decodes, and each series' buffers drop
//! before the next.
//! `MAN515_AB_BASELINE=<tsv>` (absolute, or relative to the workspace
//! root) names a saved `grep ^AB` output: every
//! row's key must exist in it, the run prints `identical / better /
//! worse` counts, and panics listing every row that lost a correct call
//! or gained a bogus one.
use manta_decode::decoder::Engine;
use manta_engine::{decode_samples, PipelineConfig};
use manta_testkit::sensitivity::{self, Condition};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};

const DURATION_S: f64 = 60.0;
const SEED: u64 = 1;
const WPMS: [u32; 3] = [15, 25, 35];
const TRIALS: [u32; 2] = [0, 1];
const SNRS_500: [i32; 7] = [0, 3, 6, 9, 12, 18, 24];
/// 500 Hz to 2500 Hz reference bandwidth: `10 log10(5)`.
const REF_BW_DB: f64 = 6.9897;

/// One recording's result: correct and bogus callsigns spotted.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    correct: BTreeSet<String>,
    bogus: BTreeSet<String>,
}

fn join(set: &BTreeSet<String>) -> String {
    if set.is_empty() {
        "-".to_string()
    } else {
        set.iter().cloned().collect::<Vec<_>>().join(",")
    }
}

fn split(field: &str) -> BTreeSet<String> {
    if field == "-" {
        BTreeSet::new()
    } else {
        field.split(',').map(str::to_string).collect()
    }
}

fn format_row(key: &str, row: &Row) -> String {
    format!("AB\t{key}\t{}\t{}", join(&row.correct), join(&row.bogus))
}

/// Parses `AB` rows into `key -> Row`; other lines are ignored.
fn parse_rows(tsv: &str) -> Result<BTreeMap<String, Row>, String> {
    let mut rows = BTreeMap::new();
    for line in tsv.lines().filter(|l| l.starts_with("AB\t")) {
        let fields: Vec<&str> = line.split('\t').collect();
        let [_, key, correct, bogus] = fields[..] else {
            return Err(format!("malformed AB row: {line:?}"));
        };
        rows.insert(
            key.to_string(),
            Row {
                correct: split(correct),
                bogus: split(bogus),
            },
        );
    }
    Ok(rows)
}

/// `(identical, better, worse rows)` of `now` against `base`. A row is
/// worse when it lost a correct call or gained a bogus one; better when
/// it is not worse but differs. Every key of `now` must be in `base`.
fn ab_compare(
    base: &BTreeMap<String, Row>,
    now: &[(String, Row)],
) -> Result<(usize, usize, Vec<String>), String> {
    let (mut identical, mut better, mut worse) = (0, 0, Vec::new());
    for (key, row) in now {
        let Some(b) = base.get(key) else {
            return Err(format!("{key} is not in the baseline"));
        };
        let lost: Vec<_> = b.correct.difference(&row.correct).cloned().collect();
        let gained: Vec<_> = row.bogus.difference(&b.bogus).cloned().collect();
        if !lost.is_empty() || !gained.is_empty() {
            worse.push(format!(
                "{key}: lost correct {lost:?}, gained bogus {gained:?}"
            ));
        } else if row == b {
            identical += 1;
        } else {
            better += 1;
        }
    }
    Ok((identical, better, worse))
}

#[test]
fn ab_compare_flags_only_rows_that_got_worse() {
    let base =
        parse_rows("AB\ta\tK1AA,K1BB\t-\nAB\tb\tK1AA\tEEETT\nAB\tc\tK1AA,K1BB\t-\nAB\td\t-\t-\n")
            .unwrap();
    let now =
        parse_rows("AB\ta\tK1AA,K1BB\t-\nAB\tb\tK1AA,K1BB\t-\nAB\tc\tK1AA\t-\nAB\td\t-\tN0PE\n")
            .unwrap();
    let now: Vec<_> = now.into_iter().collect();
    let (identical, better, worse) = ab_compare(&base, &now).unwrap();
    assert_eq!((identical, better), (1, 1));
    assert_eq!(worse.len(), 2, "{worse:?}");
    assert!(
        worse[0].starts_with("c: lost correct [\"K1BB\"]"),
        "{worse:?}"
    );
    assert!(worse[1].starts_with("d: lost correct [], gained bogus [\"N0PE\"]"));
    assert!(ab_compare(&base, &[("e".into(), now[0].1.clone())]).is_err());
}

fn engine_from_env() -> Engine {
    match std::env::var("MAN515_AB_ENGINE").as_deref() {
        Err(_) | Ok("legacy") => Engine::Legacy,
        Ok("hsmm") => Engine::Hsmm,
        Ok(other) => panic!("MAN515_AB_ENGINE must be legacy or hsmm, not {other:?}"),
    }
}

fn decode_row(iq: &[num_complex::Complex32], engine: Engine, calls: &BTreeSet<String>) -> Row {
    let mut cfg = PipelineConfig::default();
    cfg.decode.engine = engine;
    let spots = match decode_samples(
        iq,
        sensitivity::SAMPLE_RATE_HZ,
        sensitivity::CENTER_FREQ_HZ,
        &cfg,
    ) {
        Ok(r) => r.spots,
        Err(e) if e.to_string().starts_with("no signal found") => Vec::new(),
        Err(e) => panic!("decode failed: {e}"),
    };
    let (correct, bogus): (BTreeSet<_>, BTreeSet<_>) = spots
        .into_iter()
        .map(|s| s.callsign)
        .partition(|c| calls.contains(c));
    Row { correct, bogus }
}

#[test]
#[ignore = "MAN-515 measurement: 126 one-minute decodes, run in release"]
fn man515_refine_ab() {
    let engine = engine_from_env();
    let only_var = std::env::var("MAN515_AB_ONLY").unwrap_or_default();
    let only: Vec<&str> = only_var.split(',').collect();
    let jobs: usize = std::env::var("MAN515_AB_JOBS")
        .ok()
        .map(|j| j.parse().expect("MAN515_AB_JOBS is a count"))
        .unwrap_or(3)
        .max(1);
    let mut rows: Vec<(String, Row)> = Vec::new();
    for trial in TRIALS {
        let calls: BTreeSet<String> = sensitivity::series_calls(trial).into_iter().collect();
        let mut noise = None;
        for condition in Condition::ALL {
            for wpm in WPMS {
                let series = format!("{}/{wpm}wpm/t{trial}", condition.name());
                if !only
                    .iter()
                    .any(|p| series.starts_with(p) || p.starts_with(&series))
                {
                    continue;
                }
                let noise: &[_] = noise.get_or_insert_with(|| {
                    sensitivity::render_noise(DURATION_S, SEED, trial).unwrap()
                });
                let signals = sensitivity::series_signals(condition, wpm as f32, trial, SEED);
                let (clean, _) =
                    sensitivity::render_series_chunked(&signals, DURATION_S, 10).unwrap();
                let next = AtomicUsize::new(0);
                let mut out: Vec<Option<Row>> = vec![None; SNRS_500.len()];
                std::thread::scope(|s| {
                    let workers: Vec<_> = (0..jobs.min(SNRS_500.len()))
                        .map(|_| {
                            s.spawn(|| {
                                let mut done = Vec::new();
                                loop {
                                    let i = next.fetch_add(1, Ordering::Relaxed);
                                    let Some(&snr) = SNRS_500.get(i) else {
                                        break;
                                    };
                                    let gain =
                                        sensitivity::gain_for_snr_2500(snr as f64 - REF_BW_DB);
                                    let iq = sensitivity::compose(&clean, noise, gain);
                                    done.push((i, decode_row(&iq, engine, &calls)));
                                }
                                done
                            })
                        })
                        .collect();
                    for w in workers {
                        for (i, row) in w.join().expect("a decode worker panicked") {
                            out[i] = Some(row);
                        }
                    }
                });
                for (snr, row) in SNRS_500.iter().zip(out) {
                    let key = format!("{series}/{snr}dB");
                    let row = row.expect("every SNR decoded");
                    println!("{}", format_row(&key, &row));
                    rows.push((key, row));
                }
            }
        }
    }
    let correct: usize = rows.iter().map(|(_, r)| r.correct.len()).sum();
    let bogus: usize = rows.iter().map(|(_, r)| r.bogus.len()).sum();
    println!(
        "AB-TOTAL engine {engine:?} recordings {} correct {correct} bogus {bogus}",
        rows.len()
    );
    if let Ok(path) = std::env::var("MAN515_AB_BASELINE") {
        // `cargo test` runs from the package directory; a relative path
        // names a file from the workspace root, as the docs above write it.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(path);
        let text =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let base = parse_rows(&text).unwrap();
        let (identical, better, worse) = ab_compare(&base, &rows).unwrap();
        println!(
            "AB-COMPARE identical {identical} better {better} worse {}",
            worse.len()
        );
        assert!(
            worse.is_empty(),
            "rows worse than the baseline:\n{}",
            worse.join("\n")
        );
    }
}
