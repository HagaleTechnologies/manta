//! MAN-171 scratch tool: capture raw IQ via manta's own `SoapySdrIqSource`
//! (exercising manta's real capture code, not an ad-hoc external script)
//! and dump it to a WAV + JSON sidecar in exactly the format `WavIqSource`
//! expects, for offline `manta decode` reproduction. Not part of the crate
//! build; run with `cargo run -p manta-input --features soapy --example
//! iq_probe -- <center_hz> <fs> <gain_db> <duration_s> <out_stem>`.

use manta_input::{IqSource, SoapySdrIqSource};
use num_complex::Complex32;
use std::io::Write;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let center_hz: f64 = args[1].parse()?;
    let fs: f64 = args[2].parse()?;
    let gain_db: f64 = args[3].parse()?;
    let duration_s: f64 = args[4].parse()?;
    let out_stem = &args[5];

    let mut src = SoapySdrIqSource::open("driver=sdrplay", fs, center_hz, Some(gain_db))?;
    let actual_fs = src.sample_rate();
    let actual_center = src.center_freq_hz();
    eprintln!("opened: requested center={center_hz} fs={fs}; actual center={actual_center} fs={actual_fs}");

    let total_samples = (duration_s * actual_fs).round() as usize;
    let mut all: Vec<Complex32> = Vec::with_capacity(total_samples);
    let mut buf = vec![Complex32::new(0.0, 0.0); 16_384];
    let start = std::time::Instant::now();
    while all.len() < total_samples {
        let n = src.read(&mut buf)?;
        all.extend_from_slice(&buf[..n]);
        if all.len() % (actual_fs as usize) < buf.len() {
            eprintln!(
                "  {:.1}s captured, elapsed {:.1}s",
                all.len() as f64 / actual_fs,
                start.elapsed().as_secs_f64()
            );
        }
    }
    all.truncate(total_samples);

    let wav_path = format!("{out_stem}.wav");
    let json_path = format!("{out_stem}.json");
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: actual_fs as u32,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(&wav_path, spec)?;
    for s in &all {
        w.write_sample(s.re)?;
        w.write_sample(s.im)?;
    }
    w.finalize()?;

    let mut jf = std::fs::File::create(&json_path)?;
    write!(jf, "{{\"center_freq_hz\": {actual_center}}}")?;

    eprintln!(
        "wrote {} samples ({:.1}s) to {wav_path} (center={actual_center} Hz, fs={actual_fs} Hz)",
        all.len(),
        all.len() as f64 / actual_fs
    );
    Ok(())
}
