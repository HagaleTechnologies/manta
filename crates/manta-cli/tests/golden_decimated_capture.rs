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

    // Freq error <= 25 Hz, not V1's 10 Hz. Measured (deterministic across 3
    // runs): 17.396 Hz. Isolated the cause with two no-decimation control
    // renders of the same v1() scene: native 48 kHz gives 16.605 Hz error
    // and native 192 kHz gives 15.697 Hz error -- both close to this
    // decimated-path's 17.396 Hz *with zero decimator involvement*. So the
    // gap versus V1's 96 kHz-tuned 10 Hz bound is a channel-table-size-
    // dependent property of the frequency estimator itself -- a different
    // mechanism than the one documented in
    // docs/DECISIONS/2026-07-18-m2-pfb-channelizer-pins.md pin 9 (AWGN
    // corrupting the interpolator's weak neighbor bin at high-offset hops,
    // a placeholder-detector-era limitation), not a decimator artifact and
    // not a decode regression -- CER above is 0.0155, identical to V1's own
    // measured floor. The 25 Hz *magnitude* reuses pin 9's precedent as a
    // previously-accepted round number, not because pin 9 covers this same
    // failure mode -- it doesn't -- with real margin over the measured
    // 17.396 Hz.
    let freq = report["freq_hz"].as_f64().unwrap();
    assert!(
        (freq - rendered.expected_freq_hz).abs() <= 25.0,
        "freq {} expected {} (err {})",
        freq,
        rendered.expected_freq_hz,
        (freq - rendered.expected_freq_hz).abs()
    );

    for ev in report["events"].as_array().unwrap() {
        assert_eq!(ev["track_id"].as_u64(), Some(1));
    }
}
