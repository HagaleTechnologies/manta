//! MAN-200 Phase 4: repetition fusion on and off over the MAN-116
//! synthetic pileup. Each point decodes once, then replays the decoder's
//! events through two fresh production validators (fusion off, and on with
//! decision recording), so both see the same samples. One tab-separated
//! `MAN200` row per point; the totals and the ladder step reached are in
//! docs/DECISIONS/2026-11-03-man200-repetition-evidence-fusion.md.
use manta_decode::decoder::Engine;
use manta_engine::{decode_samples, PipelineConfig};
use manta_spot::{Confirmation, Spot, Verdict};
use manta_testkit::sensitivity::{self, Condition};

/// The series length the plan names (the MAN-116 benchmark defaults to 60 s).
const DURATION_S: f64 = 90.0;
/// A station call spotted further than this from its station is wrong.
const MAX_STATION_HZ: f64 = 150.0;
const SEED: u64 = 1;
const TRIAL: u32 = 0;
/// Ladder step 3 of the decision record: over the whole grid, fusion-on
/// spots this many wrong calls fusion-off does not, so fusion ships default
/// off. A run fails if the count grows (the test is ignored, so nothing runs
/// it unasked); default-on needs 0.
const WRONG_CALLS_ADDED_AT_STEP_3: usize = 1;

struct Point {
    condition: Condition,
    wpm: u32,
    snr_db: f64,
    engine: Engine,
}

struct Row {
    spotted_off: usize,
    spotted_on: usize,
    fused: usize,
    wrong_off: Vec<Spot>,
    wrong_on: Vec<Spot>,
    /// Each wrong fused spot, with every non-fused decision within
    /// `MAX_STATION_HZ` in the 90 s up to it. That is its episode's copies
    /// when each copy was withheld on the exact path; it can also list a
    /// refused winner's withhold.
    wrong_fused: Vec<(Spot, Vec<String>)>,
}

fn points() -> Vec<Point> {
    let mut out = Vec::new();
    for condition in [Condition::WattersonGood, Condition::WattersonPoor] {
        for wpm in [15, 25] {
            for snr_db in [3.0, 6.0, 9.0, 12.0, 15.0, 20.0] {
                out.push(Point {
                    condition,
                    wpm,
                    snr_db,
                    engine: Engine::Legacy,
                });
            }
        }
    }
    for condition in [Condition::WattersonGood, Condition::WattersonPoor] {
        for snr_db in [6.0, 12.0] {
            out.push(Point {
                condition,
                wpm: 25,
                snr_db,
                engine: Engine::Hsmm,
            });
        }
    }
    out
}

/// `(call, station frequency)` for the trial's ten stations.
fn stations() -> Vec<(String, f64)> {
    sensitivity::series_calls(TRIAL)
        .into_iter()
        .enumerate()
        .map(|(j, call)| {
            (
                call,
                sensitivity::CENTER_FREQ_HZ + sensitivity::station_offset_hz(j),
            )
        })
        .collect()
}

fn is_wrong(spot: &Spot, stations: &[(String, f64)]) -> bool {
    !stations
        .iter()
        .any(|(call, f)| *call == spot.callsign && (spot.freq_hz - f).abs() <= MAX_STATION_HZ)
}

fn spotted(spots: &[Spot], stations: &[(String, f64)]) -> usize {
    stations
        .iter()
        .filter(|(call, f)| {
            spots
                .iter()
                .any(|s| s.callsign == *call && (s.freq_hz - f).abs() <= MAX_STATION_HZ)
        })
        .count()
}

fn measure(iq: &[num_complex::Complex32], point: &Point) -> Row {
    let stations = stations();
    let mut cfg = PipelineConfig::default();
    cfg.decode.engine = point.engine;
    let fs = sensitivity::SAMPLE_RATE_HZ;
    let report = match decode_samples(iq, fs, sensitivity::CENTER_FREQ_HZ, &cfg) {
        Ok(r) => Some(r),
        Err(e) if e.to_string().starts_with("no signal found") => None,
        Err(e) => panic!("decode failed: {e}"),
    };
    let (events, decoded) = report.map(|r| (r.events, r.spots)).unwrap_or_default();

    let mut off = cfg.validator(fs).unwrap();
    let spots_off: Vec<Spot> = events.iter().flat_map(|e| off.ingest(e)).collect();
    assert_eq!(spots_off, decoded, "the fusion-off replay must be faithful");

    cfg.repetition_fusion = true;
    let mut on = cfg.validator(fs).unwrap();
    on.record_decisions(true);
    let spots_on: Vec<Spot> = events.iter().flat_map(|e| on.ingest(e)).collect();
    let decisions = on.take_decisions();
    let fused: Vec<Spot> = decisions
        .iter()
        .filter(|d| d.verdict == Verdict::Spotted && d.confirmation == Some(Confirmation::Fused))
        .filter_map(|d| d.spot.clone())
        .collect();
    let window = (DURATION_S * fs) as u64;
    let copies = |s: &Spot| -> Vec<String> {
        decisions
            .iter()
            .filter(|d| {
                (d.freq_hz - s.freq_hz).abs() <= MAX_STATION_HZ
                    && d.sample_ts <= s.sample_ts
                    && d.sample_ts + window >= s.sample_ts
                    && d.confirmation != Some(Confirmation::Fused)
            })
            .map(|d| format!("{}@{}:{:?}", d.call, d.sample_ts, d.reason))
            .collect()
    };

    let wrong = |spots: &[Spot]| -> Vec<Spot> {
        spots
            .iter()
            .filter(|s| is_wrong(s, &stations))
            .cloned()
            .collect()
    };
    Row {
        spotted_off: spotted(&spots_off, &stations),
        spotted_on: spotted(&spots_on, &stations),
        fused: fused.len(),
        wrong_off: wrong(&spots_off),
        wrong_on: wrong(&spots_on),
        wrong_fused: wrong(&fused)
            .into_iter()
            .map(|s| {
                let c = copies(&s);
                (s, c)
            })
            .collect(),
    }
}

