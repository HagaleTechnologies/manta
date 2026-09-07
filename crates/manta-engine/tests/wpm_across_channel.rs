//! MAN-103: "WPM is accurate across the channel, not just at channel
//! center." Reported WPM used to read systematically low, worse the closer
//! a signal sat to a channel edge (root cause: the `sqrt(E_hi*E_lo)` keying
//! threshold, `manta_decode::envelope::Demod` -- see
//! docs/DECISIONS/2026-09-07-man103-keying-edge-placement.md). This sweeps
//! residual offset from the nearest channel center, independent of any one
//! golden vector's fixed offset.

use manta_engine::{decode_samples, PipelineConfig};
use manta_testkit::scene::{render_scene, SignalSpec};

/// SPEC §1.1: channel spacing at fs=96 kHz / N=1024 channels.
const CHANNEL_SPACING_HZ: f64 = 93.75;

fn decode_at(wpm: f32, residual_ch: f64, snr_db: f32) -> f64 {
    let fs = 96_000.0;
    let sig = SignalSpec {
        text: "CQ CQ DE W1AW W1AW K".into(),
        loop_text: true,
        wpm,
        offset_hz: residual_ch * CHANNEL_SPACING_HZ,
        snr_2500_db: snr_db,
        jitter: None,
        qsb: None,
        watterson: None,
        char_wpm: None,
    };
    // Fixed seed: this is a bias/regression gate, not a noise-robustness
    // sweep -- a single deterministic realization per cell is enough to
    // pin the systematic (offset/SNR-dependent) error this ticket targets.
    let (iq, _texts) =
        render_scene(std::slice::from_ref(&sig), fs, 60.0, Some(0x4D41_4E31_3033)).unwrap();
    let report = decode_samples(&iq, fs, 0.0, &PipelineConfig::default()).unwrap();
    report
        .wpm
        .unwrap_or_else(|| panic!("no WPM reported at wpm={wpm} residual_ch={residual_ch}"))
        as f64
}

/// MAN-103: residual offsets in channel widths from the nearest channel
/// center; 0.4667 is V2's own offset, effectively the channel edge.
#[test]
fn wpm_is_within_tolerance_at_every_offset() {
    for &wpm in &[20.0f32, 25.0, 35.0] {
        for &residual_ch in &[0.0f64, 0.15, 0.30, 0.4667] {
            let reported = decode_at(wpm, residual_ch, 20.0);
            assert!(
                (reported - wpm as f64).abs() <= 2.0,
                "{wpm} WPM at residual {residual_ch} ch reported {reported}"
            );
        }
    }
}
