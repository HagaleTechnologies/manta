//! MAN-109: count rejected tracks through the production streaming loop.
use manta_engine::{
    listen_with_observers, ListenObservers, PipelineConfig, TrackLifecycleObserver,
};
use manta_input::IqSource;
use manta_testkit::scene::{render_scene, SignalSpec};
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
fn census(
    seed: u64,
    seconds: f64,
    signal: bool,
    threshold: f32,
) -> manta_engine::TrackLifecycleSnapshot {
    let signals = if signal {
        vec![SignalSpec {
            text: "CQ CQ DE W1AW W1AW K".into(),
            loop_text: true,
            wpm: 20.0,
            offset_hz: 12_340.0,
            snr_2500_db: 20.0,
            jitter: None,
            qsb: None,
            watterson: None,
            char_wpm: None,
            weight: 3.0,
            dit_units: 1.0,
            element_gap_units: 1.0,
            char_gap_units: 3.0,
            word_gap_units: 7.0,
            rise_ms: 5.0,
        }]
    } else {
        vec![]
    };
    let (samples, _) = render_scene(&signals, 96_000.0, seconds, Some(seed)).unwrap();
    let obs = Arc::new(TrackLifecycleObserver::default());
    let mut cfg = PipelineConfig::default();
    cfg.detector.on_snr_db = threshold;
    listen_with_observers(
        Box::new(Source { samples, cursor: 0 }),
        &cfg,
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
    eprintln!("seed={seed} seconds={seconds} signal={signal} threshold={threshold}: {snap:?}");
    snap
}
#[test]
fn noise_false_rate() {
    for seed in 1..=3 {
        let c = census(seed, 30.0, false, 11.5).close_counts;
        assert_eq!(c.not_cw, 0);
        assert!(c.unconfirmed <= [10, 26, 0][seed as usize - 1]);
        assert_eq!(c.false_lease_hops, 0);
    }
}
#[test]
#[ignore = "120 s baseline; run --include-ignored before handover"]
fn v1_false_rate() {
    let c = census(1, 120.0, true, 11.5).close_counts;
    assert_eq!(c.not_cw, 0);
    assert!(c.unconfirmed <= 10);
    assert_eq!(c.false_lease_hops, 0);
}
