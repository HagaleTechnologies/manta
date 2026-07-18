//! CI-scoped soak test: a genuinely automated proxy for ROADMAP's "runs >= 1
//! hour without panic or unbounded memory" M1 gate, run against a long
//! synthetic scene at file-replay pace rather than a literal wall-clock
//! hour. A real-hardware, real-duration soak is the manual runbook's job
//! (design doc §8), not CI's.
use skimmer_engine::{soak, soak_passed, PipelineConfig};
use skimmer_input::AudioIqSource;
use skimmer_testkit::keyer::{key_text_loop, KeyerSpec};
use std::time::Duration;

#[test]
fn soak_survives_a_sustained_run_without_panic_or_unbounded_memory() {
    let fs = skimmer_input::TARGET_RATE_HZ;
    let spec = KeyerSpec::new(25.0);
    let (env, _) = key_text_loop(
        "CQ CQ DE W1AW W1AW K CQ CQ DE VK9DX VK9DX K",
        &spec,
        fs as f64,
        120.0,
    )
    .unwrap();
    let mut real = vec![0.0f32; env.len()];
    let dphi = std::f64::consts::TAU * 700.0 / fs as f64;
    let mut phi = 0.0f64;
    for (i, r) in real.iter_mut().enumerate() {
        *r = env.get(i).copied().unwrap_or(0.0) * phi.cos() as f32;
        phi += dphi;
    }
    let src =
        AudioIqSource::new(Box::new(coppa_audio::WavSource::from_samples(real, fs))).unwrap();
    let report = soak(src, &PipelineConfig::default(), Duration::from_secs(120)).unwrap();
    assert!(!report.panicked, "soak panicked: {report:?}");
    assert!(soak_passed(&report), "soak failed: {report:?}");
}
