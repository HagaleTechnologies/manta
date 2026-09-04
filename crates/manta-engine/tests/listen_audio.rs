//! Integration test: a clean real-audio WAV fixture, decoded end-to-end
//! through the AudioIqSource -> listen streaming pipeline. Design doc §4.

use manta_engine::{listen, PipelineConfig};
use manta_input::AudioIqSource;
use manta_testkit::keyer::{key_text_loop, KeyerSpec};
use std::collections::BTreeSet;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

/// MAN-4: this test was `#[ignore]`d because `AudioIqSource`'s 129-tap
/// Hilbert FIR leaked the negative-frequency image of the 750 Hz tone
/// (~43 dB rejection) and the real per-channel `TrackManager` spawned
/// tracks on it. Fixed by widening the FIR to 511 taps (>= 70 dB across
/// `[HILBERT_GUARD_HZ, fs/2 - HILBERT_GUARD_HZ]`, asserted by
/// `manta-dsp::hilbert`'s `image_rejection_meets_the_guaranteed_band_contract`)
/// plus a source-declared DC/Nyquist spawn guard
/// (`manta_input::IqSource::analytic_guard_hz`,
/// `TrackManager::step_hop`'s spawn-eligibility scan). See
/// docs/DECISIONS/2026-09-04-man-4-hilbert-guard-pins.md.
/// Tracked as <https://github.com/HagaleTechnologies/manta/issues/21>.
#[test]
fn listen_decodes_a_clean_real_audio_signal() {
    let fs = 48_000.0;
    let tone_hz = 750.0; // 750 Hz = 8 * 93.75 Hz channel spacing exactly -- a channel-center frequency, avoiding the near-channel-edge decode degradation documented in docs/DECISIONS/2026-07-18-m2-pfb-channelizer-pins.md (the same root cause as V2's #[ignore]'d golden test).
    let spec = KeyerSpec::new(20.0);
    let (env, keyed_text) = key_text_loop("CQ CQ DE W1AW W1AW K", &spec, fs, 15.0).unwrap();

    // MAN-4: a realistic noise floor, not a strictly noiseless fixture --
    // see docs/DECISIONS/2026-09-04-man-4-hilbert-guard-pins.md and
    // manta_engine::listen's `a_clean_audio_tone_spawns_one_track_and_no_churn`
    // for why a zero-noise fixture defeats the percentile floor estimator
    // regardless of Hilbert filter quality.
    let amp = manta_testkit::noise::amplitude_for_snr_2500(20.0, fs);
    let mut real = vec![0.0f32; env.len()];
    let dphi = std::f64::consts::TAU * tone_hz / fs;
    let mut phi = 0.0f64;
    for (i, r) in real.iter_mut().enumerate() {
        *r = env.get(i).copied().unwrap_or(0.0) * amp * phi.cos() as f32;
        phi += dphi;
    }
    manta_testkit::noise::add_real_unit_awgn(&mut real, 4);

    let src: Box<dyn manta_input::IqSource> = Box::new(
        AudioIqSource::new(Box::new(coppa_audio::WavSource::from_samples(real, 48_000))).unwrap(),
    );

    let stop = Arc::new(AtomicBool::new(false));
    let text = Arc::new(Mutex::new(String::new()));
    let text_clone = text.clone();
    let track_ids = Arc::new(Mutex::new(BTreeSet::new()));
    let track_ids_clone = track_ids.clone();
    listen(
        src,
        &PipelineConfig::default(),
        stop,
        move |ev| {
            if let manta_decode::events::DecoderEvent::CharDecoded { glyph, track_id, .. } = ev {
                if let Some(c) = glyph.text_char() {
                    text_clone.lock().unwrap().push(c);
                }
                track_ids_clone.lock().unwrap().insert(*track_id);
            }
            if let manta_decode::events::DecoderEvent::WordBoundary { track_id, .. } = ev {
                text_clone.lock().unwrap().push(' ');
                track_ids_clone.lock().unwrap().insert(*track_id);
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
    let ids = track_ids.lock().unwrap();
    assert_eq!(
        ids.len(),
        1,
        "one clean tone must produce exactly one emitting track, got {ids:?}"
    );
}
