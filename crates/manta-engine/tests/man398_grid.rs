//! MAN-398 A/B grid: 288 deterministic looped-text cases (24 texts, 12-40
//! WPM, +/-39 kHz, 12-30 dB), CER at 14 s and 30 s plus the 30 s decode, one
//! tab-separated `GRID` row per case. Run once on each build and diff:
//! `cargo test --release -p manta-engine --test man398_grid -- --ignored --nocapture | grep ^GRID`
//!
//! MAN-544: the cases are drawn first (same xorshift order) and decoded on
//! `MAN398_GRID_JOBS` workers (default: `available_parallelism()`); rows
//! still print in row order with the same format. `MAN398_GRID_BASELINE=<tsv>`
//! names a saved `grep ^GRID` output: each row's inputs (text, WPM, offset,
//! SNR, seed) must match it, the run prints `identical / better / worse`
//! counts at 14 s and 30 s, and panics listing every row whose CER14 or
//! CER30 is higher than the baseline's.
use manta_engine::{decode_samples, PipelineConfig};
use manta_testkit::cer::cer;
use manta_testkit::scene::{render_scene, SignalSpec};

fn spec(text: &str, wpm: f32, off_khz: f64, snr: f32) -> SignalSpec {
    SignalSpec {
        text: text.to_string(),
        loop_text: true,
        wpm,
        offset_hz: off_khz * 1000.0,
        snr_2500_db: snr,
        jitter: None,
        qsb: None,
        watterson: None,
        char_wpm: None,
        weight: 3.0,
        char_gap_units: 3.0,
        word_gap_units: 7.0,
        rise_ms: 5.0,
        dit_units: 1.0,
        element_gap_units: 1.0,
    }
}

const TEXTS: [&str; 24] = [
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
];

struct Case {
    text: &'static str,
    wpm: f32,
    off: f64,
    snr: f32,
    seed: u64,
}

/// The 288 cases, in the xorshift order the sequential grid drew them.
fn cases() -> Vec<Case> {
    let mut x: u64 = 0xD1B54A32D192ED03;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    (0..288)
        .map(|i| {
            let text = TEXTS[i % TEXTS.len()];
            let wpm = 12.0 + (next() % 2800) as f32 / 100.0;
            let mut off = (next() % 7800) as f64 / 100.0 - 39.0;
            if off.abs() < 1.0 {
                off = 5.0;
            }
            let snr = 12.0 + (next() % 1800) as f32 / 100.0;
            let seed = next();
            Case {
                text,
                wpm,
                off,
                snr,
                seed,
            }
        })
        .collect()
}

fn grid_row(i: usize, c: &Case) -> String {
    let s = spec(c.text, c.wpm, c.off, c.snr);
    let mut row = format!(
        "GRID {i}\t{}\t{:?}\t{:?}\t{:?}\t{}",
        c.text, c.wpm, c.off, c.snr, c.seed
    );
    for d in [14.0, 30.0] {
        let fs = 96_000.0;
        let (iq, keyed) = render_scene(std::slice::from_ref(&s), fs, d, Some(c.seed)).unwrap();
        let text = match decode_samples(&iq, fs, 0.0, &PipelineConfig::default()) {
            Ok(r) => r.text,
            Err(_) => String::new(),
        };
        row += &format!("\t{:.4}", cer(&keyed[0], &text));
        if d == 30.0 {
            row += &format!("\t{:?}", text);
        }
    }
    row
}

/// One parsed `GRID` row: the input tuple and the two CERs.
#[derive(Debug, Clone, PartialEq)]
struct GridRow {
    index: usize,
    inputs: String,
    cer14: f64,
    cer30: f64,
}

fn parse_row(line: &str) -> Result<GridRow, String> {
    let rest = line
        .strip_prefix("GRID ")
        .ok_or_else(|| format!("not a GRID row: {line:?}"))?;
    let f: Vec<&str> = rest.splitn(9, '\t').collect();
    if f.len() < 8 {
        return Err(format!("short GRID row: {line:?}"));
    }
    let num = |s: &str| s.parse::<f64>().map_err(|e| format!("{e}: {line:?}"));
    Ok(GridRow {
        index: f[0].parse().map_err(|e| format!("{e}: {line:?}"))?,
        inputs: f[1..6].join("\t"),
        cer14: num(f[6])?,
        cer30: num(f[7])?,
    })
}

/// Per-duration counts of rows that got better, stayed identical or got
/// worse against the baseline.
#[derive(Debug, Default, PartialEq)]
struct Tally {
    identical: usize,
    better: usize,
    worse: usize,
}

impl Tally {
    fn add(&mut self, base: f64, now: f64) {
        if now > base {
            self.worse += 1;
        } else if now < base {
            self.better += 1;
        } else {
            self.identical += 1;
        }
    }
}

