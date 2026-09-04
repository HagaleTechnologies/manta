//! Scratch timing probe, not part of the plan deliverable. Deleted before
//! the PR is opened.
fn main() {
    let spec = manta_testkit::vectors::v8w();
    let t0 = std::time::Instant::now();
    let rendered = manta_testkit::vectors::render(&spec).unwrap();
    eprintln!(
        "render: {:?}, samples: {}",
        t0.elapsed(),
        rendered.samples.len()
    );
    let t1 = std::time::Instant::now();
    let cfg = manta_engine::PipelineConfig::default();
    let report =
        manta_engine::decode_samples(&rendered.samples, spec.fs, spec.center_freq_hz, &cfg)
            .unwrap();
    eprintln!(
        "decode: {:?}, events: {}",
        t1.elapsed(),
        report.events.len()
    );
}
