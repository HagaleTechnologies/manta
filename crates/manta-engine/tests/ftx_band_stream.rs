//! MAN-323: the real FT8/FT4 decoder (pancetta-ft8) behind the IQ tap, on
//! synthesised band streams. Needs the `ftx` feature:
//! `cargo test -p manta-engine --features ftx --test ftx_band_stream`.
#![cfg(feature = "ftx")]

use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use manta_engine::ftx::tap::{
    attach, pancetta_factory, CycleOutcome, FtxSink, Placement, SlotResult, TapSpec,
};
use manta_engine::ftx::{new_decoder, FtxMode, FtxWindow};
use manta_input::IqSource;
use manta_testkit::ftx::{
    add_awgn, gfsk_iq, tones, Protocol, CQ_K1ABC_FN42_FT8, CQ_W1AW_FN31_FT4, CQ_W1AW_FN31_FT8,
};
use manta_testkit::noise::amplitude_for_snr_2500;
use num_complex::Complex32;

/// 2024-11-01T12:00:00Z: on a 15 s boundary.
const BOUNDARY_MS: u64 = 1_730_462_400_000;
/// The stream starts this long before the boundary.
const LEAD_S: f64 = 4.0;
const FT8_DIAL: u32 = 14_074_000;
const FT4_DIAL: u32 = 14_080_000;
const AUDIO_HZ: f64 = 1234.0;

struct MemSource {
    fs: f64,
    centre: f64,
    iq: Vec<Complex32>,
    pos: usize,
}

