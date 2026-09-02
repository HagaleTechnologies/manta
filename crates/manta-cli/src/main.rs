//! `manta` CLI. M0 surface: decode a WAV fixture, generate golden vectors.
//! The daemon (SDR input, servers) arrives at M2/M3 (ROADMAP).

use anyhow::{anyhow, bail, Result};
use clap::{Parser, Subcommand};
use manta_engine::{decode_wav, PipelineConfig};
use manta_input::IqSource;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "manta",
    version,
    about = "Open-source wideband CW skimmer: every CW signal in an SDR passband, decoded at once, emitted as RBN-compatible spots"
)]
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
        /// Per-source frequency-calibration correction, in ppm (config key
        /// `input.freq_correction_ppm`, SPEC-decode-core.md §1.4; 0 = no
        /// correction). Applied to `freq_hz` and every spot's `freq_hz`.
        /// Corrects a drifted source clock/LO -- legacy precedent: CW
        /// Skimmer/SkimSrv's `FreqCalibration=` .ini key (a raw
        /// multiplier; this flag is ppm, per the spec's contract).
        #[arg(
            long,
            default_value_t = 0.0,
            value_parser = parse_freq_correction_ppm,
            allow_negative_numbers = true
        )]
        freq_correction_ppm: f64,
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
        /// KiwiSDR receiver hostname. Requires --kiwi-freq.
        #[arg(long, conflicts_with_all = ["device", "source"], requires = "kiwi_freq")]
        kiwi_host: Option<String>,
        /// KiwiSDR receiver port (default 8073, the standard KiwiSDR port).
        #[arg(long, default_value = "8073", requires = "kiwi_host")]
        kiwi_port: u16,
        /// RF center frequency in Hz. Required with --kiwi-host.
        #[arg(long, requires = "kiwi_host")]
        kiwi_freq: Option<f64>,
        /// KiwiSDR password (empty for anonymous/no-password receivers, the common case for public nodes).
        #[arg(long, requires = "kiwi_host", default_value = "")]
        kiwi_password: String,
        /// Emit DecoderEvents as JSON Lines instead of plain text.
        #[arg(long)]
        json: bool,
        /// Per-source frequency-calibration correction, in ppm (config key
        /// `input.freq_correction_ppm`, SPEC-decode-core.md §1.4; 0 = no
        /// correction). Applied to a spot's reported frequency before
        /// emission. Corrects a drifted source clock/LO -- legacy
        /// precedent: CW Skimmer/SkimSrv's `FreqCalibration=` .ini key
        /// (a raw multiplier; this flag is ppm, per the spec's contract).
        #[arg(
            long,
            default_value_t = 0.0,
            value_parser = parse_freq_correction_ppm,
            allow_negative_numbers = true
        )]
        freq_correction_ppm: f64,
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
        /// KiwiSDR receiver hostname. Requires --kiwi-freq.
        #[arg(long, conflicts_with_all = ["device", "source"], requires = "kiwi_freq")]
        kiwi_host: Option<String>,
        /// KiwiSDR receiver port (default 8073, the standard KiwiSDR port).
        #[arg(long, default_value = "8073", requires = "kiwi_host")]
        kiwi_port: u16,
        /// RF center frequency in Hz. Required with --kiwi-host.
        #[arg(long, requires = "kiwi_host")]
        kiwi_freq: Option<f64>,
        /// KiwiSDR password (empty for anonymous/no-password receivers, the common case for public nodes).
        #[arg(long, requires = "kiwi_host", default_value = "")]
        kiwi_password: String,
        /// Per-source frequency-calibration correction, in ppm (config key
        /// `input.freq_correction_ppm`, SPEC-decode-core.md §1.4; 0 = no
        /// correction). Applied to a spot's reported frequency before
        /// emission. Corrects a drifted source clock/LO -- legacy
        /// precedent: CW Skimmer/SkimSrv's `FreqCalibration=` .ini key
        /// (a raw multiplier; this flag is ppm, per the spec's contract).
        #[arg(
            long,
            default_value_t = 0.0,
            value_parser = parse_freq_correction_ppm,
            allow_negative_numbers = true
        )]
        freq_correction_ppm: f64,
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

