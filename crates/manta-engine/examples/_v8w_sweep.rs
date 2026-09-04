//! Scratch MAN-9 sweep harness: renders V8w ONCE, then decodes many
//! `PipelineConfig` variants in-process (bypassing the WAV/CLI/JSON round
//! trip the golden test uses), reusing the same render. Not part of the
//! plan deliverable; deleted before the PR.
use manta_decode::decoder::DecodeConfig;
use manta_decode::events::DecoderEvent;
use manta_decode::timing::MarkAdmission;
use manta_engine::{DetectorConfig, PipelineConfig};
use manta_testkit::vectors::VectorSpec;
use std::collections::BTreeMap;

fn per_track(events: &[DecoderEvent]) -> BTreeMap<u32, (String, Option<f64>)> {
    let mut texts: BTreeMap<u32, String> = BTreeMap::new();
    let mut freqs: BTreeMap<u32, f64> = BTreeMap::new();
    for ev in events {
        match ev {
            DecoderEvent::CharDecoded {
                track_id, glyph, ..
            } => {
                if let Some(c) = glyph.text_char() {
                    texts.entry(*track_id).or_default().push(c);
                }
            }
            DecoderEvent::WordBoundary { track_id, .. } => {
                let t = texts.entry(*track_id).or_default();
                if !t.is_empty() && !t.ends_with(' ') {
                    t.push(' ');
                }
            }
            DecoderEvent::TrackMeta {
                track_id, freq_hz, ..
            } => {
                freqs.insert(*track_id, *freq_hz);
            }
            _ => {}
        }
    }
    texts
        .into_iter()
        .map(|(tid, t)| (tid, (t.trim().to_string(), freqs.get(&tid).copied())))
        .collect()
}

struct Stats {
    good: usize,
    n: usize,
    median: f64,
    mean: f64,
    fragmented: usize, // len_ratio < 0.3, matching the ignore-comment's definition
}

fn strong_signal_stats(
    spec: &VectorSpec,
    keyed_texts: &[String],
    events: &[DecoderEvent],
) -> Stats {
    let tracks = per_track(events);
    let expected_freqs: Vec<f64> = spec
        .signals
        .iter()
        .map(|s| spec.center_freq_hz + s.offset_hz)
        .collect();
    let mut cers = Vec::new();
    let mut fragmented = 0usize;
    for (i, sig) in spec.signals.iter().enumerate() {
        if sig.snr_2500_db < 6.0 {
            continue;
        }
        let expected_freq = expected_freqs[i];
        let decoded = tracks
            .values()
            .min_by(|(_, fa), (_, fb)| {
                let da = (fa.unwrap_or(f64::MAX) - expected_freq).abs();
                let db = (fb.unwrap_or(f64::MAX) - expected_freq).abs();
                da.partial_cmp(&db).unwrap()
            })
            .map(|(t, _)| t.as_str())
            .unwrap_or("");
        let cer = manta_testkit::cer::cer(&keyed_texts[i], decoded);
        let len_ratio = decoded.chars().count() as f64 / keyed_texts[i].chars().count() as f64;
        if len_ratio < 0.3 {
            fragmented += 1;
        }
        cers.push(cer);
    }
    cers.sort_by(f64::total_cmp);
    let n = cers.len();
    let good = cers.iter().filter(|&&c| c < 0.10).count();
    let median = if n % 2 == 1 {
        cers[n / 2]
    } else {
        (cers[n / 2 - 1] + cers[n / 2]) / 2.0
    };
    let mean = cers.iter().sum::<f64>() / n as f64;
    Stats {
        good,
        n,
        median,
        mean,
        fragmented,
    }
}

