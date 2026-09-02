//! `manta` CLI. M0 surface: decode a WAV fixture, generate golden vectors.
//! The daemon (SDR input, servers) arrives at M2/M3 (ROADMAP).

use anyhow::{anyhow, bail, Context, Result};
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
        /// Operator bad-callsign blocklist file, one callsign per line (MAN-31).
        #[arg(long)]
        blocklist: Option<PathBuf>,
        /// Operator notched-frequency-range list file, one `low_hz-high_hz`
        /// range per line (MAN-31).
        #[arg(long)]
        notch: Option<PathBuf>,
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
        /// Operator bad-callsign blocklist file, one callsign per line (MAN-31).
        #[arg(long)]
        blocklist: Option<PathBuf>,
        /// Operator notched-frequency-range list file, one `low_hz-high_hz`
        /// range per line (MAN-31).
        #[arg(long)]
        notch: Option<PathBuf>,
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
        /// TOML config with a `[server]`-shaped `ServerConfig` (station
        /// callsign + ports). When given, also starts the telnet cluster
        /// server, JSON Lines/WebSocket stream, and metrics endpoint
        /// (ARCHITECTURE §7-§8) alongside the decode loop.
        #[arg(long)]
        server_config: Option<PathBuf>,
        /// RF dial frequency in Hz, overriding the source's own
        /// `center_freq_hz()`. Required with --server-config when the
        /// source is a plain audio device or --source WAV file, since
        /// neither reports a real RF frequency (KiwiSDR/SoapySDR already
        /// know theirs from --kiwi-freq/--soapy-freq) -- without it, spots
        /// would publish an audio-tone offset (e.g. 700 Hz) as if it were
        /// the actual DX frequency.
        #[arg(long)]
        dial_freq_hz: Option<f64>,
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
        /// Operator bad-callsign blocklist file, one callsign per line (MAN-31).
        #[arg(long)]
        blocklist: Option<PathBuf>,
        /// Operator notched-frequency-range list file, one `low_hz-high_hz`
        /// range per line (MAN-31).
        #[arg(long)]
        notch: Option<PathBuf>,
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

/// Overrides an inner source's `center_freq_hz()` with a fixed value --
/// `AudioIqSource` always reports `0.0` (audio-passband mode has no real
/// RF dial frequency of its own), so without this a spot's `freq_hz` would
/// publish a bare audio-tone offset (e.g. 700 Hz) as if it were the actual
/// DX frequency. See `--dial-freq-hz`.
struct FixedCenterFreqSource {
    inner: Box<dyn IqSource>,
    freq_hz: f64,
}

impl IqSource for FixedCenterFreqSource {
    fn sample_rate(&self) -> f64 {
        self.inner.sample_rate()
    }

    fn center_freq_hz(&self) -> f64 {
        self.freq_hz
    }

