use manta_engine::{
    listen_with_observers, ListenObservers, PipelineConfig, TrackLifecycleObserver,
};
use manta_input::IqSource;
use manta_testkit::scene::render_scene;
use num_complex::Complex32;
use std::sync::{atomic::AtomicBool, Arc};

struct Source {
    samples: Vec<Complex32>,
    cursor: usize,
}
impl IqSource for Source {
    fn sample_rate(&self) -> f64 {
        96_000.0
    }
    fn center_freq_hz(&self) -> f64 {
        14_000_000.0
    }
    fn read(&mut self, out: &mut [Complex32]) -> anyhow::Result<usize> {
        let n = out.len().min(self.samples.len() - self.cursor);
        out[..n].copy_from_slice(&self.samples[self.cursor..self.cursor + n]);
        self.cursor += n;
        Ok(n)
    }
}
#[test]
fn close_pair_on_bin_150_does_not_churn() {
    let spec = manta_testkit::vectors::close_pair("pair", 10_031.25, 150.0, 15.0, 15.0);
    let (samples, _) = render_scene(
        &spec.signals,
        spec.fs,
        spec.duration_s,
        Some(spec.noise_seed),
    )
    .unwrap();
    let obs = Arc::new(TrackLifecycleObserver::default());
    listen_with_observers(
        Box::new(Source { samples, cursor: 0 }),
        &PipelineConfig::default(),
        Arc::new(AtomicBool::new(false)),
        ListenObservers {
            track_lifecycle: Some(obs.clone()),
            ..Default::default()
        },
        |_| {},
        |_| {},
        |_| {},
    )
    .unwrap();
    let snap = obs.snapshot();
    eprintln!("close-pair census: {snap:?}");
    assert!(snap.close_counts.merged <= 2, "{snap:?}");
    assert!(
        snap.close_counts.unconfirmed + snap.close_counts.merged <= 10,
        "{snap:?}"
    );
}