fn run(
    label: &str,
    spec: &VectorSpec,
    samples: &[num_complex::Complex32],
    keyed_texts: &[String],
    cfg: &PipelineConfig,
) {
    let t0 = std::time::Instant::now();
    let report = manta_engine::decode_samples(samples, spec.fs, spec.center_freq_hz, cfg).unwrap();
    let stats = strong_signal_stats(spec, keyed_texts, &report.events);
    println!(
        "{label:40} pass={:2}/{:2} median={:.3} mean={:.3} frag={} decode={:?} closes={:?}",
        stats.good,
        stats.n,
        stats.median,
        stats.mean,
        stats.fragmented,
        t0.elapsed(),
        report.close_counts,
    );
}

fn main() {
    let spec = manta_testkit::vectors::v8w();
    let t0 = std::time::Instant::now();
    let rendered = manta_testkit::vectors::render(&spec).unwrap();
    eprintln!(
        "render: {:?}, samples: {}",
        t0.elapsed(),
        rendered.samples.len()
    );

    let phase = std::env::var("SWEEP_PHASE").unwrap_or_default();

    run(
        "baseline (all rungs off)",
        &spec,
        &rendered.samples,
        &rendered.keyed_texts,
        &PipelineConfig::default(),
    );

    if phase == "2" || phase == "all" {
        for debounce_dits in [0.15, 0.25, 0.30, 0.35] {
            let cfg = PipelineConfig {
                decode: DecodeConfig {
                    demod: manta_decode::envelope::DemodConfig {
                        debounce_dits,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            };
            run(
                &format!("phase2 debounce_dits={debounce_dits}"),
                &spec,
                &rendered.samples,
                &rendered.keyed_texts,
                &cfg,
            );
        }
    }

    if phase == "3" || phase == "all" {
        for width_low_q in [4usize, 8, 12, 16] {
            for q_low in [0.5f32, 0.6, 0.7] {
                let cfg = PipelineConfig {
                    decode: DecodeConfig {
                        beam: manta_decode::beam::BeamConfig {
                            width_low_q,
                            q_low,
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    ..Default::default()
                };
                run(
                    &format!("phase3 width_low_q={width_low_q} q_low={q_low}"),
                    &spec,
                    &rendered.samples,
                    &rendered.keyed_texts,
                    &cfg,
                );
            }
        }
    }

    if phase == "4" || phase == "all" {
        for (lo, hi) in [(0.55f32, 1.9f32), (0.45, 2.2), (0.35, 2.6)] {
            let cfg = PipelineConfig {
                decode: DecodeConfig {
                    mark_admission: MarkAdmission { lo, hi },
                    ..Default::default()
                },
                ..Default::default()
            };
            run(
                &format!("phase4 admission=({lo},{hi})"),
                &spec,
                &rendered.samples,
                &rendered.keyed_texts,
                &cfg,
            );
        }
    }

    if phase == "5" || phase == "all" {
        for hang_ms in [10_000.0f64, 15_000.0, 20_000.0] {
            let cfg = PipelineConfig {
                detector: DetectorConfig {
                    hang_hops_emitting: (hang_ms * 0.375) as u64,
                    ..Default::default()
                },
                ..Default::default()
            };
            run(
                &format!("phase5 hang_hops_emitting_ms={hang_ms}"),
                &spec,
                &rendered.samples,
                &rendered.keyed_texts,
                &cfg,
            );
        }
        for merge_radius_channels in [1.5f32, 2.0, 2.5] {
            let cfg = PipelineConfig {
                detector: DetectorConfig {
                    merge_radius_channels,
                    ..Default::default()
                },
                ..Default::default()
            };
            run(
                &format!("phase5 merge_radius_channels={merge_radius_channels}"),
                &spec,
                &rendered.samples,
                &rendered.keyed_texts,
                &cfg,
            );
        }
    }

    if phase == "combined" {
        // best-of-each-phase combined, filled in once each phase's winner is known
        let cfg = PipelineConfig::default();
        run(
            "combined placeholder",
            &spec,
            &rendered.samples,
            &rendered.keyed_texts,
            &cfg,
        );
    }
}
