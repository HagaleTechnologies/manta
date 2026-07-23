//! Proves the M2 channelizer design's core claim (design doc §4): the WOLA
//! filterbank actually separates multiple simultaneous signals into their
//! correct channels -- never exercised by V1-V6, which are all
//! single-signal scenes.

use num_complex::Complex32;
use skimmer_dsp::channelizer::Channelizer;
use skimmer_testkit::scene::{render_scene, SignalSpec};

fn channel_for_offset(offset_hz: f64, n: usize, fs: f64) -> usize {
    let delta = fs / n as f64;
    let k_signed = (offset_hz / delta).round() as i64;
    k_signed.rem_euclid(n as i64) as usize
}

fn signal(offset_hz: f64) -> SignalSpec {
    SignalSpec {
        text: "VVV VVV VVV".into(),
        loop_text: true,
        wpm: 20.0,
        offset_hz,
        snr_2500_db: 20.0,
        jitter: None,
        qsb: None,
        watterson: None,
        char_wpm: None,
    }
}

fn assert_channels_resolved(samples: &[Complex32], fs: f64, offsets: &[f64]) {
    let mut ch = Channelizer::new(fs, 0.0).unwrap();
    let hops = ch.process(samples);
    let warmup = ch.filter_len() / ch.hop() + 1;
    assert!(hops.len() > warmup, "not enough hops past warm-up");
    let steady = &hops[warmup..];

    let n = ch.n_channels();
    let mut avg_power = vec![0.0f64; n];
    for hop in steady {
        for (k, &p) in hop.power.iter().enumerate() {
            avg_power[k] += p as f64;
        }
    }
    for p in &mut avg_power {
        *p /= steady.len() as f64;
    }

    let mut sorted = avg_power.clone();
    sorted.sort_by(f64::total_cmp);
    let median = sorted[n / 2];

    for &off in offsets {
        let k = channel_for_offset(off, n, fs);
        assert!(
            avg_power[k] > median * 10.0,
            "channel {k} (offset {off} Hz) power {} not clearly above noise-floor proxy {median}",
            avg_power[k]
        );
    }
}

#[test]
fn resolves_three_simultaneous_signals_at_96khz() {
    let fs = 96_000.0;
    let offsets = [5_000.0, 10_000.0, -7_500.0];
    let signals: Vec<SignalSpec> = offsets.iter().map(|&o| signal(o)).collect();
    let (samples, _) = render_scene(&signals, fs, 5.0, Some(42)).unwrap();
    assert_channels_resolved(&samples, fs, &offsets);
}

#[test]
fn resolves_three_simultaneous_signals_at_192khz() {
    // Same scene, doubled sample rate (N=2048 instead of 1024) -- proves
    // the N = fs/93.75 generalization holds for a second table rate, not
    // just the one every existing fixture (V1-V6) happens to use.
    let fs = 192_000.0;
    let offsets = [15_000.0, -30_000.0, 40_000.0];
    let signals: Vec<SignalSpec> = offsets.iter().map(|&o| signal(o)).collect();
    let (samples, _) = render_scene(&signals, fs, 5.0, Some(43)).unwrap();
    assert_channels_resolved(&samples, fs, &offsets);
}

#[test]
fn resolves_signals_with_different_snrs() {
    // A strong and a weak signal together -- proves the weaker one isn't
    // swamped by leakage from the stronger one's stopband.
    let fs = 96_000.0;
    let mut strong = signal(8_000.0);
    strong.snr_2500_db = 25.0;
    let mut weak = signal(-12_000.0);
    weak.snr_2500_db = 8.0;
    let (samples, _) = render_scene(&[strong, weak], fs, 5.0, Some(44)).unwrap();
    assert_channels_resolved(&samples, fs, &[8_000.0, -12_000.0]);
}
