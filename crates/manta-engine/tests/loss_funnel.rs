//! MAN-198: the loss funnel end to end. One production pass over a testkit
//! scene, each truth opportunity charged to exactly one earliest failed
//! stage. Both ticket Gherkin scenarios are asserted here; the per-stage
//! rules are unit-tested in `funnel/attribute.rs`.

use anyhow::Result;
use manta_engine::funnel::{
    run_bypass, run_funnel, Bypass, FunnelReport, Opportunity, Stage, Tolerances, TRACK_TEXT_CHARS,
};
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

/// The known-frequency bypass on V1: every line is flagged oracle-assisted,
/// the opportunities without audio are unavailable, the forced stages pass,
/// W1AW's text is the oracle's own, and the bound is no lower than
/// production.
#[test]
fn known_frequency_bypass_on_v1() {
    let ops = [
        op("W1AW", 14_012_300.0, 25.0),
        op("OH2XYZ", 14_030_000.0, 25.0),
        op("W9LATE", 14_012_300.0, 660.0),
        op("VE3OUT", 14_060_000.0, 25.0),
    ];
    let mut spec = manta_testkit::vectors::v1();
    spec.duration_s = 45.0;
    let iq = manta_testkit::vectors::render(&spec).unwrap().samples;
    let production = funnel_on(spec.clone(), 0.0, &ops);
    let cfg = PipelineConfig::default();
    let (lines, summary) = run_bypass(
        Bypass::KnownFrequency,
        &iq,
        spec.center_freq_hz,
        &cfg,
        &production.records,
        &production.geometry,
        &Tolerances::default(),
    )
    .unwrap();
    assert_eq!(lines.len(), production.records.len());
    for (line, prod) in lines.iter().zip(&production.records) {
        assert!(line.oracle_assisted);
        assert_eq!(line.call, prod.call);
        assert_eq!(line.production_lost_at, prod.lost_at);
        assert_eq!(
            line.forced_stages,
            [Stage::DetectableEnergy, Stage::AdmittedTrack]
        );
        if line.available {
            assert!(line.reached >= Some(Stage::AdmittedTrack), "{line:?}");
        }
    }
    let by_call = |c: &str| lines.iter().find(|r| r.call == c).unwrap();
    for (call, why) in [
        ("W9LATE", "outsideRecording"),
        ("VE3OUT", "outsidePassband"),
    ] {
        let r = by_call(call);
        assert!(!r.available, "{r:?}");
        assert_eq!(r.detail.as_deref(), Some(why));
        assert_eq!((r.reached, r.lost_at), (None, None));
    }
    // Production lost OH2XYZ at detection; the bypass forces past it.
    let oh2 = by_call("OH2XYZ");
    assert!(oh2.available);
    assert_eq!(oh2.production_lost_at, Some(Stage::DetectableEnergy));

    let w1aw = by_call("W1AW");
    assert_eq!(w1aw.lost_at, None, "{w1aw:?}");
    let oracle_spot = manta_testkit::oracle::OracleSpot {
        call: "W1AW".into(),
        khz: 14_012.3,
        t_sec: 25.0,
        snr_db: 20,
        wpm: 20,
    };
    let (oracle, _) = manta_testkit::oracle::run_oracle(
        &iq,
        spec.fs,
        spec.center_freq_hz,
        &[oracle_spot],
        Tolerances::default().window_s,
        &cfg.decode,
    )
    .unwrap();
    let oracle_text: String = oracle[0].text.chars().take(TRACK_TEXT_CHARS).collect();
    assert_eq!(w1aw.track_text.as_deref(), Some(oracle_text.as_str()));

    assert!(summary.oracle_assisted);
    assert_eq!(summary.opportunities, 4);
    assert_eq!(summary.available, 2);
    assert_eq!(summary.unavailable.values().sum::<usize>(), 2);
    assert!(summary.upper_bound >= production.summary.delivered);
    assert_eq!(
        summary.lost_at.values().sum::<usize>() + summary.delivered,
        summary.available
    );
}

/// The iq of a 45 s V1 render, and its production report over `ops`.
fn v1_production(
    ops: &[Opportunity],
) -> (
    Vec<num_complex::Complex32>,
    FunnelReport,
    manta_testkit::vectors::VectorSpec,
) {
    let mut spec = manta_testkit::vectors::v1();
    spec.duration_s = 45.0;
    let iq = manta_testkit::vectors::render(&spec).unwrap().samples;
    let report = funnel_on(spec.clone(), 0.0, ops);
    (iq, report, spec)
}