impl IqSource for MemSource {
    fn sample_rate(&self) -> f64 {
        self.fs
    }
    fn center_freq_hz(&self) -> f64 {
        self.centre
    }
    fn read(&mut self, buf: &mut [Complex32]) -> anyhow::Result<usize> {
        let n = buf.len().min(self.iq.len() - self.pos);
        buf[..n].copy_from_slice(&self.iq[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[derive(Default)]
struct Recorder(Mutex<Vec<SlotResult>>);

impl FtxSink for Recorder {
    fn slot(&self, result: SlotResult) {
        self.0.lock().unwrap().push(result);
    }
    fn input_ended(&self, _input: usize) {}
}

/// A transmission of `tones` with tone 0 at `rf_hz`, 0.5 s after the
/// boundary, at `snr_db` (2500 Hz) over unit noise; `slot_s` long after
/// the boundary plus a little for the filter delay.
fn stream(
    fs: f64,
    centre: f64,
    slot_s: f64,
    signals: &[(&str, f64, f32, Protocol)],
    seed: u64,
) -> Vec<Complex32> {
    let len = ((LEAD_S + slot_s + 0.1) * fs) as usize;
    let start = ((LEAD_S + 0.5) * fs) as usize;
    let mut iq = vec![Complex32::new(0.0, 0.0); len];
    for &(t, rf_hz, snr_db, protocol) in signals {
        let a = amplitude_for_snr_2500(snr_db, fs);
        let sig = gfsk_iq(&tones(t), fs, rf_hz - centre, start, len, protocol);
        for (s, x) in iq.iter_mut().zip(sig) {
            *s += x * a;
        }
    }
    // Unit-power noise (0 dB scaling of an all-zero signal adds just noise).
    let mut noise = vec![Complex32::new(0.0, 0.0); len];
    add_awgn(&mut noise, fs, 0.0, seed);
    for (s, n) in iq.iter_mut().zip(noise) {
        *s += n;
    }
    iq
}

fn run(fs: f64, centre: f64, windows: Vec<FtxWindow>, iq: Vec<Complex32>) -> Vec<SlotResult> {
    let sink = Arc::new(Recorder::default());
    let mut src = attach(
        Box::new(MemSource {
            fs,
            centre,
            iq,
            pos: 0,
        }),
        TapSpec {
            input: 0,
            epoch: UNIX_EPOCH + Duration::from_millis(BOUNDARY_MS)
                - Duration::from_secs_f64(LEAD_S),
            windows,
            placement: Placement::Replay,
            sink: sink.clone(),
            decoders: pancetta_factory(),
        },
    );
    let mut buf = vec![Complex32::new(0.0, 0.0); 8192];
    while src.read(&mut buf).unwrap() > 0 {}
    let results = sink.0.lock().unwrap().clone();
    results
}

fn decoded(results: &[SlotResult]) -> Vec<&SlotResult> {
    results
        .iter()
        .filter(|r| r.outcome == CycleOutcome::Decoded)
        .collect()
}

const FT8_20M: FtxWindow = FtxWindow {
    mode: FtxMode::Ft8,
    dial_hz: FT8_DIAL,
};
const FT4_20M: FtxWindow = FtxWindow {
    mode: FtxMode::Ft4,
    dial_hz: FT4_DIAL,
};

#[test]
fn the_testkit_cycle_decodes_at_12_khz() {
    let fs = 12_000.0;
    let iq = gfsk_iq(
        &tones(CQ_W1AW_FN31_FT8),
        fs,
        AUDIO_HZ,
        6_000,
        180_000,
        Protocol::Ft8,
    );
    let audio: Vec<f32> = iq.iter().map(|s| s.re).collect();
    let mut decoder = new_decoder(FtxMode::Ft8).expect("the ftx build has a decoder");
    let messages = decoder.decode(&audio).unwrap();
    let texts: Vec<&str> = messages.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts, vec!["CQ W1AW FN31"]);
    assert!(
        (messages[0].audio_hz - AUDIO_HZ).abs() <= 3.125,
        "{}",
        messages[0].audio_hz
    );
}

#[test]
fn cq_w1aw_decodes_at_every_stream_rate() {
    let rf = f64::from(FT8_DIAL) + AUDIO_HZ;
    for (fs, centre) in [
        (48_000.0, 14_071_500.0),
        (96_000.0, 14_050_000.0),
        (96_000.0, 14_079_167.0),
        (192_000.0, 14_050_000.0),
        (384_000.0, 14_100_000.0),
    ] {
        let iq = stream(
            fs,
            centre,
            15.0,
            &[(CQ_W1AW_FN31_FT8, rf, -10.0, Protocol::Ft8)],
            11,
        );
        let results = run(fs, centre, vec![FT8_20M], iq);
        let d = decoded(&results);
        assert_eq!(d.len(), 1, "{fs} S/s at {centre}: {results:?}");
        assert_eq!(d[0].slot_start_ms, BOUNDARY_MS as i64);
        assert_eq!(d[0].slot_start_ms % 15_000, 0);
        let texts: Vec<&str> = d[0].messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, vec!["CQ W1AW FN31"], "{fs} S/s at {centre}");
        let m = &d[0].messages[0];
        assert!(
            (m.audio_hz - AUDIO_HZ).abs() <= 3.125,
            "{fs}: audio {} Hz",
            m.audio_hz
        );
        let rf_hz = f64::from(FT8_DIAL) + m.audio_hz;
        assert!(
            (14_075_230.0..=14_075_238.0).contains(&rf_hz),
            "{fs}: {rf_hz} Hz"
        );
        assert!((-13.0..=-7.0).contains(&m.snr_db), "{fs}: {} dB", m.snr_db);
    }
}

#[test]
fn a_strong_signal_below_the_dial_never_folds_in() {
    let fs = 96_000.0;
    let centre = 14_050_000.0;
    let iq = stream(
        fs,
        centre,
        15.0,
        &[
            (
                CQ_W1AW_FN31_FT8,
                f64::from(FT8_DIAL) + AUDIO_HZ,
                40.0,
                Protocol::Ft8,
            ),
            (
                CQ_K1ABC_FN42_FT8,
                f64::from(FT8_DIAL) - 1050.0,
                60.0,
                Protocol::Ft8,
            ),
        ],
        12,
    );
    let results = run(fs, centre, vec![FT8_20M], iq);
    let d = decoded(&results);
    assert_eq!(d.len(), 1);
    let texts: Vec<&str> = d[0].messages.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts, vec!["CQ W1AW FN31"]);
}

#[test]
fn ft4_cq_decodes_on_the_ft4_dial() {
    let fs = 96_000.0;
    let centre = 14_050_000.0;
    let iq = stream(
        fs,
        centre,
        7.5,
        &[(
            CQ_W1AW_FN31_FT4,
            f64::from(FT4_DIAL) + AUDIO_HZ,
            -5.0,
            Protocol::Ft4,
        )],
        13,
    );
    let results = run(fs, centre, vec![FT4_20M], iq);
    let d = decoded(&results);
    assert_eq!(d.len(), 1, "{results:?}");
    assert_eq!(d[0].window.mode, FtxMode::Ft4);
    assert_eq!(d[0].slot_start_ms % 7_500, 0);
    let texts: Vec<&str> = d[0].messages.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts, vec!["CQ W1AW FN31"]);
}

#[test]
fn replay_decodes_are_identical_across_runs() {
    let fs = 96_000.0;
    let centre = 14_050_000.0;
    let rf = f64::from(FT8_DIAL) + AUDIO_HZ;
    let once = || {
        let iq = stream(
            fs,
            centre,
            15.0,
            &[(CQ_W1AW_FN31_FT8, rf, -12.0, Protocol::Ft8)],
            14,
        );
        run(fs, centre, vec![FT8_20M], iq)
            .into_iter()
            .map(|r| {
                format!(
                    "{} {:?} {} {:?} {:?}",
                    r.input, r.window, r.slot_start_ms, r.outcome, r.messages
                )
            })
            .collect::<Vec<_>>()
    };
    let a = once();
    assert!(a.iter().any(|line| line.contains("CQ W1AW FN31")), "{a:?}");
    assert_eq!(a, once());
}