/// Compare a run against a baseline, both `GRID` TSV text. Returns the
/// tallies at 14 s and 30 s and every row worse at either duration, or an
/// error when the row sets or their inputs differ.
fn grid_compare(baseline: &str, current: &str) -> Result<(Tally, Tally, Vec<String>), String> {
    let parse = |t: &str| -> Result<Vec<GridRow>, String> {
        t.lines()
            .filter(|l| l.starts_with("GRID "))
            .map(parse_row)
            .collect()
    };
    let base = parse(baseline)?;
    let now = parse(current)?;
    if base.len() != now.len() {
        return Err(format!(
            "baseline has {} rows, this run {}",
            base.len(),
            now.len()
        ));
    }
    let (mut t14, mut t30) = (Tally::default(), Tally::default());
    let mut worse = Vec::new();
    for (b, n) in base.iter().zip(now.iter()) {
        if b.index != n.index || b.inputs != n.inputs {
            return Err(format!(
                "input mismatch: baseline row {} [{}] vs row {} [{}]",
                b.index, b.inputs, n.index, n.inputs
            ));
        }
        t14.add(b.cer14, n.cer14);
        t30.add(b.cer30, n.cer30);
        if n.cer14 > b.cer14 || n.cer30 > b.cer30 {
            worse.push(format!(
                "row {} [{}]: CER14 {:.4} -> {:.4}, CER30 {:.4} -> {:.4}",
                n.index, n.inputs, b.cer14, n.cer14, b.cer30, n.cer30
            ));
        }
    }
    Ok((t14, t30, worse))
}

#[test]
fn grid_compare_flags_only_rows_that_got_worse() {
    let base = "GRID 0\tAU\t20.0\t5.0\t15.0\t7\t0.2000\t0.1000\t\"AU\"\n\
                GRID 1\tHH\t21.0\t6.0\t16.0\t8\t0.1000\t0.0500\t\"HH\"\n\
                GRID 2\t5NN\t22.0\t7.0\t17.0\t9\t0.1000\t0.0500\t\"5NN\"\n\
                GRID 3\tCQ\t23.0\t8.0\t18.0\t10\t0.1000\t0.0500\t\"CQ\"\n";
    // Row 0 better at both, row 1 identical, row 2 worse at 14 s only,
    // row 3 worse at 30 s only.
    let now = "GRID 0\tAU\t20.0\t5.0\t15.0\t7\t0.1000\t0.0000\t\"AU\"\n\
               GRID 1\tHH\t21.0\t6.0\t16.0\t8\t0.1000\t0.0500\t\"HH\"\n\
               GRID 2\t5NN\t22.0\t7.0\t17.0\t9\t0.2000\t0.0500\t\"5NN\"\n\
               GRID 3\tCQ\t23.0\t8.0\t18.0\t10\t0.1000\t0.0600\t\"CQ\"\n";
    let (t14, t30, worse) = grid_compare(base, now).unwrap();
    assert_eq!(worse.len(), 2, "{worse:?}");
    assert!(worse[0].starts_with("row 2 "), "{worse:?}");
    assert!(worse[1].starts_with("row 3 "), "{worse:?}");
    assert_eq!(
        t14,
        Tally {
            identical: 2,
            better: 1,
            worse: 1
        }
    );
    assert_eq!(
        t30,
        Tally {
            identical: 2,
            better: 1,
            worse: 1
        }
    );
    // An input mismatch (row 1's seed) is an error, not a comparison.
    let mismatch = now.replace("16.0\t8\t", "16.0\t99\t");
    let err = grid_compare(base, &mismatch).unwrap_err();
    assert!(err.contains("input mismatch"), "{err}");
}

#[test]
#[ignore]
fn man398_grid() {
    let cases = cases();
    let jobs = std::env::var("MAN398_GRID_JOBS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
    let next = std::sync::atomic::AtomicUsize::new(0);
    let rows: Vec<std::sync::Mutex<Option<String>>> =
        cases.iter().map(|_| std::sync::Mutex::new(None)).collect();
    std::thread::scope(|s| {
        for _ in 0..jobs {
            s.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if i >= cases.len() {
                    break;
                }
                let row = grid_row(i, &cases[i]);
                *rows[i].lock().unwrap() = Some(row);
            });
        }
    });
    let rows: Vec<String> = rows
        .into_iter()
        .map(|r| r.into_inner().unwrap().unwrap())
        .collect();
    for row in &rows {
        println!("{row}");
    }
    if let Ok(path) = std::env::var("MAN398_GRID_BASELINE") {
        let baseline = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("MAN398_GRID_BASELINE={path}: {e}"));
        let (t14, t30, worse) =
            grid_compare(&baseline, &rows.join("\n")).unwrap_or_else(|e| panic!("{e}"));
        println!(
            "identical\t{}\t{}\nbetter\t{}\t{}\nworse\t{}\t{}",
            t14.identical, t30.identical, t14.better, t30.better, t14.worse, t30.worse
        );
        assert!(
            worse.is_empty(),
            "{} rows decode worse than {path}:\n{}",
            worse.len(),
            worse.join("\n")
        );
    }
}