/// MAN-493: `known-timing` is honestly unavailable when the truth layer
/// carries no speed, and when the engine's speed never passes through the
/// seedable tracker (hsmm). With both, it forces `usableTiming` past the
/// loss the known-frequency run reported for an empty channel.
#[test]
fn known_timing_unavailable_without_rbn_wpm_or_on_hsmm() {
    let mut no_wpm = op("W1AW", 14_012_300.0, 25.0);
    no_wpm.wpm = None;
    let (iq, production, spec) = v1_production(&[no_wpm]);
    let cfg = PipelineConfig::default();
    let (lines, summary) = run_bypass(
        Bypass::KnownTiming,
        &iq,
        spec.center_freq_hz,
        &cfg,
        &production.records,
        &production.geometry,
        &Tolerances::default(),
    )
    .unwrap();
    assert_eq!(lines.len(), 1);
    assert!(!lines[0].available, "{:?}", lines[0]);
    assert_eq!(lines[0].detail.as_deref(), Some("noRbnWpm"));
    assert_eq!((lines[0].reached, lines[0].lost_at), (None, None));
    // Production still delivered this one, so the bound counts it.
    assert_eq!(summary.upper_bound, 1);

    let mut hsmm_cfg = PipelineConfig::default();
    hsmm_cfg.decode.engine = manta_decode::decoder::Engine::Hsmm;
    let (lines2, _) = run_bypass(
        Bypass::KnownTiming,
        &iq,
        spec.center_freq_hz,
        &hsmm_cfg,
        &production.records,
        &production.geometry,
        &Tolerances::default(),
    )
    .unwrap();
    assert!(!lines2[0].available, "{:?}", lines2[0]);
    assert_eq!(lines2[0].detail.as_deref(), Some("hsmmEngine"));

    // With the speed present and a seedable engine, the empty-channel
    // opportunity no longer dies at usableTiming (the known-frequency
    // loss) but at the honest later stage: the text it decodes is noise.
    let ops = [
        op("W1AW", 14_012_300.0, 25.0),
        op("OH2XYZ", 14_030_000.0, 25.0),
    ];
    let (iq, production, spec) = v1_production(&ops);
    let (lines3, summary3) = run_bypass(
        Bypass::KnownTiming,
        &iq,
        spec.center_freq_hz,
        &cfg,
        &production.records,
        &production.geometry,
        &Tolerances::default(),
    )
    .unwrap();
    let by_call = |c: &str| lines3.iter().find(|r| r.call == c).unwrap();
    let w1aw = by_call("W1AW");
    assert!(w1aw.available);
    assert_eq!(w1aw.lost_at, None, "{w1aw:?}");
    let oh2 = by_call("OH2XYZ");
    assert!(oh2.available);
    assert_ne!(oh2.lost_at, Some(Stage::UsableTiming), "{oh2:?}");
    assert_eq!(summary3.lost_at.get("usableTiming"), None);
    assert!(summary3.delivered >= 1);
}

