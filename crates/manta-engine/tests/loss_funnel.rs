//! MAN-198: the loss funnel end to end. One production pass over a testkit
//! scene, each truth opportunity charged to exactly one earliest failed
//! stage. Both ticket Gherkin scenarios are asserted here; the per-stage
//! rules are unit-tested in `funnel/attribute.rs`.

use anyhow::Result;
use manta_engine::funnel::{run_funnel, FunnelReport, Opportunity, Stage, Tolerances};
use manta_engine::PipelineConfig;
use num_complex::Complex32;

struct VecSource {
    samples: Vec<Complex32>,
    cursor: usize,
    fs: f64,
    center_freq_hz: f64,
    /// MAN-273: an analytic front end's DC/Nyquist spawn guard; 0 is none.
    guard_hz: f64,
}

impl manta_input::IqSource for VecSource {
    fn sample_rate(&self) -> f64 {
        self.fs
    }
    fn center_freq_hz(&self) -> f64 {
        self.center_freq_hz
    }
    fn analytic_guard_hz(&self) -> f64 {
        self.guard_hz
    }
    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        let n = buf.len().min(self.samples.len() - self.cursor);
        buf[..n].copy_from_slice(&self.samples[self.cursor..self.cursor + n]);
        self.cursor += n;
        Ok(n)
    }
}

fn op(call: &str, freq_hz: f64, t: f64) -> Opportunity {
    Opportunity {
        call: call.into(),
        freq_hz,
        first_s: t,
        last_s: t,
        rows: 1,
        snr_db: 20,
        wpm: Some(20.0),
    }
}

/// V1 (W1AW looped at 20 WPM, 14012.34 kHz, 96 kS/s around 14.000 MHz),
/// shortened to 45 s: W1AW's first CQ spot lands about 21 s in.
fn v1_funnel(ops: &[Opportunity]) -> FunnelReport {
    let mut spec = manta_testkit::vectors::v1();
    spec.duration_s = 45.0;
    funnel_on(spec, 0.0, ops)
}

fn funnel_on(
    spec: manta_testkit::vectors::VectorSpec,
    guard_hz: f64,
    ops: &[Opportunity],
) -> FunnelReport {
    let rendered = manta_testkit::vectors::render(&spec).unwrap();
    let duration_s = rendered.samples.len() as f64 / spec.fs;
    let src = Box::new(VecSource {
        samples: rendered.samples,
        cursor: 0,
        fs: spec.fs,
        center_freq_hz: spec.center_freq_hz,
        guard_hz,
    });
    run_funnel(
        src,
        &PipelineConfig::default(),
        ops,
        &Tolerances::default(),
        duration_s,
    )
    .unwrap()
}

/// Both Gherkin scenarios end to end: the V1 station decoded as a
/// validated spot is delivered; a frequency where no track ever opened is
/// charged to detectable energy and to no later stage.
#[test]
fn funnel_charges_one_stage_per_opportunity_on_v1() {
    let ops = [
        op("W1AW", 14_012_300.0, 25.0),
        op("OH2XYZ", 14_030_000.0, 25.0),
        op("W9LATE", 14_012_300.0, 660.0),
        op("VE3OUT", 14_060_000.0, 25.0),
    ];
    let report = v1_funnel(&ops);
    let by_call = |c: &str| report.records.iter().find(|r| r.call == c).unwrap();

    // Gherkin 2: a validated spot is a full pass, counted as delivered.
    let w1aw = by_call("W1AW");
    assert_eq!(w1aw.reached, Some(Stage::Delivered), "{w1aw:?}");
    assert_eq!(w1aw.lost_at, None);
    assert!(!w1aw.spots.is_empty());
    assert!(w1aw.tracks.leased >= 1);

    // Gherkin 1: no track ever opened near OH2XYZ, so detectable energy.
    let oh2 = by_call("OH2XYZ");
    assert_eq!(oh2.reached, Some(Stage::InputCoverage), "{oh2:?}");
    assert_eq!(oh2.lost_at, Some(Stage::DetectableEnergy));
    assert_eq!(oh2.detail.as_deref(), Some("noRise"));
    assert_eq!(oh2.tracks.candidates, 0);
    assert!(oh2.spots.is_empty() && oh2.decisions.is_empty());

    assert_eq!(
        by_call("W9LATE").detail.as_deref(),
        Some("outsideRecording")
    );
    assert_eq!(by_call("VE3OUT").detail.as_deref(), Some("outsidePassband"));

    // Not double-counted: one stage per opportunity, nothing past it.
    let s = &report.summary;
    assert_eq!(s.opportunities, 4);
    assert_eq!(s.delivered, 1);
    assert_eq!(s.lost_at.input_coverage, 2);
    assert_eq!(s.lost_at.detectable_energy, 1);
    for st in &Stage::ALL[2..] {
        assert_eq!(s.lost_at.get(*st), 0, "{st:?}");
    }
    assert_eq!(
        Stage::ALL
            .iter()
            .map(|st| s.lost_at.get(*st))
            .sum::<usize>()
            + s.delivered,
        s.opportunities
    );
    assert_eq!(s.passed.get(Stage::Delivered), 1);
    assert_eq!(s.passed.get(Stage::DetectableEnergy), 1);

    // Candidate births and active-track births are counted apart.
    assert!(s.births.candidates >= s.births.confirmed);
    assert!(s.births.confirmed >= s.births.leased);
    assert!(s.births.leased >= 1);
}

#[test]
fn funnel_is_deterministic() {
    let ops = [
        op("W1AW", 14_012_300.0, 25.0),
        op("OH2XYZ", 14_030_000.0, 25.0),
    ];
    let first = v1_funnel(&ops);
    assert_eq!(v1_funnel(&ops), first);
}

/// A station inside the source's own spawn mask is an input-coverage loss,
/// not a detection loss: the analytic guard refused every rise near it.
#[test]
fn source_mask_is_input_coverage() {
    let mut spec = manta_testkit::vectors::v1();
    spec.duration_s = 45.0;
    spec.signals[0].offset_hz = 0.0;
    let report = funnel_on(spec, 600.0, &[op("W1AW", 14_000_000.0, 25.0)]);
    let r = &report.records[0];
    assert_eq!(r.reached, None, "{r:?}");
    assert_eq!(r.lost_at, Some(Stage::InputCoverage));
    assert_eq!(r.detail.as_deref(), Some("sourceMask:analyticGuard"));
    assert_eq!(r.tracks.candidates, 0);
}
