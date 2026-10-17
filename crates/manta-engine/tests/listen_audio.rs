//! Integration test: a clean real-audio WAV fixture, decoded end-to-end
//! through the AudioIqSource -> listen streaming pipeline. Design doc §4.

use manta_engine::{listen, PipelineConfig};
use manta_input::AudioIqSource;
use manta_testkit::keyer::{key_text_loop, KeyerSpec};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

/// MAN-273 (formerly issue #21, `#[ignore]`d since M1): a single clean tone
/// through `AudioIqSource` must decode with exactly one emitting track. The
/// 129-tap Hilbert FIR leaked the tone's negative-frequency image (~43 dB
/// down) and the per-channel `TrackManager` spawned and churned tracks on
/// it. Fixed by the 511-tap FIR (`manta-dsp::hilbert`) plus the
/// source-declared DC/Nyquist spawn guard (`IqSource::analytic_guard_hz`).
/// The fixture carries a realistic +20 dB SNR-in-2500 noise floor: a
/// noiseless scene is pathological on the complex-IQ path too. See
/// docs/DECISIONS/2026-10-16-man273-hilbert-image-guard.md.
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
    let sigma = manta_testkit::noise::real_awgn_sigma_for_snr_2500(20.0, fs);
    manta_testkit::noise::add_real_awgn(&mut real, sigma, 0x1234_5678);

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
    // Locked once: `assert_eq!` would otherwise lock twice in one statement.
    let seen_track_ids = track_ids.lock().unwrap().clone();
    assert_eq!(
        seen_track_ids.len(),
        1,
        "one clean tone must produce exactly one emitting track, got {seen_track_ids:?}"
    );
}

/// MAN-34: a center frequency configured on `AudioIqSource` itself reaches
/// the emitted `TrackMeta.freq_hz` as `center + baseband offset`, not a bare
/// offset. Deliberately asserts on frequency placement only, never on decoded
/// text: routing real audio through `AudioIqSource`'s Hilbert front end trips
/// the near-DC leakage of issue #21 (see this file's other test), which
/// spawns extra spurious tracks. Selecting the track closest to the expected
/// frequency makes this test immune to those extras, and the "nothing below
/// 1 MHz" assertion is what actually fails if the center frequency is dropped.
#[test]
fn listen_applies_an_audio_sources_configured_center_freq_hz() {
    const CENTER_HZ: f64 = 14_030_000.0;
    const TONE_HZ: f64 = 750.0; // 8 * 93.75 Hz -- an exact channel center.
    let fs = 48_000.0;
    let spec = KeyerSpec::new(20.0);
    let (env, _keyed) = key_text_loop("CQ CQ DE W1AW W1AW K", &spec, fs, 15.0).unwrap();

    let mut real = vec![0.0f32; env.len()];
    let dphi = std::f64::consts::TAU * TONE_HZ / fs;
    let mut phi = 0.0f64;
    for (i, r) in real.iter_mut().enumerate() {
        *r = env.get(i).copied().unwrap_or(0.0) * phi.cos() as f32;
        phi += dphi;
    }

    let src: Box<dyn manta_input::IqSource> = Box::new(
        AudioIqSource::new(Box::new(coppa_audio::WavSource::from_samples(real, 48_000)))
            .unwrap()
            .with_center_freq_hz(CENTER_HZ)
            .unwrap(),
    );

    let stop = Arc::new(AtomicBool::new(false));
    let freqs = Arc::new(Mutex::new(Vec::<f64>::new()));
    let freqs_cb = freqs.clone();
    listen(
        src,
        &PipelineConfig::default(),
        stop,
        move |ev| {
            if let manta_decode::events::DecoderEvent::TrackMeta { freq_hz, .. } = ev {
                freqs_cb.lock().unwrap().push(*freq_hz);
            }
        },
        |_spot| {},
    )
    .unwrap();

    let freqs = freqs.lock().unwrap().clone();
    assert!(!freqs.is_empty(), "expected at least one TrackMeta event");
    // A dropped center frequency reports bare audio offsets (< 24 kHz).
    for f in &freqs {
        assert!(
            (f - CENTER_HZ).abs() < fs / 2.0,
            "TrackMeta.freq_hz {f} is outside the passband around {CENTER_HZ} -- \
             a hardcoded center_freq_hz=0.0 would put it near the audio offset"
        );
    }
    let best = freqs
        .iter()
        .copied()
        .min_by(|a, b| {
            (a - (CENTER_HZ + TONE_HZ))
                .abs()
                .total_cmp(&(b - (CENTER_HZ + TONE_HZ)).abs())
        })
        .unwrap();
    assert!(
        (best - (CENTER_HZ + TONE_HZ)).abs() < 200.0,
        "closest TrackMeta.freq_hz {best} should be within 200 Hz of {} \
         (center + keyed tone offset)",
        CENTER_HZ + TONE_HZ
    );
}

