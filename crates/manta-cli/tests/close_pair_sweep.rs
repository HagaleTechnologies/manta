//! MAN-112 reproducible measurements. Run with --ignored --nocapture.
mod common;
use common::decode::{per_track, promoted_track_ids, streaming_report};
use manta_testkit::vectors::{close_pair, single_at_offset, VectorSpec};

fn measure(spec: VectorSpec, label: String, steered: bool) {
    let (report, manifest, counts) = streaming_report(&spec, steered);
    let tracks = per_track(&report);
    let mut cers = Vec::new();
    let mut errors = Vec::new();
    let mut recalls = Vec::new();
    for (i, text) in manifest.keyed_texts.iter().enumerate() {
        let expected = manifest.expected_freqs_hz[i];
        let nearest = tracks.values().min_by(|a, b| {
            (a.1.unwrap_or(f64::MAX) - expected)
                .abs()
                .total_cmp(&(b.1.unwrap_or(f64::MAX) - expected).abs())
        });
        let (cer, error, recall) = nearest.map_or((1.0, f64::INFINITY, 0.0), |(decoded, freq)| {
            let call = if i == 0 { "N1AA" } else { "N2BB" };
            let words: Vec<_> = text.split_whitespace().filter(|w| *w == call).collect();
            let hits = decoded
                .split_whitespace()
                .filter(|w| *w == call)
                .count()
                .min(words.len());
            (
                manta_testkit::cer::cer(text, decoded),
                (freq.unwrap_or(f64::MAX) - expected).abs(),
                hits as f64 / words.len().max(1) as f64,
            )
        });
        cers.push(cer);
        errors.push(error);
        recalls.push(recall);
    }
    let promoted = promoted_track_ids(&report).len();
    let pass = promoted == spec.signals.len()
        && tracks.len() == spec.signals.len()
        && cers.iter().all(|&c| c <= 0.05)
        && errors.iter().all(|&e| e <= 15.0);
    let spawns = counts.spawns;
    let counts = counts.close_counts;
    println!("{label}: steered={steered} spawns={spawns} closes={counts:?} promoted={promoted} decoded={} CER={cers:?} error={errors:?} recall={recalls:?} pass={pass}", tracks.len());
}
fn run(scenes: Vec<(VectorSpec, String)>, steered: bool) {
    // Bound fixture memory and disk use on developer machines and CI workers.
    for batch in scenes.chunks(4) {
        std::thread::scope(|scope| {
            for (spec, label) in batch {
                scope.spawn(|| measure(spec.clone(), label.clone(), steered));
            }
        });
    }
}
fn pair_scenes() -> Vec<(VectorSpec, String)> {
    let mut scenes = Vec::new();
    for (align, a) in [
        ("on", 10031.25),
        ("quarter", 10054.6875),
        ("mid", 10078.125),
    ] {
        for spacing in [100.0, 125.0, 150.0, 175.0, 200.0, 250.0] {
            scenes.push((
                close_pair("pair", a, spacing, 15.0, 15.0),
                format!("{align}/{spacing}/15/15"),
            ));
        }
    }
    for (align, a, spacings) in [
        ("on", 10031.25, vec![100.0, 125.0, 150.0, 200.0]),
        ("mid", 10078.125, vec![150.0, 200.0]),
    ] {
        for spacing in spacings {
            for (sa, sb) in [(25.0, 10.0), (10.0, 25.0)] {
                scenes.push((
                    close_pair("pair", a, spacing, sa, sb),
                    format!("{align}/{spacing}/{sa}/{sb}"),
                ));
            }
        }
    }
    scenes
}
fn offset_scenes() -> Vec<(VectorSpec, String)> {
    let mut scenes = Vec::new();
    for wpm in [8.0, 16.0, 24.0, 36.0, 48.0, 60.0] {
        for offset in [0.0, 0.125, 0.25, 0.375, 0.5] {
            for neighbor in [false, true] {
                let mut spec = single_at_offset("offset", offset, wpm, 6.0);
                if neighbor {
                    let mut signal = manta_testkit::vectors::v7().signals.remove(1);
                    signal.offset_hz = spec.signals[0].offset_hz + 250.0;
                    signal.snr_2500_db = 20.0;
                    spec.signals.push(signal);
                }
                scenes.push((
                    spec,
                    format!("wpm={wpm}/offset={offset}/neighbor={neighbor}"),
                ));
            }
        }
    }
    scenes
}

#[test]
#[ignore = "MAN-112 measurement sweep; run on demand"]
fn close_pair_matrix() {
    run(pair_scenes(), false);
}
#[test]
#[ignore = "MAN-112 measurement sweep; run on demand"]
fn carrier_offset_and_stronger_neighbor() {
    run(offset_scenes(), false);
}
#[test]
#[ignore = "MAN-112 frontend promotion measurement; run on demand"]
fn steered_close_pair_matrix() {
    run(pair_scenes(), true);
}
#[test]
#[ignore = "MAN-112 frontend promotion measurement; run on demand"]
fn steered_carrier_offset_and_stronger_neighbor() {
    run(offset_scenes(), true);
}

#[test]
#[ignore = "MAN-112 one-off 192 kS/s measurement"]
fn restored_v7_at_192khz() {
    let mut spec = manta_testkit::vectors::v7();
    spec.fs = 192000.0;
    measure(spec, "restored V7 at 192 kS/s".into(), false);
}
