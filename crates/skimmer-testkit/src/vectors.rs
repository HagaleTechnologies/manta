//! Golden test vectors. SPEC §7: definitions live here
//! (module map §8: "§7 vectors -> skimmer-testkit::vectors").

use crate::scene::{render_scene, SignalSpec};
use crate::wav::write_fixture;
use anyhow::Result;
use num_complex::Complex32;
use std::path::Path;

/// A golden test vector's generation parameters. SPEC §7.
#[derive(Debug, Clone)]
pub struct VectorSpec {
    pub name: &'static str,
    pub fs: f64,
    pub duration_s: f64,
    pub center_freq_hz: f64,
    pub noise_seed: u64,
    pub signals: Vec<SignalSpec>,
}

/// SPEC §7 V1 "clean-20": 20 WPM, +20 dB, offset +12.34 kHz, W1AW,
/// AWGN only, no jitter. M0 = V1 passing end-to-end from a WAV file.
pub fn v1() -> VectorSpec {
    VectorSpec {
        name: "v1",
        fs: 96_000.0,
        duration_s: 120.0,
        center_freq_hz: 14_000_000.0,
        noise_seed: 0x534B_494D_5631, // "SKIMV1"
        signals: vec![SignalSpec {
            text: "CQ CQ DE W1AW W1AW K".into(),
            loop_text: true,
            wpm: 20.0,
            offset_hz: 12_340.0,
            snr_2500_db: 20.0,
            jitter: None,
        }],
    }
}

/// A rendered vector: samples, ground-truth keyed text per signal, expected
/// spot frequency. SPEC §7.
pub struct RenderedVector {
    pub samples: Vec<Complex32>,
    pub keyed_texts: Vec<String>,
    pub expected_freq_hz: f64,
}

/// Render a VectorSpec to samples + ground truth. SPEC §7.
pub fn render(spec: &VectorSpec) -> Result<RenderedVector> {
    let (samples, keyed_texts) = render_scene(
        &spec.signals,
        spec.fs,
        spec.duration_s,
        Some(spec.noise_seed),
    )?;
    Ok(RenderedVector {
        samples,
        keyed_texts,
        expected_freq_hz: spec.center_freq_hz + spec.signals[0].offset_hz,
    })
}

/// Sidecar recording a rendered fixture's generation parameters and ground
/// truth, for test assertions. Pinned decision 15.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Manifest {
    pub name: String,
    pub fs: f64,
    pub duration_s: f64,
    pub center_freq_hz: f64,
    pub noise_seed: u64,
    pub expected_freq_hz: f64,
    pub keyed_texts: Vec<String>,
    pub generator: String,
}

/// Write `<name>.wav`, `<name>.json`, `<name>.manifest.json` into `dir`.
/// Pinned decision 15.
pub fn write_fixture_set(spec: &VectorSpec, dir: &Path) -> Result<Manifest> {
    let rendered = render(spec)?;
    write_fixture(
        dir,
        spec.name,
        &rendered.samples,
        spec.fs,
        spec.center_freq_hz,
    )?;
    let manifest = Manifest {
        name: spec.name.to_string(),
        fs: spec.fs,
        duration_s: spec.duration_s,
        center_freq_hz: spec.center_freq_hz,
        noise_seed: spec.noise_seed,
        expected_freq_hz: rendered.expected_freq_hz,
        keyed_texts: rendered.keyed_texts,
        generator: concat!("skimmer-testkit ", env!("CARGO_PKG_VERSION")).to_string(),
    };
    std::fs::write(
        dir.join(format!("{}.manifest.json", spec.name)),
        serde_json::to_string_pretty(&manifest)?,
    )?;
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use skimmer_input::{IqSource, WavIqSource};

    #[test]
    fn v1_spec_matches_spec_table() {
        let v = v1();
        assert_eq!(v.fs, 96_000.0);
        assert_eq!(v.duration_s, 120.0);
        let s = &v.signals[0];
        assert_eq!(s.wpm, 20.0);
        assert_eq!(s.offset_hz, 12_340.0);
        assert_eq!(s.snr_2500_db, 20.0);
        assert!(s.jitter.is_none()); // V1: no jitter
        assert_eq!(s.text, "CQ CQ DE W1AW W1AW K");
    }

    #[test]
    fn fixture_roundtrips_through_wav() {
        // Short variant so the test stays fast; same code path as V1.
        let spec = VectorSpec {
            duration_s: 3.0,
            ..v1()
        };
        let rendered = render(&spec).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let manifest = write_fixture_set(&spec, dir.path()).unwrap();
        assert_eq!(manifest.expected_freq_hz, 14_012_340.0);

        let mut src = WavIqSource::open(&dir.path().join("v1.wav")).unwrap();
        assert_eq!(src.sample_rate(), 96_000.0);
        assert_eq!(src.center_freq_hz(), 14_000_000.0);
        let back = skimmer_input::read_all(&mut src).unwrap();
        assert_eq!(back, rendered.samples); // float32 WAV is lossless
    }
}
