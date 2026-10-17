//! MAN-114 regression: a spot's reported frequency must be within the
//! SPEC's ±10 Hz tolerance (SPEC §1.4, V1) at every offset inside a
//! channelizer channel, on every channel table, not just near channel
//! centre. Before the fix, the dB-domain parabola measured −38 Hz at 0.35
//! channels from centre (96 kHz, +6 dB) and −7 Hz bin-centred -- see
//! docs/DECISIONS/2026-10-17-man114-phase-difference-spot-frequency.md.

use manta_engine::{decode_samples, PipelineConfig};
use manta_testkit::scene::{render_scene, SignalSpec};

/// Channel spacing on every SPEC §1.1 table (fs/N = 93.75 Hz). Not
/// exported by manta-dsp to integration tests; duplicated here as
/// `wpm_across_channel.rs` does.
const CHANNEL_SPACING_HZ: f64 = 93.75;
/// The V1 scene's channel (12 340 Hz is channel 131.627 at 93.75 Hz), so
/// the grid sweeps the fractional position V1 and V31 sit at.
const BASE_CHANNEL: f64 = 131.0;
const CENTER_FREQ_HZ: f64 = 14_000_000.0;
/// SPEC §1.4 / §7 V1: the spot-frequency tolerance.
const FREQ_TOL_HZ: f64 = 10.0;

/// Reported minus true frequency, Hz, for V1 keying at `frac` channels
/// above `BASE_CHANNEL`'s centre.
fn freq_error_hz(fs: f64, frac: f64, snr_db: f32) -> f64 {
    let offset_hz = (BASE_CHANNEL + frac) * CHANNEL_SPACING_HZ;
    let sig = SignalSpec {
        dit_units: 1.0,
        element_gap_units: 1.0,
        text: "CQ CQ DE W1AW W1AW K".into(),
        loop_text: true,
        wpm: 20.0,
        offset_hz,
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
    let (iq, _) = render_scene(std::slice::from_ref(&sig), fs, 20.0, Some(1)).unwrap();
    let report = decode_samples(&iq, fs, CENTER_FREQ_HZ, &PipelineConfig::default())
        .unwrap_or_else(|e| panic!("no decode at fs {fs} frac {frac} snr {snr_db}: {e}"));
    report.freq_hz - (CENTER_FREQ_HZ + offset_hz)
}

fn assert_within_spec(fs: f64, frac: f64, snr_db: f32) {
    let err = freq_error_hz(fs, frac, snr_db);
    println!("fs {fs:>6} snr {snr_db:>4} dB frac {frac:.2} -> error {err:+.2} Hz");
    assert!(
        err.abs() <= FREQ_TOL_HZ,
        "fs {fs} Hz, SNR {snr_db} dB, {frac:.2} channels above centre: reported frequency \
         off by {err:+.2} Hz (SPEC §1.4 tolerance ±{FREQ_TOL_HZ} Hz)"
    );
}

/// The ticket's scenario on the default table: every tenth of a channel
/// across one channel, at a strong and a weak (+6 dB, SPEC §1.4's gate)
/// signal.
#[test]
fn freq_error_within_spec_across_the_channel_96k() {
    for snr_db in [6.0f32, 20.0] {
        for step in 0..=10 {
            assert_within_spec(96_000.0, 0.1 * step as f64, snr_db);
        }
    }
}

/// MAN-187 (absorbed): the other two tables, at the offsets where the old
/// estimator was worst (0.35) and where the phase estimator is (0.45-0.55).
#[test]
fn freq_error_within_spec_on_every_channel_table() {
    for fs in [48_000.0, 192_000.0] {
        for frac in [0.0, 0.35, 0.45, 0.5, 0.55, 0.65] {
            assert_within_spec(fs, frac, 6.0);
        }
    }
}

/// Reproduces the decision record's per-table characterisation: every
/// table × {+6, +20} dB × frac 0.00..=1.00 step 0.05, printed as a Markdown
/// table. Asserts nothing; run with
/// `cargo test -p manta-engine --test freq_across_channel --locked -- --ignored --nocapture characterize_freq_error_per_table`.
#[test]
#[ignore = "characterisation only (~4 minutes); asserts nothing"]
fn characterize_freq_error_per_table() {
    let fracs: Vec<f64> = (0..=20).map(|s| 0.05 * s as f64).collect();
    println!("| table | SNR (2500 Hz) | worst (at frac) | errors, Hz, frac 0.00..1.00 step 0.05 |");
    println!("|---|---|---|---|");
    for (fs, n) in [(48_000.0, 512), (96_000.0, 1024), (192_000.0, 2048)] {
        for snr_db in [20.0f32, 6.0] {
            let errs: Vec<f64> = fracs
                .iter()
                .map(|&f| freq_error_hz(fs, f, snr_db))
                .collect();
            let (worst_i, worst) = errs
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
                .unwrap();
            let all: Vec<String> = errs.iter().map(|e| format!("{e:+.2}")).collect();
            println!(
                "| {} kHz / N={n} | {snr_db:+} dB | {:.2} Hz ({:.2}) | {} |",
                fs / 1000.0,
                worst.abs(),
                fracs[worst_i],
                all.join(" ")
            );
        }
    }
}
