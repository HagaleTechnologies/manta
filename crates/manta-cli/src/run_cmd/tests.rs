use super::*;

#[test]
fn conditioned_metric_registration_matches_producible_marks() {
    use manta_dsp::condition::{ConditioningConfig, StageMode};
    for (mode, impulse_marks, dc_marks) in [
        (StageMode::Off, 0, 0),
        (StageMode::Observe, 2, 0),
        (StageMode::Apply, 3, 1),
    ] {
        let metrics = manta_server::metrics::Metrics::new();
        publish_conditioned(
            &metrics,
            "rx",
            &ConditioningConfig {
                impulse: mode,
                dc: mode,
                ..Default::default()
            },
            &manta_engine::ConditioningObserver::default(),
        );
        let text = metrics.render_prometheus_text();
        let samples: Vec<_> = text
            .lines()
            .filter(|l| l.starts_with("manta_input_conditioned_samples_total{"))
            .collect();
        assert_eq!(
            samples
                .iter()
                .filter(|l| l.contains("stage=\"impulse\""))
                .count(),
            impulse_marks
        );
        assert_eq!(
            samples
                .iter()
                .filter(|l| l.contains("stage=\"dc\""))
                .count(),
            dc_marks
        );
        assert!(samples.iter().all(|l| l.ends_with(" 0")));
    }
}
