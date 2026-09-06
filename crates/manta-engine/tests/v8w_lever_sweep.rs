//! MAN-9 remediation round 2: runs the three CER-ladder rung sweeps
//! (Phases 2-4) and the track-continuity sweep (Phase 5) the plan
//! requires, in-process -- render V8w ONCE, then decode many
//! `PipelineConfig` variants against the same samples, so each additional
//! sweep point costs one `decode_samples` call over already-rendered
//! audio, not a WAV render + CLI spawn + JSON round trip.
//!
//! This restores the spirit of the round-1 implementation's
//! `examples/_v8w_sweep.rs` harness (deleted before that round's PR --
//! see docs/DECISIONS/2026-09-04-man9-v8w-fading-baseline.md's "Why
//! nothing was promoted" section) -- kept in version control this time so
//! its own cost is measured, not argued about.
//!
//! `cargo test -p manta-engine --test v8w_lever_sweep -- --ignored --nocapture`

use manta_decode::beam::BeamConfig;
use manta_decode::decoder::DecodeConfig;
use manta_decode::envelope::DemodConfig;
use manta_decode::events::DecoderEvent;
use manta_decode::timing::MarkAdmission;
use manta_engine::{DetectorConfig, PipelineConfig};
use manta_testkit::vectors::{RenderedVector, VectorSpec};
use std::collections::BTreeMap;
use std::time::Instant;

/// CR-E: strictly less than the scene's own `MIN_SEPARATION_HZ` (300.0),
/// not `<=`, so a neighboring signal's legitimate track can never be
/// counted as one of its neighbor's fragments.
const NEAR_HZ: f64 = 300.0;

/// CR-beta: `text` is `Some` only once a `CharDecoded`/`WordBoundary` event
/// has actually been seen for this track -- mirrors
/// `golden_v8_v8w.rs`'s `per_track` exactly, so `nearest_text` below can
/// never select a `TrackMeta`-only fragment (a track the CLI-pinned golden
/// harness cannot see at all) as a signal's CER match. `tracks_near`
/// (fragmentation counting) deliberately keeps counting every entry here,
/// `TrackMeta`-only ones included -- that population is what
/// `v8w_fading_diagnostics.rs`'s cluster counts describe.
#[derive(Default)]
struct TrackInfo {
    text: Option<String>,
    freq_hz: Option<f64>,
}

fn per_track(events: &[DecoderEvent]) -> BTreeMap<u32, TrackInfo> {
    let mut out: BTreeMap<u32, TrackInfo> = BTreeMap::new();
    for ev in events {
        match ev {
            DecoderEvent::CharDecoded {
                track_id, glyph, ..
            } => {
                if let Some(c) = glyph.text_char() {
                    out.entry(*track_id)
                        .or_default()
                        .text
                        .get_or_insert_with(String::new)
                        .push(c);
                }
            }
            DecoderEvent::WordBoundary { track_id, .. } => {
                let e = out.entry(*track_id).or_default();
                let t = e.text.get_or_insert_with(String::new);
                if !t.is_empty() && !t.ends_with(' ') {
                    t.push(' ');
                }
            }
            DecoderEvent::TrackMeta {
                track_id, freq_hz, ..
            } => {
                out.entry(*track_id).or_default().freq_hz = Some(*freq_hz);
            }
            _ => {}
        }
    }
    out
}

/// Nearest-by-frequency track AMONG THOSE WITH A DECODED-TEXT EVENT only --
/// same restriction as `golden_v8_v8w.rs`'s `match_tracks_by_freq`, so a
/// closer-in-frequency `TrackMeta`-only fragment can never be picked over
/// the real, farther track that actually carries the signal's text.
fn nearest_text(tracks: &BTreeMap<u32, TrackInfo>, expected_freq_hz: f64) -> &str {
    tracks
        .values()
        .filter(|t| t.text.is_some())
        .min_by(|a, b| {
            let da = (a.freq_hz.unwrap_or(f64::MAX) - expected_freq_hz).abs();
            let db = (b.freq_hz.unwrap_or(f64::MAX) - expected_freq_hz).abs();
            da.partial_cmp(&db).unwrap()
        })
        .map(|t| t.text.as_deref().unwrap_or("").trim())
        .unwrap_or("")
}

/// Count of ALL tracks (including `TrackMeta`-only fragments) within
/// `radius_hz` -- the fragmentation-counting population, deliberately wider
/// than `nearest_text`'s matching population above.
fn tracks_near(tracks: &BTreeMap<u32, TrackInfo>, expected_freq_hz: f64, radius_hz: f64) -> usize {
    tracks
        .values()
        .filter(|t| {
            t.freq_hz
                .is_some_and(|f| (f - expected_freq_hz).abs() < radius_hz)
        })
        .count()
}

struct SweepPoint {
    label: &'static str,
    cfg: PipelineConfig,
}

