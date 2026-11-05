//! MAN-493: the loss funnel's four oracle-bypass experiments on two
//! synthetic scenes -- a stage-style scene (V1, a clean 20 WPM W1AW loop,
//! opportunities carrying truth WPM) and a weak-signal scene (a looped CQ
//! at -12 dB SNR that production never detects). Production lines first,
//! then one block per bypass. The two keying-truth experiments
//! (`knownTransitions`, `knownTranscript`) are reachable only here and in
//! tests; the CLI keeps rejecting their names.
//!
//! `cargo run -p manta-engine --example loss_funnel_synthetic --locked`

use anyhow::Result;
use manta_engine::funnel::{
    run_bypass, run_funnel, run_synthetic_bypass, Bypass, FunnelReport, Opportunity, Stage,
    SyntheticTruth, Tolerances,
};
use manta_engine::PipelineConfig;
use manta_testkit::keyer::KeyerSpec;
use num_complex::Complex32;

/// A whole in-memory recording as one source, mirroring the funnel's own
/// integration tests.
struct VecSource {
    samples: Vec<Complex32>,
    cursor: usize,
    fs: f64,
    center_freq_hz: f64,
}

impl manta_input::IqSource for VecSource {
    fn sample_rate(&self) -> f64 {
        self.fs
    }
    fn center_freq_hz(&self) -> f64 {
        self.center_freq_hz
    }
    fn analytic_guard_hz(&self) -> f64 {
        0.0
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

/// The keying truth for `text` keyed once at 20 WPM starting `t0_s` in,
/// as the plain data the synthetic bypasses take.
fn truth_for(text: &str, t0_s: f64, fs: f64) -> SyntheticTruth {
    let spec = KeyerSpec::new(20.0);
    let segs = manta_testkit::funnel_truth::keying_segments(text, &spec).unwrap();
    let total_ms: f64 = segs.iter().map(|s| s.1).sum();
    SyntheticTruth {
        wpm: 20.0,
        envelope: manta_testkit::funnel_truth::envelope_at_hops(
            &segs,
            375.0,
            fs,
            t0_s,
            total_ms / 1000.0,
        ),
        words: manta_testkit::funnel_truth::transcript_items(text, &spec, fs, t0_s)
            .unwrap()
            .into_iter()
            .map(|i| (i.start_ts, i.word))
            .collect(),
    }
}

/// The scene's own call: the last word that is neither a prosign nor DE.
fn signal_call(text: &str) -> &str {
    text.split_whitespace()
        .rev()
        .find(|w| *w != "K" && *w != "DE")
        .unwrap_or("W1AW")
}

/// A bypass block's upper bound never reads below what production
/// delivered -- that bound is each experiment's contract.
fn check_bound(b: Bypass, upper: usize, delivered: usize) -> Result<()> {
    if upper < delivered {
        anyhow::bail!(
            "bypass {} upperBound {upper} below production delivered {delivered}",
            b.label()
        );
    }
    Ok(())
}

/// One scene: a production pass, then the four bypass blocks after its
/// summary (the funnel file's rule: bypass lines only after the
/// production summary).
fn scene(
    name: &str,
    iq: &[Complex32],
    fs: f64,
    center_hz: f64,
    ops: &[Opportunity],
    signal_text: &str,
) -> Result<FunnelReport> {
    let duration_s = iq.len() as f64 / fs;
    let src = Box::new(VecSource {
        samples: iq.to_vec(),
        cursor: 0,
        fs,
        center_freq_hz: center_hz,
    });
    let cfg = PipelineConfig::default();
    let tol = Tolerances::default();
    let report = run_funnel(src, &cfg, ops, &tol, duration_s)?;
    println!("--- scene: {name} ---");
    for r in &report.records {
        println!("{}", serde_json::to_string(r)?);
    }
    println!("{}", serde_json::to_string(&report.summary)?);

    let geo = report.geometry;
    // The two RBN-fed bypasses over every production record.
    for b in [Bypass::KnownFrequency, Bypass::KnownTiming] {
        let (lines, summary) = run_bypass(b, iq, center_hz, &cfg, &report.records, &geo, &tol)?;
        for l in &lines {
            println!("{}", serde_json::to_string(l)?);
        }
        println!("{}", serde_json::to_string(&summary)?);
        check_bound(b, summary.upper_bound, report.summary.delivered)?;
    }

    // The two keying-truth bypasses for the scene's own signal.
    let sig_op = op(signal_call(signal_text), center_hz + 12_340.0, 25.0);
    let truth = truth_for(signal_text, 10.0, fs);
    for b in [Bypass::KnownTransitions, Bypass::KnownTranscript] {
        let (line, summary) =
            run_synthetic_bypass(b, &sig_op, &truth, &cfg, &report.records, &geo, &tol)?;
        println!("{}", serde_json::to_string(&line)?);
        println!("{}", serde_json::to_string(&summary)?);
        check_bound(b, summary.upper_bound, report.summary.delivered)?;
    }
    Ok(report)
}

fn main() -> Result<()> {
    // Scene 1: stage-style -- V1 (clean 20 WPM W1AW at +20 dB, 96 kS/s
    // around 14.000 MHz), 45 s, opportunities carrying truth WPM.
    let mut v1 = manta_testkit::vectors::v1();
    v1.duration_s = 45.0;
    let iq1 = manta_testkit::vectors::render(&v1).unwrap().samples;
    let ops1 = [
        op("W1AW", 14_012_300.0, 25.0),
        op("OH2XYZ", 14_030_000.0, 25.0),
    ];
    let report1 = scene(
        "v1-stage",
        &iq1,
        v1.fs,
        v1.center_freq_hz,
        &ops1,
        &v1.signals[0].text,
    )?;
    assert!(report1.summary.delivered >= 1);

    // Scene 2: weak signal -- the same keying at -12 dB SNR, where
    // production loses the call at detectable energy and only the bypass
    // bounds can say anything.
    let mut weak = manta_testkit::vectors::v1();
    weak.duration_s = 45.0;
    weak.signals[0] = manta_testkit::scene::SignalSpec {
        text: "CQ CQ DE OH2XYZ OH2XYZ K".into(),
        loop_text: true,
        wpm: 20.0,
        offset_hz: 12_340.0,
        snr_2500_db: -12.0,
        jitter: None,
        qsb: None,
        watterson: None,
        char_wpm: None,
        weight: 3.0,
        char_gap_units: 3.0,
        word_gap_units: 7.0,
        rise_ms: 5.0,
        dit_units: 1.0,
        element_gap_units: 1.0,
    };
    let iq2 = manta_testkit::vectors::render(&weak).unwrap().samples;
    let ops2 = [op("OH2XYZ", 14_012_340.0, 25.0)];
    let report2 = scene(
        "weak-signal",
        &iq2,
        weak.fs,
        weak.center_freq_hz,
        &ops2,
        &weak.signals[0].text,
    )?;
    assert_eq!(
        report2.records[0].lost_at,
        Some(Stage::DetectableEnergy),
        "production must lose the weak signal at detection"
    );
    Ok(())
}
