//! MAN-324: measures the live-state tap's record rates and its wall-time
//! overhead. Runs `listen_with_observers` over the V1, V8 and V8w testkit
//! vectors (120 s each), once with no observers and once with a counting
//! `LiveStateSink` that always wants records, and prints records/s per
//! kind plus both wall times. Bytes per second are not measured here: the
//! wire mapping lives in `manta-cli`. The decision record
//! (docs/DECISIONS/2026-10-12-man324-live-state-stream.md) quotes this
//! example's output. Usage: `cargo run --release -p manta-engine --example
//! man324_live_rates`.

use manta_engine::{
    listen_with_observers, ListenObservers, LiveRecord, LiveStateSink, PipelineConfig,
};
use num_complex::Complex32;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

#[derive(Default)]
struct Counting {
    bands: AtomicU64,
    tracks: AtomicU64,
    decisions: AtomicU64,
    closed: AtomicU64,
}

impl LiveStateSink for Counting {
    fn wants_records(&self) -> bool {
        true
    }
    fn publish(&self, record: LiveRecord) {
        let counter = match record {
            LiveRecord::Bands(_) => &self.bands,
            LiveRecord::Track(_) => &self.tracks,
            LiveRecord::Decision(_) => &self.decisions,
            LiveRecord::TrackClosed { .. } => &self.closed,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// The rendered vector, read back in fixed chunks.
struct Samples {
    samples: Vec<Complex32>,
    cursor: usize,
    fs: f64,
    center_freq_hz: f64,
}

impl manta_input::IqSource for Samples {
    fn sample_rate(&self) -> f64 {
        self.fs
    }
    fn center_freq_hz(&self) -> f64 {
        self.center_freq_hz
    }
    fn read(&mut self, buf: &mut [Complex32]) -> anyhow::Result<usize> {
        let n = buf.len().min(self.samples.len() - self.cursor);
        buf[..n].copy_from_slice(&self.samples[self.cursor..self.cursor + n]);
        self.cursor += n;
        Ok(n)
    }
}

fn run(
    spec: &manta_testkit::vectors::VectorSpec,
    samples: &[Complex32],
    sink: Option<Arc<Counting>>,
) -> anyhow::Result<(usize, f64)> {
    let src = Box::new(Samples {
        samples: samples.to_vec(),
        cursor: 0,
        fs: spec.fs,
        center_freq_hz: spec.center_freq_hz,
    });
    let observers = ListenObservers {
        live_state: sink.map(|s| s as Arc<dyn LiveStateSink>),
        ..Default::default()
    };
    let mut spots = 0usize;
    let t0 = Instant::now();
    listen_with_observers(
        src,
        &PipelineConfig::default(),
        Arc::new(AtomicBool::new(false)),
        observers,
        |_| {},
        |_| spots += 1,
        |_| {},
    )?;
    Ok((spots, t0.elapsed().as_secs_f64()))
}

fn main() -> anyhow::Result<()> {
    use manta_testkit::vectors;
    for (name, spec) in [
        ("V1", vectors::v1()),
        ("V8", vectors::v8()),
        ("V8w", vectors::v8w()),
    ] {
        let samples = vectors::render(&spec)?.samples;
        let (spots_off, wall_off) = run(&spec, &samples, None)?;
        let sink = Arc::new(Counting::default());
        let (spots_on, wall_on) = run(&spec, &samples, Some(sink.clone()))?;
        assert_eq!(spots_off, spots_on, "the tap must never change spots");
        let per_s = |c: &AtomicU64| c.load(Ordering::Relaxed) as f64 / spec.duration_s;
        println!(
            "{name}: {:.0} s, {spots_on} spots; records/s bandState={:.2} trackState={:.2} \
             candidateDecision={:.2} trackClosed={:.2}; wall without tap {wall_off:.2} s, \
             with tap {wall_on:.2} s ({:+.1}%)",
            spec.duration_s,
            per_s(&sink.bands),
            per_s(&sink.tracks),
            per_s(&sink.decisions),
            per_s(&sink.closed),
            (wall_on / wall_off - 1.0) * 100.0,
        );
    }
    Ok(())
}
