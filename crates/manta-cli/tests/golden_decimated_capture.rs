//! New golden vector (issue #169): a clean-20-style scene synthesized at
//! 192 kHz, decimated through the real `manta_dsp::decimate::Decimator`
//! cascade down to 48 kHz, then decoded end-to-end via `manta decode`.
//! Proves the decimator itself doesn't corrupt decode -- distinct from
//! `channelizer_multisignal.rs`, which only proves the channelizer
//! generalizes across table sizes when fed IQ synthesized clean at that
//! rate to begin with, never IQ that was captured at one rate and
//! decimated down (the actually-new code path here).

use manta_dsp::decimate::Decimator;
use num_complex::Complex32;
use std::process::Command;

#[test]
fn clean_signal_decodes_correctly_after_192khz_to_48khz_decimation() {
    // Same clean-20 signal shape as V1 (20 WPM, +20 dB, offset +12.34 kHz,
    // W1AW), but synthesized at 192 kHz so a real decimate-by-4 cascade
    // is exercised (V1 itself is defined at 96 kHz).
    let mut spec = manta_testkit::vectors::v1();
    spec.fs = 192_000.0;
    let rendered = manta_testkit::vectors::render(&spec).unwrap();

    let mut decimator = Decimator::new(192_000.0, 4).unwrap();
    let decimated: Vec<Complex32> = decimator.process(&rendered.samples);
    assert_eq!(decimator.fs_out(), 48_000.0);

    let dir = tempfile::tempdir().unwrap();
    manta_testkit::wav::write_fixture(
        dir.path(),
        "decimated_v1",
        &decimated,
        48_000.0,
        spec.center_freq_hz,
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_manta"))
        .args(["decode", "--json"])
        .arg(dir.path().join("decimated_v1.wav"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();

    // Same V1 char-accuracy and single-track pass criteria (golden_v1.rs).
    let decoded = report["text"].as_str().unwrap();
    let cer_val = manta_testkit::cer::cer(&rendered.keyed_texts[0], decoded);
    assert!(
        cer_val < 0.02,
        "decimated V1 char accuracy must be >= 98% (CER < 0.02), got CER {cer_val:.4}\n\
         expected: {}\ndecoded:  {}",
        rendered.keyed_texts[0],
        decoded
    );

    // Freq error <= 10 Hz, V1's bound. The dB-parabola spot frequency
    // measured 17.396 Hz here (and 16.605/15.697 Hz on native 48/192 kHz
    // control renders), which forced a 25 Hz bound. MAN-114 found the cause
    // was the scene's 0.627-channel fractional offset plus AWGN on the
    // parabola's weak neighbour bin, at every table size, not the channel
    // table or the decimator; the phase estimator measures 1.41 Hz. See
    // docs/DECISIONS/2026-10-17-man114-phase-difference-spot-frequency.md.
    let freq = report["freq_hz"].as_f64().unwrap();
    assert!(
        (freq - rendered.expected_freq_hz).abs() <= 10.0,
        "freq {} expected {} (err {})",
        freq,
        rendered.expected_freq_hz,
        (freq - rendered.expected_freq_hz).abs()
    );

    for ev in report["events"].as_array().unwrap() {
        assert_eq!(ev["track_id"].as_u64(), Some(1));
    }
}

/// MAN-196: a signal just inside the trusted band still decodes at the
/// golden standard. +21 kHz on a 96k->48k decimation sits at the -0.05 dB
/// point of the final halfband stage -- well inside the 21 724.8 Hz
/// trusted boundary (0.4526 x 48 000) -- so the rim-inside channel must
/// be unaffected by the trusted band's advertisement and spawn veto.
/// Research-measured at this cell: freq error 1.5 Hz, CER 0.0103, one
/// track. The 25 Hz / CER < 0.02 bounds reuse this file's existing
/// golden gate.
#[test]
fn rim_inside_the_trusted_band_still_decodes_at_the_golden_standard() {
    let mut spec = manta_testkit::vectors::v1();
    spec.fs = 96_000.0;
    spec.signals[0].offset_hz = 21_000.0;
    let rendered = manta_testkit::vectors::render(&spec).unwrap();

    let mut decimator = Decimator::new(96_000.0, 2).unwrap();
    let decimated: Vec<Complex32> = decimator.process(&rendered.samples);
    assert_eq!(decimator.fs_out(), 48_000.0);

    let dir = tempfile::tempdir().unwrap();
    manta_testkit::wav::write_fixture(
        dir.path(),
        "decimated_v1_rim_inside",
        &decimated,
        48_000.0,
        spec.center_freq_hz,
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_manta"))
        .args(["decode", "--json"])
        .arg(dir.path().join("decimated_v1_rim_inside.wav"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();

    let decoded = report["text"].as_str().unwrap();
    let cer_val = manta_testkit::cer::cer(&rendered.keyed_texts[0], decoded);
    assert!(
        cer_val < 0.02,
        "rim-inside char accuracy must be >= 98% (CER < 0.02), got CER {cer_val:.4}\n\
         expected: {}\ndecoded:  {}",
        rendered.keyed_texts[0],
        decoded
    );

    // V1's 10 Hz (MAN-114; measured 0.02 Hz, a bin-centred offset).
    let freq = report["freq_hz"].as_f64().unwrap();
    assert!(
        (freq - rendered.expected_freq_hz).abs() <= 10.0,
        "freq {} expected {} (err {})",
        freq,
        rendered.expected_freq_hz,
        (freq - rendered.expected_freq_hz).abs()
    );

    for ev in report["events"].as_array().unwrap() {
        assert_eq!(ev["track_id"].as_u64(), Some(1));
    }
}