/// KiwiSDR connection flags, grouped to keep `open_source`'s arity down.
struct KiwiOpts {
    host: Option<String>,
    port: u16,
    freq: Option<f64>,
    password: String,
}

/// SoapySDR connection flags (feature `soapy`), grouped for the same reason.
#[cfg(feature = "soapy")]
struct SoapyOpts {
    driver: Option<String>,
    freq: Option<f64>,
    rate: Option<f64>,
    gain: Option<f64>,
}

/// Open a live audio device, WAV replay, KiwiSDR network source, or
/// SoapySDR device (feature `soapy`) based on which CLI flags were set.
/// `kiwi.host` takes priority over `soapy.driver` (clap's
/// `conflicts_with_all` on each already rules out `device`/`source` being
/// set alongside either).
#[cfg(feature = "soapy")]
fn open_source(
    device: Option<String>,
    source: Option<PathBuf>,
    kiwi: KiwiOpts,
    soapy: SoapyOpts,
) -> Result<Box<dyn IqSource>> {
    if let Some(host) = kiwi.host {
        let freq = kiwi
            .freq
            .ok_or_else(|| anyhow!("--kiwi-freq is required with --kiwi-host"))?;
        return Ok(Box::new(manta_input::kiwi::KiwiIqSource::connect(
            &host,
            kiwi.port,
            freq,
            &kiwi.password,
        )?));
    }
    if let Some(driver) = soapy.driver {
        let freq = soapy
            .freq
            .ok_or_else(|| anyhow!("--soapy-freq is required with --soapy-driver"))?;
        let rate = soapy
            .rate
            .ok_or_else(|| anyhow!("--soapy-rate is required with --soapy-driver"))?;
        return Ok(Box::new(manta_input::soapy::SoapySdrIqSource::open(
            &driver, rate, freq, soapy.gain,
        )?));
    }
    open_audio_source(device, source)
}

#[cfg(not(feature = "soapy"))]
fn open_source(
    device: Option<String>,
    source: Option<PathBuf>,
    kiwi: KiwiOpts,
) -> Result<Box<dyn IqSource>> {
    if let Some(host) = kiwi.host {
        let freq = kiwi
            .freq
            .ok_or_else(|| anyhow!("--kiwi-freq is required with --kiwi-host"))?;
        return Ok(Box::new(manta_input::kiwi::KiwiIqSource::connect(
            &host,
            kiwi.port,
            freq,
            &kiwi.password,
        )?));
    }
    open_audio_source(device, source)
}

fn open_audio_source(device: Option<String>, source: Option<PathBuf>) -> Result<Box<dyn IqSource>> {
    Ok(match source {
        Some(path) => Box::new(manta_input::AudioIqSource::from_wav_file(&path)?),
        None => Box::new(manta_input::AudioIqSource::from_device(device.as_deref())?),
    })
}

