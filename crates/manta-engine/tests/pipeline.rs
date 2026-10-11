use manta_engine::{decode_samples, PipelineConfig};
use manta_testkit::cer::cer;
use manta_testkit::scene::{render_scene, SignalSpec};

/// The 0.12 CER bound was pinned against the old 750-hop (2.0 s) warmup
/// floor, which lost this 20 s scene's leading "CQ " before the detector
/// promoted a track (Task 11 Step 0: CER 0.09375, fixed `noise_seed=1`;
/// docs/superpowers/plans/2026-07-19-m2-detector-track-pool.md). MAN-115's
/// 25 ms warmup after the pre-roll (SPEC §2.2 "Startup") promotes within
/// ~0.07 s, so only the first, mid-element character is still at risk; the
/// bound is kept as an upper limit, not re-tightened. See
/// docs/DECISIONS/2026-10-11-man115-startup-latency.md.
#[test]
fn v1_lite_decodes_end_to_end() {
    // 20 s slice of the V1 scene: same parameters, faster test. The full
    // 120 s V1 gate lives in manta-cli/tests/golden_v1.rs.
    let sig = SignalSpec {
        text: "CQ CQ DE W1AW W1AW K".into(),
        loop_text: true,
        wpm: 20.0,
        offset_hz: 12_340.0,
        snr_2500_db: 20.0,
        jitter: None,
        qsb: None,
        watterson: None,
        char_wpm: None,
        weight: 3.0,
        char_gap_units: 3.0,
        word_gap_units: 7.0,
        rise_ms: 5.0,
    };
    let (iq, texts) = render_scene(std::slice::from_ref(&sig), 96_000.0, 20.0, Some(1)).unwrap();
    let report = decode_samples(&iq, 96_000.0, 14_000_000.0, &PipelineConfig::default()).unwrap();
    // Re-measured (Task 11 Step 3) against SPEC's original 10 Hz: this 20 s
    // scene measures 11.51 Hz, deterministic (reproduced identically across
    // 3 runs) -- just over SPEC's original bound, unlike golden_v1.rs's
    // full 120 s V1 gate (which now clears 10 Hz: more signal time gives
    // Task 9's EMA freq estimate more room to converge). Partially
    // tightened to 15 Hz rather than fully reverted or left at the old
    // 25 Hz -- gives real (~30 %) margin over the measured 11.51 Hz without
    // claiming a bound this shorter scene doesn't reliably clear. See
    // docs/superpowers/plans/2026-07-19-m2-detector-track-pool.md.
    assert!(
        (report.freq_hz - 14_012_340.0).abs() <= 15.0,
        "freq {} off by {}",
        report.freq_hz,
        (report.freq_hz - 14_012_340.0).abs()
    );
    let cer_val = cer(&texts[0], &report.text);
    assert!(
        cer_val < 0.12,
        "expected CER < 0.12 (bound pinned under the pre-MAN-115 2 s warmup floor, see this \
         test's doc comment), got {cer_val:.4}\n\
         expected {:?}\ngot      {:?}",
        texts[0],
        report.text
    );
    let wpm = report.wpm.expect("wpm reported");
    // Re-measured (Task 11 Step 3) against SPEC's original +/-2 WPM: still
    // doesn't clear it -- measured error 2.692 WPM (17.308 reported),
    // deterministic (reproduced identically across 3 runs), same pattern as
    // golden_v1.rs's V1 gate. Left at the wider +/-3 WPM bound (pin 10,
    // docs/DECISIONS/2026-07-18-m2-pfb-channelizer-pins.md), which the
    // measured error clears with real (~11 %) margin. Not SPEC-gated (a
    // "free" bonus check), so left as a measured, explained deviation.
    assert!((wpm - 20.0).abs() < 3.0, "wpm {wpm}");
}

#[test]
fn silence_errors_cleanly() {
    let iq = vec![num_complex::Complex32::new(0.0, 0.0); 96_000];
    // Pure digital silence has no peak; must be an error, not a panic.
    assert!(decode_samples(&iq, 96_000.0, 0.0, &PipelineConfig::default()).is_err());
}
