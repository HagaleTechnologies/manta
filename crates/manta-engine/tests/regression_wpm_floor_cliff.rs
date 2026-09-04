//! Regression for MAN-5: a sharp decode-garbling cliff at WPM in roughly
//! [10.0, 10.15], clearing completely by 10.2 WPM.
//!
//! Root cause: a freshly-promoted track's `Demod` buffers its first ~1 s of
//! magnitude samples (the SPEC §3.1 init window) and, once that window's
//! `A_ref`/rail estimate succeeds, replays it through the run-detection
//! state machine from a blank `open: None` state (`envelope.rs::push`).
//! That replay has no notion of what the physical signal was doing before
//! this `Demod` started observing it -- if the window happens to begin
//! mid-element, the run already in progress is measured as starting fresh
//! at the window's first sample, so its duration can be drastically
//! truncated (a real ~360 ms dah measured as an 8-hop/21 ms "mark"). Fed
//! straight into `SpeedTracker::on_mark` (the only place the 2-means
//! dit/dah bootstrap gets its samples), that one bogus short duration
//! permanently anchors `mu_dit_ms` far below any real dit -- every
//! subsequent genuine mark (dit or dah alike) then classifies as the "hi"
//! (dah) cluster, which is exactly the observed all-dah ("...TTTT...")
//! garble. `TrackDecoder::on_run` (`crates/manta-decode/src/decoder.rs`)
//! now excludes only its very first run from the speed-tracker bootstrap.
//!
//! Whether the init window's replay-start lands mid-element or on a clean
//! boundary is a smooth function of WPM (it shifts the fixed ~2 s
//! warmup+confirm-hop promotion time relative to the keyed waveform), which
//! is why this reproduces as a sharp, narrow WPM cliff for any fixed
//! text/offset/SNR/seed rather than a broad failure band.

use manta_engine::{decode_samples, PipelineConfig};
use manta_testkit::cer::cer;
use manta_testkit::scene::{render_scene, SignalSpec};

#[test]
fn wpm_at_10_floor_does_not_garble() {
    let fs = 96_000.0;
    let text = "GAMMAFOX".to_string();
    let offset_hz = -2000.0;
    let snr_2500_db = 15.0;
    let noise_seed = 572453789768900049u64;
    let duration_s = 12.0;

    // The ticket's reported broken band (WPM 10.0-10.15) plus 10.2 as the
    // first previously-clean point, all held to the same tolerance: the
    // ordinary "leading repetition(s) lost to warmup" CER floor every other
    // continuously-keyed scenario in this suite already allows (see
    // `regression_char_gap_high_wpm.rs`), not the near-total (CER > 1)
    // garble this ticket reports.
    for &wpm in &[10.0f32, 10.05, 10.1, 10.15, 10.2] {
        let sig = SignalSpec {
            text: text.clone(),
            loop_text: true,
            wpm,
            offset_hz,
            snr_2500_db,
            jitter: None,
            qsb: None,
            watterson: None,
            char_wpm: None,
        };
        let (iq, texts) =
            render_scene(std::slice::from_ref(&sig), fs, duration_s, Some(noise_seed)).unwrap();
        let report = decode_samples(&iq, fs, 0.0, &PipelineConfig::default()).unwrap();
        assert!(
            !report.text.contains("TTTT"),
            "wpm {wpm}: repeating spurious dah pattern (the historical MAN-5 garble) -- \
             keyed {:?} decoded {:?}",
            texts[0],
            report.text
        );
        let cer_val = cer(&texts[0], &report.text);
        assert!(
            cer_val < 0.3,
            "wpm {wpm}: expected CER < 0.3 (normal warmup-floor error margin; measured floor \
             0.25), got {cer_val:.4}\nkeyed {:?}\ndecoded {:?}",
            texts[0],
            report.text
        );
    }
}