    fn read(&mut self, buf: &mut [num_complex::Complex32]) -> Result<usize> {
        self.inner.read(buf)
    }
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

/// Strips a leading UTF-8 BOM (`\u{feff}`), common in Windows-authored text
/// files -- `str::trim` does not remove it, so left unstripped it corrupts
/// the first line's parse (a blocklist callsign that never matches, or a
/// notch range silently rejected).
fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

/// Builds a `PipelineConfig` from the CLI's shared flags: the MAN-29
/// frequency-calibration correction plus the MAN-31 operator suppression
/// lists. Either suppression flag is optional; an absent one leaves that
/// list empty (no suppression), matching `PipelineConfig`'s own defaults.
fn build_pipeline_config(
    freq_correction_ppm: f64,
    blocklist: Option<PathBuf>,
    notch: Option<PathBuf>,
) -> Result<PipelineConfig> {
    let mut cfg = PipelineConfig {
        freq_correction_ppm,
        ..Default::default()
    };
    if let Some(path) = blocklist {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading blocklist file {}", path.display()))?;
        cfg.blocklist = manta_engine::Blocklist::parse(strip_bom(&text));
    }
    if let Some(path) = notch {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading notch file {}", path.display()))?;
        cfg.notch = manta_engine::NotchList::parse(strip_bom(&text));
    }
    Ok(cfg)
}

/// Handles the `Listen` on-spot closure needs to feed a running spot server.
struct SpotServer {
    bus: std::sync::Arc<manta_server::bus::SpotBus>,
    metrics: std::sync::Arc<manta_server::metrics::Metrics>,
}

/// Starts the telnet/JSON-Lines-and-WebSocket/metrics servers on their own
/// tokio runtime (ARCHITECTURE §7-§8). The returned `Runtime` must be kept
/// alive for the servers to keep running -- dropping it stops them; call
/// `Runtime::shutdown_timeout` on it before exit to let in-flight writes
/// (e.g. spots from `TrackManager::finish()`) drain instead of vanishing.
///
/// `epoch` is the bus's session epoch (see `SpotBus::new`) -- pass a fixed
/// value (not `SystemTime::now()`) when replaying a file, or two runs of
/// the same fixture emit different JSON `timestamp`s and spot `id`s,
/// breaking this repo's file-input-is-deterministic requirement.
fn start_spot_server(
    config_path: &std::path::Path,
    sample_rate_hz: f64,
    epoch: std::time::SystemTime,
) -> Result<(tokio::runtime::Runtime, SpotServer)> {
    let cfg_text = std::fs::read_to_string(config_path)?;
    let file: manta_server::config::DaemonConfigFile = toml::from_str(&cfg_text)?;
    let cfg = file.server;

    let bus = std::sync::Arc::new(manta_server::bus::SpotBus::new(sample_rate_hz, epoch));
    let metrics = std::sync::Arc::new(manta_server::metrics::Metrics::new());
    let cty = std::sync::Arc::new(manta_spot::cty::Table::parse(manta_spot::CTY_DAT));
    let decoder_version = format!("manta-{}", env!("CARGO_PKG_VERSION"));

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let telnet_listener =
            tokio::net::TcpListener::bind((cfg.bind_addr.as_str(), cfg.telnet_port)).await?;
        let json_listener =
            tokio::net::TcpListener::bind((cfg.bind_addr.as_str(), cfg.json_port)).await?;
        let metrics_listener =
            tokio::net::TcpListener::bind((cfg.bind_addr.as_str(), cfg.metrics_port)).await?;

        tokio::spawn(manta_server::telnet::serve(
            telnet_listener,
            bus.clone(),
            metrics.clone(),
            cfg.station_callsign.clone(),
        ));
        tokio::spawn(manta_server::json_stream::serve(
            json_listener,
            bus.clone(),
            metrics.clone(),
            cty,
            cfg.station_callsign.clone(),
            decoder_version,
        ));
        tokio::spawn(manta_server::metrics_http::serve(
            metrics_listener,
            metrics.clone(),
        ));

        anyhow::Ok(())
    })?;

    Ok((rt, SpotServer { bus, metrics }))
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Decode {
            path,
            json,
            freq_correction_ppm,
            blocklist,
            notch,
        } => {
            let cfg = build_pipeline_config(freq_correction_ppm, blocklist, notch)?;
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
            blocklist,
            notch,
            #[cfg(feature = "soapy")]
            soapy_driver,
            #[cfg(feature = "soapy")]
            soapy_freq,
            #[cfg(feature = "soapy")]
            soapy_rate,
            #[cfg(feature = "soapy")]
            soapy_gain,
            server_config,
            dial_freq_hz,
        } => {
            let is_file_replay = source.is_some();
            #[cfg(feature = "soapy")]
            let has_soapy_source = soapy_driver.is_some();
            #[cfg(not(feature = "soapy"))]
            let has_soapy_source = false;
            let has_rf_aware_source = kiwi_host.is_some() || has_soapy_source;
            let source_name = if kiwi_host.is_some() {
                "kiwi"
            } else if has_soapy_source {
                "soapy"
            } else if is_file_replay {
                "file"
            } else {
                "audio"
            };

            if server_config.is_some() && !has_rf_aware_source && dial_freq_hz.is_none() {
                bail!(
                    "--dial-freq-hz is required with --server-config when using a plain \
                     audio device or --source WAV file -- neither reports a real RF \
                     frequency (KiwiSDR/SoapySDR already know theirs from \
                     --kiwi-freq/--soapy-freq)"
                );
            }

            let kiwi = KiwiOpts {
                host: kiwi_host,
                port: kiwi_port,
                freq: kiwi_freq,
                password: kiwi_password,
            };
            let cfg = build_pipeline_config(freq_correction_ppm, blocklist, notch)?;
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
            let src: Box<dyn IqSource> = match dial_freq_hz {
                Some(freq_hz) => Box::new(FixedCenterFreqSource {
                    inner: src,
                    freq_hz,
                }),
                None => src,
            };

            // A fixed epoch for file replay keeps JSON timestamps/ids
            // byte-identical across runs (this repo's determinism
            // requirement); a live source gets a real wall-clock epoch.
            let epoch = if is_file_replay {
                std::time::SystemTime::UNIX_EPOCH
            } else {
                std::time::SystemTime::now()
            };

            // Kept alive for the process lifetime: dropping it would stop
            // the spawned server tasks. `None` when --server-config wasn't
            // given, in which case `spot_server` stays None too.
            let (server_runtime, spot_server) = match server_config {
                Some(path) => {
                    let (rt, server) = start_spot_server(&path, src.sample_rate(), epoch)?;
                    // Real, if coarse, health signal: this source opened
                    // and is running. `active_tracks` has no equivalent
                    // hook yet -- manta-engine exposes no live track-count
                    // API for `listen()`'s callbacks to read, so it stays
                    // at Metrics::default()'s 0 until that surface exists.
                    server.metrics.set_source_health(source_name, true);
                    (Some(rt), Some(server))
                }
                None => (None, None),
            };

            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let stop_handler = stop.clone();
            ctrlc::set_handler(move || {
                stop_handler.store(true, std::sync::atomic::Ordering::Relaxed);
            })?;
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
                // Provisional CLI-debugging text/JSON printed below is NOT
                // the ecosystem wire contract -- that's `spot_server`
                // (manta-server's telnet/JSON-Lines/WebSocket fan-out,
                // ARCHITECTURE §7), fed here when --server-config is set.
                |spot| {
                    if let Some(server) = &spot_server {
                        server.bus.publish(spot.clone());
                        server.metrics.record_spot();
                    }
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

            // Let in-flight writes (e.g. spots from TrackManager::finish()
            // just before `listen` returned) drain instead of vanishing
            // when the runtime and its spawned client tasks are dropped.
            if let Some(rt) = server_runtime {
                rt.shutdown_timeout(std::time::Duration::from_secs(2));
            }
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
            blocklist,
            notch,
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
            let cfg = build_pipeline_config(freq_correction_ppm, blocklist, notch)?;
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
            let report = manta_engine::soak(src, &cfg, std::time::Duration::from_secs(duration))?;
            eprintln!("{report:?}");
            if !manta_engine::soak_passed(&report) {
                std::process::exit(1);
            }
        }
    }
    Ok(())
}
