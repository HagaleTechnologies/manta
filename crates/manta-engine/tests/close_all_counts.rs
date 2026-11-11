use manta_engine::{
    listen_with_observers, ListenObservers, PipelineConfig, TrackLifecycleObserver,
};
use manta_input::IqSource;
use manta_testkit::scene::{render_scene, SignalSpec};
use num_complex::Complex32;
use std::sync::{atomic::AtomicBool, Arc};

struct Src {
    samples: Vec<Complex32>,
    cursor: usize,
    gap_after: Option<usize>,
    pending_gap: Option<u64>,
    served: usize,
    armed: bool,
}
impl IqSource for Src {
    fn sample_rate(&self) -> f64 {
        96_000.0
    }
    fn center_freq_hz(&self) -> f64 {
        14_000_000.0
    }
    fn read(&mut self, out: &mut [Complex32]) -> anyhow::Result<usize> {
        if let Some(k) = self.gap_after {
            if !self.armed && self.served >= k {
                self.armed = true;
                self.cursor = 0;
                self.pending_gap = Some(96_000);
            }
        }
        let limit = match (self.gap_after, self.armed) {
            (Some(k), false) => k.min(self.samples.len()),
            _ => self.samples.len(),
        };
        let n = out.len().min(limit - self.cursor).min(4096);
        out[..n].copy_from_slice(&self.samples[self.cursor..self.cursor + n]);
        self.cursor += n;
        self.served += n;
        Ok(n)
    }
    fn take_discontinuity(&mut self) -> Option<u64> {
        self.pending_gap.take()
    }
}

fn scene(n_signals: usize, secs: f64) -> Vec<Complex32> {
    let v1 = manta_testkit::vectors::v1();
    let proto = &v1.signals[0];
    let signals: Vec<SignalSpec> = (0..n_signals)
        .map(|i| {
            let mut s = proto.clone();
            s.offset_hz = -40_000.0 + 2_000.0 * i as f64;
            s.snr_2500_db = 25.0;
            s.wpm = 18.0 + (i % 5) as f32;
            s.text = "CQ CQ DE W1AW W1AW K".into();
            s
        })
        .collect();
    render_scene(&signals, 96_000.0, secs, Some(7)).unwrap().0
}

fn run(src: Src, reconnect: u64) {
    let obs = Arc::new(TrackLifecycleObserver::default());
    listen_with_observers(
        Box::new(src),
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
    let s = obs.snapshot();
    let c = s.close_counts;
    let sum = c.restricted
        + c.unconfirmed
        + c.hang_expired
        + c.silent
        + c.merged
        + c.evicted
        + c.rtty
        + c.not_cw
        + c.unkeyed
        + c.source_end
        + c.reconnect;
    assert_eq!(s.spawns, sum);
    assert_eq!(c.source_end, 40);
    assert_eq!(c.reconnect, reconnect);
}

#[test]
fn close_all_closes_count_toward_source_end_and_reconnect() {
    let samples = scene(40, 20.0);
    run(
        Src {
            samples: samples.clone(),
            cursor: 0,
            gap_after: None,
            pending_gap: None,
            served: 0,
            armed: false,
        },
        0,
    );
    let k = samples.len() / 2;
    run(
        Src {
            samples,
            cursor: 0,
            gap_after: Some(k),
            pending_gap: None,
            served: 0,
            armed: false,
        },
        40,
    );
}
