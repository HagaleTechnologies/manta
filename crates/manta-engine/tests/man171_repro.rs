//! MAN-171 repro: does decode_samples ever promote a track for two
//! synthetic signals placed at the exact offsets/SNRs of the real
//! synchronized-RBN-capture dead zone (JN1THL @ +1150 Hz/38dB, VK2GR
//! @ +7800 Hz/20dB, relative to a 14030 kHz / 192 kHz-passband center)?

use manta_engine::{decode_samples, PipelineConfig};
use manta_testkit::scene::SignalSpec;

fn sig(offset_hz: f64, snr_2500_db: f32) -> SignalSpec {
    SignalSpec {
        text: "CQ CQ DE TEST TEST K".into(),
        loop_text: true,
        wpm: 25.0,
        offset_hz,
        snr_2500_db,
        jitter: None,
        qsb: None,
        watterson: None,
        char_wpm: None,
    }
}

#[test]
fn two_signals_near_dc_both_promote() {
    let fs = 192_000.0;
    let center = 14_030_000.0;
    let signals = vec![sig(1_150.0, 38.0), sig(7_800.0, 20.0)];
    let (samples, _texts) =
        manta_testkit::scene::render_scene(&signals, fs, 15.0, Some(171)).unwrap();

    let report = decode_samples(&samples, fs, center, &PipelineConfig::default()).unwrap();

    let promoted: Vec<f64> = report
        .events
        .iter()
        .filter_map(|e| match e {
            manta_decode::events::DecoderEvent::TrackPromoted { freq_hz, .. } => Some(*freq_hz),
            _ => None,
        })
        .collect();

    eprintln!("TrackPromoted freqs: {promoted:?}");
    eprintln!("all events: {:#?}", report.events);

    assert!(
        !promoted.is_empty(),
        "expected at least one TrackPromoted event, got none"
    );
}

#[test]
fn two_signals_near_dc_with_dc_bias_still_promote() {
    let fs = 192_000.0;
    let center = 14_030_000.0;
    let signals = vec![sig(1_150.0, 38.0), sig(7_800.0, 20.0)];
    let (mut samples, _texts) =
        manta_testkit::scene::render_scene(&signals, fs, 15.0, Some(171)).unwrap();

    // Simulate an uncorrected ADC/USB DC bias (common on cheap SDR front
    // ends when the driver doesn't apply DC-offset correction) -- a
    // constant real offset added to every I/Q sample, well above the
    // scene's own noise floor.
    let bias = 0.02f32; // ~ -34 dBFS given MASTER_SCALE=0.05, noise sigma ~0.0354*0.05
    for s in &mut samples {
        s.re += bias;
        s.im += bias;
    }

    let report = decode_samples(&samples, fs, center, &PipelineConfig::default()).unwrap();

    let promoted: Vec<f64> = report
        .events
        .iter()
        .filter_map(|e| match e {
            manta_decode::events::DecoderEvent::TrackPromoted { freq_hz, .. } => Some(*freq_hz),
            _ => None,
        })
        .collect();

    eprintln!("[dc-bias] TrackPromoted freqs: {promoted:?}");

    let near_jn1thl = promoted.iter().any(|&f| (f - 14_031_150.0).abs() < 200.0);
    let near_vk2gr = promoted.iter().any(|&f| (f - 14_037_800.0).abs() < 200.0);
    eprintln!("[dc-bias] near_jn1thl={near_jn1thl} near_vk2gr={near_vk2gr}");
}