#[derive(Debug)]
struct SweepResult {
    label: &'static str,
    passes: usize,
    median_cer: f64,
    frag_25: usize,
    frag_41: usize,
    frag_44: usize,
    elapsed_s: f64,
}

fn strong_indices(spec: &VectorSpec) -> Vec<usize> {
    spec.signals
        .iter()
        .enumerate()
        .filter(|(_, s)| s.snr_2500_db >= 6.0)
        .map(|(i, _)| i)
        .collect()
}

fn run_point(
    spec: &VectorSpec,
    rendered: &RenderedVector,
    strong: &[usize],
    point: &SweepPoint,
) -> SweepResult {
    let t0 = Instant::now();
    let report =
        manta_engine::decode_samples(&rendered.samples, spec.fs, spec.center_freq_hz, &point.cfg)
            .unwrap();
    let elapsed_s = t0.elapsed().as_secs_f64();
    let tracks = per_track(&report.events);

    let mut cers: Vec<f64> = strong
        .iter()
        .map(|&i| {
            let expected_freq = spec.center_freq_hz + spec.signals[i].offset_hz;
            let decoded = nearest_text(&tracks, expected_freq);
            manta_testkit::cer::cer(&rendered.keyed_texts[i], decoded)
        })
        .collect();
    let passes = cers.iter().filter(|&&c| c < 0.10).count();
    cers.sort_by(f64::total_cmp);
    let n = cers.len();
    let median = if n % 2 == 0 {
        (cers[n / 2 - 1] + cers[n / 2]) / 2.0
    } else {
        cers[n / 2]
    };
    let frag = |idx: usize| {
        let expected_freq = spec.center_freq_hz + spec.signals[idx].offset_hz;
        tracks_near(&tracks, expected_freq, NEAR_HZ)
    };
    SweepResult {
        label: point.label,
        passes,
        median_cer: median,
        frag_25: frag(25),
        frag_41: frag(41),
        frag_44: frag(44),
        elapsed_s,
    }
}

fn print_header() {
    println!(
        "{:<28} {:>10} {:>12} {:>15} {:>8}",
        "point", "passes/34", "median_cer", "frag[25,41,44]", "elapsed_s"
    );
}

fn print_row(r: &SweepResult) {
    println!(
        "{:<28} {:>10} {:>12.4} {:>15} {:>8.1}",
        r.label,
        r.passes,
        r.median_cer,
        format!("{},{},{}", r.frag_25, r.frag_41, r.frag_44),
        r.elapsed_s
    );
}

/// Runs every point in `points` against the shared render, printing (and
/// flushing) each row as soon as it's computed rather than batching until
/// the end -- this test's own points cost minutes apiece, so a `--nocapture`
/// log tailed mid-run must show progress, not just a final dump.
fn run_and_print(
    spec: &VectorSpec,
    rendered: &RenderedVector,
    strong: &[usize],
    points: &[SweepPoint],
) {
    use std::io::Write;
    print_header();
    for p in points {
        let r = run_point(spec, rendered, strong, p);
        print_row(&r);
        std::io::stdout().flush().ok();
    }
}

/// One V8w render shared by every sweep test in this binary. `cargo test`
/// runs a binary's tests as threads in one process, so a `OnceLock` is
/// honored across them; rendering costs ~340 s (this doc's own "Why nothing
/// was promoted" cost model already assumes exactly one render is paid),
/// while each additional `decode_samples` call against it costs ~8-9 s --
/// paying the render four times over (once per sweep test) would be the
/// unaffordable version of this harness the pin doc argues against.
fn samples_and_spec() -> &'static (VectorSpec, RenderedVector) {
    static CACHE: std::sync::OnceLock<(VectorSpec, RenderedVector)> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| {
        let spec = manta_testkit::vectors::v8w();
        let rendered = manta_testkit::vectors::render(&spec).unwrap();
        (spec, rendered)
    })
}