/// The weak-signal scene: a looped CQ at an SNR the production pass never
/// detects, with its keying truth as plain data.
fn weak_signal_scene() -> (
    Vec<Complex32>,
    FunnelReport,
    manta_testkit::vectors::VectorSpec,
) {
    let mut spec = manta_testkit::vectors::v1();
    spec.duration_s = 45.0;
    spec.signals[0] = manta_testkit::scene::SignalSpec {
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
    let iq = manta_testkit::vectors::render(&spec).unwrap().samples;
    let report = funnel_on(spec.clone(), 0.0, &[op("OH2XYZ", 14_012_340.0, 25.0)]);
    (iq, report, spec)
}

/// The synthetic truth for `text` keyed once at 20 WPM, starting `t0_s`
/// into a stream at `fs`.
fn synthetic_truth(text: &str, t0_s: f64, fs: f64) -> manta_engine::funnel::SyntheticTruth {
    let spec = manta_testkit::keyer::KeyerSpec::new(20.0);
    let segs = manta_testkit::funnel_truth::keying_segments(text, &spec).unwrap();
    let total_ms: f64 = segs.iter().map(|s| s.1).sum();
    manta_engine::funnel::SyntheticTruth {
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

/// MAN-493: `known-transitions` lifts a weak signal production loses at
/// detectableEnergy: fed the exact keying timeline, the bypass decodes
/// the truth text and reaches at least textContainsCall.
#[test]
fn known_transitions_lifts_a_weak_signal() {
    let (_iq, production, spec) = weak_signal_scene();
    let prod = &production.records[0];
    assert_eq!(prod.lost_at, Some(Stage::DetectableEnergy), "{prod:?}");
    assert_eq!(prod.detail.as_deref(), Some("noRise"));

    let truth = synthetic_truth("CQ CQ DE OH2XYZ OH2XYZ K", 10.0, spec.fs);
    let (line, summary) = manta_engine::funnel::run_synthetic_bypass(
        Bypass::KnownTransitions,
        &op("OH2XYZ", 14_012_340.0, 25.0),
        &truth,
        &PipelineConfig::default(),
        &production.records,
        &production.geometry,
        &Tolerances::default(),
    )
    .unwrap();
    assert!(line.available, "{line:?}");
    assert!(line.reached >= Some(Stage::TextContainsCall), "{line:?}");
    assert!(
        line.track_text.as_deref().unwrap_or("").contains("OH2XYZ"),
        "{line:?}"
    );
    assert_eq!(
        summary.delivered + summary.lost_at.values().sum::<usize>(),
        1
    );
}

/// MAN-493: `known-transcript` forces every decode stage and keeps the
/// validator's own gates honest: a call that only ever appears once stays
/// at sufficientEvidence (its single repetition is real evidence about
/// evidence), and a call the text only addresses stays at
/// selfIdRecognized.
#[test]
fn known_transcript_keeps_role_and_evidence_losses() {
    // The truth text's calls as production opportunities: both are
    // undetectable in the weak scene, which is the point -- the bypass
    // forces every decode stage and the validator's gates stay honest.
    let (iq, _, spec) = weak_signal_scene();
    let _ = &iq;
    let production = funnel_on(
        spec.clone(),
        0.0,
        &[
            op("JA1ZZZ", 14_012_340.0, 25.0),
            op("W1XYZ", 14_012_340.0, 25.0),
        ],
    );
    let cfg = PipelineConfig::default();
    let geo = &production.geometry;
    let tol = Tolerances::default();

    // A single-repetition CQ: the text contains the call, the validator
    // still needs a second repetition before it will spot.
    let truth = synthetic_truth("CQ CQ DE JA1ZZZ K", 10.0, spec.fs);
    let (line, _) = manta_engine::funnel::run_synthetic_bypass(
        Bypass::KnownTranscript,
        &op("JA1ZZZ", 14_012_340.0, 25.0),
        &truth,
        &cfg,
        &production.records,
        geo,
        &tol,
    )
    .unwrap();
    assert!(line.available, "{line:?}");
    assert!(line.reached >= Some(Stage::TextContainsCall), "{line:?}");
    // With track metadata present, the validator recognizes the self-ID
    // but withholds the spot because one occurrence needs a repetition.
    assert_eq!(line.lost_at, Some(Stage::SufficientEvidence), "{line:?}");
    assert_eq!(line.detail.as_deref(), Some("repetition 1/2"));
    assert_eq!(line.decisions, ["repetition"]);

    // An addressed call: the text names it, but only as the addressee --
    // the same honest stop, for the role the text gives it.
    let truth = synthetic_truth("W1XYZ DE K1ABC K", 10.0, spec.fs);
    let (line2, _) = manta_engine::funnel::run_synthetic_bypass(
        Bypass::KnownTranscript,
        &op("W1XYZ", 14_012_340.0, 25.0),
        &truth,
        &cfg,
        &production.records,
        geo,
        &tol,
    )
    .unwrap();
    assert!(line2.available, "{line2:?}");
    assert!(line2.reached >= Some(Stage::TextContainsCall), "{line2:?}");
    assert_eq!(line2.lost_at, Some(Stage::SelfIdRecognized), "{line2:?}");
    assert_eq!(line2.detail.as_deref(), Some("noDecision"));
}

/// MAN-493: every bypass line, of all four experiments, is flagged
/// oracle-assisted, names its forced stages, and carries no key a spot
/// reader could mistake for a spot.
#[test]
fn bypass_lines_follow_an_unchanged_production_prefix() {
    let ops = [
        op("W1AW", 14_012_300.0, 25.0),
        op("OH2XYZ", 14_030_000.0, 25.0),
    ];
    let (iq, production, spec) = v1_production(&ops);
    let cfg = PipelineConfig::default();
    let tol = Tolerances::default();

    // The production lines are the same bytes whatever bypasses run
    // later: serialize the report once, compare against itself after the
    // bypass runs (the bypasses cannot touch it, and the pinned shape is
    // what the CLI test checks byte-for-byte on disk).
    let before: Vec<String> = production
        .records
        .iter()
        .map(|r| serde_json::to_string(r).unwrap())
        .chain([serde_json::to_string(&production.summary).unwrap()])
        .collect();

    let mut lines = Vec::new();
    for b in [Bypass::KnownFrequency, Bypass::KnownTiming] {
        let (l, _) = run_bypass(
            b,
            &iq,
            spec.center_freq_hz,
            &cfg,
            &production.records,
            &production.geometry,
            &tol,
        )
        .unwrap();
        lines.extend(l);
    }
    let truth = synthetic_truth("CQ CQ DE W1AW W1AW K", 10.0, spec.fs);
    for b in [Bypass::KnownTransitions, Bypass::KnownTranscript] {
        let (l, _) = manta_engine::funnel::run_synthetic_bypass(
            b,
            &op("W1AW", 14_012_300.0, 25.0),
            &truth,
            &cfg,
            &production.records,
            &production.geometry,
            &tol,
        )
        .unwrap();
        lines.push(l);
    }
    let after: Vec<String> = production
        .records
        .iter()
        .map(|r| serde_json::to_string(r).unwrap())
        .chain([serde_json::to_string(&production.summary).unwrap()])
        .collect();
    assert_eq!(before, after);

    assert_eq!(lines.len(), 2 + 2 + 2);
    for l in &lines {
        let v = serde_json::to_value(l).unwrap();
        let o = v.as_object().unwrap();
        assert_eq!(o["oracleAssisted"], serde_json::json!(true));
        assert!(o["forcedStages"].as_array().unwrap().len() >= 2);
        for key in ["dxCall", "frequency", "spots"] {
            assert!(!o.contains_key(key), "{key} in {v}");
        }
    }
}