/// Clap value parser for `--freq-correction-ppm`: fails at CLI-parse time
/// (before opening any source) rather than deep in the pipeline, using the
/// same validation `manta_spot::calibration_factor_from_ppm` applies
/// (MAN-29 review).
fn parse_freq_correction_ppm(s: &str) -> std::result::Result<f64, String> {
    let ppm: f64 = s
        .parse()
        .map_err(|e| format!("invalid --freq-correction-ppm {s:?}: {e}"))?;
    manta_spot::calibration_factor_from_ppm(ppm).map_err(|e| e.to_string())?;
    Ok(ppm)
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Decode {
            path,
            json,
            freq_correction_ppm,
        } => {
            let cfg = PipelineConfig {
                freq_correction_ppm,
                ..Default::default()
            };
            let report = decode_wav(&path, &cfg)?;
            if json {
                println!("{}", serde_json::to_string(&report)?);
            } else {
                println!("{}", report.text);
                eprintln!("freq_hz: {:.1}  wpm: {:?}", report.freq_hz, report.wpm);
                eprintln!("spots: {}", report.spots.len());
            }
        }
        Command::Gen { vector, out } => {
            let spec = match vector.as_str() {
                "v1" => manta_testkit::vectors::v1(),
                "v2" => manta_testkit::vectors::v2(),
                "v3" => manta_testkit::vectors::v3(),
                "v4" => manta_testkit::vectors::v4(),
                "v5" => manta_testkit::vectors::v5(),
                "v6" => manta_testkit::vectors::v6(),
                other => bail!("unknown vector {other:?} (available: v1-v6)"),
            };
            std::fs::create_dir_all(&out)?;
            let manifest = manta_testkit::vectors::write_fixture_set(&spec, &out)?;
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
            kiwi_host,
            kiwi_port,
            kiwi_freq,
            kiwi_password,
            json,
            freq_correction_ppm,
            #[cfg(feature = "soapy")]
            soapy_driver,
            #[cfg(feature = "soapy")]
            soapy_freq,
            #[cfg(feature = "soapy")]
            soapy_rate,
            #[cfg(feature = "soapy")]
            soapy_gain,
        } => {
            let kiwi = KiwiOpts {
                host: kiwi_host,
                port: kiwi_port,
                freq: kiwi_freq,
                password: kiwi_password,
            };
            #[cfg(feature = "soapy")]
            let src = open_source(
                device,
                source,
                kiwi,
                SoapyOpts {
                    driver: soapy_driver,
                    freq: soapy_freq,
                    rate: soapy_rate,
                    gain: soapy_gain,
                },
            )?;
            #[cfg(not(feature = "soapy"))]
            let src = open_source(device, source, kiwi)?;
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let stop_handler = stop.clone();
            ctrlc::set_handler(move || {
                stop_handler.store(true, std::sync::atomic::Ordering::Relaxed);
            })?;
            let cfg = PipelineConfig {
                freq_correction_ppm,
                ..Default::default()
            };
            manta_engine::listen(
                src,
                &cfg,
                stop,
                |ev| {
                    if json {
                        println!("{}", serde_json::to_string(ev).unwrap());
                        return;
                    }
                    use manta_decode::events::DecoderEvent;
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
                },
                // Provisional CLI-debugging spot output only -- NOT the
                // ecosystem JSON contract. manta-server (a later M3
                // sub-project) defines the real spot wire format.
                |spot| {
                    if json {
                        println!("{}", serde_json::json!({ "spot": spot }));
                        return;
                    }
                    eprintln!(
                        "SPOT: {} ({:?}) {:.1} Hz {:.0} dB {:.0} wpm conf={:.2}",
                        spot.callsign,
                        spot.spot_type,
                        spot.freq_hz,
                        spot.snr_db,
                        spot.wpm,
                        spot.confidence
                    );
                },
            )?;
        }
        Command::Soak {
            duration,
            device,
            source,
            kiwi_host,
            kiwi_port,
            kiwi_freq,
            kiwi_password,
            freq_correction_ppm,
            #[cfg(feature = "soapy")]
            soapy_driver,
            #[cfg(feature = "soapy")]
            soapy_freq,
            #[cfg(feature = "soapy")]
            soapy_rate,
            #[cfg(feature = "soapy")]
            soapy_gain,
        } => {
            let kiwi = KiwiOpts {
                host: kiwi_host,
                port: kiwi_port,
                freq: kiwi_freq,
                password: kiwi_password,
            };
            #[cfg(feature = "soapy")]
            let src = open_source(
                device,
                source,
                kiwi,
                SoapyOpts {
                    driver: soapy_driver,
                    freq: soapy_freq,
                    rate: soapy_rate,
                    gain: soapy_gain,
                },
            )?;
            #[cfg(not(feature = "soapy"))]
            let src = open_source(device, source, kiwi)?;
            let cfg = PipelineConfig {
                freq_correction_ppm,
                ..Default::default()
            };
            let report = manta_engine::soak(src, &cfg, std::time::Duration::from_secs(duration))?;
            eprintln!("{report:?}");
            if !manta_engine::soak_passed(&report) {
                std::process::exit(1);
            }
        }
    }
    Ok(())
}
