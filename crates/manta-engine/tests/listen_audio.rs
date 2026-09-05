//! Integration test: a clean real-audio WAV fixture, decoded end-to-end
//! through the AudioIqSource -> listen streaming pipeline. Design doc §4.

use manta_engine::{listen, PipelineConfig};
use manta_input::AudioIqSource;
use manta_testkit::keyer::{key_text_loop, KeyerSpec};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

/// MAN-4: this test was `#[ignore]`d because `AudioIqSource`'s 129-tap
/// Hilbert FIR leaked the negative-frequency image of the 750 Hz tone
/// (~43 dB rejection) and the real per-channel `TrackManager` (SPEC §2,
/// landed in M2 sub-project 2) spawned dozens of spurious tracks on that
/// leakage, several churning IDs while locked onto the same real signal in
/// parallel. Fixed by widening the Hilbert FIR to 511 taps (>= 70 dB image
/// rejection across `[HILBERT_GUARD_HZ, fs/2 - HILBERT_GUARD_HZ]`, asserted
/// by `manta-dsp::hilbert`'s `image_rejection_meets_the_guaranteed_band_contract`)
/// plus a source-declared DC/Nyquist spawn guard
/// (`IqSource::analytic_guard_hz`, `DetectorConfig::guard_hz`).
///
/// **Remediation round finding, not in the original plan:** those two
/// fixes alone left a second, dominant cluster of spurious CANDIDATE
/// spawns spread broadly across the audio band (~500 for this fixture),
/// unrelated to Hilbert image rejection (present identically at 129 and
/// 511 taps). Root cause: this fixture's "key up" gaps are exact digital
/// silence, so `manta-dsp::floor::FloorBank`'s percentile estimator clamps
/// every quiet channel to its histogram minimum (-140 dBFS) -- no real
/// receiver ever produces that. Ordinary keying-edge transient splatter,
/// many dB under any real noise floor, then reads as tens of dB above
/// that clamp and clears the 12 dB rise gate. `manta_testkit::noise::add_real_awgn`
/// at a realistic +30 dB SNR-in-2500 floor (`real_noise_sigma_for_snr_2500`)
/// gives every channel a real percentile floor and collapses that cluster
/// to a couple of already-understood, bounded residuals (see
/// `manta_engine::listen`'s `a_clean_audio_tone_spawns_one_track_and_no_churn`
/// unit test for the per-channel census). See
/// docs/DECISIONS/2026-09-04-man-4-hilbert-guard-pins.md pin 12 for the
/// full measurement. Formerly tracked at
/// <https://github.com/HagaleTechnologies/manta/issues/21>.
#[test]
fn listen_decodes_a_clean_real_audio_signal() {
    let fs = 48_000.0;
    let tone_hz = 750.0; // 750 Hz = 8 * 93.75 Hz channel spacing exactly -- a channel-center frequency, avoiding the near-channel-edge decode degradation documented in docs/DECISIONS/2026-07-18-m2-pfb-channelizer-pins.md (the same root cause as V2's #[ignore]'d golden test).
    let spec = KeyerSpec::new(20.0);
    let (env, keyed_text) = key_text_loop("CQ CQ DE W1AW W1AW K", &spec, fs, 15.0).unwrap();

    let mut real = vec![0.0f32; env.len()];
    let dphi = std::f64::consts::TAU * tone_hz / fs;
    let mut phi = 0.0f64;
    for (i, r) in real.iter_mut().enumerate() {
        *r = env.get(i).copied().unwrap_or(0.0) * phi.cos() as f32;
        phi += dphi;
    }
    // MAN-4 remediation: a real receiver never delivers digital silence
    // during key-up -- see this test's doc comment.
    manta_testkit::noise::add_real_awgn(
        &mut real,
        manta_testkit::noise::real_noise_sigma_for_snr_2500(30.0, fs),
        0xC0FFEE,
    );

    let src: Box<dyn manta_input::IqSource> = Box::new(
        AudioIqSource::new(Box::new(coppa_audio::WavSource::from_samples(real, 48_000))).unwrap(),
    );

    let stop = Arc::new(AtomicBool::new(false));
    let text = Arc::new(Mutex::new(String::new()));
    let text_clone = text.clone();
    let track_ids = Arc::new(Mutex::new(std::collections::BTreeSet::new()));
    let track_ids_clone = track_ids.clone();
    listen(
        src,
        &PipelineConfig::default(),
        stop,
        move |ev| {
            if let manta_decode::events::DecoderEvent::CharDecoded {
                glyph, track_id, ..
            } = ev
            {
                if let Some(c) = glyph.text_char() {
                    text_clone.lock().unwrap().push(c);
                }
                track_ids_clone.lock().unwrap().insert(*track_id);
            }
            if matches!(ev, manta_decode::events::DecoderEvent::WordBoundary { .. }) {
                text_clone.lock().unwrap().push(' ');
            }
        },
        |_spot| {},
    )
    .unwrap();

    let decoded = text.lock().unwrap().trim().to_string();
    assert!(
        decoded.contains("W1AW"),
        "expected W1AW in decoded text, got {decoded:?} (keyed: {keyed_text:?})"
    );
    // Locked once, into a local: `assert_eq!` would otherwise evaluate
    // `track_ids.lock()` twice (once for the compared value, once for the
    // failure-message format arg) within the same statement, and
    // `std::sync::Mutex` is not reentrant.
    let seen_track_ids = track_ids.lock().unwrap().clone();
    assert_eq!(
        seen_track_ids.len(),
        1,
        "one clean tone must produce exactly one emitting track, got {seen_track_ids:?}"
    );
}
