//! MAN-207: `decode_source_observed` returns the validator's observation
//! records for an offline decode without changing the report, and starts a
//! new epoch at each MAN-309 segment.

use anyhow::Result;
use manta_engine::{decode_source, decode_source_observed, ObservationPolicy, PipelineConfig};
use manta_input::IqSource;
use manta_testkit::vectors::VectorSpec;
use num_complex::Complex32;

/// Whole parts, each after an optional reported gap (as decode_source.rs's
/// `Gapped`), read at most 65 536 samples at a time.
struct Gapped {
    parts: Vec<(Option<u64>, Vec<Complex32>)>,
    fs: f64,
    center: f64,
    part: usize,
    at: usize,
    pending: Option<u64>,
}

impl Gapped {
    fn new(parts: Vec<(Option<u64>, Vec<Complex32>)>, fs: f64, center: f64) -> Self {
        Gapped {
            parts,
            fs,
            center,
            part: 0,
            at: 0,
            pending: None,
        }
    }
}

impl IqSource for Gapped {
    fn sample_rate(&self) -> f64 {
        self.fs
    }

    fn center_freq_hz(&self) -> f64 {
        self.center
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        loop {
            let Some((gap, iq)) = self.parts.get(self.part) else {
                return Ok(0);
            };
            if self.at == iq.len() {
                self.part += 1;
                self.at = 0;
                continue;
            }
            if self.at == 0 {
                self.pending = *gap;
            }
            let n = 65_536.min(buf.len()).min(iq.len() - self.at);
            buf[..n].copy_from_slice(&iq[self.at..self.at + n]);
            self.at += n;
            return Ok(n);
        }
    }

    fn take_discontinuity(&mut self) -> Option<u64> {
        self.pending.take()
    }
}

fn shortened(spec: VectorSpec, seconds: f64) -> VectorSpec {
    VectorSpec {
        duration_s: seconds,
        ..spec
    }
}

/// A gap-free source over `samples`.
fn source(spec: &VectorSpec, samples: &[Complex32]) -> Gapped {
    Gapped::new(vec![(None, samples.to_vec())], spec.fs, spec.center_freq_hz)
}

fn rendered(spec: &VectorSpec) -> Vec<Complex32> {
    manta_testkit::vectors::render(spec).unwrap().samples
}

/// Short enough to keep the suite quick, long enough to decide something.
fn vectors() -> Vec<VectorSpec> {
    vec![
        shortened(manta_testkit::vectors::v1(), 30.0),
        shortened(manta_testkit::vectors::v8(), 20.0),
        shortened(manta_testkit::vectors::v8w(), 20.0),
    ]
}

#[test]
fn observations_never_change_the_report() {
    let cfg = PipelineConfig::default();
    for spec in vectors() {
        let samples = rendered(&spec);
        let plain = decode_source(&mut source(&spec, &samples), &cfg).unwrap();
        let plain_json = serde_json::to_string(&plain).unwrap();
        for policy in [
            ObservationPolicy::summary(),
            ObservationPolicy::records(),
            ObservationPolicy::text(),
        ] {
            let (report, run) =
                decode_source_observed(&mut source(&spec, &samples), &cfg, policy).unwrap();
            assert_eq!(
                serde_json::to_string(&report).unwrap(),
                plain_json,
                "{} at {policy:?}",
                spec.name
            );
            assert_eq!(
                run.records.is_empty(),
                policy == ObservationPolicy::summary()
            );
            assert!(run.counts.total() > 0, "{} decides something", spec.name);
        }
    }
}

#[test]
fn every_appended_record_is_returned() {
    let spec = shortened(manta_testkit::vectors::v8(), 20.0);
    let (_, run) = decode_source_observed(
        &mut source(&spec, &rendered(&spec)),
        &PipelineConfig::default(),
        ObservationPolicy::records(),
    )
    .unwrap();
    assert_eq!(run.losses.unsent, 0);
    let seqs: Vec<u64> = run.records.iter().map(|o| o.seq).collect();
    let want: Vec<u64> = (0..run.records.len() as u64).collect();
    assert_eq!(seqs, want);
    assert!(!run.records.is_empty());
}

#[test]
fn the_same_input_gives_the_same_records() {
    let spec = shortened(manta_testkit::vectors::v8(), 20.0);
    let samples = rendered(&spec);
    let run = || {
        decode_source_observed(
            &mut source(&spec, &samples),
            &PipelineConfig::default(),
            ObservationPolicy::text(),
        )
        .unwrap()
        .1
        .records
    };
    assert_eq!(run(), run());
}

#[test]
fn a_known_gap_starts_a_new_epoch() {
    let spec = shortened(manta_testkit::vectors::v1(), 40.0);
    let samples = rendered(&spec);
    let split = (20.0 * spec.fs) as usize;
    let missing = (5.0 * spec.fs) as u64;
    let parts = vec![
        (None, samples[..split].to_vec()),
        (Some(missing), samples[split..].to_vec()),
    ];
    let mut src = Gapped::new(parts, spec.fs, spec.center_freq_hz);
    let (_, run) = decode_source_observed(
        &mut src,
        &PipelineConfig::default(),
        ObservationPolicy::records(),
    )
    .unwrap();
    let resumed = split as u64 + missing;
    let before: Vec<u32> = run
        .records
        .iter()
        .filter(|o| o.draft.sample_ts() < resumed)
        .map(|o| o.draft.source.epoch)
        .collect();
    let after: Vec<u32> = run
        .records
        .iter()
        .filter(|o| o.draft.sample_ts() >= resumed)
        .map(|o| o.draft.source.epoch)
        .collect();
    assert!(
        !before.is_empty() && !after.is_empty(),
        "{:#?}",
        run.records
    );
    assert!(before.iter().all(|&e| e == 0), "{before:?}");
    assert!(after.iter().all(|&e| e == 1), "{after:?}");
}
