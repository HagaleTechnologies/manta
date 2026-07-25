//! `skimmer` CLI. M0 surface: decode a WAV fixture, generate golden vectors.
//! The daemon (SDR input, servers) arrives at M2/M3 (ROADMAP).

#[cfg(feature = "soapy")]
use anyhow::anyhow;
use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use skimmer_engine::{decode_wav, PipelineConfig};
use skimmer_input::IqSource;
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
        /// SoapySDR driver args (e.g. "driver=rtlsdr"), feature `soapy`.
        /// Requires --soapy-freq and --soapy-rate.
        #[cfg(feature = "soapy")]
        #[arg(long, conflicts_with_all = ["device", "source"])]
        soapy_driver: Option<String>,
        /// RF center frequency in Hz. Required with --soapy-driver.
        #[cfg(feature = "soapy")]
        #[arg(long, requires = "soapy_driver")]
        soapy_freq: Option<f64>,
        /// Sample rate in Hz. Required with --soapy-driver.
        #[cfg(feature = "soapy")]
        #[arg(long, requires = "soapy_driver")]
        soapy_rate: Option<f64>,
        /// Gain in dB (omit for AGC, if the device supports it).
        #[cfg(feature = "soapy")]
        #[arg(long, requires = "soapy_driver")]
        soapy_gain: Option<f64>,
    },
    /// Run the listen pipeline for a fixed duration, checking for panics
    /// and unbounded memory growth (ROADMAP M1 accept criterion).
    Soak {
        /// Duration in seconds.
        #[arg(long)]
        duration: u64,
        #[arg(long, conflicts_with = "source")]
        device: Option<String>,
        #[arg(long, conflicts_with = "device")]
        source: Option<PathBuf>,
        /// SoapySDR driver args (e.g. "driver=rtlsdr"), feature `soapy`.
        /// Requires --soapy-freq and --soapy-rate.
        #[cfg(feature = "soapy")]
        #[arg(long, conflicts_with_all = ["device", "source"])]
        soapy_driver: Option<String>,
        /// RF center frequency in Hz. Required with --soapy-driver.
        #[cfg(feature = "soapy")]
        #[arg(long, requires = "soapy_driver")]
        soapy_freq: Option<f64>,
        /// Sample rate in Hz. Required with --soapy-driver.
        #[cfg(feature = "soapy")]
        #[arg(long, requires = "soapy_driver")]
        soapy_rate: Option<f64>,
        /// Gain in dB (omit for AGC, if the device supports it).
        #[cfg(feature = "soapy")]
        #[arg(long, requires = "soapy_driver")]
        soapy_gain: Option<f64>,
    },
}

#[cfg(feature = "soapy")]
fn open_source(
    device: Option<String>,
    source: Option<PathBuf>,
    soapy_driver: Option<String>,
    soapy_freq: Option<f64>,
    soapy_rate: Option<f64>,
    soapy_gain: Option<f64>,
) -> Result<Box<dyn IqSource>> {
    if let Some(driver) = soapy_driver {
        let freq =
            soapy_freq.ok_or_else(|| anyhow!("--soapy-freq is required with --soapy-driver"))?;
        let rate =
            soapy_rate.ok_or_else(|| anyhow!("--soapy-rate is required with --soapy-driver"))?;
        return Ok(Box::new(skimmer_input::soapy::SoapySdrIqSource::open(
            &driver, rate, freq, soapy_gain,
        )?));
    }
    open_audio_source(device, source)
}

#[cfg(not(feature = "soapy"))]
fn open_source(device: Option<String>, source: Option<PathBuf>) -> Result<Box<dyn IqSource>> {
    open_audio_source(device, source)
}

fn open_audio_source(device: Option<String>, source: Option<PathBuf>) -> Result<Box<dyn IqSource>> {
    Ok(match source {
        Some(path) => Box::new(skimmer_input::AudioIqSource::from_wav_file(&path)?),
        None => Box::new(skimmer_input::AudioIqSource::from_device(
            device.as_deref(),
        )?),
    })
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
                "v2" => skimmer_testkit::vectors::v2(),
                "v3" => skimmer_testkit::vectors::v3(),
                "v4" => skimmer_testkit::vectors::v4(),
                "v5" => skimmer_testkit::vectors::v5(),
                "v6" => skimmer_testkit::vectors::v6(),
                other => bail!("unknown vector {other:?} (available: v1-v6)"),
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
            #[cfg(feature = "soapy")]
            soapy_driver,
            #[cfg(feature = "soapy")]
            soapy_freq,
            #[cfg(feature = "soapy")]
            soapy_rate,
            #[cfg(feature = "soapy")]
            soapy_gain,
        } => {
            #[cfg(feature = "soapy")]
            let src = open_source(
                device,
                source,
                soapy_driver,
                soapy_freq,
                soapy_rate,
                soapy_gain,
            )?;
            #[cfg(not(feature = "soapy"))]
            let src = open_source(device, source)?;
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
        Command::Soak {
            duration,
            device,
            source,
            #[cfg(feature = "soapy")]
            soapy_driver,
            #[cfg(feature = "soapy")]
            soapy_freq,
            #[cfg(feature = "soapy")]
            soapy_rate,
            #[cfg(feature = "soapy")]
            soapy_gain,
        } => {
            #[cfg(feature = "soapy")]
            let src = open_source(
                device,
                source,
                soapy_driver,
                soapy_freq,
                soapy_rate,
                soapy_gain,
            )?;
            #[cfg(not(feature = "soapy"))]
            let src = open_source(device, source)?;
            let report = skimmer_engine::soak(
                src,
                &PipelineConfig::default(),
                std::time::Duration::from_secs(duration),
            )?;
            eprintln!("{report:?}");
            if !skimmer_engine::soak_passed(&report) {
                std::process::exit(1);
            }
        }
    }
    Ok(())
}
