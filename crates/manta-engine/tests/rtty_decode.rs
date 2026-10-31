//! First MAN-321 slice: bounded diagnostic text after FSK recognition.
use manta_engine::{decode_samples, PipelineConfig};
use manta_testkit::fsk::{add_fsk, FskSpec};
use manta_testkit::scene::render_scene;

#[test]
fn recognised_rtty_exposes_diagnostic_text_in_both_polarities() {
    for reversed in [false, true] {
        let fs = 96_000.0;
        let (mut iq, _) = render_scene(&[], fs, 12.0, Some(11)).unwrap();
        add_fsk(
            &mut iq,
            fs,
            &FskSpec {
                mark_below_space: reversed,
                mark_offset_hz: 15000.0,
                shift_hz: 170.0,
                baud: 45.45,
                snr_2500_db: 20.0,
                text: "CQ TEST W1AW W1AW ".into(),
                start_s: 0.0,
                watterson: None,
                seed: 1,
            },
        );
        let mut config = PipelineConfig::default();
        config.detector.rtty_dwell_hops = 750;
        let report = decode_samples(&iq, fs, 14_070_000.0, &config).unwrap();
        assert_eq!(report.rtty.len(), 1);
        let text = &report.rtty[0].decoded_text;
        assert!(
            text.contains("CQ TEST W1AW W1AW"),
            "reversed={reversed}: {text:?}"
        );
        assert!(text.len() <= 256);
        // No RTTY DecoderEvents or spots are emitted by this diagnostic slice.
        assert!(report.spots.is_empty());
    }
}
