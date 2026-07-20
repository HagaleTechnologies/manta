//! Regression for a high-WPM inter-character gap misclassification.
//! docs/DECISIONS/2026-07-18-char-gap-threshold-fix.md.
//!
//! At 33 WPM, Demod's hysteresis+debounce overshoot (SPEC §3.3) inflates
//! mu_dit_ms enough that this real "AB" inter-character gap computed to
//! 1.82 dits -- under the old CHAR_GAP_DITS=2.0 nominal threshold -- so it
//! was classified inter-element instead of inter-character. Both letters'
//! marks then merged into one invalid 6-element run, which the beam decoder
//! force-fit to ':'.

use skimmer_engine::{decode_samples, PipelineConfig};
use skimmer_testkit::cer::cer;
use skimmer_testkit::keyer::{key_text, KeyerSpec};
use skimmer_testkit::scene::{render_scene, SignalSpec};

/// SPEC §2.1's ~2.05 s warmup+confirm floor: this scene's duration
/// (`keyed_length + 1.5 s` ≈ 2.1 s) is too close to that floor for the
/// real detector to ever promote a track and decode before the clip ends
/// -- not a bug, a scene-duration gap exposed by wiring the real detector
/// in. Tracked to be fixed by Task 11's Step 0 (duration-floor fix),
/// docs/superpowers/plans/2026-07-19-m2-detector-track-pool.md.
#[test]
#[ignore]
fn ab_at_33wpm_does_not_merge_into_one_character() {
    let fs = 96_000.0;
    let text = "AB".to_string();
    let wpm = 33.14012f32;
    let snr = 24.410885f32;
    let offset_khz = -7i32;
    let noise_seed = 694100648224208083u64;

    let (probe_env, _) = key_text(&text, &KeyerSpec::new(wpm), fs).unwrap();
    let duration_s = probe_env.len() as f64 / fs + 1.5;
    let sig = SignalSpec {
        text: text.clone(),
        loop_text: false,
        wpm,
        offset_hz: offset_khz as f64 * 1000.0,
        snr_2500_db: snr,
        jitter: None,
        qsb: None,
        watterson: None,
    };
    let (iq, texts) =
        render_scene(std::slice::from_ref(&sig), fs, duration_s, Some(noise_seed)).unwrap();
    let report = decode_samples(&iq, fs, 0.0, &PipelineConfig::default()).unwrap();
    assert_eq!(
        cer(&texts[0], &report.text),
        0.0,
        "keyed {:?} decoded {:?}",
        texts[0],
        report.text
    );
}