/// MAN-273 (plan AC2): the same +20 dB audio scene churns no more
/// unconfirmed candidates than the complex-IQ control's noise-driven level
/// (17 measured; ceiling 25), and never holds more than a handful of
/// tracks at once. Before the fix this scene churned 263 at a peak of 9.
#[test]
fn a_clean_audio_tone_does_not_churn_candidates() {
    let fs = 48_000.0;
    let spec = KeyerSpec::new(20.0);
    let (env, _) = key_text_loop("CQ CQ DE W1AW W1AW K", &spec, fs, 15.0).unwrap();
    let dphi = std::f64::consts::TAU * 750.0 / fs;
    let mut real: Vec<f32> = env
        .iter()
        .enumerate()
        .map(|(i, &e)| e * (dphi * i as f64).cos() as f32)
        .collect();
    let sigma = manta_testkit::noise::real_awgn_sigma_for_snr_2500(20.0, fs);
    manta_testkit::noise::add_real_awgn(&mut real, sigma, 0x1234_5678);
    let src: Box<dyn manta_input::IqSource> = Box::new(
        AudioIqSource::new(Box::new(coppa_audio::WavSource::from_samples(real, 48_000))).unwrap(),
    );
    let lifecycle = Arc::new(manta_engine::TrackLifecycleObserver::default());
    let observers = manta_engine::ListenObservers {
        track_lifecycle: Some(lifecycle.clone()),
        ..Default::default()
    };
    manta_engine::listen_with_observers(
        src,
        &PipelineConfig::default(),
        Arc::new(AtomicBool::new(false)),
        observers,
        |_| {},
        |_| {},
        |_| {},
    )
    .unwrap();
    let snap = lifecycle.snapshot();
    eprintln!("MAN-273 churn census: {snap:?}");
    assert!(
        snap.close_counts.unconfirmed <= 25,
        "unconfirmed churn {} > 25: {snap:?}",
        snap.close_counts.unconfirmed
    );
    assert!(snap.peak_active_tracks <= 3, "{snap:?}");
}

/// MAN-273 validate round 1: rig audio decimated to 6 kHz
/// (`--capture-rate-hz 6000`) still spawns a track near the top of its
/// usable band. The decimator filters out the Hilbert FIR's Nyquist-rim
/// leak, so `DecimatingSource` declares a DC rim only; forwarding the
/// 48 kHz guard to both rims masked 2484-2672 Hz, inside the advertised
/// 300-2716 Hz band, and this tone decoded nothing.
#[test]
fn a_decimated_audio_tone_near_the_band_top_still_decodes() {
    let fs = 48_000.0;
    let tone_hz = 2_625.0; // 28 * 93.75 Hz: a channel centre at 6 kHz too.
    let spec = KeyerSpec::new(20.0);
    let (env, keyed_text) = key_text_loop("CQ CQ DE W1AW W1AW K", &spec, fs, 15.0).unwrap();
    let dphi = std::f64::consts::TAU * tone_hz / fs;
    let mut real: Vec<f32> = env
        .iter()
        .enumerate()
        .map(|(i, &e)| e * (dphi * i as f64).cos() as f32)
        .collect();
    let sigma = manta_testkit::noise::real_awgn_sigma_for_snr_2500(20.0, fs);
    manta_testkit::noise::add_real_awgn(&mut real, sigma, 0x1234_5678);
    let audio =
        AudioIqSource::new(Box::new(coppa_audio::WavSource::from_samples(real, 48_000))).unwrap();
    let src: Box<dyn manta_input::IqSource> =
        Box::new(manta_input::DecimatingSource::new(Box::new(audio), 6_000.0).unwrap());
    let (_, hi) = src
        .trusted_band_hz()
        .expect("a decimated source has a trusted band");
    assert!(
        tone_hz < hi,
        "the tone must sit inside the trusted band (< {hi} Hz)"
    );

    let text = Arc::new(Mutex::new(String::new()));
    let text_clone = text.clone();
    listen(
        src,
        &PipelineConfig::default(),
        Arc::new(AtomicBool::new(false)),
        move |ev| {
            if let manta_decode::events::DecoderEvent::CharDecoded { glyph, .. } = ev {
                if let Some(c) = glyph.text_char() {
                    text_clone.lock().unwrap().push(c);
                }
            }
        },
        |_spot| {},
    )
    .unwrap();
    let decoded = text.lock().unwrap().clone();
    assert!(
        decoded.contains("W1AW"),
        "expected W1AW in decoded text, got {decoded:?} (keyed: {keyed_text:?})"
    );
}
