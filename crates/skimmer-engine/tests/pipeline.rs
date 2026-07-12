use skimmer_engine::{decode_samples, PipelineConfig};
use skimmer_testkit::cer::cer;
use skimmer_testkit::scene::{render_scene, SignalSpec};

#[test]
fn v1_lite_decodes_end_to_end() {
    // 20 s slice of the V1 scene: same parameters, faster test. The full
    // 120 s V1 gate lives in skimmer-cli/tests/golden_v1.rs.
    let sig = SignalSpec {
        text: "CQ CQ DE W1AW W1AW K".into(),
        loop_text: true,
        wpm: 20.0,
        offset_hz: 12_340.0,
        snr_2500_db: 20.0,
        jitter: None,
    };
    let (iq, texts) = render_scene(std::slice::from_ref(&sig), 96_000.0, 20.0, Some(1)).unwrap();
    let report = decode_samples(&iq, 96_000.0, 14_000_000.0, &PipelineConfig::default()).unwrap();
    assert!(
        (report.freq_hz - 14_012_340.0).abs() <= 10.0,
        "freq {} off by {}",
        report.freq_hz,
        (report.freq_hz - 14_012_340.0).abs()
    );
    assert_eq!(
        cer(&texts[0], &report.text),
        0.0,
        "expected {:?} got {:?}",
        texts[0],
        report.text
    );
    let wpm = report.wpm.expect("wpm reported");
    assert!((wpm - 20.0).abs() < 2.0, "wpm {wpm}");
}

#[test]
fn silence_errors_cleanly() {
    let iq = vec![num_complex::Complex32::new(0.0, 0.0); 96_000];
    // Pure digital silence has no peak; must be an error, not a panic.
    assert!(decode_samples(&iq, 96_000.0, 0.0, &PipelineConfig::default()).is_err());
}
