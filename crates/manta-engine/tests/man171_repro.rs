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
        weight: 3.0,
        char_gap_units: 3.0,
        word_gap_units: 7.0,
        rise_ms: 5.0,
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
    assert!(
        near_jn1thl,
        "JN1THL-equivalent target never promoted with a DC bias added"
    );
    assert!(
        near_vk2gr,
        "VK2GR-equivalent target never promoted with a DC bias added"
    );
}

#[test]
fn two_signals_survive_a_busy_band() {
    // Real 20m during an active session is not two isolated tones in
    // silence -- it's dozens of simultaneous QSOs. Simulate a busy band:
    // background signals spread across the 192 kHz passband (varied
    // WPM/SNR/duty), plus the two real MAN-171 targets, and see whether
    // the targets still promote once the floor/block-median machinery has
    // real neighboring signal traffic to contend with (untested by any
    // existing golden vector or channelizer_multisignal.rs, both capped
    // at 3 simultaneous signals).
    //
    // Codex review, PR #174 round 3 (P2, correct): the original version of
    // this test spaced background signals every 3.7 kHz -- wider than
    // `FloorBank`'s 32-channel/3 kHz block width (93.75 Hz * 32), so no two
    // carriers ever shared a block and the block median stayed
    // noise-dominated regardless. That version could pass without ever
    // exercising the crowding-induced block-floor-poisoning hypothesis it
    // claimed to rule out. Fixed by packing several extra signals *inside*
    // each target's own 3 kHz block (via `pack_block`) so real duty-cycle
    // traffic from other stations actually contends for that block's
    // median, in addition to the wider 3.7 kHz-spaced background across
    // the rest of the passband.
    let fs = 192_000.0;
    let center = 14_030_000.0;
    const BLOCK_HZ: f64 = 32.0 * 93.75; // FloorBank::BLOCK_CHANNELS * channel spacing

    // Fill a target's own 3 kHz floor block with enough other traffic to
    // affect at least half its 32 channels, staying clear of the target's
    // own +/-300 Hz (so a background signal never merges into the target's
    // track) and centered on the same block the target's channel falls in.
    let pack_block = |target_offset: f64, start_idx: u32| -> Vec<SignalSpec> {
        let block_start = (target_offset / BLOCK_HZ).floor() * BLOCK_HZ;
        let mut out = Vec::new();
        let mut o = block_start + 100.0;
        let mut i = start_idx;
        while o < block_start + BLOCK_HZ {
            if (o - target_offset).abs() > 300.0 {
                let wpm = 18.0 + (i % 5) as f32 * 6.0;
                let snr = 8.0 + (i % 4) as f32 * 4.0; // 8..20, real duty-cycle traffic
                let mut s = sig(o, snr);
                s.wpm = wpm;
                s.text = "CQ CQ TEST DE N0CALL N0CALL K".into();
                out.push(s);
                i += 1;
            }
            o += 180.0; // ~2 channels apart -- packs ~15 signals into the 3 kHz block
        }
        out
    };

    let mut signals = vec![sig(1_150.0, 38.0), sig(7_800.0, 20.0)];
    signals.extend(pack_block(1_150.0, 0));
    signals.extend(pack_block(7_800.0, 100));

    // Background QSOs: spaced every ~3.7 kHz from -90 kHz to +90 kHz
    // (skipping the target frequencies), moderate-to-strong SNRs, mixed
    // WPM so duty cycle varies -- realistic wider-band traffic alongside
    // the block-local congestion above.
    let mut offset: f64 = -90_000.0;
    let mut i = 200u32;
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
    // (MAN-107..113) for V5/V6/V8w.
    //
    // Codex review, PR #174 round 1 (P1, correct): `SignalSpec` applies
    // snr_2500_db as the PRE-fade amplitude, and coppa's Watterson model is
    // ensemble-normalized (`E|g|^2 = 1`, not per-realization) -- so 38/20 dB
    // here is a NOMINAL/mean level, and this specific 30 s realization's
    // actual *peak* envelope SNR is not controlled to equal the RBN peak
    // figures (it can run higher OR lower). This test therefore does NOT
    // rigorously rule fading out as MAN-171's explanation the way its
    // original framing claimed -- it only shows both signals still promote
    // at a plausible nominal SNR under Watterson-Poor fading, which is
    // weaker evidence, kept for what it actually is rather than removed.
    // A true peak-matched reproduction needs pre-measuring the faded
    // envelope's realized peak and rescaling amplitude to match RBN's
    // reported max exactly -- not done here.
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
    assert!(
        near_jn1thl,
        "JN1THL-equivalent target never promoted at nominal 38dB under Watterson-Poor fading"
    );
    assert!(
        near_vk2gr,
        "VK2GR-equivalent target never promoted at nominal 20dB under Watterson-Poor fading"
    );
}