/// Rung 1 (Phase 2): `debounce_dits in {0.0 [baseline], 0.15, 0.25, 0.35}`,
/// `debounce_ceiling_ms` left at its default (30.0). Accept test: >= 0.010
/// absolute median-CER improvement over baseline with no strong-signal
/// pass-count regression.
#[test]
#[ignore]
fn v8w_lever_sweep_debounce_dits() {
    let (spec, rendered) = samples_and_spec();
    let strong = strong_indices(spec);
    let points = [
        SweepPoint {
            label: "debounce_dits=0.00 (baseline)",
            cfg: PipelineConfig::default(),
        },
        SweepPoint {
            label: "debounce_dits=0.15",
            cfg: PipelineConfig {
                decode: DecodeConfig {
                    demod: DemodConfig {
                        debounce_dits: 0.15,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
        },
        SweepPoint {
            label: "debounce_dits=0.25",
            cfg: PipelineConfig {
                decode: DecodeConfig {
                    demod: DemodConfig {
                        debounce_dits: 0.25,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
        },
        SweepPoint {
            label: "debounce_dits=0.35",
            cfg: PipelineConfig {
                decode: DecodeConfig {
                    demod: DemodConfig {
                        debounce_dits: 0.35,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
        },
    ];
    run_and_print(spec, rendered, &strong, &points);
}

/// Rung 2 (Phase 3): `(width_low_q, q_low) in {4,8,12,16} x {0.5,0.6,0.7}`.
/// `width_low_q=4` is mathematically identical to the `width_low_q=width`
/// inert default for every `q_low` (`effective_width` always returns 4),
/// so that column is skipped -- it cannot differ from the baseline point.
/// Accept test: same as Rung 1's.
#[test]
#[ignore]
fn v8w_lever_sweep_beam_width_low_q() {
    let (spec, rendered) = samples_and_spec();
    let strong = strong_indices(spec);
    let mut points = vec![SweepPoint {
        label: "width_low_q=4 (baseline)",
        cfg: PipelineConfig::default(),
    }];
    let cells: Vec<(usize, f32, &'static str)> = vec![
        (8, 0.5, "width_low_q=8 q_low=0.5"),
        (8, 0.6, "width_low_q=8 q_low=0.6"),
        (8, 0.7, "width_low_q=8 q_low=0.7"),
        (12, 0.5, "width_low_q=12 q_low=0.5"),
        (12, 0.6, "width_low_q=12 q_low=0.6"),
        (12, 0.7, "width_low_q=12 q_low=0.7"),
        (16, 0.5, "width_low_q=16 q_low=0.5"),
        (16, 0.6, "width_low_q=16 q_low=0.6"),
        (16, 0.7, "width_low_q=16 q_low=0.7"),
    ];
    for (width_low_q, q_low, label) in cells {
        points.push(SweepPoint {
            label,
            cfg: PipelineConfig {
                decode: DecodeConfig {
                    beam: BeamConfig {
                        width_low_q: Some(width_low_q),
                        q_low,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
        });
    }
    run_and_print(spec, rendered, &strong, &points);
}

/// Rung 3 (Phase 4): `mark_admission (lo, hi) in {(0.0, inf) [baseline],
/// (0.55, 1.9), (0.45, 2.2), (0.35, 2.6)}`. Accept test: same as Rung 1's.
#[test]
#[ignore]
fn v8w_lever_sweep_mark_admission() {
    let (spec, rendered) = samples_and_spec();
    let strong = strong_indices(spec);
    let points = [
        SweepPoint {
            label: "mark_admission=default (baseline)",
            cfg: PipelineConfig::default(),
        },
        SweepPoint {
            label: "mark_admission=(0.55,1.9)",
            cfg: PipelineConfig {
                decode: DecodeConfig {
                    mark_admission: MarkAdmission { lo: 0.55, hi: 1.9 },
                    ..Default::default()
                },
                ..Default::default()
            },
        },
        SweepPoint {
            label: "mark_admission=(0.45,2.2)",
            cfg: PipelineConfig {
                decode: DecodeConfig {
                    mark_admission: MarkAdmission { lo: 0.45, hi: 2.2 },
                    ..Default::default()
                },
                ..Default::default()
            },
        },
        SweepPoint {
            label: "mark_admission=(0.35,2.6)",
            cfg: PipelineConfig {
                decode: DecodeConfig {
                    mark_admission: MarkAdmission { lo: 0.35, hi: 2.6 },
                    ..Default::default()
                },
                ..Default::default()
            },
        },
    ];
    run_and_print(spec, rendered, &strong, &points);
}

/// Phase 5: `merge_radius_channels in {1.0 [baseline], 1.5, 2.0, 2.5}`,
/// hard-ceilinged below the scene's 3.2-channel `MIN_SEPARATION_HZ`.
/// Accept test: idx 25/41/44's `frag[...]` count drops toward 1 (single
/// track, no longer fragmented) with no regression on the V8 AWGN
/// sibling -- checked separately by the existing, un-ignored
/// `v8_pileup_validates_at_least_45_of_50_with_no_bogus_calls` gate
/// against whichever value this sweep promotes.
#[test]
#[ignore]
fn v8w_lever_sweep_merge_radius_channels() {
    let (spec, rendered) = samples_and_spec();
    let strong = strong_indices(spec);
    let points = [1.0f32, 1.5, 2.0, 2.5].map(|r| SweepPoint {
        label: match r {
            1.0 => "merge_radius_channels=1.0 (baseline)",
            1.5 => "merge_radius_channels=1.5",
            2.0 => "merge_radius_channels=2.0",
            _ => "merge_radius_channels=2.5",
        },
        cfg: PipelineConfig {
            detector: DetectorConfig {
                merge_radius_channels: r,
                ..Default::default()
            },
            ..Default::default()
        },
    });
    run_and_print(spec, rendered, &strong, &points);
}
