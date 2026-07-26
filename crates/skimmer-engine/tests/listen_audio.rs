//! Integration test: a clean real-audio WAV fixture, decoded end-to-end
//! through the AudioIqSource -> listen streaming pipeline. Design doc §4.

use skimmer_engine::{listen, PipelineConfig};
use skimmer_input::AudioIqSource;
use skimmer_testkit::keyer::{key_text_loop, KeyerSpec};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

/// Wiring `listen()` onto the real `TrackManager` (this task) exposed a
/// pre-existing M1-era limitation of `skimmer-input::AudioIqSource`'s
/// `HilbertTransformer`: its own doc comment already admits it's only
/// "well-behaved" "from a few hundred Hz to several kHz" -- outside that
/// band (near DC/Nyquist) its real-to-analytic image rejection is weak.
/// Under the old M1 single-channel `calibrate_channel` design this never
/// mattered (only the one loudest channel was ever examined); `TrackManager`
/// (SPEC §2) watches every channel via a real per-channel floor/gate, so the
/// near-DC leakage now trips spurious CANDIDATE/ACTIVE tracks -- confirmed
/// via a scratch `decode_samples` repro with the identical tone/keying/+20dB
/// SNR AWGN scene but bypassing `AudioIqSource`/Hilbert entirely (raw
/// complex IQ): that path decodes cleanly through the same `TrackManager`
/// (exactly one track), so this is not a `TrackManager`/`FloorBank`/`Gate`
/// bug (Tasks 5-7) and not introduced by this task's `listen()` wiring --
/// it's an audio-front-end gap this task's correct wiring newly exposes.
/// Adding a realistic background noise floor to this fixture (attempted)
/// does not clear it either, since the leakage is structural to the Hilbert
/// filter's near-DC/Nyquist response, not a "zero noise floor" artifact.
/// Not fixable within this task's listen.rs-only scope -- needs either a
/// wider/better Hilbert design or a DC/Nyquist guard band in
/// `skimmer-dsp::hilbert`. Tracked for follow-up alongside the SPEC §2.1
/// warmup-floor item already deferred to Task 11's Step 0 (see
/// `pipeline.rs`'s `v1_lite_decodes_end_to_end`). Tracked as
/// <https://github.com/HagaleTechnologies/skimmer/issues/21>.
#[test]
#[ignore]
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

    let src: Box<dyn skimmer_input::IqSource> = Box::new(
        AudioIqSource::new(Box::new(coppa_audio::WavSource::from_samples(real, 48_000))).unwrap(),
    );

    let stop = Arc::new(AtomicBool::new(false));
    let text = Arc::new(Mutex::new(String::new()));
    let text_clone = text.clone();
    listen(src, &PipelineConfig::default(), stop, move |ev| {
        if let skimmer_decode::events::DecoderEvent::CharDecoded { glyph, .. } = ev {
            if let Some(c) = glyph.text_char() {
                text_clone.lock().unwrap().push(c);
            }
        }
        if matches!(
            ev,
            skimmer_decode::events::DecoderEvent::WordBoundary { .. }
        ) {
            text_clone.lock().unwrap().push(' ');
        }
    }, |_spot| {})
    .unwrap();

    let decoded = text.lock().unwrap().trim().to_string();
    assert!(
        decoded.contains("W1AW"),
        "expected W1AW in decoded text, got {decoded:?} (keyed: {keyed_text:?})"
    );
}
