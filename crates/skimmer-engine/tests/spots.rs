//! `decode_samples`'s new `spots` field: a real `skimmer-spot::Validator`
//! run over the full multi-track event stream. M3 engine-wiring sub-
//! project, docs/superpowers/specs/2026-07-26-m3-engine-wiring-design.md.

use skimmer_engine::SpotType;
use skimmer_engine::{decode_samples, PipelineConfig};
use skimmer_testkit::scene::{render_scene, SignalSpec};

#[test]
fn decode_samples_spots_a_repeated_valid_callsign() {
    let sig = SignalSpec {
        text: "CQ CQ DE K5ARH K5ARH K".into(),
        loop_text: true,
        wpm: 20.0,
        offset_hz: 12_340.0,
        snr_2500_db: 20.0,
        jitter: None,
        qsb: None,
        watterson: None,
        char_wpm: None,
    };
    let (iq, _texts) = render_scene(std::slice::from_ref(&sig), 96_000.0, 30.0, Some(1)).unwrap();
    let report = decode_samples(&iq, 96_000.0, 14_000_000.0, &PipelineConfig::default()).unwrap();

    assert!(
        report.spots.iter().any(|s| s.callsign == "K5ARH"),
        "expected a K5ARH spot, got spots: {:?}",
        report.spots
    );
    let spot = report.spots.iter().find(|s| s.callsign == "K5ARH").unwrap();
    assert_eq!(spot.spot_type, SpotType::Cq);
}
