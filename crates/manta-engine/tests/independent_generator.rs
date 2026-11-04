//! MAN-206: decoded-text comparison between the testkit's scenes and the
//! independent generator's, trusted only after the generator cross-check
//! gate passes. `crosscheck::independent_scene` takes the gate's `Verified`
//! token, so this test cannot decode an independent scene first.

use manta_engine::{decode_samples, PipelineConfig};
use manta_testkit::cer::cer;
use manta_testkit::crosscheck;
use manta_testkit::vectors::{self, VectorSpec};
use std::collections::BTreeSet;

const SECONDS: f64 = 30.0;

fn decode(spec: &VectorSpec, samples: &[num_complex::Complex32]) -> (BTreeSet<String>, String) {
    let report = decode_samples(
        samples,
        spec.fs,
        spec.center_freq_hz,
        &PipelineConfig::default(),
    )
    .unwrap_or_else(|e| panic!("{}: decode failed: {e:#}", spec.name));
    let calls = report.spots.iter().map(|s| s.callsign.clone()).collect();
    (calls, report.text)
}

#[test]
fn independent_scenes_decode_like_testkit_scenes() {
    let verified = crosscheck::verified().unwrap_or_else(|report| {
        panic!("generator cross-check failed, so no decoded-text comparison is trusted:\n{report}")
    });
    for full in [
        vectors::v1(),
        vectors::v2(),
        vectors::vr4(),
        vectors::vw1(),
        vectors::v7(),
    ] {
        let spec = VectorSpec {
            duration_s: SECONDS,
            ..full
        };
        let testkit = vectors::render(&spec).unwrap();
        let (ind, ind_texts) = crosscheck::independent_scene(verified, &spec, SECONDS).unwrap();
        assert_eq!(
            ind_texts, testkit.keyed_texts,
            "{}: the generators keyed different text",
            spec.name
        );
        let (calls_t, text_t) = decode(&spec, &testkit.samples);
        let (calls_i, text_i) = decode(&spec, &ind);
        let cer_t = cer(&testkit.keyed_texts[0], &text_t);
        let cer_i = cer(&testkit.keyed_texts[0], &text_i);
        eprintln!(
            "{}: spots {calls_t:?} / {calls_i:?}, CER {cer_t:.4} / {cer_i:.4}",
            spec.name
        );
        assert_eq!(calls_t, calls_i, "{}: spot callsigns differ", spec.name);
        assert!(!calls_t.is_empty(), "{}: no spot", spec.name);
        assert!(
            (cer_t - cer_i).abs() <= 0.02,
            "{}: CER {cer_t:.4} (testkit) vs {cer_i:.4} (independent)",
            spec.name
        );
    }
}
