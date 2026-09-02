//! `decode_samples`'s new `spots` field: a real `manta-spot::Validator`
//! run over the full multi-track event stream. M3 engine-wiring sub-
//! project, docs/superpowers/specs/2026-07-26-m3-engine-wiring-design.md.

use manta_decode::events::DecoderEvent;
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

/// MAN-29 review round 3: the raw `TrackMeta.freq_hz` values in
/// `DecodeReport::events` (consumed directly by `decode --json`/`listen
/// --json`) must also be calibrated, not just the summary `freq_hz` and
/// `spots[*].freq_hz` -- without double-correcting the copy fed to the
/// validator.
#[test]
fn decode_samples_calibrates_track_meta_events_too() {
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

    let uncalibrated =
        decode_samples(&iq, 96_000.0, 14_000_000.0, &PipelineConfig::default()).unwrap();
    let cfg = PipelineConfig {
        freq_correction_ppm: PPM,
        ..Default::default()
    };
    let calibrated = decode_samples(&iq, 96_000.0, 14_000_000.0, &cfg).unwrap();

    let raw_freqs: Vec<f64> = uncalibrated
        .events
        .iter()
        .filter_map(|e| match e {
            DecoderEvent::TrackMeta { freq_hz, .. } => Some(*freq_hz),
            _ => None,
        })
        .collect();
    let calibrated_freqs: Vec<f64> = calibrated
        .events
        .iter()
        .filter_map(|e| match e {
            DecoderEvent::TrackMeta { freq_hz, .. } => Some(*freq_hz),
            _ => None,
        })
        .collect();

    assert!(
        !raw_freqs.is_empty(),
        "expected at least one TrackMeta event"
    );
    assert_eq!(raw_freqs.len(), calibrated_freqs.len());
    let factor = 1.0 + PPM * 1e-6;
    for (raw, corrected) in raw_freqs.iter().zip(calibrated_freqs.iter()) {
        assert!(
            (corrected - raw * factor).abs() < 1e-6,
            "TrackMeta.freq_hz {corrected} should equal raw {raw} * factor {factor}"
        );
    }

    // The validator's own spot output must NOT be double-corrected by this
    // fix -- it should match exactly what round 1's fix already produced.
    let spot = calibrated
        .spots
        .iter()
        .find(|s| s.callsign == "K5ARH")
        .expect("expected a K5ARH spot");
    let uncalibrated_spot = uncalibrated
        .spots
        .iter()
        .find(|s| s.callsign == "K5ARH")
        .expect("expected a K5ARH spot");
    assert!(
        (spot.freq_hz - uncalibrated_spot.freq_hz * factor).abs() < 1e-6,
        "spot.freq_hz {} should equal uncalibrated spot.freq_hz {} * factor {factor} exactly \
         once, not twice",
        spot.freq_hz,
        uncalibrated_spot.freq_hz
    );
}

/// MAN-28 Watch List: `PipelineConfig::allowlist` must actually reach the
/// production validator, not just `manta_spot::Validator::allowlist` in
/// isolation -- otherwise an operator has no way to use the documented
/// bypass via `manta decode`/`listen`. Single-decode repetition-gate
/// bypass is already covered at the `Validator` level (V17); this proves
/// the config wiring itself, via a bogus cty prefix that grammar/cty
/// validation would otherwise reject regardless of repetition count.
#[test]
fn decode_samples_spots_an_allowlisted_call_despite_bogus_cty_prefix() {
    let sig = SignalSpec {
        text: "CQ CQ DE QQ9ZZZ QQ9ZZZ K".into(),
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
        allowlist: vec!["QQ9ZZZ".into()],
        ..Default::default()
    };
    let report = decode_samples(&iq, 96_000.0, 14_000_000.0, &cfg).unwrap();

    assert!(
        report.spots.iter().any(|s| s.callsign == "QQ9ZZZ"),
        "allowlisted QQ9ZZZ (unallocated cty prefix) should still spot, got: {:?}",
        report.spots
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