#[test]
fn two_signals_survive_a_busy_band() {
    // Real 20m during an active session is not two isolated tones in
    // silence -- it's dozens of simultaneous QSOs. Simulate a busy band:
    // ~24 background signals spread across the 192 kHz passband (varied
    // WPM/SNR/duty), plus the two real MAN-171 targets, and see whether
    // the targets still promote once the floor/block-median machinery has
    // real neighboring signal traffic to contend with (untested by any
    // existing golden vector or channelizer_multisignal.rs, both capped
    // at 3 simultaneous signals).
    let fs = 192_000.0;
    let center = 14_030_000.0;

    let mut signals = vec![sig(1_150.0, 38.0), sig(7_800.0, 20.0)];
    // Background QSOs: spaced every ~3.7 kHz from -90 kHz to +90 kHz
    // (skipping the target frequencies), moderate-to-strong SNRs, mixed
    // WPM so duty cycle varies.
    let mut offset: f64 = -90_000.0;
    let mut i = 0u32;
    while offset < 90_000.0 {
        if (offset - 1_150.0).abs() > 2_000.0 && (offset - 7_800.0).abs() > 2_000.0 {
            let wpm = 18.0 + (i % 5) as f32 * 6.0; // 18..42
            let snr = 10.0 + (i % 4) as f32 * 5.0; // 10..25
            let mut s = sig(offset, snr);
            s.wpm = wpm;
            s.text = "CQ CQ TEST DE N0CALL N0CALL K".into();
            signals.push(s);
            i += 1;
        }
        offset += 3_700.0;
    }
    eprintln!("[busy-band] total signals: {}", signals.len());

    let (samples, _texts) =
        manta_testkit::scene::render_scene(&signals, fs, 30.0, Some(171)).unwrap();

    let report = decode_samples(&samples, fs, center, &PipelineConfig::default()).unwrap();

    let promoted: Vec<f64> = report
        .events
        .iter()
        .filter_map(|e| match e {
            manta_decode::events::DecoderEvent::TrackPromoted { freq_hz, .. } => Some(*freq_hz),
            _ => None,
        })
        .collect();

    let near_jn1thl = promoted.iter().any(|&f| (f - 14_031_150.0).abs() < 200.0);
    let near_vk2gr = promoted.iter().any(|&f| (f - 14_037_800.0).abs() < 200.0);
    eprintln!(
        "[busy-band] {} promotions total, near_jn1thl={near_jn1thl} near_vk2gr={near_vk2gr}",
        promoted.len()
    );
    assert!(
        near_jn1thl,
        "JN1THL-equivalent target never promoted in busy band"
    );
    assert!(
        near_vk2gr,
        "VK2GR-equivalent target never promoted in busy band"
    );
}

#[test]
fn two_signals_with_realistic_hf_fading() {
    // "Up to 38 dB"/"up to 20 dB" are RBN *peak* readings over a 90 s
    // window, not a sustained level. Real ionospheric propagation fades
    // (Watterson 2-path model, already used for V5/V6/V8w) could plausibly
    // drop a peak-38dB signal below the 12 dB on_snr_db threshold for long
    // enough that confirm_hops=19 (48ms of *unbroken* rise) never
    // completes -- CLAUDE.md already tracks a fading-robustness gap
    // (MAN-107..113) for V5/V6/V8w. Test whether that same gap explains
    // MAN-171's total dead zone once peak-SNR signals get realistic fading.
    use manta_testkit::scene::WattersonFade;

    let fs = 192_000.0;
    let center = 14_030_000.0;
    let mut jn1thl = sig(1_150.0, 38.0);
    jn1thl.watterson = Some(WattersonFade {
        preset: coppa_channel::watterson::WattersonPreset::Poor,
        seed: 171,
    });
    let mut vk2gr = sig(7_800.0, 20.0);
    vk2gr.watterson = Some(WattersonFade {
        preset: coppa_channel::watterson::WattersonPreset::Poor,
        seed: 172,
    });
    let signals = vec![jn1thl, vk2gr];

    let (samples, _texts) =
        manta_testkit::scene::render_scene(&signals, fs, 30.0, Some(171)).unwrap();

    let report = decode_samples(&samples, fs, center, &PipelineConfig::default()).unwrap();

    let promoted: Vec<f64> = report
        .events
        .iter()
        .filter_map(|e| match e {
            manta_decode::events::DecoderEvent::TrackPromoted { freq_hz, .. } => Some(*freq_hz),
            _ => None,
        })
        .collect();

    let near_jn1thl = promoted.iter().any(|&f| (f - 14_031_150.0).abs() < 200.0);
    let near_vk2gr = promoted.iter().any(|&f| (f - 14_037_800.0).abs() < 200.0);
    eprintln!(
        "[fading] {} promotions total, near_jn1thl={near_jn1thl} near_vk2gr={near_vk2gr}: {:?}",
        promoted.len(),
        promoted
    );
}
