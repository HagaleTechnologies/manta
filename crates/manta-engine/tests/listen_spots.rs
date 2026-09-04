//! `listen()`'s new `on_spot` callback: a real `manta-spot::Validator`
//! run over the streamed event sequence. Uses a raw-complex-IQ in-memory
//! source (not `AudioIqSource`) for isolation and speed -- this test
//! doesn't need real audio-hardware semantics to exercise `on_spot`. The
//! near-DC Hilbert leakage this file's sources used to dodge (issue #21)
//! is fixed as of MAN-4; see `listen_audio.rs`'s doc comment.

use manta_engine::{listen, PipelineConfig, Spot};
use manta_testkit::scene::{render_scene, SignalSpec};
use num_complex::Complex32;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

struct FixedFreqSource {
    samples: Vec<Complex32>,
    cursor: usize,
    fs: f64,
    center_freq_hz: f64,
}

impl manta_input::IqSource for FixedFreqSource {
    fn sample_rate(&self) -> f64 {
        self.fs
    }
    fn center_freq_hz(&self) -> f64 {
        self.center_freq_hz
    }
    fn read(&mut self, buf: &mut [Complex32]) -> anyhow::Result<usize> {
        let n = buf.len().min(self.samples.len() - self.cursor);
        buf[..n].copy_from_slice(&self.samples[self.cursor..self.cursor + n]);
        self.cursor += n;
        Ok(n)
    }
}

#[test]
fn listen_emits_a_spot_via_on_spot() {
    let sig = SignalSpec {
        text: "CQ CQ DE K5ARH K5ARH K".into(),
        loop_text: true,
        wpm: 20.0,
        offset_hz: 12_340.0,
        snr_2500_db: 20.0,
        jitter: None,
        qsb: None,
        watterson: None,
        char_wpm: None,
    };
    let (samples, _texts) =
        render_scene(std::slice::from_ref(&sig), 96_000.0, 30.0, Some(1)).unwrap();
    let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
        samples,
        cursor: 0,
        fs: 96_000.0,
        center_freq_hz: 14_000_000.0,
    });

    let stop = Arc::new(AtomicBool::new(false));
    let mut spots: Vec<Spot> = Vec::new();
    listen(
        src,
        &PipelineConfig::default(),
        stop,
        |_ev| {},
        |spot| spots.push(spot.clone()),
    )
    .unwrap();

    assert!(
        spots.iter().any(|s| s.callsign == "K5ARH"),
        "expected a K5ARH spot from on_spot, got: {spots:?}"
    );
}