/// The measuring instrument behind MAN-200's decision record. Run with:
///
/// cargo test --release -p manta-engine --test man200_fusion_grid -- --ignored --nocapture
#[test]
#[ignore = "measuring instrument, several minutes release"]
fn man200_fusion_grid() {
    let points = points();
    let noise = sensitivity::render_noise(DURATION_S, SEED, TRIAL).unwrap();
    let mut cleans = Vec::new();
    for condition in [Condition::WattersonGood, Condition::WattersonPoor] {
        for wpm in [15u32, 25] {
            let series = sensitivity::series_signals(condition, wpm as f32, TRIAL, SEED);
            let (clean, _) = sensitivity::render_series_chunked(&series, DURATION_S, 10).unwrap();
            cleans.push(((condition, wpm), clean));
        }
    }
    let clean_for = |p: &Point| {
        &cleans
            .iter()
            .find(|((c, w), _)| *c == p.condition && *w == p.wpm)
            .expect("rendered")
            .1
    };
    let rows: Vec<Row> = std::thread::scope(|s| {
        let handles: Vec<_> = points
            .iter()
            .map(|p| {
                let (clean, noise) = (clean_for(p), &noise);
                s.spawn(move || {
                    let gain = sensitivity::gain_for_snr_2500(p.snr_db);
                    measure(&sensitivity::compose(clean, noise, gain), p)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    for (call, f) in stations() {
        println!("MAN200-STATION\t{call}\t{f:.1}");
    }
    println!(
        "MAN200\tcondition\twpm\tsnr_2500_db\tengine\tspotted_off\tspotted_on\tfused\twrong_off\twrong_on"
    );
    let mut failures = Vec::new();
    for (p, r) in points.iter().zip(&rows) {
        let engine = match p.engine {
            Engine::Hsmm => "hsmm",
            _ => "legacy",
        };
        println!(
            "MAN200\t{}\t{}\t{}\t{engine}\t{}\t{}\t{}\t{}\t{}",
            p.condition.name(),
            p.wpm,
            p.snr_db,
            r.spotted_off,
            r.spotted_on,
            r.fused,
            r.wrong_off.len(),
            r.wrong_on.len(),
        );
        for (s, copies) in &r.wrong_fused {
            println!(
                "MAN200-WRONG-FUSED\t{}\t{}\t{}\t{engine}\t{}\t{:.1}\ttrack={}\tts={}\t{}",
                p.condition.name(),
                p.wpm,
                p.snr_db,
                s.callsign,
                s.freq_hz,
                s.track_id,
                s.sample_ts,
                copies.join(" ")
            );
        }
        if r.wrong_on.len() > r.wrong_off.len() {
            failures.push(format!(
                "{} {} WPM {} dB {engine}: wrong on {} > off {}",
                p.condition.name(),
                p.wpm,
                p.snr_db,
                r.wrong_on.len(),
                r.wrong_off.len()
            ));
        }
    }
    let total = |f: fn(&Row) -> usize| rows.iter().map(f).sum::<usize>();
    println!(
        "MAN200-TOTAL\tspotted_off={}\tspotted_on={}\tfused={}\twrong_off={}\twrong_on={}",
        total(|r| r.spotted_off),
        total(|r| r.spotted_on),
        total(|r| r.fused),
        total(|r| r.wrong_off.len()),
        total(|r| r.wrong_on.len()),
    );
    // Wrong fusion-on spots the fusion-off replay did not make, so a wrong
    // call fusion swaps in for one it suppresses still counts.
    let added: usize = rows
        .iter()
        .map(|r| {
            r.wrong_on
                .iter()
                .filter(|s| !r.wrong_off.contains(s))
                .count()
        })
        .sum();
    println!("MAN200-BAR\tadded_wrong={added}\tdefault_on_needs=0\t{failures:?}");
    assert!(
        added <= WRONG_CALLS_ADDED_AT_STEP_3,
        "fusion added {added} wrong calls, more than the {WRONG_CALLS_ADDED_AT_STEP_3} recorded: {failures:?}"
    );
}
