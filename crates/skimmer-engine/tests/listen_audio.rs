//! Integration test: a clean real-audio WAV fixture, decoded end-to-end
//! through the AudioIqSource -> listen streaming pipeline. Design doc §4.

use skimmer_engine::{listen, PipelineConfig};
use skimmer_input::AudioIqSource;
use skimmer_testkit::keyer::{key_text_loop, KeyerSpec};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

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

    let src =
        AudioIqSource::new(Box::new(coppa_audio::WavSource::from_samples(real, 48_000))).unwrap();

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
    })
    .unwrap();

    let decoded = text.lock().unwrap().trim().to_string();
    assert!(
        decoded.contains("W1AW"),
        "expected W1AW in decoded text, got {decoded:?} (keyed: {keyed_text:?})"
    );
}
