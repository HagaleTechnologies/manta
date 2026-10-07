//! MAN-103 regression: `DecodeReport::wpm` must be within the SPEC's +/-2
//! WPM tolerance at every offset from channel centre, not just on-center.
//! Before the fix, error grew with distance from centre (the near-edge case,
//! V2, read up to 6.5 WPM low) -- see
//! docs/DECISIONS/2026-09-07-man103-keying-edge-placement.md.

use manta_engine::{decode_samples, PipelineConfig};
use manta_testkit::cer::cer;
use manta_testkit::scene::{render_scene, SignalSpec};

/// Channel spacing at manta's SPEC §1.1 default (fs=96000, N=1024 channels).
/// Not exported by manta-dsp; duplicated here as v7's golden vector does.
const CHANNEL_SPACING_HZ: f64 = 93.75;
/// An arbitrary channel far from DC/Nyquist, so every residual offset below
/// stays well inside the passband regardless of sign.
const BASE_CHANNEL: f64 = 96.0; // 9000.0 Hz
/// Per-cell character-error ceiling. Every cell of this grid is a clean
/// 20 dB AWGN scene, so the only errors expected are the warmup ones every
/// looped scene carries (SPEC §2.1's ~2.05 s detector floor loses the
/// leading repetitions). Measured worst cell is well under this; the gate
/// is sized to catch a *classification* collapse (CER climbing into the
/// tenths), not to police warmup noise.
const CER_MAX: f64 = 0.10;

/// Reported WPM and the decode's character error rate for one grid cell.
fn decode_at(wpm: f32, residual_ch: f64, snr_db: f32, seed: u64) -> (f64, f64) {
    let fs = 96_000.0;
    let duration_s = 60.0;
    let sig = SignalSpec {
        text: "CQ CQ DE W1AW W1AW K".into(),
        loop_text: true,
        wpm,
        offset_hz: BASE_CHANNEL * CHANNEL_SPACING_HZ + residual_ch * CHANNEL_SPACING_HZ,
        snr_2500_db: snr_db,
        jitter: None,
        qsb: None,
        watterson: None,
        char_wpm: None,
        weight: 3.0,
        char_gap_units: 3.0,
        word_gap_units: 7.0,
        rise_ms: 5.0,
    };
    let (iq, texts) = render_scene(std::slice::from_ref(&sig), fs, duration_s, Some(seed)).unwrap();
    let report = decode_samples(&iq, fs, 0.0, &PipelineConfig::default()).unwrap();
    let reported = report
        .wpm
        .unwrap_or_else(|| panic!("no SpeedUpdate reported at wpm={wpm} residual_ch={residual_ch}"))
        as f64;
    (reported, cer(&texts[0], &report.text))
}

/// MAN-103: "WPM is accurate across the channel, not just at channel
/// center." Residual offsets are in channel widths from the nearest channel
/// centre; 0.4667 is V2's own offset, effectively the channel edge.
///
/// The grid also asserts a **CER ceiling** at every cell, and runs up to
/// 40 WPM. Both were added by MAN-103's validation round: a WPM-only gate
/// that stopped at 35 WPM reported 39.83 WPM (a pass) on a 40 WPM scene
/// whose text had collapsed to CER 0.38 -- the reported speed can stay
/// right while gap classification goes wrong, so the gate has to watch
/// both, over the whole documented 10-40 WPM range.
#[test]
fn wpm_is_within_tolerance_at_every_offset() {
    let mut seed = 0x4D414E5F313033u64; // "MAN_103"
    for &wpm in &[20.0f32, 25.0, 35.0, 40.0] {
        for &residual_ch in &[0.0f64, 0.15, 0.30, 0.4667] {
            seed = seed.wrapping_add(1);
            let (reported, cer_val) = decode_at(wpm, residual_ch, 20.0, seed);
            println!("{wpm:>5} WPM  residual {residual_ch:<7} -> reported {reported:>6.2}  CER {cer_val:.4}");
            assert!(
                (reported - wpm as f64).abs() <= 2.0,
                "{wpm} WPM at residual {residual_ch} ch from channel centre reported {reported}"
            );
            assert!(
                cer_val <= CER_MAX,
                "{wpm} WPM at residual {residual_ch} ch from channel centre decoded at \
                 CER {cer_val:.4} (gate {CER_MAX})"
            );
        }
    }
}
