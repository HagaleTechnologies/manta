//! `skimmer` CLI. M0 surface: decode a WAV fixture, generate golden vectors.
//! The daemon (SDR input, servers) arrives at M2/M3 (ROADMAP).

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use skimmer_engine::{decode_wav, PipelineConfig};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "skimmer", version, about = "Wideband multi-signal CW skimmer")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Decode a single CW signal from an IQ WAV file (M0 pipeline).
    Decode {
        /// Stereo IQ WAV (ch0 = I, ch1 = Q); center freq from <stem>.json sidecar.
        path: PathBuf,
        /// Emit the full DecodeReport as one JSON object on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Generate a golden test vector fixture set (SPEC §7).
    Gen {
        /// Vector name (M0: "v1").
        vector: String,
        /// Output directory for <name>.wav / .json / .manifest.json.
        #[arg(long)]
        out: PathBuf,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Decode { path, json } => {
            let report = decode_wav(&path, &PipelineConfig::default())?;
            if json {
                println!("{}", serde_json::to_string(&report)?);
            } else {
                println!("{}", report.text);
                eprintln!("freq_hz: {:.1}  wpm: {:?}", report.freq_hz, report.wpm);
            }
        }
        Command::Gen { vector, out } => {
            let spec = match vector.as_str() {
                "v1" => skimmer_testkit::vectors::v1(),
                other => bail!("unknown vector {other:?} (available: v1)"),
            };
            std::fs::create_dir_all(&out)?;
            let manifest = skimmer_testkit::vectors::write_fixture_set(&spec, &out)?;
            eprintln!(
                "wrote {}/{{{}.wav,{}.json,{}.manifest.json}} (expected freq {:.1} Hz)",
                out.display(),
                spec.name,
                spec.name,
                spec.name,
                manifest.expected_freq_hz
            );
        }
    }
    Ok(())
}
