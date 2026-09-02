//! `decode_samples`'s new `spots` field: a real `manta-spot::Validator`
//! run over the full multi-track event stream. M3 engine-wiring sub-
//! project, docs/superpowers/specs/2026-07-26-m3-engine-wiring-design.md.

use manta_engine::SpotType;
use manta_engine::{decode_samples, PipelineConfig};
use manta_testkit::scene::{render_scene, SignalSpec};

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

/// MAN-29 review: the top-level `DecodeReport::freq_hz` and every emitted
/// spot's `freq_hz` are both documented as the absolute spot frequency and
/// serialized together -- a caller must never see them disagree.
#[test]
fn decode_samples_applies_freq_correction_consistently_to_report_and_spots() {
    const PPM: f64 = 10.0;

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
    let cfg = PipelineConfig {
        freq_correction_ppm: PPM,
        ..Default::default()
    };
    let report = decode_samples(&iq, 96_000.0, 14_000_000.0, &cfg).unwrap();

    let spot = report
        .spots
        .iter()
        .find(|s| s.callsign == "K5ARH")
        .expect("expected a K5ARH spot");
    // report.freq_hz (last TrackMeta for the track) and spot.freq_hz (the
    // validator's own snapshot at word-boundary time) are captured at
    // different points in the event stream even before calibration, so
    // they only agree within the ~10 Hz decode-accuracy figure
    // (ARCHITECTURE §6 step 5) -- this asserts the *same* correction was
    // applied to both, not bit-for-bit equality.
    assert!(
        (report.freq_hz - spot.freq_hz).abs() < 20.0,
        "report.freq_hz {} and spot.freq_hz {} both claim to be the same signal's absolute \
         frequency and must agree (within decode-accuracy noise) once freq_correction_ppm is \
         applied to both",
        report.freq_hz,
        spot.freq_hz
    );

    let uncorrected = report.freq_hz / (1.0 + PPM * 1e-6);
    assert!(
        (uncorrected - (14_000_000.0 + 12_340.0)).abs() < 100.0,
        "report.freq_hz {} divided back by the calibration factor should land near the raw \
         decoded frequency {}",
        report.freq_hz,
        14_000_000.0 + 12_340.0
    );
}

#[test]
fn decode_samples_rejects_an_invalid_freq_correction_ppm() {
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
    let cfg = PipelineConfig {
        freq_correction_ppm: f64::NAN,
        ..Default::default()
    };
    assert!(decode_samples(&iq, 96_000.0, 14_000_000.0, &cfg).is_err());
}
