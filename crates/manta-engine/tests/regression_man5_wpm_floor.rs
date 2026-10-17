//! Regression for MAN-5 / issue #22
//! (<https://github.com/HagaleTechnologies/manta/issues/22>): keyed CW at WPM
//! in roughly [10.0, 10.15] decoded as T/E spam ("E TT TT TT TTTT TTT TTTT",
//! CER ~3) while 10.2+ decoded cleanly.
//!
//! Root cause: `TrackManager` leases a track's decoder on CANDIDATE ->
//! ACTIVE promotion, `confirm_hops` into a mark, so the demod's first run is
//! the truncated tail of that mark. In this scene at 10.0-10.1 WPM the
//! promotion lands in a dit and leaves an 8-15 hop tail (vs ~46 hop real
//! dits); fed to the speed tracker as an init mark it became the dit
//! centroid (first `SpeedUpdate` 30-56 WPM), SPEC §4.1's ratio clamp pinned
//! mu_dah at 3x that, and every later element classified as a dah, with no
//! recovery. At 10.15+ the tail fell under the demod's debounce and was
//! absorbed into the next space, which is why the edge was so sharp. Fixed
//! by `TrackDecoder::new_mid_signal`: the engine's decoder keeps that
//! left-censored first run out of speed tracking.

use manta_decode::events::DecoderEvent;
use manta_engine::{decode_samples, PipelineConfig};
use manta_testkit::cer::cer;
use manta_testkit::scene::{render_scene, SignalSpec};

/// The ticket's exact repro parameters.
const FS: f64 = 96_000.0;
const DURATION_S: f64 = 12.0;
const NOISE_SEED: u64 = 572453789768900049;

/// Inclusive warmup-floor CER bound: the leading characters keyed during
/// the ~2.05 s warmup+confirm floor are structurally lost. At 10.0 WPM the
/// 12 s scene keys only "GAMMAFOX" once and both G and A fall inside that
/// floor, so the clean decode sits at exactly 2/8 = 0.25 (measured; 0.20
/// from 10.1 WPM up, where a trailing "G" also fits). The T/E-spam
/// signature measured CER 2.9-3.0, so this bound alone excludes it.
const MAX_CER: f64 = 0.25;

/// Renders and decodes one looped scene, then asserts the CER bound and a
/// non-garble guard: at least 2 decoded characters drawn from `text`'s own
/// letters other than E/T (the only glyphs the garbled lock produced).
fn assert_clean_decode(text: &str, wpm: f32, offset_hz: f64, snr_2500_db: f32) {
    let sig = SignalSpec {
        text: text.to_string(),
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
        render_scene(std::slice::from_ref(&sig), FS, DURATION_S, Some(NOISE_SEED)).unwrap();
    let report = decode_samples(&iq, FS, 0.0, &PipelineConfig::default()).unwrap();
    let first_speed = report.events.iter().find_map(|e| match e {
        DecoderEvent::SpeedUpdate { wpm, .. } => Some(*wpm),
        _ => None,
    });
    let cer_val = cer(&texts[0], &report.text);
    assert!(
        cer_val <= MAX_CER,
        "{text:?} at {wpm} WPM, offset {offset_hz} Hz, {snr_2500_db} dB: expected CER <= \
         {MAX_CER}, got {cer_val:.4} (first SpeedUpdate {first_speed:?})\nkeyed {:?}\ndecoded {:?}",
        texts[0],
        report.text
    );
    let distinctive = report
        .text
        .chars()
        .filter(|&c| c != 'E' && c != 'T' && text.contains(c))
        .count();
    assert!(
        distinctive >= 2,
        "{text:?} at {wpm} WPM: decoded {:?} has only {distinctive} non-E/T chars from the \
         keyed text (T/E-spam signature)",
        report.text
    );
}

#[test]
fn gammafox_decodes_cleanly_across_the_10_wpm_floor_band() {
    for wpm in [10.0f32, 10.05, 10.1, 10.15, 10.2] {
        assert_clean_decode("GAMMAFOX", wpm, -2000.0, 15.0);
    }
}

/// A second fractional channel offset, on the other side of DC, so the fix
/// is not pinned to the -2000 Hz scene's channel geometry.
#[test]
fn gammafox_decodes_cleanly_at_a_second_fractional_offset() {
    assert_clean_decode("GAMMAFOX", 10.1, 1500.7, 15.0);
}

/// "PARIS" opens with a dit, unlike "GAMMAFOX"'s dah-dah, so it exercises
/// the speed tracker's init split differently.
#[test]
fn paris_decodes_cleanly_at_the_10_wpm_floor() {
    for wpm in [10.0f32, 10.1] {
        assert_clean_decode("PARIS", wpm, -2000.0, 15.0);
    }
}

#[test]
fn gammafox_decodes_cleanly_at_the_10_wpm_floor_high_snr() {
    for wpm in [10.0f32, 10.1] {
        assert_clean_decode("GAMMAFOX", wpm, -2000.0, 28.0);
    }
}
