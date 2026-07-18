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
    /// Decode a live off-air CW signal continuously from real audio.
    Listen {
        /// Input device name substring (default input device if omitted).
        #[arg(long, conflicts_with = "source")]
        device: Option<String>,
        /// Replay a WAV file instead of a live device (paced by its own
        /// sample rate via AudioIqSource; used for demos and testing).
        #[arg(long, conflicts_with = "device")]
        source: Option<PathBuf>,
        /// Emit DecoderEvents as JSON Lines instead of plain text.
        #[arg(long)]
        json: bool,
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
        Command::Listen {
            device,
            source,
            json,
        } => {
            let src = match source {
                Some(path) => skimmer_input::AudioIqSource::from_wav_file(&path)?,
                None => skimmer_input::AudioIqSource::from_device(device.as_deref())?,
            };
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let stop_handler = stop.clone();
            ctrlc::set_handler(move || {
                stop_handler.store(true, std::sync::atomic::Ordering::Relaxed);
            })?;
            skimmer_engine::listen(src, &PipelineConfig::default(), stop, |ev| {
                if json {
                    println!("{}", serde_json::to_string(ev).unwrap());
                    return;
                }
                use skimmer_decode::events::DecoderEvent;
                use std::io::Write as _;
                match ev {
                    DecoderEvent::CharDecoded { glyph, .. } => {
                        if let Some(c) = glyph.text_char() {
                            print!("{c}");
                            let _ = std::io::stdout().flush();
                        }
                    }
                    DecoderEvent::WordBoundary { .. } => {
                        print!(" ");
                        let _ = std::io::stdout().flush();
                    }
                    _ => {}
                }
            })?;
        }
    }
    Ok(())
}
