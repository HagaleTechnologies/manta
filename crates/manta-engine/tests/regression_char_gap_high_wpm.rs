//! Regression for a high-WPM inter-character gap misclassification.
//! docs/DECISIONS/2026-07-18-char-gap-threshold-fix.md.
//!
//! At 33 WPM, Demod's hysteresis+debounce overshoot (SPEC §3.3) inflates
//! mu_dit_ms enough that this real "AB" inter-character gap computed to
//! 1.82 dits -- under the old CHAR_GAP_DITS=2.0 nominal threshold -- so it
//! was classified inter-element instead of inter-character. Both letters'
//! marks then merged into one invalid 6-element run, which the beam decoder
//! force-fit to ':'.

use manta_engine::{decode_samples, PipelineConfig};
use manta_testkit::cer::cer;
use manta_testkit::scene::{render_scene, SignalSpec};

/// SPEC §2.1's ~2.05 s mandatory warmup(750 hops)+confirm(19 hops) floor:
/// this scene's original duration (`keyed_length + 1.5 s` ≈ 2.1 s) put the
/// entire once-keyed "AB" signal *before* hop 750 -- the real detector
/// (unlike the old placeholder) refuses to promote any track before then,
/// so the whole clip ended with zero tracks and `decode_samples` returned
/// "no signal found". Not a CER problem, a scene-construction gap exposed
/// by wiring the real detector in (Task 11 Step 0,
/// docs/superpowers/plans/2026-07-19-m2-detector-track-pool.md).
///
/// First tried: keep the once-keyed "AB" and prepend a silent lead-in
/// comfortably over the 2.05 s floor. Measured and rejected: with a
/// genuinely cold (zero-activity) lead-in, "AB"'s own 2 marks are the
/// *first* marks the demod ever sees, and SPEC §4.1's online 2-means speed
/// tracker only becomes `ready()` after its 5th mark (same quorum noted in
/// `roundtrip_iq.rs`) -- so "A" decoded as "EE" (measured), an artifact of
/// this test's cold start, unrelated to the char-gap bug under regression.
/// Continuously-keyed golden vectors (V1 etc.) don't hit this: their signal
/// has been running since before hop 0, so the speed tracker is already
/// warmed up by the time a track promotes and starts decoding forward.
///
/// Fixed instead by looping "AB" (`loop_text: true`) across an extended
/// (30 s) duration, matching that same "signal already running before
/// promotion" shape: several repetitions play out during warmup+confirm
/// (lost, same as V1's leading "CQ "), then many more decode cleanly
/// afterward with a warmed-up speed tracker. CER floor measured empirically
/// at 0.0874 (deterministic, fixed noise_seed, reproduced identically across
/// 3 runs) -- 0.12 gives ~37 % margin over that floor, roughly matching
/// `track.rs::active_track_decodes_real_text`'s own floor-to-threshold
/// ratio. The specific historical bug signature (a spurious ':' from the
/// "AB" merge) is checked directly, preserving this test's original
/// regression intent.
#[test]
fn ab_at_33wpm_does_not_merge_into_one_character() {
    let fs = 96_000.0;
    let text = "AB".to_string();
    let wpm = 33.14012f32;
    let snr = 24.410885f32;
    let offset_khz = -7i32;
    let noise_seed = 694100648224208083u64;
    // Comfortably past the ~2.05 s warmup+confirm floor, plus several more
    // seconds so multiple "AB" repetitions decode after track promotion.
    let duration_s = 30.0;

    let sig = SignalSpec {
        text: text.clone(),
        loop_text: true,
        wpm,
        offset_hz: offset_khz as f64 * 1000.0,
        snr_2500_db: snr,
        jitter: None,
        qsb: None,
        watterson: None,
        char_wpm: None,
    };
    let (iq, texts) =
        render_scene(std::slice::from_ref(&sig), fs, duration_s, Some(noise_seed)).unwrap();
    let report = decode_samples(&iq, fs, 0.0, &PipelineConfig::default()).unwrap();
    assert!(
        !report.text.contains(':'),
        "AB merged into ':' (the historical char-gap bug) -- keyed {:?} decoded {:?}",
        texts[0],
        report.text
    );
    let cer_val = cer(&texts[0], &report.text);
    assert!(
        cer_val < 0.12,
        "expected CER < 0.12 (measured floor 0.0874; ~2.05 s warmup+confirm floor loses the \
         leading repetitions of this looped scene, see this test's doc comment), got \
         {cer_val:.4}\nkeyed {:?}\ndecoded {:?}",
        texts[0],
        report.text
    );
}
