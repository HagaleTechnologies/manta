//! Fixture I/O: float32 stereo IQ WAV + JSON sidecar (pinned decision 15).

use anyhow::Result;
use num_complex::Complex32;
use std::path::{Path, PathBuf};

/// Write `<name>.wav` + `<name>.json` sidecar (pinned decision 15).
pub fn write_fixture(
    dir: &Path,
    name: &str,
    samples: &[Complex32],
    fs: f64,
    center_freq_hz: f64,
) -> Result<PathBuf> {
    let wav_path = dir.join(format!("{name}.wav"));
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: fs as u32,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(&wav_path, spec)?;
    for s in samples {
        w.write_sample(s.re)?;
        w.write_sample(s.im)?;
    }
    w.finalize()?;

    let sidecar = serde_json::json!({ "center_freq_hz": center_freq_hz });
    std::fs::write(
        dir.join(format!("{name}.json")),
        serde_json::to_string_pretty(&sidecar)?,
    )?;
    Ok(wav_path)
}
