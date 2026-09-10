//! `manta` CLI: decode a WAV fixture, generate golden vectors, and run the
//! daemon (SDR input, telnet/JSON/metrics servers, RBN uplinks) via `run`.

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use manta_decode::decoder::Engine;
use manta_engine::{decode_wav, PipelineConfig};
use manta_input::IqSource;
use std::path::{Path, PathBuf};

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
        /// Operator Watch List (ARCHITECTURE §6, MAN-28): a callsign that
        /// bypasses grammar/cty validation and the repetition gate
        /// entirely -- legacy precedent: CW Skimmer's Watch List
        /// (Aggregator manual Appendix A2). Repeatable.
        #[arg(long)]
        allowlist: Vec<String>,
        /// Operator bad-callsign blocklist file, one callsign per line (MAN-31).
        #[arg(long)]
        blocklist: Option<PathBuf>,
        /// Operator notched-frequency-range list file, one `low_hz-high_hz`
        /// range per line (MAN-31).
        #[arg(long)]
        notch: Option<PathBuf>,
        /// TOML config with a `[decode]`-shaped table (SPEC v2 §7 keys). When
        /// given, its values are the baseline; an explicit --engine overrides
        /// just the `engine` key (merge_cli_engine). `--server-config` is a
        /// deprecated alias, matching `run`/`listen`'s D11/MAN-77 rename.
        #[arg(long, alias = "server-config")]
        config: Option<PathBuf>,
        /// Decode engine (SPEC v2 §0): `legacy` (default), `edge-legacy`, or
        /// `hsmm` (fully implemented and reviewed since Task 8; still
        /// experimental/unmeasured for production use -- SPEC v2 §8.4/Tasks
        /// 11-12 measure it). Unset (rather than defaulting to `legacy`) so
        /// an explicit flag can be told apart from an absent one: when
        /// --config's `[decode]` table also sets `engine`, this flag
        /// takes precedence over it when given, and the file's value is the
        /// baseline otherwise (SPEC v2 §7).
        #[arg(long, value_parser = parse_engine)]
        engine: Option<Engine>,
    },
    /// Real-signal decode oracle: decode each RBN-spotted station's channel
    /// directly (tracker bypassed) and report callsign recovery (SPEC v2 §8.3).
    Oracle {
        /// Stereo IQ WAV with <stem>.json sidecar.
        path: PathBuf,
        /// RBN daily-dump CSV pre-filtered to the recording's window.
        rbn_csv: PathBuf,
        /// Spotter whose spots define the reference set (the co-located skimmer).
        #[arg(long, default_value = "K5TR")]
        spotter: String,
        /// Recording's capture start, ISO-8601 UTC (e.g.
        /// 2025-11-29T00:00:00Z). Anchors RBN spot times to the actual
        /// recording, rather than assuming the capture starts exactly on
        /// the hour (Codex review, PR #161).
        #[arg(long, value_parser = parse_capture_start)]
        capture_start: i64,
        #[arg(long, default_value_t = 40.0, value_parser = parse_window_s)]
        window_s: f64,
        /// TOML config with a `[decode]`-shaped table (SPEC v2 §7 keys). When
        /// given, its values are the baseline; an explicit --engine overrides
        /// just the `engine` key (merge_cli_engine). `--server-config` is a
        /// deprecated alias, matching `run`/`listen`'s D11/MAN-77 rename.
        #[arg(long, alias = "server-config")]
        config: Option<PathBuf>,
        /// Decode engine (SPEC v2 §0): `legacy` (default), `edge-legacy`, or
        /// `hsmm` (fully implemented and reviewed since Task 8; still
        /// experimental/unmeasured for production use -- SPEC v2 §8.4/Tasks
        /// 11-12 measure it). Unset (rather than defaulting to `legacy`) so
        /// an explicit flag can be told apart from an absent one: when
        /// --config's `[decode]` table also sets `engine`, this flag
        /// takes precedence over it when given, and the file's value is the
        /// baseline otherwise (SPEC v2 §7).
        #[arg(long, value_parser = parse_engine)]
        engine: Option<Engine>,
        /// Write per-spot results as JSON Lines here (summary always goes to stdout).
        #[arg(long)]
        jsonl: Option<PathBuf>,
    },
    /// Generate a golden test vector fixture set (SPEC §7).
    Gen {
        /// Vector name (M0: "v1").
        vector: String,
        /// Output directory for <name>.wav / .json / .manifest.json.
        #[arg(long)]
        out: PathBuf,
    },
    /// Run manta as a daemon, or copy live off-air CW continuously.
    ///
    /// D11/MAN-77: `run` is the daemon entry point. `listen` is kept as a
    /// visible alias for ad hoc audio/dev testing (see
    /// docs/DECISIONS/2026-09-06-broad-review-decisions.md).
    #[command(visible_alias = "listen")]
    Run {
        /// Input device name substring (default input device if omitted).
        #[arg(long, conflicts_with = "source")]
        device: Option<String>,
        /// Replay a WAV file instead of a live device (paced by its own
        /// sample rate via AudioIqSource; used for demos and testing).
        #[arg(long, conflicts_with = "device")]
        source: Option<PathBuf>,
        /// KiwiSDR receiver hostname. Requires --kiwi-freq.
        #[cfg_attr(all(feature = "hpsdr", feature = "soapy"), arg(long, conflicts_with_all = ["device", "source", "hpsdr_host", "soapy_driver"], requires = "kiwi_freq"))]
        #[cfg_attr(all(feature = "hpsdr", not(feature = "soapy")), arg(long, conflicts_with_all = ["device", "source", "hpsdr_host"], requires = "kiwi_freq"))]
        #[cfg_attr(all(not(feature = "hpsdr"), feature = "soapy"), arg(long, conflicts_with_all = ["device", "source", "soapy_driver"], requires = "kiwi_freq"))]
        #[cfg_attr(not(any(feature = "hpsdr", feature = "soapy")), arg(long, conflicts_with_all = ["device", "source"], requires = "kiwi_freq"))]
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
        /// Operator Watch List (ARCHITECTURE §6, MAN-28): a callsign that
        /// bypasses grammar/cty validation and the repetition gate
        /// entirely -- legacy precedent: CW Skimmer's Watch List
        /// (Aggregator manual Appendix A2). Repeatable.
        #[arg(long)]
        allowlist: Vec<String>,
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
        #[cfg_attr(feature = "hpsdr", arg(long, conflicts_with_all = ["device", "source", "hpsdr_host", "kiwi_host"]))]
        #[cfg_attr(not(feature = "hpsdr"), arg(long, conflicts_with_all = ["device", "source", "kiwi_host"]))]
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
        /// HPSDR/Hermes (Metis) device hostname or IP, feature `hpsdr`.
        /// Requires --hpsdr-freq and --hpsdr-rate.
        #[cfg(feature = "hpsdr")]
        #[cfg_attr(feature = "soapy", arg(long, conflicts_with_all = ["device", "source", "kiwi_host", "soapy_driver"]))]
        #[cfg_attr(not(feature = "soapy"), arg(long, conflicts_with_all = ["device", "source", "kiwi_host"]))]
        hpsdr_host: Option<String>,
        /// HPSDR/Hermes control port (default 1024, the standard Metis
        /// discovery/control port).
        #[cfg(feature = "hpsdr")]
        #[arg(long, default_value_t = manta_input::hpsdr::CONTROL_PORT, requires = "hpsdr_host")]
        hpsdr_port: u16,
        /// RF center frequency in Hz. Required with --hpsdr-host.
        #[cfg(feature = "hpsdr")]
        #[arg(long, requires = "hpsdr_host", value_parser = parse_hpsdr_freq_hz)]
        hpsdr_freq: Option<f64>,
        /// Sample rate in Hz. Required with --hpsdr-host.
        #[cfg(feature = "hpsdr")]
        #[arg(long, requires = "hpsdr_host", value_parser = parse_hpsdr_rate_hz)]
        hpsdr_rate: Option<f64>,
        /// TOML config with a `[server]`-shaped `ServerConfig` (station
        /// callsign + ports). When given, also starts the telnet cluster
        /// server, JSON Lines/WebSocket stream, and metrics endpoint
        /// (ARCHITECTURE §7-§8) alongside the decode loop.
        #[arg(long, alias = "server-config")]
        config: Option<PathBuf>,
        /// RF dial frequency in Hz, overriding the source's own
        /// `center_freq_hz()`. Required with --config when the
        /// source is a plain audio device or --source WAV file, since
        /// neither reports a real RF frequency (KiwiSDR/SoapySDR already
        /// know theirs from --kiwi-freq/--soapy-freq) -- without it, spots
        /// would publish an audio-tone offset (e.g. 700 Hz) as if it were
        /// the actual DX frequency.
        #[arg(long, value_parser = parse_dial_freq_hz)]
        dial_freq_hz: Option<f64>,
        /// Fixed replay epoch, Unix seconds -- overrides the replayed
        /// file's own mtime as the wall-clock instant SpotBus treats as
        /// `sample_ts == 0`. Only meaningful with --source (file replay)
        /// and --config; ignored for a live source. Without this,
        /// the epoch is the file's mtime, which is real and reproducible
        /// for an untouched file but changes if the file is copied,
        /// downloaded, or restored without preserving filesystem metadata
        /// -- pass this explicitly when byte-identical JSON `timestamp`/
        /// RBN Zulu output across environments matters more than "whatever
        /// this machine's copy of the file happens to say."
        #[arg(long, value_parser = parse_replay_epoch)]
        replay_epoch: Option<i64>,
        /// Decode engine (SPEC v2 §0): `legacy` (default), `edge-legacy`, or
        /// `hsmm` (fully implemented and reviewed since Task 8; still
        /// experimental/unmeasured for production use -- SPEC v2 §8.4/Tasks
        /// 11-12 measure it). Unset (rather than defaulting to `legacy`) so
        /// an explicit flag can be told apart from an absent one: when
        /// `--config`'s `[decode]` table also sets `engine`, this
        /// flag takes precedence over it when given, and the file's value
        /// is the baseline otherwise (SPEC v2 §7).
        #[arg(long, value_parser = parse_engine)]
        engine: Option<Engine>,
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
        #[cfg_attr(all(feature = "hpsdr", feature = "soapy"), arg(long, conflicts_with_all = ["device", "source", "hpsdr_host", "soapy_driver"], requires = "kiwi_freq"))]
        #[cfg_attr(all(feature = "hpsdr", not(feature = "soapy")), arg(long, conflicts_with_all = ["device", "source", "hpsdr_host"], requires = "kiwi_freq"))]
        #[cfg_attr(all(not(feature = "hpsdr"), feature = "soapy"), arg(long, conflicts_with_all = ["device", "source", "soapy_driver"], requires = "kiwi_freq"))]
        #[cfg_attr(not(any(feature = "hpsdr", feature = "soapy")), arg(long, conflicts_with_all = ["device", "source"], requires = "kiwi_freq"))]
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
        /// Operator Watch List (ARCHITECTURE §6, MAN-28): a callsign that
        /// bypasses grammar/cty validation and the repetition gate
        /// entirely -- legacy precedent: CW Skimmer's Watch List
        /// (Aggregator manual Appendix A2). Repeatable.
        #[arg(long)]
        allowlist: Vec<String>,
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
        #[cfg_attr(feature = "hpsdr", arg(long, conflicts_with_all = ["device", "source", "hpsdr_host", "kiwi_host"]))]
        #[cfg_attr(not(feature = "hpsdr"), arg(long, conflicts_with_all = ["device", "source", "kiwi_host"]))]
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
        /// HPSDR/Hermes (Metis) device hostname or IP, feature `hpsdr`.
        /// Requires --hpsdr-freq and --hpsdr-rate.
        #[cfg(feature = "hpsdr")]
        #[cfg_attr(feature = "soapy", arg(long, conflicts_with_all = ["device", "source", "kiwi_host", "soapy_driver"]))]
        #[cfg_attr(not(feature = "soapy"), arg(long, conflicts_with_all = ["device", "source", "kiwi_host"]))]
        hpsdr_host: Option<String>,
        /// HPSDR/Hermes control port (default 1024, the standard Metis
        /// discovery/control port).
        #[cfg(feature = "hpsdr")]
        #[arg(long, default_value_t = manta_input::hpsdr::CONTROL_PORT, requires = "hpsdr_host")]
        hpsdr_port: u16,
        /// RF center frequency in Hz. Required with --hpsdr-host.
        #[cfg(feature = "hpsdr")]
        #[arg(long, requires = "hpsdr_host", value_parser = parse_hpsdr_freq_hz)]
        hpsdr_freq: Option<f64>,
        /// Sample rate in Hz. Required with --hpsdr-host.
        #[cfg(feature = "hpsdr")]
        #[arg(long, requires = "hpsdr_host", value_parser = parse_hpsdr_rate_hz)]
        hpsdr_rate: Option<f64>,
    },
    /// Query a running manta daemon's health (MAN-44). Reads the same
    /// `/status` document the metrics listener serves on `GET /status` --
    /// no separate control socket, so this works identically on every
    /// platform manta builds for.
    Status {
        /// Daemon config to read the metrics bind address/port from. A
        /// wildcard `bind_addr` (e.g. `0.0.0.0`) resolves to loopback for
        /// dialing purposes -- see `resolve_status_addr`.
        ///
        /// `--server-config` is a deprecated alias, exactly as on
        /// `run`/`listen`/`decode` (D11/MAN-77). It has to be spelled
        /// this way round: `warn_deprecations` scans raw argv without
        /// knowing the verb, so `manta status --server-config m.toml`
        /// prints "use `--config` instead" -- and before this alias
        /// existed, that advice named a flag clap then rejected
        /// (Codex review, PR #95).
        #[arg(long, alias = "server-config", conflicts_with = "addr")]
        config: Option<PathBuf>,
        /// Explicit `host:port` of the daemon's metrics listener (default
        /// 127.0.0.1:7302, the documented default metrics port).
        #[arg(long)]
        addr: Option<String>,
        /// Emit the raw status JSON instead of the human-readable summary.
        #[arg(long)]
        json: bool,
        /// Give up after this many seconds if the daemon doesn't respond.
        #[arg(long, default_value_t = 5)]
        timeout_secs: u64,
    },
    /// Bounded-duration health check: is this source hearing anything real?
    /// Runs the real decode pipeline for --duration, then reports track/SNR/
    /// spot stats and a verdict -- distinguishes "no signal" from "signal but
    /// not decoding" from "working end to end," which a bare `listen` run
    /// with zero spots can't tell apart on its own.
    Doctor {
        /// Duration in seconds (3-3600; see manta_engine::doctor::{MIN_DURATION,MAX_DURATION}).
        #[arg(long, default_value_t = 10)]
        duration: u64,
        #[arg(long, conflicts_with = "source")]
        device: Option<String>,
        #[arg(long, conflicts_with = "device")]
        source: Option<PathBuf>,
        /// KiwiSDR receiver hostname. Requires --kiwi-freq.
        #[cfg_attr(all(feature = "hpsdr", feature = "soapy"), arg(long, conflicts_with_all = ["device", "source", "hpsdr_host", "soapy_driver"], requires = "kiwi_freq"))]
        #[cfg_attr(all(feature = "hpsdr", not(feature = "soapy")), arg(long, conflicts_with_all = ["device", "source", "hpsdr_host"], requires = "kiwi_freq"))]
        #[cfg_attr(all(not(feature = "hpsdr"), feature = "soapy"), arg(long, conflicts_with_all = ["device", "source", "soapy_driver"], requires = "kiwi_freq"))]
        #[cfg_attr(not(any(feature = "hpsdr", feature = "soapy")), arg(long, conflicts_with_all = ["device", "source"], requires = "kiwi_freq"))]
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
        /// correction).
        #[arg(
            long,
            default_value_t = 0.0,
            value_parser = parse_freq_correction_ppm,
            allow_negative_numbers = true
        )]
        freq_correction_ppm: f64,
        /// Operator Watch List (ARCHITECTURE §6, MAN-28). Repeatable.
        #[arg(long)]
        allowlist: Vec<String>,
        /// Operator bad-callsign blocklist file, one callsign per line (MAN-31).
        #[arg(long)]
        blocklist: Option<PathBuf>,
        /// Operator notched-frequency-range list file, one `low_hz-high_hz`
        /// range per line (MAN-31).
        #[arg(long)]
        notch: Option<PathBuf>,
        /// SoapySDR driver args (e.g. "driver=sdrplay"), feature `soapy`.
        /// Requires --soapy-freq and --soapy-rate.
        #[cfg(feature = "soapy")]
        #[cfg_attr(feature = "hpsdr", arg(long, conflicts_with_all = ["device", "source", "hpsdr_host", "kiwi_host"]))]
        #[cfg_attr(not(feature = "hpsdr"), arg(long, conflicts_with_all = ["device", "source", "kiwi_host"]))]
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
        /// HPSDR/Hermes (Metis) device hostname or IP, feature `hpsdr`.
        /// Requires --hpsdr-freq and --hpsdr-rate.
        #[cfg(feature = "hpsdr")]
        #[cfg_attr(feature = "soapy", arg(long, conflicts_with_all = ["device", "source", "kiwi_host", "soapy_driver"]))]
        #[cfg_attr(not(feature = "soapy"), arg(long, conflicts_with_all = ["device", "source", "kiwi_host"]))]
        hpsdr_host: Option<String>,
        /// HPSDR/Hermes control port (default 1024, the standard Metis
        /// discovery/control port).
        #[cfg(feature = "hpsdr")]
        #[arg(long, default_value_t = manta_input::hpsdr::CONTROL_PORT, requires = "hpsdr_host")]
        hpsdr_port: u16,
        /// RF center frequency in Hz. Required with --hpsdr-host.
        #[cfg(feature = "hpsdr")]
        #[arg(long, requires = "hpsdr_host", value_parser = parse_hpsdr_freq_hz)]
        hpsdr_freq: Option<f64>,
        /// Sample rate in Hz. Required with --hpsdr-host.
        #[cfg(feature = "hpsdr")]
        #[arg(long, requires = "hpsdr_host", value_parser = parse_hpsdr_rate_hz)]
        hpsdr_rate: Option<f64>,
        /// Emit the DoctorReport as one JSON object on stdout instead of a
        /// human-readable summary.
        #[arg(long)]
        json: bool,
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

/// HPSDR/Hermes connection flags (feature `hpsdr`), grouped for the same reason.
#[cfg(feature = "hpsdr")]
struct HpsdrOpts {
    host: Option<String>,
    port: u16,
    freq: Option<f64>,
    rate: Option<f64>,
}

/// Open a single-DDC HPSDR/Hermes device (feature `hpsdr`) as an
/// `IqSource`, or `None` if `--hpsdr-host` wasn't given. Checked ahead of
/// `open_source`'s kiwi/soapy/audio chain, so `--hpsdr-host` takes priority
/// over those the same way `kiwi.host` already takes priority over
/// `soapy.driver` inside that chain -- in practice only one of
/// kiwi/soapy/hpsdr is ever set, since each already `conflicts_with_all`
/// `device`/`source`.
#[cfg(feature = "hpsdr")]
fn open_hpsdr_source(hpsdr: HpsdrOpts) -> Result<Option<Box<dyn IqSource>>> {
    let Some(host) = hpsdr.host else {
        return Ok(None);
    };
    let freq = hpsdr
        .freq
        .ok_or_else(|| anyhow!("--hpsdr-freq is required with --hpsdr-host"))?;
    let rate = hpsdr
        .rate
        .ok_or_else(|| anyhow!("--hpsdr-rate is required with --hpsdr-host"))?;
    let cfg = manta_input::hpsdr::HpsdrConfig {
        host,
        port: hpsdr.port,
        ddc_count: 1,
        sample_rate_hz: rate,
        center_freq_hz: vec![freq],
    };
    let mut sources = manta_input::hpsdr::HpsdrDevice::open(cfg)?;
    Ok(Some(Box::new(sources.remove(0))))
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

    fn confirmed_live_handle(&self) -> Option<std::sync::Arc<std::sync::atomic::AtomicBool>> {
        self.inner.confirmed_live_handle()
    }

    fn health_counters(&self) -> Option<std::sync::Arc<manta_input::InputHealthCounters>> {
        self.inner.health_counters()
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

/// `Engine::Hsmm` (Task 8: `TrackDecoder::push_hop_hsmm`) is a real, fully
/// implemented and reviewed engine as of Task 11 -- it parses through like
/// `legacy`/`edge-legacy`. It remains experimental/unmeasured for
/// production use (SPEC v2 §8.4, Tasks 11-12 measure it), but that's a
/// deployment/support-posture question for operators choosing `--engine
/// hsmm` explicitly, not a reason to reject it at the CLI.
fn parse_engine(s: &str) -> std::result::Result<Engine, String> {
    s.parse()
}

/// Derives the replay session's wall-clock epoch (fed to `SpotBus`, and
/// from there into every JSON `timestamp`/RBN Zulu field a client
/// observes) from the replayed file's own filesystem modification time.
/// This satisfies two constraints an earlier version traded off against
/// each other across several review rounds: it must be a GENUINE
/// wall-clock instant (a file-content hash reinterpreted as nanoseconds
/// produced technically-unique but fabricated dates spanning 1970-2554 --
/// round 5), and it must be STABLE across reruns of the same replay file
/// (unconditionally using `SystemTime::now()` made every rerun's JSON/RBN
/// output non-reproducible -- round 6's finding). A file's mtime is a real
/// system fact -- not perfect (it's "when this file was last written,"
/// not "when the recording happened"), but honest and non-arbitrary,
/// unlike either prior approach -- and it doesn't change between two
/// reads of the same untouched file. Session-identity uniqueness (the
/// separate concern that originally motivated the content hash) is
/// handled by `session_nonce_for_replay_path` below, independently of
/// this epoch.
fn epoch_for_replay_path(path: &std::path::Path) -> Result<std::time::SystemTime> {
    let mtime = std::fs::metadata(path)
        .with_context(|| {
            format!(
                "reading metadata for {} to derive its replay epoch",
                path.display()
            )
        })?
        .modified()
        .with_context(|| {
            format!(
                "{} has no modification time on this platform",
                path.display()
            )
        })?;
    // A Unix filesystem can represent a pre-1970 mtime. Left unvalidated,
    // this SystemTime flows all the way to SpotBus::unix_ts_for, whose
    // `.duration_since(UNIX_EPOCH).expect(...)` panics on the very first
    // spot delivered to any client -- reject it here, at startup, with a
    // clear error instead (round-8 review finding).
    if mtime < std::time::SystemTime::UNIX_EPOCH {
        bail!(
            "{} has a modification time before the Unix epoch (1970-01-01), which can't be used \
             as a replay epoch -- pass --replay-epoch explicitly instead",
            path.display()
        );
    }
    Ok(mtime)
}

/// Resolves the wall-clock epoch fed to `SpotBus`: an explicit
/// `--replay-epoch` wins when given AND this is a replay session (the
/// escape hatch for a copy/download that didn't preserve the file's
/// mtime); a replay session with no explicit epoch falls back to the
/// file's own mtime; a live session (`replay_path` is `None`) always uses
/// the current time, ignoring `replay_epoch` entirely -- matching the
/// flag's own documented "ignored for a live source" contract. Applying it
/// to a live session (round-8 review finding) would publish spots with a
/// fabricated historical timestamp and derive the live session_nonce from
/// that same fixed value, breaking the "two live sessions started within
/// the same wall-clock second don't collide" guarantee.
fn resolve_epoch(
    replay_path: Option<&std::path::Path>,
    replay_epoch: Option<i64>,
) -> Result<std::time::SystemTime> {
    match (replay_path, replay_epoch) {
        (Some(_), Some(secs)) => {
            Ok(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64))
        }
        (Some(path), None) => epoch_for_replay_path(path),
        (None, _) => Ok(std::time::SystemTime::now()),
    }
}

/// Derives a stable, recording-specific session nonce from a WAV file's
/// CONTENT (not its path): same bytes -> same nonce on every run
/// (deterministic spot `id`s across reruns) *regardless of where the file
/// lives* -- a different checkout, mount point, rename, or machine must
/// not change it, since it's the same recording. Two different recordings
/// hash to (almost certainly) different nonces, so their spots don't
/// collide in JSON `id` even at the same track/sample position. Uses
/// FNV-1a-64 (`hash = (hash XOR byte) * FNV_PRIME`, from the published
/// offset basis) -- a small, independently specified, versioned algorithm
/// with no dependency on any std or compiler internals -- NOT `std`'s
/// `DefaultHasher`, whose own docs disclaim any stability guarantee across
/// Rust releases (round-12 review finding: the same replay file could
/// hash differently across builds/toolchains, and this value feeds every
/// JSON spot `id`). This keeps the nonce stable across different
/// builds/toolchains too, not just within one binary -- the same
/// determinism guarantee this repo's own "3 runs, same binary ->
/// identical output" CI rule already relies on (that rule covers `manta
/// decode --json`'s sample-relative Spot output, which never carries a
/// wall-clock field at all -- see wiki/pages/determinism.md; it does not
/// extend to manta-server's live wall-clock `timestamp`/RBN Zulu fields,
/// which SpotBus's `epoch` -- always real `SystemTime::now()`, see
/// `start_spot_server` -- covers separately and deliberately does NOT
/// reproduce across reruns).
///
/// This value is ONLY a session nonce (`SpotBus::session_nonce`), never
/// fed into `SpotBus::epoch`/`unix_ts_for` -- an earlier version derived
/// both from this same hash, which meant a replayed file's JSON
/// `timestamp`/RBN Zulu time was a fabricated date with no relation to
/// real time (nanoseconds-since-Unix-epoch reinterpreted as a wall clock).
/// A network client's `timestamp` must always be truthful.
fn session_nonce_for_replay_path(path: &std::path::Path) -> Result<u128> {
    use std::io::Read;

    let mut file = std::fs::File::open(path)
        .with_context(|| format!("opening {} to derive its replay identity", path.display()))?;
    const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("reading {} to derive its replay identity", path.display()))?;
        if n == 0 {
            break;
        }
        for &byte in &buf[..n] {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(PRIME);
        }
    }
    Ok(hash as u128)
}

/// Clap value parser for `--dial-freq-hz`: rejects non-finite (NaN/infinity)
/// and non-positive values at CLI-parse time, before they're baked into
/// `FixedCenterFreqSource` and silently propagate into malformed RBN/JSON
/// frequency fields (e.g. a literal `NaN`, or a "0"/`band: "unknown"` from
/// a zero or negative dial frequency).
/// `--capture-start` for `Command::Oracle`: ISO-8601 UTC (e.g.
/// "2025-11-29T00:00:00Z"), parsed to Unix epoch seconds via
/// `manta_testkit::oracle::parse_utc_timestamp` -- anchors RBN spot times
/// to the actual recording start (Codex review, PR #161).
fn parse_capture_start(s: &str) -> std::result::Result<i64, String> {
    manta_testkit::oracle::parse_utc_timestamp(s)
        .map_err(|e| format!("invalid --capture-start {s:?}: {e}"))
}

// Codex review, PR #161 round 2: a negative --window-s makes run_oracle's
// s1 < s0, panicking on the iq[s0..s1] slice; zero/NaN silently produces an
// empty window; infinity decodes the whole capture per spot. Reject at the
// CLI boundary too (run_oracle itself now also validates -- see that
// function's doc comment -- but a CLI-level rejection gives a clap usage
// error instead of a bail! from inside the command's execution path).
fn parse_window_s(s: &str) -> std::result::Result<f64, String> {
    let window_s: f64 = s
        .parse()
        .map_err(|e| format!("invalid --window-s {s:?}: {e}"))?;
    if !window_s.is_finite() || window_s <= 0.0 {
        return Err(format!(
            "--window-s must be finite and positive, got {window_s}"
        ));
    }
    Ok(window_s)
}

fn parse_dial_freq_hz(s: &str) -> std::result::Result<f64, String> {
    let hz: f64 = s
        .parse()
        .map_err(|e| format!("invalid --dial-freq-hz {s:?}: {e}"))?;
    if !hz.is_finite() || hz <= 0.0 {
        return Err(format!(
            "--dial-freq-hz must be a finite, positive number of Hz, got {hz}"
        ));
    }
    Ok(hz)
}

/// Lower bound for `--hpsdr-rate`: comfortably below every real HPSDR/
/// Hermes sample rate (48 kHz-1.536 MHz) while still guaranteeing
/// `GapDetector::new`'s `Duration::from_secs_f64(126.0 / sample_rate_hz)`
/// (126 = `USB_FRAMES_PER_PACKET * samples_per_usb_frame(1)`, this CLI's
/// fixed single-DDC case) stays far inside `Duration`'s representable range
/// -- a finite, positive but tiny rate like `1e-20` still overflows it and
/// panics (round-2 review finding: the round-1 fix rejected NaN/inf/<=0 but
/// not an unrealistically small positive value).
#[cfg(feature = "hpsdr")]
const MIN_HPSDR_RATE_HZ: f64 = 1_000.0;
/// Upper bound for `--hpsdr-rate`: generous headroom above any real
/// HPSDR/Hermes rate, purely to keep the range symmetric and reject
/// obviously-wrong input (e.g. a value with stray zeros) rather than to
/// pin an exact hardware ceiling this CLI layer has no authority over.
#[cfg(feature = "hpsdr")]
const MAX_HPSDR_RATE_HZ: f64 = 10_000_000.0;

/// Clap value parser for `--hpsdr-rate`: rejects non-finite (NaN/infinity)
/// and out-of-range values at CLI-parse time. `HpsdrConfig::validate`'s own
/// `validate_ddc_config` bandwidth check silently passes a NaN rate
/// (comparisons against NaN are always false), and the value then reaches
/// `GapDetector::new`'s `Duration::from_secs_f64(samples_per_packet as f64
/// / sample_rate_hz)`, which panics on NaN or an unrepresentable Duration
/// -- caught here instead, before any source is opened, matching
/// `parse_dial_freq_hz`'s pattern.
#[cfg(feature = "hpsdr")]
fn parse_hpsdr_rate_hz(s: &str) -> std::result::Result<f64, String> {
    let hz: f64 = s
        .parse()
        .map_err(|e| format!("invalid --hpsdr-rate {s:?}: {e}"))?;
    if !hz.is_finite() || !(MIN_HPSDR_RATE_HZ..=MAX_HPSDR_RATE_HZ).contains(&hz) {
        return Err(format!(
            "--hpsdr-rate must be a finite number of Hz between {MIN_HPSDR_RATE_HZ} and \
             {MAX_HPSDR_RATE_HZ}, got {hz}"
        ));
    }
    Ok(hz)
}

/// Clap value parser for `--hpsdr-freq`: rejects non-finite (NaN/infinity)
/// and non-positive values at CLI-parse time, matching
/// `parse_dial_freq_hz`'s pattern (round-2 review finding: an unvalidated
/// `--hpsdr-freq NaN`/`inf` reaches `HpsdrConfig.center_freq_hz`, which is
/// only length-checked, not value-checked, and then propagates into every
/// emitted spot's frequency field).
#[cfg(feature = "hpsdr")]
fn parse_hpsdr_freq_hz(s: &str) -> std::result::Result<f64, String> {
    let hz: f64 = s
        .parse()
        .map_err(|e| format!("invalid --hpsdr-freq {s:?}: {e}"))?;
    if !hz.is_finite() || hz <= 0.0 {
        return Err(format!(
            "--hpsdr-freq must be a finite, positive number of Hz, got {hz}"
        ));
    }
    Ok(hz)
}

/// Upper bound for `--replay-epoch`: 2100-01-01T00:00:00Z in Unix seconds.
/// No real recording needs an epoch beyond this; the bound exists purely
/// to keep `secs` far away from the range where `SpotBus::unix_ts_for`'s
/// `epoch + elapsed` (`SystemTime` arithmetic) could overflow and panic on
/// the first spot delivered to any client (round-9 review finding) --
/// generous, not tight, since the actual overflow point depends on the
/// platform's `SystemTime` representation and isn't worth pinning exactly.
const MAX_REPLAY_EPOCH_SECS: i64 = 4_102_444_800;

/// Clap value parser for `--replay-epoch`: Unix seconds, bounded to a
/// plausible calendar range (non-negative, before `MAX_REPLAY_EPOCH_SECS`)
/// -- a `SystemTime` before `UNIX_EPOCH` isn't representable via the
/// `UNIX_EPOCH + Duration` construction this flag feeds, and an
/// unrealistically large value risks overflowing later `SystemTime`
/// arithmetic instead of failing cleanly here. Deliberately a plain
/// integer, not RFC3339 or similar -- avoids pulling in a date/time-
/// parsing dependency for one CLI flag; any real timestamp source (a
/// recording tool's own metadata, `date +%s`) can produce Unix seconds
/// directly.
fn parse_replay_epoch(s: &str) -> std::result::Result<i64, String> {
    let secs: i64 = s
        .parse()
        .map_err(|e| format!("invalid --replay-epoch {s:?}: {e}"))?;
    if !(0..=MAX_REPLAY_EPOCH_SECS).contains(&secs) {
        return Err(format!(
            "--replay-epoch must be Unix seconds between 0 and {MAX_REPLAY_EPOCH_SECS} \
             (2100-01-01), got {secs}"
        ));
    }
    Ok(secs)
}

/// Strips a leading UTF-8 BOM (`\u{feff}`), common in Windows-authored text
/// files -- `str::trim` does not remove it, so left unstripped it corrupts
/// the first line's parse (a blocklist callsign that never matches, or a
/// notch range silently rejected).
fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

/// Builds a `PipelineConfig` from the CLI's shared flags: the MAN-29
/// frequency-calibration correction, the MAN-28 operator Watch List, and
/// the MAN-31 operator suppression lists. Each is optional/repeatable; an
/// absent one leaves that list empty, matching `PipelineConfig`'s own
/// defaults.
fn build_pipeline_config(
    freq_correction_ppm: f64,
    allowlist: Vec<String>,
    blocklist: Option<PathBuf>,
    notch: Option<PathBuf>,
    engine: Engine,
) -> Result<PipelineConfig> {
    let mut cfg = PipelineConfig {
        freq_correction_ppm,
        allowlist,
        ..Default::default()
    };
    cfg.decode.engine = engine;
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

/// Loads the `[decode]` TOML table (SPEC v2 §7) from `--server-config`, if
/// given, into a full `manta_decode::decoder::DecodeConfig`. Parses the
/// same file's raw text a SECOND time, independent of
/// `manta_server::config::DaemonConfigFile` -- that struct deliberately
/// does not model `[decode]` (see its own doc comment), and re-parsing the
/// same text into a separately-modeled top-level table is the existing
/// pattern for this unified daemon config (`ServerConfig`/`RbnUplinkConfig`
/// already work this way). `Ok(DecodeConfig::default())` when no
/// `--server-config` path is given, matching `PipelineConfig::default()`'s
/// own decode baseline.
fn load_decode_config_file(
    server_config: Option<&Path>,
) -> Result<manta_decode::decoder::DecodeConfig> {
    let Some(path) = server_config else {
        return Ok(manta_decode::decoder::DecodeConfig::default());
    };
    let cfg_text = std::fs::read_to_string(path)
        .with_context(|| format!("reading --server-config {}", path.display()))?;
    let file: manta_decode::config_file::DecodeConfigFile = toml::from_str(&cfg_text)
        .with_context(|| format!("parsing [decode] table in {}", path.display()))?;
    let cfg = file.decode.into_decode_config();
    // Codex review, PR #161: `fallback_hops = 0` deserializes successfully (it's a
    // plain u32 with no serde-level range check) but Evidence::push's anchor
    // computation divides by it (`self.hop_out % self.cfg.fallback_hops as u64`),
    // panicking on the first hop for both edge-legacy and hsmm. Reject it here,
    // at the one place a `[decode]` table from disk enters the process, rather
    // than scattering a zero-guard into the hot per-hop path.
    if cfg.evidence.fallback_hops == 0 {
        bail!(
            "[decode] fallback_hops must be nonzero in {} (0 would divide by zero on the \
             first evidence hop)",
            path.display()
        );
    }
    // Codex review, PR #161 round 2: `tau_hi_bounds_ms = [400, 100]` (or any
    // non-finite/inverted pair) deserializes successfully, but
    // `Demod::set_dit_ms` later passes it straight to `f64::clamp`, which
    // panics on `min > max` and takes down the whole decode/listen/run
    // process on its first speed update. Reject an invalid bound here,
    // same reasoning as the fallback_hops check above.
    let (tau_hi_lo, tau_hi_hi) = cfg.demod.tau_hi_bounds_ms;
    if !tau_hi_lo.is_finite() || !tau_hi_hi.is_finite() || tau_hi_lo > tau_hi_hi || tau_hi_lo <= 0.0
    {
        bail!(
            "[decode] tau_hi_bounds_ms must be a finite [low, high] pair with 0 < low <= high \
             in {} (got [{tau_hi_lo}, {tau_hi_hi}], which would panic in f64::clamp on the \
             first speed update)",
            path.display()
        );
    }
    if !cfg.demod.tau_lo_ms.is_finite() || cfg.demod.tau_lo_ms <= 0.0 {
        bail!(
            "[decode] tau_lo_ms must be a finite, positive number of milliseconds in {} \
             (got {})",
            path.display(),
            cfg.demod.tau_lo_ms
        );
    }
    // Codex review, PR #161 rounds 3 and 5: `timing_sigma = 0` (or NaN)
    // reaches beam::log_likelihood's `2 * sigma * sigma` denominator,
    // producing infinite/NaN confidence scores instead of a load-time
    // error. Round 3's `> 0.0` check alone isn't a strong enough floor: an
    // f32-representable but tiny value (e.g. 1e-30) still passes it, yet
    // `2.0 * sigma * sigma` underflows to exactly 0.0 in f32 (f32's
    // smallest positive normal is ~1.18e-38, so anything with
    // sigma^2 below ~5.9e-39 flushes to zero), giving 0.0/0.0 = NaN for
    // any candidate with a perfectly-matched duration. 1e-3 (0.1% relative
    // timing tolerance) is nowhere near that underflow threshold and is
    // already far stricter than any real keying signal's timing jitter --
    // SPEC v2's own default is 0.25 -- so it rejects only configs that
    // could never usefully decode real audio, not legitimate tuning.
    const MIN_TIMING_SIGMA: f32 = 1e-3;
    if !cfg.beam.sigma.is_finite() || cfg.beam.sigma < MIN_TIMING_SIGMA {
        bail!(
            "[decode] timing_sigma must be finite and >= {MIN_TIMING_SIGMA} in {} (got {}; \
             smaller values can underflow log_likelihood's denominator to a NaN score)",
            path.display(),
            cfg.beam.sigma
        );
    }
    if cfg.beam.width == 0 {
        bail!(
            "[decode] beam_width must be nonzero in {} (0 disables the beam decoder entirely)",
            path.display()
        );
    }
    // Codex review, PR #161 round 3: `[decode] beam = 0` (the hsmm
    // engine's own beam size, SPEC v2 §4 -- distinct from the legacy
    // `beam_width` checked above) deserializes and passes every check
    // above, but `HsmmDecoder::push`'s `merged.truncate(0)` then
    // permanently empties the live hypothesis set on the very first
    // anchor step -- the command silently emits no decoded text or
    // spots, no error. Same class of gap as `beam_width`, just the other
    // engine's beam.
    if cfg.hsmm.beam == 0 {
        bail!(
            "[decode] hsmm beam must be nonzero in {} (0 empties the live hypothesis set on the \
             first anchor step, silently emitting no decoded text)",
            path.display()
        );
    }
    // Codex review, PR #161 round 4: `sigma_u = 0` puts a hop exactly on
    // the normalized half-amplitude decision surface at `0 / 0`, making
    // the LLR (and every downstream accumulated prefix) permanently NaN;
    // a non-finite value corrupts every present hop the same way. Both
    // edge-legacy and hsmm then silently stop decoding or propagate NaN
    // scores.
    // Codex review, PR #161 round 20: fresh evidence beyond the zero-value
    // case above -- a finite-but-tiny sigma_u (e.g. 1e-30) still passes
    // `> 0.0`, yet `sigma_u * sigma_u` underflows to exactly 0.0 in f32,
    // permanently poisoning the evidence prefix with NaN. Same underflow
    // class as dur_sigma's MIN_DUR_SIGMA floor.
    const MIN_SIGMA_U: f32 = 1e-3;
    if !cfg.evidence.sigma_u.is_finite() || cfg.evidence.sigma_u < MIN_SIGMA_U {
        bail!(
            "[decode] sigma_u must be finite and >= {MIN_SIGMA_U} in {} (got {}; smaller values \
             can underflow the LLR denominator to 0/0)",
            path.display(),
            cfg.evidence.sigma_u
        );
    }
    // Codex review, PR #161 round 4: an empty `seed_units_hops` list
    // deserializes and passes every check above, but every keying onset
    // then seeds zero tokens -- candidate generation stays empty forever
    // and the command silently emits no decoded text or spots. Require at
    // least one seed unit, and that every seed is itself finite and
    // positive (a bad seed is exactly as silently broken as an empty
    // list, just one hypothesis worth instead of all of them).
    if cfg.hsmm.seed_units_hops.is_empty() {
        bail!(
            "[decode] seed_units_hops must have at least one entry in {} (an empty list seeds \
             zero tokens at every keying onset, silently emitting no decoded text)",
            path.display()
        );
    }
    if let Some(bad) = cfg
        .hsmm
        .seed_units_hops
        .iter()
        .find(|u| !u.is_finite() || **u <= 0.0)
    {
        bail!(
            "[decode] every seed_units_hops entry must be finite and positive in {} (got {bad})",
            path.display()
        );
    }
    // Codex review, PR #161 round 18: a seed outside the decoder's
    // supported [u_min, u_max] speed range (7.5..=56 hops/dit) is finite
    // and positive and so passed the check above, but can't produce a
    // valid initial mark transition for any real 8-60 WPM signal -- a
    // seed list containing only such values silently emits no decoded
    // text.
    if let Some(bad) = cfg
        .hsmm
        .seed_units_hops
        .iter()
        .find(|u| **u < cfg.hsmm.u_min || **u > cfg.hsmm.u_max)
    {
        bail!(
            "[decode] every seed_units_hops entry must be within the supported speed range \
             [{}, {}] hops/dit in {} (got {bad}; outside that range, the seed can't produce a \
             valid initial mark transition for any real signal)",
            cfg.hsmm.u_min,
            cfg.hsmm.u_max,
            path.display()
        );
    }
    // Codex review, PR #161 round 4: `conf_kappa = 0` makes the common
    // no-competing-hypothesis path (s_alt == best.score) compute `0 / 0`
    // in `margin`'s confidence sigmoid, emitting a NaN character/word-
    // boundary confidence that then contaminates every downstream spot-
    // confidence calculation and JSON report.
    if !cfg.hsmm.conf_kappa.is_finite() || cfg.hsmm.conf_kappa <= 0.0 {
        bail!(
            "[decode] conf_kappa must be finite and strictly positive in {} (got {}; 0 makes the \
             no-competing-hypothesis confidence path compute 0/0)",
            path.display(),
            cfg.hsmm.conf_kappa
        );
    }
    // Codex review, PR #161 rounds 5 and 16: `dur_sigma = 0` reaches
    // `log_dur_prior`'s `2 * dur_sigma * dur_sigma` denominator -- an
    // exactly-nominal-duration segment computes 0/0, and every other
    // segment computes an infinite (non-nominal) score, corrupting
    // pruning and every downstream confidence. Round 5's `> 0.0` check
    // alone isn't a strong enough floor: a finite-but-tiny value (e.g.
    // 1e-30) still passes it, yet `2.0 * dur_sigma * dur_sigma`
    // underflows to exactly 0.0 in f32 -- same underflow class as
    // timing_sigma's `MIN_TIMING_SIGMA` floor below.
    const MIN_DUR_SIGMA: f32 = 1e-3;
    if !cfg.hsmm.dur_sigma.is_finite() || cfg.hsmm.dur_sigma < MIN_DUR_SIGMA {
        bail!(
            "[decode] dur_sigma must be finite and >= {MIN_DUR_SIGMA} in {} (got {}; smaller \
             values can underflow log_dur_prior's denominator to 0/0)",
            path.display(),
            cfg.hsmm.dur_sigma
        );
    }
    // Codex review, PR #161 round 6: a large but individually-plausible
    // `hold_dits` (e.g. 300) pushes `Evidence`'s hold-window width `h`
    // (`hold_dits * u_max`) past `MAX_RETAIN` -- a debug build panics on
    // the internal `debug_assert!`, a release build silently caps the
    // delay line and discards centers with no error, decoding only the
    // retained tail at EOF. `cfg.hsmm.u_max` is the largest `u_ref` the
    // live speed-feedback loop can ever request (`Token::successor`
    // clamps every speed update to `[u_min, u_max]`), so that's the
    // correct worst case to bound against -- not just the config's
    // initial `u_init_hops`.
    let max_h = cfg.evidence.hold_dits as f64 * cfg.hsmm.u_max as f64;
    if !cfg.evidence.hold_dits.is_finite() || cfg.evidence.hold_dits <= 0.0 || !max_h.is_finite() {
        bail!(
            "[decode] hold_dits must be finite and positive in {} (got {})",
            path.display(),
            cfg.evidence.hold_dits
        );
    }
    // Codex review, PR #161 round 20: fresh evidence beyond the earlier
    // retention-bound fix -- `Evidence::set_u_ref` rounds `hold_dits *
    // u_ref` before enforcing `h < MAX_RETAIN` (`.round().max(1.0)`), so
    // comparing the unrounded product here can accept a value that
    // rounds UP into the cap once actually used (e.g. hold_dits=73.14 at
    // u_max=56 gives 4095.84, accepted here, but rounds to 4096).
    // Compare the same rounded value Evidence itself uses.
    let max_h_rounded = max_h.round().max(1.0);
    if max_h_rounded >= manta_decode::evidence::MAX_RETAIN as f64 {
        bail!(
            "[decode] hold_dits={} is too large in {}: at the configured u_max={}, the rounded \
             hold window (round(hold_dits * u_max) = {max_h_rounded}) would reach or exceed \
             Evidence's internal retention cap ({}), silently truncating the delay line and \
             discarding evidence centers",
            cfg.evidence.hold_dits,
            path.display(),
            cfg.hsmm.u_max,
            manta_decode::evidence::MAX_RETAIN,
        );
    }
    // Codex review, PR #161 round 8: `speed_alpha = nan` deserializes and
    // passes every check above; the first duration update
    // (`u += speed_alpha * (target - u)`) then makes `u` NaN, and every
    // subsequent duration prior/score derived from it goes NaN too --
    // beam ordering can then retain those hypotheses independently of
    // real evidence, silently corrupting or emptying the output.
    if !cfg.hsmm.speed_alpha.is_finite() {
        bail!(
            "[decode] speed_alpha must be finite in {} (got {}; a non-finite value poisons \
             every subsequent speed update and duration prior with NaN)",
            path.display(),
            cfg.hsmm.speed_alpha
        );
    }
    // Codex review, PR #161 round 17: a finite but NEGATIVE speed_alpha
    // moves `u += speed_alpha * (target - u)` away from the observed
    // segment duration instead of toward it -- repeated short segments
    // then drive `u` toward a clamp boundary, producing incorrect WPM
    // reports and killing otherwise-valid duration hypotheses.
    if cfg.hsmm.speed_alpha < 0.0 {
        bail!(
            "[decode] speed_alpha must be nonnegative in {} (got {}; a negative gain moves the \
             speed estimate away from observed durations instead of toward them)",
            path.display(),
            cfg.hsmm.speed_alpha
        );
    }
    // Codex review, PR #161 round 10: `mark_insert_penalty = nan` reaches
    // `SegType::log_type_prior`; the first Dit/Dah transition then gives
    // every candidate a NaN score, so beam ordering no longer reflects
    // the evidence and emitted confidence can also become NaN.
    if !cfg.hsmm.mark_insert_penalty.is_finite() {
        bail!(
            "[decode] mark_insert_penalty must be finite in {} (got {}; a non-finite value \
             poisons every Dit/Dah transition's score with NaN)",
            path.display(),
            cfg.hsmm.mark_insert_penalty
        );
    }
    // Codex review, PR #161 round 12: `noise_min_bias_db = inf` (used by
    // both edge-legacy and hsmm) makes `NoiseTracker::new`'s `b_min`
    // infinite; every temporal noise estimate then becomes infinite and
    // the evidence gate stays closed forever, silently emitting nothing.
    if !cfg.noise.noise_min_bias_db.is_finite() {
        bail!(
            "[decode] noise_min_bias_db must be finite in {} (got {}; a non-finite value makes \
             every temporal noise estimate infinite, silently closing the evidence gate)",
            path.display(),
            cfg.noise.noise_min_bias_db
        );
    }
    // Codex review, PR #161 round 13: a negative `lookahead_dits` makes
    // every non-consensus history entry's nonnegative age always exceed
    // the (negative) forced-commit threshold, reducing the HSMM to
    // greedy commits and producing misleading confidence/decoded text; a
    // NaN or infinite value disables forced commits entirely.
    if !cfg.hsmm.lookahead_dits.is_finite() || cfg.hsmm.lookahead_dits < 0.0 {
        bail!(
            "[decode] lookahead_dits must be finite and nonnegative in {} (got {}; a negative \
             value forces every non-consensus entry immediately, and a non-finite value \
             disables forced commits entirely)",
            path.display(),
            cfg.hsmm.lookahead_dits
        );
    }
    // Codex review, PR #161 round 14: `noise_window_ms` <= 0 or NaN casts
    // to zero in `ms_to_hops`, silently reducing the minimum-statistics
    // window to one hop (keyed power itself becomes the noise floor,
    // suppressing EdgeLegacy/HSMM output); infinity becomes `u32::MAX`,
    // letting each track's deque grow for effectively the process
    // lifetime.
    if !cfg.noise.noise_window_ms.is_finite() || cfg.noise.noise_window_ms <= 0.0 {
        bail!(
            "[decode] noise_window_ms must be finite and positive in {} (got {})",
            path.display(),
            cfg.noise.noise_window_ms
        );
    }
    // Codex review, PR #161 round 4: the remaining newly-exposed v1 §9
    // fields have the same class of gap -- `hyst_up`/`hyst_down` reaching
    // `Demod::step`'s `a < hyst_down * t` / `a > hyst_up * t` comparisons
    // with NaN makes every comparison false (Legacy silently emits nothing
    // at all, no error, no panic); a non-positive or inverted hysteresis
    // band (hyst_up <= hyst_down) breaks the open/close asymmetry
    // hysteresis exists for; `flush_gap_dits <= 0` forces an instant/
    // premature word flush on every hop. Validate all of them here too,
    // for the same reason as every check above: this is the one place a
    // `[decode]` table from disk enters the process.
    if !cfg.demod.hyst_up.is_finite() || !cfg.demod.hyst_down.is_finite() {
        bail!(
            "[decode] hyst_up/hyst_down must be finite in {} (got hyst_up={}, hyst_down={})",
            path.display(),
            cfg.demod.hyst_up,
            cfg.demod.hyst_down
        );
    }
    if cfg.demod.hyst_down <= 0.0 || cfg.demod.hyst_up <= cfg.demod.hyst_down {
        bail!(
            "[decode] hyst_up must be > hyst_down > 0 in {} (got hyst_up={}, hyst_down={})",
            path.display(),
            cfg.demod.hyst_up,
            cfg.demod.hyst_down
        );
    }
    if !cfg.demod.debounce_ms.is_finite() || cfg.demod.debounce_ms <= 0.0 {
        bail!(
            "[decode] debounce_ms must be finite and positive in {} (got {})",
            path.display(),
            cfg.demod.debounce_ms
        );
    }
    if !cfg.flush_gap_dits.is_finite() || cfg.flush_gap_dits <= 0.0 {
        bail!(
            "[decode] flush_gap_dits must be finite and positive in {} (got {})",
            path.display(),
            cfg.flush_gap_dits
        );
    }
    // Codex review, PR #161 round 2: a negative or non-finite `llr_clip`
    // reaches `f32::clamp(-llr_clip, llr_clip)` on the first keying-present
    // evidence hop in both edge-legacy and hsmm, panicking on inverted or
    // NaN bounds exactly like the tau_hi_bounds_ms case above.
    if !cfg.evidence.llr_clip.is_finite() || cfg.evidence.llr_clip <= 0.0 {
        bail!(
            "[decode] llr_clip must be finite and positive in {} (got {}; f32::clamp panics on \
             a negative or non-finite bound)",
            path.display(),
            cfg.evidence.llr_clip
        );
    }
    Ok(cfg)
}

/// SPEC v2 §7: an explicit `--engine` flag overrides the `[decode]` table's
/// `engine` key; the file's value (or `Engine::Legacy` if there's no
/// `--server-config`/no `[decode]` table) is the baseline otherwise. Every
/// other `DecodeConfig` field always comes from `file_decode` (i.e. from
/// the file, or its defaults) -- there is no CLI flag for them.
fn merge_cli_engine(
    cli_engine: Option<Engine>,
    mut file_decode: manta_decode::decoder::DecodeConfig,
) -> manta_decode::decoder::DecodeConfig {
    if let Some(engine) = cli_engine {
        file_decode.engine = engine;
    }
    file_decode
}

/// Handles what the `Run` on-spot closure needs to feed a running spot server.
struct SpotServer {
    bus: std::sync::Arc<manta_server::bus::SpotBus>,
    metrics: std::sync::Arc<manta_server::metrics::Metrics>,
    /// The metrics/status HTTP listener's real bound address (MAN-44) --
    /// captured from `TcpListener::local_addr()` so a port-0 (OS-assigned)
    /// bind is still reachable by `manta status`/tests without guessing or
    /// binding a fixed port (CLAUDE.md multi-agent hygiene: don't bind
    /// fixed ports).
    #[cfg_attr(not(test), allow(dead_code))]
    metrics_addr: std::net::SocketAddr,
    /// Signals the telnet/JSON/WS client tasks to drain their already-
    /// queued spots and exit, instead of being forcibly cut off by
    /// `Runtime::shutdown_timeout`'s raw deadline with no chance to finish
    /// an in-flight write. Call `.send(true)` before shutting the runtime
    /// down.
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    /// Every spawned telnet/JSON/WS per-client connection task, tracked so
    /// shutdown can genuinely AWAIT their completion (bounded by
    /// `SHUTDOWN_DRAIN_DEADLINE`) instead of guessing a fixed sleep
    /// duration -- see `shutdown_runtime_after_drain`.
    tasks: manta_server::tasks::ClientTasks,
    /// MAN-136/MAN-45: the same `cty::Table` handed to `JsonStreamConfig`,
    /// kept here too so the publish callback can check resolvability once
    /// per spot for `manta_spots_unresolved_geography_total` -- checking
    /// inside `SpotMessage::from_spot` would scale with connected client
    /// count instead of spot count.
    cty: std::sync::Arc<manta_spot::cty::Table>,
    /// Whether the operator's OWN station callsign (config, not decoder
    /// output -- and not required to be cty-resolvable) already forces the
    /// de-side `UNKNOWN_*` sentinels. Resolved ONCE at `start_spot_server`
    /// time rather than per spot: `station_callsign` cannot change for the
    /// life of the process, so re-running the same binary search on every
    /// spot only re-derives a constant.
    station_geography_unresolved: bool,
}

/// True when `SpotMessage::from_spot` would emit the `UNKNOWN_DXCC` /
/// `UNKNOWN_CONTINENT` / `UNKNOWN_CQ_ZONE` sentinels for `callsign`, i.e.
/// exactly the condition `manta_spots_unresolved_geography_total` counts.
///
/// Deliberately keyed on the RESOLVED ADIF entity number, not merely on
/// whether `lookup` returned an entry: `from_spot` emits `UNKNOWN_DXCC` on
/// `dx.and_then(|e| e.dxcc).is_none()`, which is also true when `cty.dat`
/// resolves the call but the vendored `dxcc.tsv` has no row for its primary
/// prefix -- the drift state that arises when `cty.dat` is hand-refreshed
/// (data/SOURCES.md) without regenerating the TSV. Counting `lookup`
/// alone would let those spots go out carrying `dxDxcc: -1` with the
/// counter still at zero, silently withholding the one signal this metric
/// exists to give (round-1 validate code-review finding 1).
///
/// A maritime-mobile (`/MM`) or aeronautical-mobile (`/AM`) call counts too
/// (round-7 review finding 2): `cty.lookup` answers for it through the base
/// call's prefix, but `from_spot` deliberately discards that answer and emits
/// `UNKNOWN_CONTINENT`/`UNKNOWN_CQ_ZONE` with null lat/lon -- the station's
/// real position is unknown -- so the spot does carry the sentinels this
/// counter is defined over. Its `dxDxcc` is ADIF's `NO_DXCC_ENTITY` (0)
/// rather than `UNKNOWN_DXCC`, which is why the entity number alone can't be
/// the whole test.
fn geography_is_unresolved(cty: &manta_spot::cty::Table, callsign: &str) -> bool {
    manta_server::spot_message::is_outside_any_dxcc_entity(callsign)
        || cty.lookup(callsign).and_then(|e| e.dxcc).is_none()
}

/// Starts the telnet/JSON-Lines-and-WebSocket/metrics servers on their own
/// tokio runtime (ARCHITECTURE §7-§8). The returned `Runtime` must be kept
/// alive for the servers to keep running -- dropping it stops them.
/// Before exit: send `true` on `SpotServer::shutdown_tx` so client tasks
/// get a chance to drain (e.g. spots from `TrackManager::finish()`), THEN
/// call `Runtime::shutdown_timeout` as the bounded safety net.
///
/// `epoch` is the bus's real wall-clock session start (see `SpotBus::new`)
/// -- always pass `SystemTime::now()` (this daemon's actual start time),
/// live or replay: it feeds every client-observed `timestamp`/RBN Zulu
/// field, which must stay truthful. `session_nonce` is the separate,
/// spot-`id`-uniqueness-only value -- pass a fixed one (e.g.
/// `session_nonce_for_replay_path`) when replaying a file, or two runs of
/// the same fixture emit colliding spot `id`s.
/// Upper bound on how long `shutdown_runtime_after_drain` waits for
/// spawned client-connection tasks to actually finish draining before
/// falling through to `Runtime::shutdown_timeout`'s hard cutoff. Unlike a
/// fixed sleep, this is a ceiling, not a guess that's always fully paid --
/// `tasks::await_all` returns as soon as every tracked task completes, so
/// shutdown with zero (or quickly-finishing) clients is fast regardless of
/// this value; it only matters when a task is genuinely still writing.
///
/// Must stay comfortably >= the worst-case time a SINGLE legitimately-slow
/// client's final drain write is itself permitted to take, or this deadline
/// cuts a write off before it could ever finish even under its own
/// individual timeout -- not a lagged/dead client, just an ordinary slow
/// one. `telnet::handle_client`'s drain loop writes each spot via TWO
/// separately-timed `write_with_timeout` calls (the RBN line, then
/// `\r\n`), each up to telnet's own `WRITE_TIMEOUT` (10s) -- up to ~20s for
/// one spot. The previous 2s value was shorter than even a single one of
/// those 10s writes, so a genuinely slow-but-completing client was
/// routinely cut off mid-drain for no reason (round-15 review finding).
///
/// MAN-45 (round-16 finding): as of this change, the value that actually
/// bounds ONE client's drain is `manta_server::tasks::CLIENT_DRAIN_DEADLINE`
/// -- each of the three per-client drain loops (telnet's, json_stream's TCP
/// and WS) now enforces its own inner deadline and counts whatever it
/// abandons when that fires, so a healthy handler always returns from
/// `await_all` well within its own budget. This constant is now a
/// registry-wide *scheduling backstop* above that per-client bound (see the
/// `the_outer_shutdown_deadline_outlives_every_handlers_own_drain_deadline`
/// test below) -- it no longer needs sizing against any particular spot
/// count, only against `CLIENT_DRAIN_DEADLINE` plus scheduling margin.
///
/// MAN-45 remediate (round-16 P1, finding 2): "scheduling margin" above
/// CLIENT_DRAIN_DEADLINE isn't the whole story -- `CLIENT_DRAIN_DEADLINE`
/// only bounds a handler's OWN `_ = shutdown.changed() =>` branch body.
/// `tokio::select!` doesn't poll that branch again until whichever OTHER
/// branch is currently running resolves, so a handler already mid-write
/// when shutdown fires can burn up to its own current branch's full
/// worst-case time BEFORE it even reaches the drain branch and starts
/// that 20s clock. The largest such branch across all three handlers is
/// telnet's live-spot write (`manta_server::telnet::WRITE_TIMEOUT`, TWO
/// separately-timed writes per spot) -- json_stream's TCP/WS write and
/// Pong-reply arms are each a single `WRITE_TIMEOUT`, strictly smaller.
/// So the true worst case this deadline must outlive is `2 *
/// telnet::WRITE_TIMEOUT + CLIENT_DRAIN_DEADLINE`, not
/// `CLIENT_DRAIN_DEADLINE` alone (asserted directly by
/// `the_outer_shutdown_deadline_outlives_every_handlers_own_drain_deadline`
/// below). telnet's `sh/dx` replay loop is bounded to the SAME worst case
/// as the live-write arm rather than its own unbounded backlog depth: it
/// re-checks `shutdown.has_changed()` before every history entry and, the
/// moment it's observed, `break`s back to the `select!` loop's own drain
/// branch to deliver the live `rx` backlog with that branch's full unused
/// budget, rather than abandoning it (validation round 17, CR-2/CR-3 --
/// the remaining history replay itself is simply not re-attempted, since
/// those entries were already published and counted once).
///
/// Validation round 17 (CR-1): this model above only accounts for
/// branches INSIDE the `select!` loop -- it does NOT need to also budget
/// for `telnet::handle_client`'s pre-loop login handshake (prompt write,
/// login-line read, banner write; up to `WRITE_TIMEOUT +
/// bounded_io::IDLE_READ_TIMEOUT + WRITE_TIMEOUT` = 50s) because that
/// handshake itself now races `shutdown.changed()` at every step and
/// bails out (counting its subscribed `rx` backlog) the moment shutdown
/// fires, instead of running any of those three waits to completion
/// first. A stalled pre-login client therefore contributes close to zero
/// to shutdown latency, not up to 50s -- if a future change ever makes
/// that handshake NOT shutdown-aware again, this deadline's true worst
/// case would need to grow to include it.
///
/// MAN-45 remediate (code-review round 18, finding 3): the same was true,
/// but NOT yet fixed, of `json_stream::serve`'s pre-loop phase --
/// `looks_like_websocket_handshake`'s classifying peek (up to
/// `PEEK_TIMEOUT`, or `HANDSHAKE_TIMEOUT` once any byte had arrived) and
/// `handle_ws_client`'s own `accept_async_with_config` step (up to another
/// `HANDSHAKE_TIMEOUT`) previously never observed `shutdown` either. Both
/// now race `shutdown.changed()` the same way telnet's handshake does, so
/// this deadline's safety margin no longer rests on the coincidence that
/// json_stream's *unraced* worst case (20s) happened to be smaller than
/// telnet's live-write branch (`2 * telnet::WRITE_TIMEOUT` = 20s) already
/// budgeted for above -- it now holds because BOTH pre-loop phases are
/// shutdown-aware by design, matching this deadline's own model.
///
/// MAN-45 remediate (code-review round 19, P1): the "at most ONE in-flight
/// branch body precedes the drain" step of that model is now ENFORCED, not
/// assumed. `tokio::select!` picks a random ready arm, so a client with a
/// backlog could previously win the live-spot arm repeatedly after shutdown
/// was signalled -- an unbounded number of `2 * WRITE_TIMEOUT` writes
/// before its own `CLIENT_DRAIN_DEADLINE` clock ever started, which this
/// deadline cannot cover at any constant value. Every client-write-capable
/// arm in all three handler loops (`telnet::handle_client`'s live-spot and
/// command-read arms, `json_stream`'s TCP live-spot and socket-read arms,
/// and its WS live-spot and frame arms) now carries an
/// `if !shutdown.has_changed()` precondition, so once shutdown is pending
/// the drain arm is the only arm those loops can still select. The worst
/// case therefore really is one already-selected branch body plus
/// `CLIENT_DRAIN_DEADLINE`, which is what the value below is sized for.
///
/// MAN-45 remediate (round-19 P1, re-raised against an earlier head): the
/// "one branch body" half of that budget is now also asserted END-TO-END,
/// not only arithmetically here --
/// `telnet_acceptance::shutdown_bounds_live_writes_to_at_most_one_before_the_drain`
/// queues a backlog, signals shutdown before the client task can wake, and
/// asserts across repeated trials that at most ONE live spot write precedes
/// the drain and that every queued spot is then delivered or counted. "At
/// most one", not zero, is deliberate: a handler already parked in
/// `select!` when shutdown fires evaluated its preconditions before the
/// flag was set, so it can still take the live-spot arm once -- which is
/// precisely the single branch body this deadline budgets for, above. See
/// that test's own doc comment for what it does and does not prove (with
/// fast localhost writes the unguarded build stays inside the bound too;
/// exceeding it needs a client that has stopped reading, so each write runs
/// the full `WRITE_TIMEOUT`).
/// MAN-45 remediate (code-review round 19, P1): **changing this value is
/// not self-contained** -- it is the floor for the CALLER-side stop grace
/// period an operator must configure, and two documents state that period
/// as a literal number: `README.md`'s Docker install section (`docker stop
/// -t 60`) and `Dockerfile`'s STOPSIGNAL comment block. Both said 30s,
/// sized against the pre-MAN-45 25s value; against 50s here, a 30s
/// container timeout SIGKILLs the daemon partway through the very drain
/// this constant exists to allow, before it can record the abandoned
/// backlog on `manta_spots_dropped_write_failed_total` (the counter each
/// handler's drain loop charges when its own `CLIENT_DRAIN_DEADLINE`
/// expires -- `manta_spots_dropped_shutdown_total` covers only a client
/// still in pre-login/handshake, whose backlog had not been offered for
/// delivery yet; that is not the same as the connection having written
/// nothing, since the telnet banner and WS-accept branches are reached
/// after the login prompt / part of the 101 response is already on the
/// wire) --
/// recreating the silent truncation the drain work removed. Both are now
/// 60s, leaving margin over this deadline. If this constant grows again,
/// raise them with it.
const SHUTDOWN_DRAIN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(50);

/// How often the server runtime copies the engine's live track count into
/// the `manta_active_tracks` gauge. The decode loop runs on the MAIN
/// thread, outside the tokio runtime that owns `Metrics`, so a poller is
/// the bridge -- the same shape MAN-55's `confirmed_live_handle` watcher
/// already uses. 4 Hz is far finer than any Prometheus scrape interval and
/// costs one relaxed atomic load per tick.
const ACTIVE_TRACKS_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// Shuts down `rt`, first AWAITING (not just giving scheduler time to)
/// every spawned client-connection task tracked in `tasks`, bounded by
/// `SHUTDOWN_DRAIN_DEADLINE`. `Runtime::shutdown_timeout`'s `duration`
/// parameter does not do this on its own -- verified against tokio
/// 1.53.1's source (`runtime/runtime.rs`): `shutdown_timeout` calls
/// `self.handle.inner.shutdown()` synchronously and IMMEDIATELY, tearing
/// down the async executor and dropping in-flight tasks the moment they
/// next yield; its `duration` argument bounds only the SEPARATE blocking-
/// thread-pool's shutdown. An earlier version of this function papered
/// over that with a fixed blind sleep before `shutdown_timeout` -- real
/// scheduler time, but no guarantee the tasks actually FINISHED before the
/// sleep elapsed and `shutdown_timeout` tore things down anyway (round-10
/// review finding). Awaiting `tasks::await_all` instead genuinely waits
/// for completion, up to the deadline, then still falls through to
/// `shutdown_timeout` as a final hard backstop for anything left running
/// past it.
fn shutdown_runtime_after_drain(
    rt: tokio::runtime::Runtime,
    tasks: &manta_server::tasks::ClientTasks,
) {
    rt.block_on(manta_server::tasks::await_all(
        tasks,
        SHUTDOWN_DRAIN_DEADLINE,
    ));
    rt.shutdown_timeout(std::time::Duration::from_secs(2));
}

/// How often the daemon samples an input source's `InputHealthCounters`
/// into `Metrics` (MAN-56). An order of magnitude below any realistic
/// Prometheus scrape interval, so a scrape never sees more than ~1 s of
/// staleness; the tick itself is three relaxed atomic loads and one
/// `BTreeMap` insert. Deliberately slower than the `confirmed_live` poll
/// (200 ms, see the `confirmed_live_handle` wiring below), which is tuned
/// for a single startup transition rather than a forever-loop.
const INPUT_HEALTH_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Snapshot `manta-input`'s counters into `manta-server`'s own,
/// dependency-free mirror struct. The two crates deliberately share no
/// type -- that disjointness is what keeps `manta-server` free of any
/// `manta-input` dependency (ARCHITECTURE §3/§8) -- so `manta-cli`, which
/// depends on both, is where the translation belongs.
fn input_health_of(
    counters: &manta_input::InputHealthCounters,
) -> manta_server::metrics::InputHealth {
    manta_server::metrics::InputHealth {
        dropped_packets: counters.dropped_packets(),
        gaps_detected: counters.gaps_detected(),
        malformed_packets: counters.malformed_packets(),
    }
}

fn start_spot_server(
    config_path: &std::path::Path,
    sample_rate_hz: f64,
    epoch: std::time::SystemTime,
    session_nonce: u128,
) -> Result<(tokio::runtime::Runtime, SpotServer)> {
    // MAN-59: the daemon's only durable record of connection events/
    // rejections was the live Prometheus counters (no history, reset on
    // restart) -- nothing to reconstruct WHAT happened or FROM WHERE
    // after an abuse incident. `try_init` (not `init`, which panics on a
    // second call) since this function is the sole place the daemon's
    // Tokio runtime is constructed, but a defensive no-op on an
    // already-initialized global subscriber costs nothing. `RUST_LOG`
    // overrides; unset defaults to `info` -- connection/rejection events
    // below are logged at `info`/`warn`, so an operator gets useful
    // output with zero configuration, and can raise verbosity for deeper
    // debugging without a code change.
    //
    // MAN-59 review round 6 (P1): `fmt()` writes to stdout by default,
    // but `Command::Run --json` ALSO writes DecoderEvents/spots as
    // JSON Lines to stdout (below) -- AGENTS.md's "file input ->
    // byte-identical spot logs" hard requirement means any interleaved
    // non-JSON tracing line corrupts that machine-readable stream for
    // real consumers and breaks deterministic-replay byte-identity.
    // stderr is a separate stream a JSON-Lines consumer never reads.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let cfg_text = std::fs::read_to_string(config_path)?;
    let file: manta_server::config::DaemonConfigFile = toml::from_str(&cfg_text)?;
    let rbn_uplink_cfgs = file.rbn_uplink.clone();
    let cfg = file.server;

    let bus = std::sync::Arc::new(manta_server::bus::SpotBus::new(
        sample_rate_hz,
        epoch,
        session_nonce,
    ));
    let metrics = std::sync::Arc::new(manta_server::metrics::Metrics::new());
    let cty = std::sync::Arc::new(manta_spot::cty::Table::parse(manta_spot::CTY_DAT));
    let decoder_version = format!("manta-{}", env!("CARGO_PKG_VERSION"));
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let tasks = manta_server::tasks::new_client_tasks();

    let rt = tokio::runtime::Runtime::new()?;
    let metrics_addr = rt.block_on(async {
        let telnet_listener =
            tokio::net::TcpListener::bind((cfg.bind_addr.as_str(), cfg.telnet_port)).await?;
        let json_listener =
            tokio::net::TcpListener::bind((cfg.bind_addr.as_str(), cfg.json_port)).await?;
        let metrics_listener =
            tokio::net::TcpListener::bind((cfg.bind_addr.as_str(), cfg.metrics_port)).await?;
        // Captured before the listener moves into metrics_http::serve
        // below (MAN-44) -- see SpotServer::metrics_addr's doc comment.
        let metrics_addr = metrics_listener.local_addr()?;

        let telnet_ip_command_limiter = manta_server::rate_limit::IpRateLimiter::new_with_override(
            manta_server::telnet::MAX_TELNET_COMMANDS,
            manta_server::telnet::COMMAND_RATE_WINDOW,
            cfg.telnet_max_commands_per_ip,
        );
        manta_server::rate_limit::spawn_stale_entry_reaper(telnet_ip_command_limiter.clone());
        tokio::spawn(manta_server::telnet::serve(
            telnet_listener,
            bus.clone(),
            metrics.clone(),
            cfg.station_callsign.clone(),
            shutdown_rx.clone(),
            tasks.clone(),
            manta_server::tasks::new_connection_limiter(
                manta_server::telnet::MAX_TELNET_CONNECTIONS,
            ),
            manta_server::tasks::IpQuota::new_with_override(
                manta_server::telnet::MAX_TELNET_CONNECTIONS_PER_IP,
                cfg.telnet_max_connections_per_ip,
            ),
            telnet_ip_command_limiter,
            manta_server::tasks::CLIENT_DRAIN_DEADLINE,
        ));
        let json_ip_ping_limiter = manta_server::rate_limit::IpRateLimiter::new_with_override(
            manta_server::json_stream::MAX_INBOUND_PINGS,
            manta_server::json_stream::PING_RATE_WINDOW,
            cfg.json_max_pings_per_ip,
        );
        manta_server::rate_limit::spawn_stale_entry_reaper(json_ip_ping_limiter.clone());
        tokio::spawn(manta_server::json_stream::serve(
            json_listener,
            manta_server::json_stream::JsonStreamConfig {
                bus: bus.clone(),
                metrics: metrics.clone(),
                cty: cty.clone(),
                station_call: cfg.station_callsign.clone(),
                decoder_version,
                // .clone(): MAN-32/MAN-42's uplink::serve spawns below also
                // need shutdown_rx -- can't let this be the moving consumer
                // anymore now that there are more consumers.
                shutdown: shutdown_rx.clone(),
                drain_deadline: manta_server::tasks::CLIENT_DRAIN_DEADLINE,
            },
            tasks.clone(),
            manta_server::tasks::new_connection_limiter(
                manta_server::json_stream::MAX_JSON_STREAM_CONNECTIONS,
            ),
            manta_server::tasks::IpQuota::new_with_override(
                manta_server::json_stream::MAX_JSON_STREAM_CONNECTIONS_PER_IP,
                cfg.json_max_connections_per_ip,
            ),
            json_ip_ping_limiter,
        ));
        // Reaps completed per-client tasks continuously, independent of
        // shutdown -- without this, `tasks` only ever shrinks at
        // shutdown_runtime_after_drain's one-time `await_all`, so ordinary
        // connect/disconnect churn grows it without bound for the life of
        // the process (round-11 review finding).
        manta_server::tasks::spawn_reaper(tasks.clone());
        // MAN-44 review: the ENTIRE uplink registry is published before
        // `metrics_http::serve` is spawned below, never after. This is a
        // multi-thread runtime, so the metrics/status endpoint starts
        // accepting on another worker the instant its task is spawned --
        // a readiness probe that raced this loop could observe a
        // half-registered (or empty) registry and be told `disabled`, or
        // a healthy-looking subset, for a daemon whose configured
        // targets are in fact down. Registering first makes "every
        // configured target is visible" true before the endpoint that
        // reports it can be reached at all. The uplink tasks themselves
        // are spawned afterwards -- a registered-but-not-yet-connected
        // target reads as down/flapping, which is the honest answer
        // during startup, whereas an unregistered one reads as
        // not-configured, which is a lie.
        let uplink_specs = manta_server::uplink::target_specs(&rbn_uplink_cfgs);
        let uplink_tasks: Vec<_> = rbn_uplink_cfgs
            .into_iter()
            .zip(uplink_specs)
            .map(|(uplink_cfg, spec)| (uplink_cfg, metrics.register_uplink_target(spec)))
            .collect();
        tokio::spawn(manta_server::metrics_http::serve(
            metrics_listener,
            metrics.clone(),
            manta_server::tasks::new_connection_limiter(
                manta_server::metrics_http::MAX_METRICS_CONNECTIONS,
            ),
            manta_server::tasks::IpQuota::new_with_override(
                manta_server::metrics_http::MAX_METRICS_CONNECTIONS_PER_IP,
                cfg.metrics_max_connections_per_ip,
            ),
        ));
        // MAN-32/MAN-42/MAN-44: one independent uplink::serve task per
        // configured [[rbn_uplink]] entry -- the common case for existing
        // single-node operators is no [[rbn_uplink]] tables at all (empty
        // Vec, loop body never runs), and uplink::serve itself also
        // no-ops when `enabled = false` (belt-and-suspenders, not a
        // duplicate check: this loop additionally avoids spawning a task
        // at all when the Vec is empty). Each task owns its own SpotBus
        // subscription and backoff state, so one target being down never
        // affects another's delivery or retry timing. Registered BEFORE
        // spawning (MAN-44, loop above) -- so a target that is disabled,
        // or that never manages a single successful connection, still
        // appears in `manta status`: an operator must be able to tell
        // "configured and stuck" from "not configured at all".
        for (uplink_cfg, target) in uplink_tasks {
            tokio::spawn(manta_server::uplink::serve(
                uplink_cfg,
                cfg.station_callsign.clone(),
                bus.clone(),
                target,
                shutdown_rx.clone(),
            ));
        }

        anyhow::Ok(metrics_addr)
    })?;

    Ok((
        rt,
        SpotServer {
            bus,
            metrics,
            metrics_addr,
            shutdown_tx,
            tasks,
            station_geography_unresolved: geography_is_unresolved(&cty, &cfg.station_callsign),
            cty,
        },
    ))
}

/// Resolves the address(es) `manta status` should DIAL to reach a running
/// daemon's metrics/status listener (MAN-44). An explicit `--addr` always
/// wins; otherwise a `--server-config`'s `[server]` table supplies the
/// port, with its `bind_addr` translated to a real dialable address --
/// `0.0.0.0`/`::` mean "listening on every interface," which isn't itself
/// something a client can connect TO, so those collapse to loopback (the
/// one address guaranteed to reach a same-host daemon). With neither,
/// falls back to the documented default metrics port on loopback.
///
/// Returns every address a hostname resolves to, not just the first
/// (code-review fix): `ToSocketAddrs` on a hostname can return several
/// candidates in resolver-dependent order -- e.g. `localhost` resolving
/// `::1` before `127.0.0.1` on a dual-stack host -- and the daemon's own
/// default `bind_addr = "0.0.0.0"` only listens on IPv4. Keeping just
/// `.next()` picked whichever candidate the resolver happened to list
/// first, reporting a healthy daemon as unreachable whenever that guess
/// was wrong. `fetch_status` tries every returned address in turn (same
/// precedent as `uplink::connect_first_reachable`).
fn resolve_status_addr(
    addr: Option<&str>,
    server: Option<&manta_server::config::ServerConfig>,
) -> Result<Vec<std::net::SocketAddr>> {
    if let Some(addr) = addr {
        if let Ok(sock) = addr.parse() {
            return Ok(vec![sock]);
        }
        // CR-B applies equally here: the daemon accepts a hostname in its
        // own `bind_addr` (resolved via `ToSocketAddrs` in
        // `start_spot_server`), and the runbook tells operators to reach a
        // remote daemon with `--addr <host>:<metrics_port>` -- rejecting a
        // literal-IP-only `--addr` would contradict both.
        use std::net::ToSocketAddrs;
        let addrs: Vec<_> = addr
            .to_socket_addrs()
            .with_context(|| format!("invalid --addr {addr:?}"))?
            .collect();
        if addrs.is_empty() {
            bail!("--addr {addr:?} resolved to no addresses");
        }
        return Ok(addrs);
    }
    let Some(server) = server else {
        return Ok(vec![std::net::SocketAddr::from(([127, 0, 0, 1], 7302))]);
    };
    match server.bind_addr.as_str() {
        "0.0.0.0" => Ok(vec![std::net::SocketAddr::new(
            std::net::Ipv4Addr::LOCALHOST.into(),
            server.metrics_port,
        )]),
        "::" => Ok(vec![std::net::SocketAddr::new(
            std::net::Ipv6Addr::LOCALHOST.into(),
            server.metrics_port,
        )]),
        other => match other.parse::<std::net::IpAddr>() {
            Ok(ip) => Ok(vec![std::net::SocketAddr::new(ip, server.metrics_port)]),
            // CR-B: the daemon itself binds `bind_addr` through
            // `TcpListener::bind((host, port))`, which resolves a
            // hostname via `ToSocketAddrs` (main.rs's `start_spot_server`)
            // rather than requiring a literal IP -- so `bind_addr =
            // "localhost"` is a config the daemon happily runs on. `manta
            // status` must resolve the same way instead of rejecting a
            // config the daemon itself accepts.
            Err(_) => {
                use std::net::ToSocketAddrs;
                let addrs: Vec<_> = (other, server.metrics_port)
                    .to_socket_addrs()
                    .with_context(|| format!("resolving server.bind_addr {other:?}"))?
                    .collect();
                if addrs.is_empty() {
                    bail!("server.bind_addr {other:?} resolved to no addresses");
                }
                Ok(addrs)
            }
        },
    }
}

/// Renders a list of candidate addresses for an error message.
fn format_addrs(addrs: &[std::net::SocketAddr]) -> String {
    addrs
        .iter()
        .map(std::net::SocketAddr::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The full pre-render `manta status` flow: read/parse an optional
/// `--config` (a.k.a. the deprecated `--server-config` alias), resolve
/// the dial address, and fetch+parse the daemon's `/status` document.
/// Extracted so every failure along this path -- not just
/// `fetch_status`'s -- goes through the same exit-2 handling (CR-A).
///
/// `resolve_status_addr` runs INSIDE the tokio runtime, on the blocking
/// pool (MAN-44 code review CR-2): the previous version called it before
/// the runtime -- and therefore before any timer -- existed, so
/// `--timeout-secs` bounded connect+read but not the blocking
/// `ToSocketAddrs` lookup a hostname `--addr` or `bind_addr` triggers. An
/// unreachable or slow resolver then blocked for the OS's own
/// `resolv.conf` budget (commonly 10-40s) regardless of what the operator
/// asked for.
///
/// Resolution and the fetch share ONE end-to-end deadline rather than a
/// timeout window each (see `deadline`/`remaining` in the body): giving
/// each leg its own full `timeout` would let `--timeout-secs 5` take
/// nearly ten seconds, twice the give-up bound the flag advertises.
fn run_status(
    server_config: Option<&std::path::Path>,
    addr: Option<&str>,
    timeout_secs: u64,
) -> Result<manta_server::status::StatusDoc> {
    let file = server_config
        .map(|path| -> Result<manta_server::config::DaemonConfigFile> {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
        })
        .transpose()?;
    let timeout = std::time::Duration::from_secs(timeout_secs);
    let addr_owned = addr.map(str::to_string);
    let server = file.map(|f| f.server);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let outcome = rt.block_on(async move {
        // MAN-44 review: ONE end-to-end deadline spans resolution and the
        // fetch. Giving each leg its own full `timeout` let a lookup that
        // finished just under the wire be followed by a fresh, full-length
        // connect/read window, so `--timeout-secs 5` could take nearly ten
        // seconds -- twice the give-up bound the flag advertises, and twice
        // what a cron/Nagios check budgeted for.
        let deadline = tokio::time::Instant::now() + timeout;
        let targets = tokio::time::timeout_at(
            deadline,
            tokio::task::spawn_blocking(move || {
                resolve_status_addr(addr_owned.as_deref(), server.as_ref())
            }),
        )
        .await
        .map_err(|_| anyhow!("resolving the daemon's address timed out"))?
        .context("resolving the daemon's address panicked")??;
        // Only what's LEFT of the deadline goes to the fetch. A zero
        // remainder is not special-cased: `fetch_status` bounds itself with
        // this duration and reports the same "timed out talking to ..."
        // error it would for any other exhausted budget.
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        fetch_status(&targets, remaining)
            .await
            .with_context(|| format!("could not reach daemon at {}", format_addrs(&targets)))
    });
    // MAN-44 review: the timeout above only DROPS the JoinHandle -- a
    // `spawn_blocking` task cannot be aborted once it is running, so a
    // wedged `ToSocketAddrs` keeps occupying a blocking-pool thread after
    // `--timeout-secs` has already elapsed. Letting `rt` drop here would
    // then block the caller a second time, for the resolver's own
    // `resolv.conf` budget, which is exactly the wait `--timeout-secs`
    // exists to bound: the timeout would fire and the command would still
    // hang, wedging a monitoring invocation. `shutdown_background`
    // detaches the runtime instead of joining it, so the bound the
    // operator asked for is the bound they get; the orphaned lookup is
    // pure-read, owns no caller-visible state, and dies with the process
    // (which `Command::Status` reaches immediately after this returns).
    rt.shutdown_background();
    outcome
}

/// Exit code contract for scripting (cron/Nagios-style): `0` when every
/// enabled uplink target is connected, or there's no uplink configured at
/// all; `1` when the daemon was reached but the uplink is unhealthy. (A
/// third case -- `2`, "could not reach or parse the daemon's status at
/// all" -- is returned directly by `Command::Status`'s handler, since it
/// never gets as far as a `StatusDoc` to pass here.)
fn status_exit_code(doc: &manta_server::status::StatusDoc) -> i32 {
    use manta_server::metrics::OverallUplinkHealth;
    match doc.uplink.health {
        OverallUplinkHealth::Ok | OverallUplinkHealth::Disabled => 0,
        OverallUplinkHealth::Degraded | OverallUplinkHealth::Down => 1,
    }
}

/// Bounds a fetched status document's body size (MAN-44): a real status
/// document is kilobytes at most, but a wrong or hostile endpoint
/// answering `GET /status` must not be able to make this allocate without
/// bound.
const MAX_STATUS_BODY_BYTES: u64 = 256 * 1024;
/// Bounds the status line plus header block the same way
/// `manta_server::metrics_http`'s own `MAX_HEADER_LINES` bounds its side of
/// the identical parsing job (CR-E) -- without this, an unbounded
/// `read_line`-per-line loop against a hostile or misbehaving peer that
/// never terminates a line, or never ends its header block, could grow a
/// buffer or spin without bound; the body already had `MAX_STATUS_BODY_BYTES`
/// but the status line and headers did not.
const MAX_STATUS_HEADER_LINES: usize = 100;

/// Fetches and parses `GET /status` from a running daemon's metrics
/// listener (MAN-44) -- a small hand-rolled HTTP/1.1 GET, matching
/// `manta_server::metrics_http`'s own hand-rolled precedent rather than
/// adding an HTTP client dependency for one request. The whole operation
/// (connect, write, read) is bounded by `timeout` so a silent or
/// half-open peer can't hang `manta status` indefinitely.
async fn fetch_status(
    addrs: &[std::net::SocketAddr],
    timeout: std::time::Duration,
) -> Result<manta_server::status::StatusDoc> {
    tokio::time::timeout(timeout, fetch_status_inner(addrs, timeout))
        .await
        .map_err(|_| anyhow!("timed out talking to {}", format_addrs(addrs)))?
}

/// Tries every candidate in turn, returning the first that accepts a TCP
/// connection, or the last error if all of them fail (code-review fix --
/// see `resolve_status_addr`). Same precedent as
/// `uplink::connect_first_reachable`: a hostname resolving to more than
/// one address must not make the CLI give up after the first, resolver-
/// order-dependent candidate.
async fn connect_any(
    addrs: &[std::net::SocketAddr],
    overall_timeout: std::time::Duration,
) -> std::io::Result<(tokio::net::TcpStream, std::net::SocketAddr)> {
    connect_any_bounded(addrs, overall_timeout, tokio::net::TcpStream::connect).await
}

/// `connect_any`'s real logic, with the per-candidate connect operation
/// injectable (MAN-44 code review CR-1) so the fall-through-past-a-stalled-
/// candidate behavior is unit-testable with a fake, instantly-controllable
/// "hangs forever" attempt under paused tokio time -- same precedent as
/// `uplink::connect_first_reachable_bounded`. Each candidate gets its own
/// slice of `overall_timeout` (split evenly across every candidate) rather
/// than a bare, unbounded `TcpStream::connect`: without a per-candidate
/// bound, a first address that silently black-holes SYNs (a firewall drop,
/// not a refusal) consumed `fetch_status`'s ENTIRE outer timeout before a
/// later, live candidate was ever dialled -- the exact failure this
/// function exists to prevent, defeated by having no bound of its own.
async fn connect_any_bounded<T, F, Fut>(
    addrs: &[std::net::SocketAddr],
    overall_timeout: std::time::Duration,
    connect: F,
) -> std::io::Result<(T, std::net::SocketAddr)>
where
    F: Fn(std::net::SocketAddr) -> Fut,
    Fut: std::future::Future<Output = std::io::Result<T>>,
{
    let per_addr_timeout = overall_timeout / (addrs.len().max(1) as u32);
    let mut last_err = None;
    for &addr in addrs {
        match tokio::time::timeout(per_addr_timeout, connect(addr)).await {
            Ok(Ok(stream)) => return Ok((stream, addr)),
            Ok(Err(e)) => last_err = Some(e),
            Err(_) => {
                last_err = Some(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("connect to {addr} timed out"),
                ));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "no addresses to try")
    }))
}

async fn fetch_status_inner(
    addrs: &[std::net::SocketAddr],
    timeout: std::time::Duration,
) -> Result<manta_server::status::StatusDoc> {
    use manta_server::bounded_io::read_line_bounded;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};

    let (mut stream, addr) = connect_any(addrs, timeout)
        .await
        .with_context(|| format!("connecting to {}", format_addrs(addrs)))?;
    stream
        .write_all(
            format!("GET /status HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .context("writing the status request")?;

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    read_line_bounded(&mut reader, &mut status_line)
        .await
        .context("reading the status line")?;
    if !status_line.starts_with("HTTP/1.1 200") {
        bail!("daemon returned {}", status_line.trim_end());
    }

    for _ in 0..MAX_STATUS_HEADER_LINES {
        let mut line = String::new();
        let n = read_line_bounded(&mut reader, &mut line)
            .await
            .context("reading response headers")?;
        if n == 0 || line == "\r\n" {
            break;
        }
    }

    let mut body = Vec::new();
    reader
        .take(MAX_STATUS_BODY_BYTES)
        .read_to_end(&mut body)
        .await
        .context("reading the status body")?;
    let body = String::from_utf8(body).context("status body was not valid UTF-8")?;
    parse_status_doc(&body)
}

/// Parses a `/status` body AND enforces its `schema_version` (Codex
/// review, PR #95).
///
/// `schema_version` only earns its place in the document if someone
/// checks it: a newer daemon that removed or re-meaning'd a field can
/// still deserialize structurally into this build's `StatusDoc` -- serde
/// ignores unknown keys and every field this build requires may well
/// still be present -- and `manta status` would then render it, and pick
/// an exit code from it, under semantics that no longer hold. An operator
/// running a monitoring one-liner would get a confident `0`/`1` computed
/// from a document this build cannot actually interpret.
///
/// So a version this build does not understand is a hard error, which
/// `Command::Status` turns into the documented exit 2 ("could not reach
/// or parse the daemon's status at all", `docs/RUNBOOKS/uplink-health.md`)
/// -- deliberately NOT exit 1, which means "asked, and the uplink is
/// unhealthy". Version skew is a tooling problem, not an uplink problem.
///
/// Separated from `fetch_status_inner` so this is testable without a
/// socket; `fetch_status_inner` has no other post-read logic to keep.
fn parse_status_doc(body: &str) -> Result<manta_server::status::StatusDoc> {
    use manta_server::status::STATUS_SCHEMA_VERSION;

    let doc: manta_server::status::StatusDoc =
        serde_json::from_str(body).context("status body was not a valid status document")?;
    if doc.schema_version != STATUS_SCHEMA_VERSION {
        bail!(
            "daemon reports status schema_version {} but this manta build understands only {} \
             -- upgrade whichever of the daemon and the CLI is older",
            doc.schema_version,
            STATUS_SCHEMA_VERSION
        );
    }
    Ok(doc)
}

fn main() -> Result<()> {
    warn_deprecations();
    match Cli::parse().command {
        Command::Decode {
            path,
            json,
            freq_correction_ppm,
            allowlist,
            blocklist,
            notch,
            config,
            engine,
        } => {
            let decode_from_file = load_decode_config_file(config.as_deref())?;
            let decode_cfg = merge_cli_engine(engine, decode_from_file);
            let mut cfg = build_pipeline_config(
                freq_correction_ppm,
                allowlist,
                blocklist,
                notch,
                decode_cfg.engine,
            )?;
            cfg.decode = decode_cfg;
            let report = decode_wav(&path, &cfg)?;
            if json {
                println!("{}", serde_json::to_string(&report)?);
            } else {
                println!("{}", report.text);
                eprintln!("freq_hz: {:.1}  wpm: {:?}", report.freq_hz, report.wpm);
                eprintln!("spots: {}", report.spots.len());
            }
        }
        Command::Oracle {
            path,
            rbn_csv,
            spotter,
            capture_start,
            window_s,
            config,
            engine,
            jsonl,
        } => {
            let mut src = manta_input::WavIqSource::open(&path)?;
            let (fs, center) = (src.sample_rate(), src.center_freq_hz());
            let iq = manta_input::read_all(&mut src)?;
            let spots = manta_testkit::oracle::parse_rbn_spots(&rbn_csv, &spotter, capture_start)?;
            let decode_from_file = load_decode_config_file(config.as_deref())?;
            let cfg = merge_cli_engine(engine, decode_from_file);
            let (results, summary) =
                manta_testkit::oracle::run_oracle(&iq, fs, center, &spots, window_s, &cfg)?;
            if let Some(p) = jsonl {
                let mut w = std::io::BufWriter::new(std::fs::File::create(p)?);
                for r in &results {
                    use std::io::Write;
                    writeln!(w, "{}", serde_json::to_string(r)?)?;
                }
            }
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
        Command::Gen { vector, out } => {
            let spec = match vector.as_str() {
                "v1" => manta_testkit::vectors::v1(),
                "v2" => manta_testkit::vectors::v2(),
                "v3" => manta_testkit::vectors::v3(),
                "v4" => manta_testkit::vectors::v4(),
                "v5" => manta_testkit::vectors::v5(),
                "v6" => manta_testkit::vectors::v6(),
                "vr1" => manta_testkit::vectors::vr1(),
                "vr2" => manta_testkit::vectors::vr2(),
                "vr3" => manta_testkit::vectors::vr3(),
                "vr4" => manta_testkit::vectors::vr4(),
                "vr5" => manta_testkit::vectors::vr5(),
                "vr6a" => manta_testkit::vectors::vr6a(),
                "vr6b" => manta_testkit::vectors::vr6b(),
                "vr7" => manta_testkit::vectors::vr7(),
                "vr8" => manta_testkit::vectors::vr8(),
                other => bail!(
                    "unknown vector {other:?} (available: v1-v6, vr1-vr5, vr6a, vr6b, vr7-vr8)"
                ),
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
        Command::Run {
            device,
            source,
            kiwi_host,
            kiwi_port,
            kiwi_freq,
            kiwi_password,
            json,
            freq_correction_ppm,
            allowlist,
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
            #[cfg(feature = "hpsdr")]
            hpsdr_host,
            #[cfg(feature = "hpsdr")]
            hpsdr_port,
            #[cfg(feature = "hpsdr")]
            hpsdr_freq,
            #[cfg(feature = "hpsdr")]
            hpsdr_rate,
            config,
            dial_freq_hz,
            replay_epoch,
            engine,
        } => {
            let is_file_replay = source.is_some();
            // Captured before `open_source` consumes `source` below --
            // needed to derive a recording-specific replay epoch.
            let replay_path = source.clone();
            #[cfg(feature = "soapy")]
            let has_soapy_source = soapy_driver.is_some();
            #[cfg(not(feature = "soapy"))]
            let has_soapy_source = false;
            #[cfg(feature = "hpsdr")]
            let has_hpsdr_source = hpsdr_host.is_some();
            #[cfg(not(feature = "hpsdr"))]
            let has_hpsdr_source = false;
            let has_rf_aware_source = kiwi_host.is_some() || has_soapy_source || has_hpsdr_source;
            let source_name = if kiwi_host.is_some() {
                "kiwi"
            } else if has_soapy_source {
                "soapy"
            } else if has_hpsdr_source {
                "hpsdr"
            } else if is_file_replay {
                "file"
            } else {
                "audio"
            };

            if config.is_some() && !has_rf_aware_source && dial_freq_hz.is_none() {
                bail!(
                    "--dial-freq-hz is required with --config when using a plain \
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
            // SPEC v2 §7: `[decode]` (from --config, if given) is
            // the baseline; an explicit --engine overrides just its
            // `engine` key (merge_cli_engine). `engine = "hsmm"` is a fully
            // implemented and reviewed engine (Task 8) and needs no gate
            // here as of Task 11.
            let decode_from_file = load_decode_config_file(config.as_deref())?;
            let decode_cfg = merge_cli_engine(engine, decode_from_file);
            let mut cfg = build_pipeline_config(
                freq_correction_ppm,
                allowlist,
                blocklist,
                notch,
                decode_cfg.engine,
            )?;
            cfg.decode = decode_cfg;
            #[cfg(feature = "hpsdr")]
            let hpsdr_source = open_hpsdr_source(HpsdrOpts {
                host: hpsdr_host,
                port: hpsdr_port,
                freq: hpsdr_freq,
                rate: hpsdr_rate,
            })?;
            #[cfg(not(feature = "hpsdr"))]
            let hpsdr_source: Option<Box<dyn IqSource>> = None;
            let src = match hpsdr_source {
                Some(src) => src,
                None => {
                    #[cfg(feature = "soapy")]
                    {
                        open_source(
                            device,
                            source,
                            kiwi,
                            SoapyOpts {
                                driver: soapy_driver,
                                freq: soapy_freq,
                                rate: soapy_rate,
                                gain: soapy_gain,
                            },
                        )?
                    }
                    #[cfg(not(feature = "soapy"))]
                    {
                        open_source(device, source, kiwi)?
                    }
                }
            };
            let src: Box<dyn IqSource> = match dial_freq_hz {
                Some(freq_hz) => Box::new(FixedCenterFreqSource {
                    inner: src,
                    freq_hz,
                }),
                None => src,
            };

            // Kept alive for the process lifetime: dropping it would stop
            // the spawned server tasks. `None` when --config wasn't
            // given, in which case `spot_server` stays None too. `epoch`/
            // `session_nonce` are deliberately computed IN this branch, not
            // above it -- `--source`-only replay (no --config) never
            // consumes either, and computing `session_nonce` means hashing
            // the entire replayed file a second time after it's already
            // been opened; skip that full-file pass entirely when nothing
            // downstream needs it (round-7 review finding).
            let (server_runtime, spot_server, active_tracks, mut active_tracks_poller) =
                match config {
                    Some(path) => {
                        // `epoch` feeds SpotBus's wall-clock conversion (every
                        // JSON `timestamp`/RBN Zulu field a client observes) --
                        // a live session's epoch is this process's real start
                        // time; a replay session's defaults to the replayed
                        // file's own mtime, a genuine timestamp that's stable
                        // across reruns of the SAME untouched file, but changes
                        // across a copy/download/restore that doesn't preserve
                        // filesystem metadata even though the recording's
                        // content is identical -- pass --replay-epoch to pin an
                        // exact value when that matters more than "whatever
                        // this machine's copy says" (round-7 review finding;
                        // see the flag's own doc comment for the full
                        // rationale, and `epoch_for_replay_path`'s for why
                        // neither "always now()" nor a content-hash alone was
                        // right before this flag existed). `session_nonce` is
                        // the separate, spot-id-uniqueness-only value:
                        // recording-content-derived for file replay (so
                        // different recordings never collide on id even at the
                        // same track/sample position), nanosecond-precision-now
                        // for a live session (so two live sessions started
                        // within the same wall-clock second don't collide
                        // either).
                        let epoch = resolve_epoch(replay_path.as_deref(), replay_epoch)?;
                        let session_nonce: u128 = match &replay_path {
                            Some(replay_path) => session_nonce_for_replay_path(replay_path)?,
                            // Live session: `epoch` above is already SystemTime::now().
                            None => epoch
                                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                                .expect("epoch predates the Unix epoch")
                                .as_nanos(),
                        };

                        let (rt, server) =
                            start_spot_server(&path, src.sample_rate(), epoch, session_nonce)?;
                        // MAN-45 (round-9 finding): the daemon's own copy of the
                        // gauge `manta_engine::listen_with_observers` updates as
                        // it runs (on the MAIN thread, outside this tokio
                        // runtime) -- polled into `Metrics` below, the same
                        // bridge shape MAN-55's `confirmed_live_handle` watcher
                        // uses for source liveness.
                        let active_tracks =
                            std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
                        // MAN-45 remediate (code-review finding 1): the
                        // `JoinHandle` is kept, not discarded, so the shutdown
                        // sequence below can abort this poller and WAIT for it
                        // to actually stop before writing the deterministic
                        // zero -- otherwise a tick already in flight can read
                        // the still-stale gauge and write it right back after
                        // the zero, undoing it.
                        let active_tracks_poller = {
                            let gauge = active_tracks.clone();
                            let track_metrics = server.metrics.clone();
                            rt.spawn(async move {
                                loop {
                                    track_metrics.set_active_tracks(
                                        gauge.load(std::sync::atomic::Ordering::Relaxed),
                                    );
                                    tokio::time::sleep(ACTIVE_TRACKS_POLL_INTERVAL).await;
                                }
                            })
                        };
                        // MAN-55: for a source where `open()` succeeding
                        // doesn't confirm a live device (HPSDR's UDP
                        // connect/send need no peer response at all),
                        // `confirmed_live_handle()` returns Some, and health
                        // starts false, flipping true only once the source's
                        // own read loop has actually processed a valid
                        // packet. Every other source type (Kiwi/Soapy/audio/
                        // file) returns None from the trait's default and
                        // keeps the original immediate-true behavior, since
                        // opening those already implies liveness.
                        match src.confirmed_live_handle() {
                            Some(live) => {
                                server.metrics.set_source_health(source_name, false);
                                let metrics = server.metrics.clone();
                                rt.spawn(async move {
                                    while !live.load(std::sync::atomic::Ordering::Relaxed) {
                                        tokio::time::sleep(std::time::Duration::from_millis(200))
                                            .await;
                                    }
                                    metrics.set_source_health(source_name, true);
                                });
                            }
                            None => server.metrics.set_source_health(source_name, true),
                        }

                        // MAN-56: HPSDR's packet loss/malformed counters are
                        // input-layer state manta-server cannot compute itself
                        // (it has no manta-input dependency). Sample them into
                        // Metrics on a timer, the same wiring-layer-injection
                        // shape `set_source_health` uses above -- and read the
                        // handle HERE, before `listen(src, ..)` below takes
                        // ownership of the source for the rest of the run.
                        // Sources with no wire-packet loss model return None
                        // and publish no series at all, which is deliberate:
                        // a permanently-zero counter reads as "no loss" rather
                        // than "not measured" (ARCHITECTURE §8's
                        // "absent means not measured" distinction).
                        if let Some(counters) = src.health_counters() {
                            let metrics = server.metrics.clone();
                            // Published once eagerly so the series exists (at
                            // 0) from the very first scrape rather than only
                            // after one poll interval.
                            metrics.set_input_health(source_name, input_health_of(&counters));
                            rt.spawn(async move {
                                loop {
                                    tokio::time::sleep(INPUT_HEALTH_POLL_INTERVAL).await;
                                    metrics
                                        .set_input_health(source_name, input_health_of(&counters));
                                }
                            });
                        }

                        (
                            Some(rt),
                            Some(server),
                            Some(active_tracks),
                            Some(active_tracks_poller),
                        )
                    }
                    None => (None, None, None, None),
                };

            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let stop_handler = stop.clone();
            ctrlc::set_handler(move || {
                stop_handler.store(true, std::sync::atomic::Ordering::Relaxed);
            })?;
            // Printed AFTER the handler is installed, and via `eprintln!`
            // rather than `tracing::info!` because the subscriber is only
            // initialized inside `start_spot_server` -- a plain `listen`
            // (no --server-config) has no subscriber at all. Two jobs:
            // `listen` otherwise prints nothing at startup (2026-09-05
            // review, lens 1 #4/#7), and it is the readiness handshake
            // `tests/signal_shutdown.rs` waits for -- signalling any
            // earlier races `set_handler` and kills the child under the OS
            // default disposition regardless of MAN-85's fix. If the
            // fuller startup banner (lens 1 #7) ever replaces this line,
            // it must still be emitted here, after `set_handler`, and
            // `READY_MARKER` updated to match. stdout stays pure JSON
            // under `--json` (MAN-59 round 6); this goes to stderr.
            eprintln!("manta: listening; send SIGINT or SIGTERM to stop");
            let listen_result = manta_engine::listen_with_observers(
                src,
                &cfg,
                stop,
                manta_engine::ListenObservers {
                    active_tracks: active_tracks.clone(),
                },
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
                // ARCHITECTURE §7), fed here when --config is set.
                |spot| {
                    if let Some(server) = &spot_server {
                        server.bus.publish(spot.clone());
                        server.metrics.record_spot();
                        // MAN-136/MAN-45: counted ONCE per spot here, NOT
                        // inside `SpotMessage::from_spot` -- that runs once
                        // per connected JSON/WS client (json_stream.rs:126),
                        // so counting there would scale with client count
                        // instead of spot count. Checks BOTH sides: the
                        // operator's own station_callsign is config, not
                        // decoder output, and isn't required to resolve --
                        // but it also never changes, so its side is
                        // resolved once at start_spot_server time.
                        if geography_is_unresolved(&server.cty, &spot.callsign)
                            || server.station_geography_unresolved
                        {
                            server.metrics.record_unresolved_geography();
                        }
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
            );

            // Run the same server-shutdown sequence on BOTH the success and
            // error paths -- an SDR disconnect or WAV read failure from
            // `listen` must not skip draining already-published spots or
            // abort in-flight client writes with no chance to finish, which
            // a bare `listen(...)?` before this block used to do on any
            // error (round-7 review finding). Explicitly signal the client
            // tasks to drain (e.g. spots from TrackManager::finish() just
            // before `listen` returned) before tearing the runtime down.
            if let Some(server) = &spot_server {
                // MAN-45 remediate (code-review finding 1): the engine's
                // own gauge is already 0 on the SUCCESS path
                // (TrackManager::finish() closed every track before
                // `listen_with_observers` returned), but NOT on the ERROR
                // path -- `listen_result` above is deliberately captured
                // rather than `?`-ed so an SDR disconnect or WAV read
                // failure still runs this drain sequence (round-7 finding),
                // and on that path `listen_with_observers` returns before
                // reaching `tm.finish()`'s trailing zero, leaving the
                // shared `AtomicU64` at the last processed chunk's nonzero
                // count. The still-running poller reads that stale value
                // every `ACTIVE_TRACKS_POLL_INTERVAL` and would overwrite
                // the deterministic zero below within one tick if left
                // running -- abort it and AWAIT its actual termination
                // first (not just issue the abort and hope), so no
                // in-flight tick can race the zero-write below. A metrics
                // scrape landing anywhere in the `SHUTDOWN_DRAIN_DEADLINE`
                // window that follows must never see a stale nonzero
                // count for a daemon with no live tracks.
                if let Some(poller) = active_tracks_poller.take() {
                    poller.abort();
                    if let Some(rt) = server_runtime.as_ref() {
                        let _ = rt.block_on(poller);
                    }
                }
                server.metrics.set_active_tracks(0);
                let _ = server.shutdown_tx.send(true);
            }
            // `server_runtime`/`spot_server` are always constructed as a
            // matched pair (both `Some` or both `None`, see their
            // construction above) -- `zip` makes that invariant explicit
            // instead of a defensive branch for a case that can't happen.
            if let Some((rt, server)) = server_runtime.zip(spot_server.as_ref()) {
                shutdown_runtime_after_drain(rt, &server.tasks);
            }
            listen_result?;
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
            allowlist,
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
            #[cfg(feature = "hpsdr")]
            hpsdr_host,
            #[cfg(feature = "hpsdr")]
            hpsdr_port,
            #[cfg(feature = "hpsdr")]
            hpsdr_freq,
            #[cfg(feature = "hpsdr")]
            hpsdr_rate,
        } => {
            let kiwi = KiwiOpts {
                host: kiwi_host,
                port: kiwi_port,
                freq: kiwi_freq,
                password: kiwi_password,
            };
            let cfg = build_pipeline_config(
                freq_correction_ppm,
                allowlist,
                blocklist,
                notch,
                Engine::Legacy,
            )?;
            #[cfg(feature = "hpsdr")]
            let hpsdr_source = open_hpsdr_source(HpsdrOpts {
                host: hpsdr_host,
                port: hpsdr_port,
                freq: hpsdr_freq,
                rate: hpsdr_rate,
            })?;
            #[cfg(not(feature = "hpsdr"))]
            let hpsdr_source: Option<Box<dyn IqSource>> = None;
            let src = match hpsdr_source {
                Some(src) => src,
                None => {
                    #[cfg(feature = "soapy")]
                    {
                        open_source(
                            device,
                            source,
                            kiwi,
                            SoapyOpts {
                                driver: soapy_driver,
                                freq: soapy_freq,
                                rate: soapy_rate,
                                gain: soapy_gain,
                            },
                        )?
                    }
                    #[cfg(not(feature = "soapy"))]
                    {
                        open_source(device, source, kiwi)?
                    }
                }
            };
            let report = manta_engine::soak(src, &cfg, std::time::Duration::from_secs(duration))?;
            eprintln!("{report:?}");
            if !manta_engine::soak_passed(&report) {
                std::process::exit(1);
            }
        }
        Command::Status {
            config,
            addr,
            json,
            timeout_secs,
        } => {
            // CR-A: EVERY failure short of a parsed StatusDoc -- an
            // unreadable/unparseable --config, a bad --addr, or a
            // daemon that couldn't be reached -- exits 2 ("couldn't ask"),
            // distinct from exit 1 ("asked, the uplink is unhealthy",
            // `status_exit_code`). Letting the first two `?`-propagate out
            // of `main` used to exit 1 for those cases too, which is the
            // documented "uplink unhealthy" code
            // (`docs/RUNBOOKS/uplink-health.md`'s exit-code table) -- a
            // typo'd config path was indistinguishable from a genuinely
            // degraded uplink.
            let doc = match run_status(config.as_deref(), addr.as_deref(), timeout_secs) {
                Ok(doc) => doc,
                Err(e) => {
                    eprintln!("manta status: {e:#}");
                    std::process::exit(2);
                }
            };
            if json {
                print!("{}", doc.to_json());
            } else {
                print!("{}", manta_server::status::render_human(&doc));
            }
            std::process::exit(status_exit_code(&doc));
        }
        Command::Doctor {
            duration,
            device,
            source,
            kiwi_host,
            kiwi_port,
            kiwi_freq,
            kiwi_password,
            freq_correction_ppm,
            allowlist,
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
            #[cfg(feature = "hpsdr")]
            hpsdr_host,
            #[cfg(feature = "hpsdr")]
            hpsdr_port,
            #[cfg(feature = "hpsdr")]
            hpsdr_freq,
            #[cfg(feature = "hpsdr")]
            hpsdr_rate,
            json,
        } => {
            // Checked before any source is opened -- otherwise an invalid
            // --duration only surfaces after a KiwiSDR/SoapySDR/HPSDR
            // connect/activate already spent real time (or hung/failed for
            // an unrelated hardware reason), and the user never sees the
            // actual duration error at all (round-5 review finding).
            let duration_secs = duration;
            if !(manta_engine::MIN_DURATION.as_secs()..=manta_engine::MAX_DURATION.as_secs())
                .contains(&duration_secs)
            {
                bail!(
                    "--duration must be between {} and {} seconds, got {duration_secs}",
                    manta_engine::MIN_DURATION.as_secs(),
                    manta_engine::MAX_DURATION.as_secs()
                );
            }
            let kiwi = KiwiOpts {
                host: kiwi_host,
                port: kiwi_port,
                freq: kiwi_freq,
                password: kiwi_password,
            };
            let cfg = build_pipeline_config(
                freq_correction_ppm,
                allowlist,
                blocklist,
                notch,
                Engine::Legacy,
            )?;
            #[cfg(feature = "hpsdr")]
            let hpsdr_source = open_hpsdr_source(HpsdrOpts {
                host: hpsdr_host,
                port: hpsdr_port,
                freq: hpsdr_freq,
                rate: hpsdr_rate,
            })?;
            #[cfg(not(feature = "hpsdr"))]
            let hpsdr_source: Option<Box<dyn IqSource>> = None;
            let src = match hpsdr_source {
                Some(src) => src,
                None => {
                    #[cfg(feature = "soapy")]
                    {
                        open_source(
                            device,
                            source,
                            kiwi,
                            SoapyOpts {
                                driver: soapy_driver,
                                freq: soapy_freq,
                                rate: soapy_rate,
                                gain: soapy_gain,
                            },
                        )?
                    }
                    #[cfg(not(feature = "soapy"))]
                    {
                        open_source(device, source, kiwi)?
                    }
                }
            };
            let report = manta_engine::doctor(src, &cfg, std::time::Duration::from_secs(duration))?;
            if json {
                // `verdict()` is computed, not a stored field, so a plain
                // `serde_json::to_string(&report)` omits the command's
                // primary health classification entirely -- merge it in as
                // an extra key rather than making JSON consumers duplicate
                // the classification policy themselves.
                let mut value = serde_json::to_value(&report)?;
                if let serde_json::Value::Object(ref mut map) = value {
                    map.insert(
                        "verdict".to_string(),
                        serde_json::to_value(report.verdict())?,
                    );
                }
                println!("{}", serde_json::to_string(&value)?);
            } else {
                print_doctor_report(&report);
            }
        }
    }
    Ok(())
}

/// Which replaced CLI spelling the operator typed, if any.
///
/// D11/MAN-77 promoted `listen --server-config` to `run --config`. clap
/// cannot answer this: an alias is normalized to the subcommand's canonical
/// name inside `Parser::possible_subcommand` before `ArgMatches` ever sees
/// it, and `MatchedArg` records no alias spelling for flags either. So the
/// only source of truth is raw argv, read before `Cli::parse()`.
#[derive(Debug, PartialEq, Eq)]
enum Deprecation {
    /// `listen` used to start the daemon (i.e. with a config file). Plain
    /// `listen --device`/`--kiwi-host` is NOT deprecated -- MAN-77's title
    /// keeps `listen` for ad hoc audio/dev testing.
    ListenVerb,
    /// `--server-config`, under either verb.
    ServerConfigFlag,
}

fn deprecations<I: IntoIterator<Item = String>>(args: I) -> Vec<Deprecation> {
    let argv: Vec<String> = args.into_iter().collect();
    let is_flag = |name: &str| {
        argv.iter()
            .any(|a| a == name || a.strip_prefix(name).is_some_and(|r| r.starts_with('=')))
    };
    let mut out = Vec::new();
    let has_config = is_flag("--config") || is_flag("--server-config");
    if argv.get(1).map(String::as_str) == Some("listen") && has_config {
        out.push(Deprecation::ListenVerb);
    }
    if is_flag("--server-config") {
        out.push(Deprecation::ServerConfigFlag);
    }
    out
}

/// stderr, not `tracing::warn!`: the only `tracing_subscriber` init in this
/// binary lives inside `start_spot_server`, so a parse-time `warn!` would be
/// dropped. stderr is also the stream `--json`'s JSON Lines consumer never
/// reads (see `start_spot_server`'s MAN-59 round-6 note), so this cannot
/// corrupt the byte-identical spot log AGENTS.md requires.
///
/// Known, accepted limitation: a flag *value* that is literally the string
/// `--server-config` (e.g. a blocklist path so named) would trigger a
/// spurious notice, because the scan is positional-unaware by design -- it
/// runs before clap, so it cannot know which tokens are values. The failure
/// mode is one extra stderr line, never a wrong exit code or a changed
/// behavior.
fn warn_deprecations() {
    let argv = std::env::args_os().map(|a| a.to_string_lossy().into_owned());
    for d in deprecations(argv) {
        match d {
            Deprecation::ListenVerb => eprintln!(
                "warning: starting the daemon with `manta listen` is deprecated and will be \
                 removed in a future release; use `manta run --config` instead."
            ),
            Deprecation::ServerConfigFlag => eprintln!(
                "warning: `--server-config` is deprecated and will be removed in a future \
                 release; use `--config` instead."
            ),
        }
    }
}

/// Human-readable `manta doctor` summary. `--json` bypasses this entirely
/// in favor of the raw `DoctorReport`.
fn print_doctor_report(report: &manta_engine::DoctorReport) {
    println!(
        "source: {:.0} Hz sample rate, {:.1} Hz center, observed for {:.1}s",
        report.sample_rate_hz,
        report.center_freq_hz,
        report.duration.as_secs_f64()
    );
    println!(
        "tracks: {} promoted, {} TrackMeta updates, {} closed",
        report.tracks_promoted, report.track_meta_count, report.tracks_closed
    );
    match (report.snr_db_min, report.snr_db_median, report.snr_db_max) {
        (Some(min), Some(median), Some(max)) => {
            println!("snr_2500_db: min={min:.1} median={median:.1} max={max:.1}");
        }
        // `tracks_promoted` is verdict()'s own authoritative signal for
        // "did anything really happen" -- match its logic exactly rather
        // than re-deriving it from a different combination of fields.
        _ if report.tracks_promoted == 0 => {
            println!("snr_2500_db: no TrackMeta events -- no track ever promoted")
        }
        _ => println!(
            "snr_2500_db: a track was promoted but no TrackMeta ever landed for it before this \
             run ended"
        ),
    }
    println!(
        "decode: {} chars ({} distinct), {} confirmed spots",
        report.chars_decoded, report.distinct_chars, report.spots_confirmed
    );
    println!("verdict: {}", report.verdict().summary());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp_file(contents: &[u8]) -> tempfile::NamedTempFile {
        use std::io::Write as _;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(contents).unwrap();
        f.flush().unwrap();
        f
    }

    /// MAN-45 (PR #63 round-16 finding): the outer registry-wide deadline
    /// must never fire before a handler's own drain deadline, or
    /// `shutdown_timeout` aborts a drain that was still inside its budget
    /// and the abandoned queue goes uncounted again -- the exact failure
    /// rounds 15 and 16 both landed on from different directions. The
    /// margin covers task scheduling, not another spot's write.
    ///
    /// MAN-45 remediate (round-16 P1, finding 2): `CLIENT_DRAIN_DEADLINE`
    /// alone under-counts the true worst case -- a handler already mid-
    /// write when shutdown fires doesn't even START its own drain
    /// deadline until that in-progress `select!` branch resolves. Asserts
    /// the FULL relationship (`2 * telnet::WRITE_TIMEOUT +
    /// CLIENT_DRAIN_DEADLINE`, telnet's live-spot write being the largest
    /// such in-progress branch across all three handlers), not just the
    /// drain deadline in isolation -- see `SHUTDOWN_DRAIN_DEADLINE`'s own
    /// doc comment for the full argument.
    #[test]
    fn the_outer_shutdown_deadline_outlives_every_handlers_own_drain_deadline() {
        let worst_case_before_drain_starts = 2 * manta_server::telnet::WRITE_TIMEOUT;
        let true_worst_case =
            worst_case_before_drain_starts + manta_server::tasks::CLIENT_DRAIN_DEADLINE;
        assert!(
            SHUTDOWN_DRAIN_DEADLINE > true_worst_case,
            "SHUTDOWN_DRAIN_DEADLINE ({SHUTDOWN_DRAIN_DEADLINE:?}) must exceed the true \
             worst case of {true_worst_case:?} (2 * telnet::WRITE_TIMEOUT = \
             {worst_case_before_drain_starts:?}, the largest in-progress `select!` branch \
             a handler can already be running when shutdown fires, plus \
             CLIENT_DRAIN_DEADLINE = {:?} for its own drain loop once it gets there)",
            manta_server::tasks::CLIENT_DRAIN_DEADLINE,
        );
    }

    #[test]
    fn merge_cli_engine_prefers_the_explicit_cli_flag_over_the_file() {
        // SPEC v2 §7: --engine overrides [decode]'s engine key when both
        // are given -- this is the exact precedence rule Command::Run's
        // (and, since MAN-166's final-review fix batch, Decode's/Oracle's)
        // handler relies on `merge_cli_engine` for.
        let file_decode = manta_decode::decoder::DecodeConfig {
            engine: Engine::Legacy,
            ..manta_decode::decoder::DecodeConfig::default()
        };
        let resolved = merge_cli_engine(Some(Engine::EdgeLegacy), file_decode);
        assert_eq!(resolved.engine, Engine::EdgeLegacy);
    }

    #[test]
    fn merge_cli_engine_falls_back_to_the_file_engine_when_the_flag_is_absent() {
        let file_decode = manta_decode::decoder::DecodeConfig {
            engine: Engine::EdgeLegacy,
            ..manta_decode::decoder::DecodeConfig::default()
        };
        let resolved = merge_cli_engine(None, file_decode);
        assert_eq!(resolved.engine, Engine::EdgeLegacy);
    }

    #[test]
    fn merge_cli_engine_leaves_every_other_decode_field_from_the_file_untouched() {
        // The CLI has no flag for sigma_u/beam/etc. -- only `engine` may be
        // overridden; everything else must come through verbatim from the
        // file-derived DecodeConfig.
        let mut file_decode = manta_decode::decoder::DecodeConfig::default();
        file_decode.evidence.sigma_u = 0.31;
        file_decode.hsmm.beam = 10;
        let resolved = merge_cli_engine(Some(Engine::EdgeLegacy), file_decode);
        assert_eq!(resolved.evidence.sigma_u, 0.31);
        assert_eq!(resolved.hsmm.beam, 10);
    }

    #[test]
    fn load_decode_config_file_defaults_when_no_server_config_given() {
        let cfg = load_decode_config_file(None).unwrap();
        assert_eq!(cfg.engine, Engine::Legacy);
        assert_eq!(
            cfg.evidence.sigma_u,
            manta_decode::evidence::EvidenceConfig::default().sigma_u
        );
    }

    #[test]
    fn load_decode_config_file_reads_the_decode_table_from_server_config() {
        let f = write_temp_file(
            br#"
            [server]
            station_callsign = "W3XYZ"
            [decode]
            engine = "edge-legacy"
            sigma_u = 0.31
            beam = 10
            "#,
        );
        let cfg = load_decode_config_file(Some(f.path())).unwrap();
        assert_eq!(cfg.engine, Engine::EdgeLegacy);
        assert_eq!(cfg.evidence.sigma_u, 0.31);
        assert_eq!(cfg.hsmm.beam, 10);
    }

    #[test]
    fn load_decode_config_file_rejects_zero_fallback_hops() {
        let f = write_temp_file(b"[decode]\nfallback_hops = 0\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_inverted_tau_hi_bounds() {
        // Codex review, PR #161 round 2: this would otherwise panic in
        // f64::clamp on the first speed update instead of failing to load.
        let f = write_temp_file(b"[decode]\ntau_hi_bounds_ms = [400.0, 100.0]\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_nonpositive_tau_lo_ms() {
        let f = write_temp_file(b"[decode]\ntau_lo_ms = 0.0\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
        let f = write_temp_file(b"[decode]\ntau_lo_ms = -5.0\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_accepts_valid_tau_bounds() {
        let f =
            write_temp_file(b"[decode]\ntau_lo_ms = 450.0\ntau_hi_bounds_ms = [120.0, 380.0]\n");
        let cfg = load_decode_config_file(Some(f.path())).unwrap();
        assert_eq!(cfg.demod.tau_lo_ms, 450.0);
        assert_eq!(cfg.demod.tau_hi_bounds_ms, (120.0, 380.0));
    }

    #[test]
    fn load_decode_config_file_rejects_nonpositive_timing_sigma() {
        // Codex review, PR #161 round 3: this would otherwise produce
        // infinite/NaN confidence scores instead of failing to load.
        let f = write_temp_file(b"[decode]\ntiming_sigma = 0.0\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
        let f = write_temp_file(b"[decode]\ntiming_sigma = -0.5\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_a_timing_sigma_too_small_to_avoid_underflow() {
        // Codex review, PR #161 round 5: 1e-30 is finite and > 0.0 (so
        // round 3's original check alone would accept it), but
        // beam::log_likelihood's `2.0 * sigma * sigma` underflows to
        // exactly 0.0 in f32, giving NaN confidence for a perfectly-timed
        // candidate.
        let f = write_temp_file(b"[decode]\ntiming_sigma = 1e-30\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_zero_beam_width() {
        let f = write_temp_file(b"[decode]\nbeam_width = 0\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_zero_hsmm_beam() {
        let f = write_temp_file(b"[decode]\nbeam = 0\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_zero_sigma_u() {
        let f = write_temp_file(b"[decode]\nsigma_u = 0\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_nan_sigma_u() {
        let f = write_temp_file(b"[decode]\nsigma_u = nan\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_a_sigma_u_too_small_to_avoid_underflow() {
        // Codex review, PR #161 round 20: 1e-30 is finite and > 0.0 (so
        // the original check alone passed it), but sigma_u * sigma_u
        // underflows to exactly 0.0 in f32.
        let f = write_temp_file(b"[decode]\nsigma_u = 1e-30\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_empty_seed_units_hops() {
        let f = write_temp_file(b"[decode]\nseed_units_hops = []\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_a_nonpositive_seed_unit() {
        let f = write_temp_file(b"[decode]\nseed_units_hops = [9.0, 0.0, 18.0]\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_a_seed_unit_below_u_min() {
        let f = write_temp_file(b"[decode]\nseed_units_hops = [1.0]\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_a_seed_unit_above_u_max() {
        let f = write_temp_file(b"[decode]\nseed_units_hops = [100.0]\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_zero_conf_kappa() {
        let f = write_temp_file(b"[decode]\nconf_kappa = 0\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_zero_dur_sigma() {
        let f = write_temp_file(b"[decode]\ndur_sigma = 0\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_a_dur_sigma_too_small_to_avoid_underflow() {
        // Codex review, PR #161 round 16: 1e-30 is finite and > 0.0 (so
        // round 5's original check alone passed it), but 2*dur_sigma^2
        // underflows to exactly 0.0 in f32.
        let f = write_temp_file(b"[decode]\ndur_sigma = 1e-30\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_nonpositive_hold_dits() {
        let f = write_temp_file(b"[decode]\nhold_dits = 0\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_a_hold_dits_that_overflows_max_retain() {
        // Codex review, PR #161 round 6: a large but individually-plausible
        // hold_dits (300) at the default u_max (56.0) computes h = 16800,
        // far past Evidence's MAX_RETAIN (4096).
        let f = write_temp_file(b"[decode]\nhold_dits = 300\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_a_hold_dits_that_only_overflows_after_rounding() {
        // Codex review, PR #161 round 20: hold_dits=73.14 at the default
        // u_max (56.0) computes an UNROUNDED product of 4095.84 -- under
        // MAX_RETAIN (4096) -- but Evidence::set_u_ref rounds before
        // enforcing the cap, and round(4095.84) = 4096, which is not
        // "well under" MAX_RETAIN.
        let f = write_temp_file(b"[decode]\nhold_dits = 73.14\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_accepts_the_default_hold_dits() {
        let f = write_temp_file(b"[decode]\n");
        assert!(load_decode_config_file(Some(f.path())).is_ok());
    }

    #[test]
    fn load_decode_config_file_rejects_nan_speed_alpha() {
        let f = write_temp_file(b"[decode]\nspeed_alpha = nan\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_negative_speed_alpha() {
        let f = write_temp_file(b"[decode]\nspeed_alpha = -0.1\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_nan_mark_insert_penalty() {
        let f = write_temp_file(b"[decode]\nmark_insert_penalty = nan\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_infinite_noise_min_bias_db() {
        let f = write_temp_file(b"[decode]\nnoise_min_bias_db = inf\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_negative_lookahead_dits() {
        let f = write_temp_file(b"[decode]\nlookahead_dits = -1.0\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_nan_lookahead_dits() {
        let f = write_temp_file(b"[decode]\nlookahead_dits = nan\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_accepts_zero_lookahead_dits() {
        let f = write_temp_file(b"[decode]\nlookahead_dits = 0.0\n");
        assert!(load_decode_config_file(Some(f.path())).is_ok());
    }

    #[test]
    fn load_decode_config_file_rejects_nonpositive_noise_window_ms() {
        let f = write_temp_file(b"[decode]\nnoise_window_ms = 0.0\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_infinite_noise_window_ms() {
        let f = write_temp_file(b"[decode]\nnoise_window_ms = inf\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_nan_hysteresis() {
        // Codex review, PR #161 round 4: NaN makes every `Demod::step`
        // comparison false, silently disabling Legacy's decode entirely.
        let f = write_temp_file(b"[decode]\nhyst_up = nan\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_inverted_hysteresis() {
        let f = write_temp_file(b"[decode]\nhyst_up = 0.5\nhyst_down = 0.8\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
        let f = write_temp_file(b"[decode]\nhyst_down = -1.0\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_nonpositive_debounce_ms() {
        let f = write_temp_file(b"[decode]\ndebounce_ms = 0.0\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    #[test]
    fn load_decode_config_file_rejects_nonpositive_flush_gap_dits() {
        // Codex review, PR #161 round 4: <= 0 forces an instant/premature
        // word flush on every hop.
        let f = write_temp_file(b"[decode]\nflush_gap_dits = 0.0\n");
        assert!(load_decode_config_file(Some(f.path())).is_err());
    }

    /// Regression: an earlier version of this task rejected `engine =
    /// "hsmm"` at TOML-deserialize time (inside `load_decode_config_file`,
    /// i.e. BEFORE `merge_cli_engine` ever runs), which meant an explicit
    /// `--engine legacy` could never override a config file staging
    /// `engine = "hsmm"` -- the file's parse error fired first, and the
    /// override never got a chance to apply. As of Task 11, `hsmm` is no
    /// longer rejected anywhere in this path, but the precedence rule this
    /// test protects (an explicit `--engine` beats the file's `engine` key)
    /// still matters, so it's kept with `hsmm` as the file-staged value to
    /// prove `merge_cli_engine` reads the CLI override, not the file, when
    /// both are given.
    #[test]
    fn cli_engine_override_beats_a_hsmm_staged_file() {
        let f = write_temp_file(
            br#"
            [server]
            station_callsign = "W3XYZ"
            [decode]
            engine = "hsmm"
            "#,
        );
        let file_decode = load_decode_config_file(Some(f.path())).unwrap();
        assert_eq!(
            file_decode.engine,
            Engine::Hsmm,
            "the file's own value must still be hsmm going into the merge"
        );
        let result = merge_cli_engine(Some(Engine::Legacy), file_decode);
        assert_eq!(
            result.engine,
            Engine::Legacy,
            "an explicit --engine must override a hsmm-staged file"
        );
    }

    /// The other direction: with NO CLI override, a file staging `engine =
    /// "hsmm"` is honored (not rejected) -- `hsmm` is a fully implemented,
    /// reviewed engine (Task 8) with no CLI-level gate as of Task 11.
    #[test]
    fn hsmm_staged_file_is_honored_without_a_cli_override() {
        let f = write_temp_file(
            br#"
            [decode]
            engine = "hsmm"
            "#,
        );
        let file_decode = load_decode_config_file(Some(f.path())).unwrap();
        let result = merge_cli_engine(None, file_decode);
        assert_eq!(result.engine, Engine::Hsmm);
    }

    #[test]
    fn deprecation_notices_fire_only_for_the_replaced_spellings() {
        fn notices(argv: &[&str]) -> Vec<Deprecation> {
            deprecations(argv.iter().map(|s| s.to_string()))
        }
        // D-3: the daemon-via-listen path is deprecated ...
        assert_eq!(
            notices(&["manta", "listen", "--server-config", "m.toml"]),
            vec![Deprecation::ListenVerb, Deprecation::ServerConfigFlag]
        );
        assert_eq!(
            notices(&["manta", "listen", "--config", "m.toml"]),
            vec![Deprecation::ListenVerb]
        );
        // ... the ad hoc audio path the ticket title preserves is NOT.
        assert_eq!(notices(&["manta", "listen", "--device", "hw:1"]), vec![]);
        assert_eq!(notices(&["manta", "listen"]), vec![]);
        // The flag is deprecated under either verb.
        assert_eq!(
            notices(&["manta", "run", "--server-config", "m.toml"]),
            vec![Deprecation::ServerConfigFlag]
        );
        // `--flag=value` form must be caught too.
        assert_eq!(
            notices(&["manta", "run", "--server-config=m.toml"]),
            vec![Deprecation::ServerConfigFlag]
        );
        // The canonical spelling is silent.
        assert_eq!(notices(&["manta", "run", "--config", "m.toml"]), vec![]);
        assert_eq!(notices(&["manta", "run", "--device", "hw:1"]), vec![]);
        // Other subcommands are never implicated.
        assert_eq!(notices(&["manta", "decode", "/tmp/v1.wav"]), vec![]);
        assert_eq!(notices(&["manta", "soak", "--duration", "10"]), vec![]);
        // `status` is scanned like any other verb -- which is only
        // honest because `status` now ACCEPTS the flag the notice tells
        // the operator to switch to (Codex review, PR #95); see
        // `status_accepts_the_canonical_config_flag_the_deprecation_notice_names`.
        assert_eq!(
            notices(&["manta", "status", "--server-config", "m.toml"]),
            vec![Deprecation::ServerConfigFlag]
        );
        assert_eq!(notices(&["manta", "status", "--config", "m.toml"]), vec![]);
    }

    /// Codex review, PR #95: `warn_deprecations` scans raw argv without
    /// knowing the verb, so `manta status --server-config m.toml` printed
    /// "use `--config` instead" while `status` accepted no `--config` at
    /// all -- the notice pointed operators at a spelling clap rejected.
    /// Both spellings must now parse to the same field.
    #[test]
    fn status_accepts_the_canonical_config_flag_the_deprecation_notice_names() {
        use clap::Parser;

        for spelling in ["--config", "--server-config"] {
            let cli = Cli::try_parse_from(["manta", "status", spelling, "m.toml"])
                .unwrap_or_else(|e| panic!("`manta status {spelling}` must parse: {e}"));
            match cli.command {
                Command::Status { config, .. } => {
                    assert_eq!(config.as_deref(), Some(std::path::Path::new("m.toml")));
                }
                _ => panic!("`manta status {spelling}` must parse as Command::Status"),
            }
        }
    }

    /// Codex review, PR #95: a newer daemon's document can stay
    /// structurally deserializable while its fields mean something else,
    /// so the version has to be checked rather than merely carried.
    #[test]
    fn a_status_document_with_an_unknown_schema_version_is_rejected() {
        let doc =
            manta_server::status::StatusDoc::from_metrics(&manta_server::metrics::Metrics::new());
        let ok = serde_json::to_string(&doc).unwrap();
        assert_eq!(
            parse_status_doc(&ok).unwrap().schema_version,
            manta_server::status::STATUS_SCHEMA_VERSION,
            "the version this build emits must be the version it accepts"
        );

        let mut v: serde_json::Value = serde_json::from_str(&ok).unwrap();
        v["schema_version"] = serde_json::json!(2);
        let err = parse_status_doc(&v.to_string())
            .expect_err("a schema_version this build does not understand must not be rendered")
            .to_string();
        assert!(
            err.contains("schema_version 2") && err.contains("understands only 1"),
            "the error must name both versions so an operator knows which side to upgrade: {err}"
        );
    }

    /// The neighbouring failure mode: a body that is not a status
    /// document at all (a wrong port answering `GET /status`) must still
    /// fail on its own message, not on the new version check.
    #[test]
    fn a_body_that_is_not_a_status_document_fails_before_the_version_check() {
        let err = parse_status_doc("{\"hello\":\"world\"}")
            .expect_err("a non-status body must not parse")
            .to_string();
        assert!(
            err.contains("not a valid status document"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn shutdown_runtime_after_drain_awaits_a_tracked_task_to_completion() {
        // Regression (round-9/round-10 review, verified against tokio
        // 1.53's own source): `Runtime::shutdown_timeout`'s `duration`
        // parameter only bounds the BLOCKING thread pool's shutdown -- the
        // async executor itself is torn down synchronously and
        // immediately via `self.handle.inner.shutdown()`. An earlier
        // version of this function papered over that with a fixed blind
        // `sleep` before `shutdown_timeout`: real scheduler time, but no
        // guarantee the task actually FINISHED before the sleep elapsed.
        // This test spawns a task INTO the tracked `ClientTasks` registry
        // that only sets a flag after being signaled AND doing a small
        // amount of real async work (standing in for a socket write) --
        // proving `shutdown_runtime_after_drain` genuinely awaits its
        // completion rather than guessing a duration.
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let (tx, mut rx) = tokio::sync::watch::channel(false);
        let drained = Arc::new(AtomicBool::new(false));
        let drained_task = drained.clone();
        let tasks = manta_server::tasks::new_client_tasks();

        rt.block_on({
            let tasks = tasks.clone();
            async move {
                tasks.lock().await.spawn(async move {
                    let _ = rx.changed().await;
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    drained_task.store(true, Ordering::SeqCst);
                });
            }
        });

        let _ = tx.send(true);
        shutdown_runtime_after_drain(rt, &tasks);

        assert!(
            drained.load(Ordering::SeqCst),
            "the tracked task must have been genuinely awaited to completion before the runtime shut down"
        );
    }

    #[test]
    fn epoch_for_replay_path_rejects_a_pre_unix_epoch_mtime() {
        // Regression (round-8 review): a Unix filesystem can represent an
        // mtime before 1970 (rare, but real). Left unvalidated, that
        // SystemTime flows all the way to SpotBus::unix_ts_for, whose
        // `.duration_since(UNIX_EPOCH).expect(...)` panics on the very
        // first spot delivered to any client -- a crash discovered at
        // spot-delivery time instead of a clean error at startup.
        let f = write_temp_file(b"pre-epoch mtime fixture");
        let pre_epoch = std::time::SystemTime::UNIX_EPOCH - std::time::Duration::from_secs(1);
        std::fs::File::open(f.path())
            .unwrap()
            .set_modified(pre_epoch)
            .expect("this platform must support setting mtime for the test to be meaningful");

        let result = epoch_for_replay_path(f.path());
        assert!(
            result.is_err(),
            "a pre-1970 mtime must be rejected at startup, not deferred to a later panic"
        );
    }

    #[test]
    fn resolve_epoch_ignores_replay_epoch_for_a_live_session() {
        // Regression (round-8 review): --replay-epoch's own doc comment
        // says "ignored for a live source," but the old match applied it
        // unconditionally on `Some(secs)` regardless of `replay_path`. A
        // live session given the flag would then publish spots with a
        // fabricated historical timestamp AND derive its session_nonce
        // from that same fixed value instead of a fresh nanosecond-
        // precision now -- breaking the "two live sessions started within
        // the same wall-clock second don't collide" guarantee entirely,
        // since every live start with the same flag value would collide.
        let before = std::time::SystemTime::now();
        let epoch = resolve_epoch(None, Some(1_751_635_200)).unwrap();
        let after = std::time::SystemTime::now();
        assert!(
            epoch >= before && epoch <= after,
            "a live session (no replay_path) must ignore --replay-epoch and use now(), got {epoch:?}"
        );
    }

    #[test]
    fn resolve_epoch_prefers_an_explicit_replay_epoch_over_file_mtime() {
        // --replay-epoch must win even when a replay path is also given --
        // it's the escape hatch for exactly the case where mtime isn't
        // trustworthy (a copy/download that didn't preserve it).
        let f = write_temp_file(b"resolve_epoch fixture");
        let explicit =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_751_635_200);
        assert_eq!(
            resolve_epoch(Some(f.path()), Some(1_751_635_200)).unwrap(),
            explicit
        );
    }

    #[test]
    fn resolve_epoch_falls_back_to_file_mtime_for_replay_without_an_explicit_epoch() {
        let f = write_temp_file(b"resolve_epoch fixture");
        assert_eq!(
            resolve_epoch(Some(f.path()), None).unwrap(),
            epoch_for_replay_path(f.path()).unwrap()
        );
    }

    #[test]
    fn resolve_epoch_is_now_for_a_live_session_with_no_replay_path() {
        let before = std::time::SystemTime::now();
        let epoch = resolve_epoch(None, None).unwrap();
        let after = std::time::SystemTime::now();
        assert!(epoch >= before && epoch <= after);
    }

    #[test]
    fn parse_replay_epoch_accepts_a_unix_seconds_value() {
        assert_eq!(parse_replay_epoch("1751635200").unwrap(), 1_751_635_200);
    }

    #[test]
    fn parse_replay_epoch_rejects_negative_and_non_numeric_values() {
        assert!(parse_replay_epoch("-1").is_err());
        assert!(parse_replay_epoch("not-a-number").is_err());
        assert!(parse_replay_epoch("2026-07-04T12:00:00Z").is_err());
    }

    #[test]
    fn parse_replay_epoch_rejects_unrealistic_far_future_values() {
        // Regression (round-9 review): an unbounded upper end let
        // i64::MAX (or anything close to it) through, which later
        // overflows SystemTime arithmetic in SpotBus::unix_ts_for
        // (`epoch + elapsed`) and panics on the very first spot delivered
        // to any client -- reject it here, at CLI-parse time, with a
        // clear error instead.
        assert!(parse_replay_epoch(&i64::MAX.to_string()).is_err());
        // A plausible near-future value must still be accepted.
        assert!(parse_replay_epoch("2000000000").is_ok());
    }

    #[test]
    fn epoch_for_replay_path_is_a_real_deterministic_timestamp() {
        // Regression (round-6 review): the epoch fed into SpotBus (and
        // from there into every JSON `timestamp`/RBN Zulu field) must be
        // BOTH a genuine wall-clock instant (not a content-hash reinterpreted
        // as nanoseconds, which produced dates spanning 1970-2554) AND
        // stable across reruns of the same replay file (a fresh
        // SystemTime::now() every run broke reproducible replay output,
        // the specific regression this round's finding flagged). A file's
        // own mtime satisfies both: it's a real filesystem fact, and it
        // doesn't change between two reads of the same untouched file.
        let f = write_temp_file(b"replay epoch fixture");
        let a = epoch_for_replay_path(f.path()).unwrap();
        let b = epoch_for_replay_path(f.path()).unwrap();
        assert_eq!(a, b, "must be stable across reruns of the same file");

        let now = std::time::SystemTime::now();
        let drift = now
            .duration_since(a)
            .or_else(|_| a.duration_since(now))
            .unwrap();
        assert!(
            drift < std::time::Duration::from_secs(60),
            "must be a genuine near-present timestamp, not a fabricated far date; drift was {drift:?}"
        );
    }

    #[test]
    fn session_nonce_for_replay_path_matches_the_published_fnv_1a_algorithm() {
        // Regression (round-12 review): std::collections::hash_map::
        // DefaultHasher's algorithm is explicitly documented as
        // UNSPECIFIED across Rust releases, so the same replay file could
        // hash differently across builds/toolchains -- and this value
        // feeds every JSON spot `id`. FNV-1a-64 is a small, independently
        // published, versioned algorithm with no dependency on any std or
        // compiler internals: `hash = (hash XOR byte) * FNV_PRIME`,
        // starting from the published offset basis. Recomputed here from
        // the same published constants via a separate expression (not
        // just self-consistency) to pin the implementation against the
        // actual formula, catching e.g. an accidentally swapped
        // XOR/multiply order or wrong constant.
        const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
        const PRIME: u64 = 0x0000_0100_0000_01b3;
        let expected_empty = OFFSET_BASIS as u128;
        let expected_a = (OFFSET_BASIS ^ 0x61u64).wrapping_mul(PRIME) as u128;
        let expected_ab =
            (((OFFSET_BASIS ^ 0x61u64).wrapping_mul(PRIME) ^ 0x62u64).wrapping_mul(PRIME)) as u128;

        assert_eq!(
            session_nonce_for_replay_path(write_temp_file(b"").path()).unwrap(),
            expected_empty
        );
        assert_eq!(
            session_nonce_for_replay_path(write_temp_file(b"a").path()).unwrap(),
            expected_a
        );
        assert_eq!(
            session_nonce_for_replay_path(write_temp_file(b"ab").path()).unwrap(),
            expected_ab
        );
    }

    #[test]
    fn session_nonce_for_replay_path_is_deterministic_for_the_same_content() {
        let f = write_temp_file(b"same recording bytes");
        assert_eq!(
            session_nonce_for_replay_path(f.path()).unwrap(),
            session_nonce_for_replay_path(f.path()).unwrap()
        );
    }

    #[test]
    fn session_nonce_for_replay_path_is_stable_across_different_paths_for_the_same_content() {
        // The exact bug this fix exists to prevent: the same recording,
        // re-read from a different path (a rename, a different mount, a
        // different checkout) must derive the SAME replay session nonce.
        let a = write_temp_file(b"identical recording bytes");
        let b = write_temp_file(b"identical recording bytes");
        assert_eq!(
            session_nonce_for_replay_path(a.path()).unwrap(),
            session_nonce_for_replay_path(b.path()).unwrap(),
            "the same content at two different paths must derive the same nonce"
        );
    }

    #[test]
    fn session_nonce_for_replay_path_differs_across_different_recordings() {
        let a = session_nonce_for_replay_path(write_temp_file(b"contest-weekend bytes").path())
            .unwrap();
        let b = session_nonce_for_replay_path(write_temp_file(b"quiet-weeknight bytes").path())
            .unwrap();
        assert_ne!(
            a, b,
            "two different recordings must not collide on the same replay session nonce"
        );
    }

    // MAN-32/MAN-42: start_spot_server spawns one RBN uplink task per
    // configured [[rbn_uplink]] target, only for those that are enabled.

    #[test]
    fn disabled_uplink_makes_no_connection_attempt_from_the_daemon() {
        let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let target_port = target.local_addr().unwrap().port();

        let cfg_file = write_temp_file(
            format!(
                r#"
                [server]
                station_callsign = "W3XYZ"
                bind_addr = "127.0.0.1"
                telnet_port = 0
                json_port = 0
                metrics_port = 0

                [[rbn_uplink]]
                enabled = false
                target_host = "127.0.0.1"
                target_port = {target_port}
                "#
            )
            .as_bytes(),
        );

        let (rt, _server) = start_spot_server(
            cfg_file.path(),
            96_000.0,
            std::time::SystemTime::UNIX_EPOCH,
            0,
        )
        .unwrap();

        let accepted = rt.block_on(async {
            tokio::time::timeout(std::time::Duration::from_millis(300), async {
                loop {
                    if target.accept().is_ok() {
                        return true;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
        });
        assert!(
            accepted.is_err(),
            "enabled=false must never attempt a connection"
        );
    }

    #[test]
    fn enabled_uplink_connects_to_its_configured_target() {
        let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let target_port = target.local_addr().unwrap().port();

        let cfg_file = write_temp_file(
            format!(
                r#"
                [server]
                station_callsign = "W3XYZ"
                bind_addr = "127.0.0.1"
                telnet_port = 0
                json_port = 0
                metrics_port = 0

                [[rbn_uplink]]
                enabled = true
                target_host = "127.0.0.1"
                target_port = {target_port}
                "#
            )
            .as_bytes(),
        );

        let (rt, _server) = start_spot_server(
            cfg_file.path(),
            96_000.0,
            std::time::SystemTime::UNIX_EPOCH,
            0,
        )
        .unwrap();

        let accepted = rt.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if target.accept().is_ok() {
                        return true;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
        });
        assert!(
            accepted.unwrap_or(false),
            "enabled=true must connect to the configured target"
        );
    }

    #[test]
    fn two_enabled_uplink_targets_each_independently_connect() {
        let target1 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        target1.set_nonblocking(true).unwrap();
        let target1_port = target1.local_addr().unwrap().port();

        let target2 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        target2.set_nonblocking(true).unwrap();
        let target2_port = target2.local_addr().unwrap().port();

        let cfg_file = write_temp_file(
            format!(
                r#"
                [server]
                station_callsign = "W3XYZ"
                bind_addr = "127.0.0.1"
                telnet_port = 0
                json_port = 0
                metrics_port = 0

                [[rbn_uplink]]
                enabled = true
                target_host = "127.0.0.1"
                target_port = {target1_port}

                [[rbn_uplink]]
                enabled = true
                target_host = "127.0.0.1"
                target_port = {target2_port}
                "#
            )
            .as_bytes(),
        );

        let (rt, _server) = start_spot_server(
            cfg_file.path(),
            96_000.0,
            std::time::SystemTime::UNIX_EPOCH,
            0,
        )
        .unwrap();

        async fn wait_for_accept(listener: &std::net::TcpListener) -> bool {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if listener.accept().is_ok() {
                        return true;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap_or(false)
        }
        let (accepted1, accepted2) = rt
            .block_on(async { tokio::join!(wait_for_accept(&target1), wait_for_accept(&target2)) });
        assert!(accepted1, "first configured target must be connected to");
        assert!(accepted2, "second configured target must be connected to");
    }

    // MAN-44: uplink health at a glance -- `manta status` + `GET /status`.

    #[test]
    fn every_configured_uplink_target_is_registered_even_when_disabled() {
        // Two [[rbn_uplink]] entries, one enabled=false: the daemon's
        // Metrics must report BOTH, the disabled one as
        // UplinkHealth::Disabled -- an operator must be able to see
        // "configured but off", not an empty list.
        let cfg_file = write_temp_file(
            br#"
            [server]
            station_callsign = "W3XYZ"
            bind_addr = "127.0.0.1"
            telnet_port = 0
            json_port = 0
            metrics_port = 0

            [[rbn_uplink]]
            enabled = true
            target_host = "127.0.0.1"
            target_port = 1

            [[rbn_uplink]]
            enabled = false
            target_host = "127.0.0.1"
            target_port = 2
            "#,
        );

        let (_rt, server) = start_spot_server(
            cfg_file.path(),
            96_000.0,
            std::time::SystemTime::UNIX_EPOCH,
            0,
        )
        .unwrap();

        let snap = server.metrics.uplink_snapshot();
        assert_eq!(
            snap.len(),
            2,
            "both configured targets must be registered, including the disabled one"
        );
        assert!(snap.iter().any(|t| t.label == "127.0.0.1:1" && t.enabled));
        assert!(snap.iter().any(|t| t.label == "127.0.0.1:2" && !t.enabled));
    }

    /// MAN-44 end-to-end: a real daemon (port 0, discovered via
    /// SpotServer::metrics_addr) answers `manta status`'s own fetch path.
    #[test]
    fn status_reports_a_configured_uplink_target_from_a_live_daemon() {
        let cfg_file = write_temp_file(
            br#"
            [server]
            station_callsign = "W3XYZ"
            bind_addr = "127.0.0.1"
            telnet_port = 0
            json_port = 0
            metrics_port = 0

            [[rbn_uplink]]
            enabled = true
            target_host = "127.0.0.1"
            target_port = 1
            "#,
        );

        let (rt, server) = start_spot_server(
            cfg_file.path(),
            96_000.0,
            std::time::SystemTime::UNIX_EPOCH,
            0,
        )
        .unwrap();

        let doc = rt
            .block_on(fetch_status(
                &[server.metrics_addr],
                std::time::Duration::from_secs(5),
            ))
            .unwrap();

        assert!(
            doc.uplink.targets.iter().any(|t| t.label == "127.0.0.1:1"),
            "expected the configured target to be visible, got: {doc:?}"
        );
        assert_eq!(
            status_exit_code(&doc),
            1,
            "a never-yet-connected enabled target must not read as healthy"
        );

        let _ = server.shutdown_tx.send(true);
        shutdown_runtime_after_drain(rt, &server.tasks);
    }

    fn cfg_with(bind_addr: &str, metrics_port: u16) -> manta_server::config::ServerConfig {
        manta_server::config::ServerConfig {
            station_callsign: "W3XYZ".to_string(),
            bind_addr: bind_addr.to_string(),
            telnet_port: 7300,
            json_port: 7301,
            metrics_port,
            telnet_max_connections_per_ip: None,
            json_max_connections_per_ip: None,
            metrics_max_connections_per_ip: None,
            telnet_max_commands_per_ip: None,
            json_max_pings_per_ip: None,
        }
    }

    #[test]
    fn status_address_prefers_explicit_addr_then_config_then_default() {
        assert_eq!(
            resolve_status_addr(Some("1.2.3.4:9999"), None).unwrap(),
            vec!["1.2.3.4:9999".parse().unwrap()]
        );
        // bind_addr 0.0.0.0 in config means "listening everywhere"; the
        // CLI still has to DIAL something, and loopback is the only
        // address guaranteed to reach the local daemon.
        assert_eq!(
            resolve_status_addr(None, Some(&cfg_with("0.0.0.0", 17302))).unwrap(),
            vec!["127.0.0.1:17302".parse().unwrap()]
        );
        assert_eq!(
            resolve_status_addr(None, Some(&cfg_with("::", 17302))).unwrap(),
            vec!["[::1]:17302".parse().unwrap()]
        );
        assert_eq!(
            resolve_status_addr(None, Some(&cfg_with("10.0.0.5", 17302))).unwrap(),
            vec!["10.0.0.5:17302".parse().unwrap()]
        );
        assert_eq!(
            resolve_status_addr(None, None).unwrap(),
            vec!["127.0.0.1:7302".parse().unwrap()]
        );
    }

    /// MAN-44 remediate regression: `--addr` must accept a hostname too,
    /// the same way the `--server-config`/`bind_addr` path already does
    /// (`status_address_resolves_a_hostname_bind_addr_like_the_daemon_does`
    /// below) -- an explicit `--addr localhost:PORT` was being rejected
    /// outright by a literal `SocketAddr` parse, contradicting
    /// `docs/RUNBOOKS/uplink-health.md`'s documented cross-host invocation.
    #[test]
    fn status_address_resolves_a_hostname_passed_via_addr() {
        let addrs = resolve_status_addr(Some("localhost:17302"), None).unwrap();
        assert!(!addrs.is_empty(), "expected at least one resolved address");
        for addr in &addrs {
            assert!(
                addr.ip().is_loopback(),
                "expected localhost to resolve to a loopback address, got {addr}"
            );
            assert_eq!(addr.port(), 17302);
        }
    }

    /// MAN-44 CR-B regression: the daemon binds `bind_addr` via
    /// `TcpListener::bind((host, port))`, which resolves a hostname (not
    /// just a literal IP) through `ToSocketAddrs` -- so `bind_addr =
    /// "localhost"` is a config the daemon runs on happily. `manta status
    /// --server-config` must resolve it the same way instead of rejecting
    /// a config the daemon itself accepts.
    #[test]
    fn status_address_resolves_a_hostname_bind_addr_like_the_daemon_does() {
        let addrs = resolve_status_addr(None, Some(&cfg_with("localhost", 17302))).unwrap();
        assert!(!addrs.is_empty(), "expected at least one resolved address");
        for addr in &addrs {
            assert!(
                addr.ip().is_loopback(),
                "expected localhost to resolve to a loopback address, got {addr}"
            );
            assert_eq!(addr.port(), 17302);
        }
    }

    /// Code-review regression (finding 1): a hostname that resolves to
    /// several addresses -- e.g. `localhost` returning `::1` before
    /// `127.0.0.1` on a dual-stack host -- must not make `manta status`
    /// give up after dialing only the FIRST candidate. The previous
    /// `resolve_status_addr`/`fetch_status_inner` kept only
    /// `to_socket_addrs().next()`, so a real daemon bound IPv4-only (the
    /// project's own default `bind_addr = "0.0.0.0"`) was reported as
    /// unreachable whenever the resolver listed an unreachable address
    /// first. This reproduces that shape directly -- a dead IPv6 loopback
    /// candidate followed by a live IPv4-only listener -- so it fails on
    /// any implementation that dials only the first address, regardless
    /// of what a given machine's real resolver happens to return for
    /// "localhost".
    #[tokio::test]
    async fn fetch_status_falls_back_past_an_unreachable_first_address() {
        // A closed port on ::1: nothing is listening, so connecting here
        // fails immediately (connection refused) rather than hanging.
        let dead = std::net::SocketAddr::new(std::net::Ipv6Addr::LOCALHOST.into(), 1);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live = listener.local_addr().unwrap();
        let body = doc_with(manta_server::metrics::OverallUplinkHealth::Disabled).to_json();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let (mut socket, _) = listener.accept().await.unwrap();
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let _ = socket.shutdown().await;
        });

        let doc = fetch_status(&[dead, live], std::time::Duration::from_secs(5))
            .await
            .expect("must fall back to the second address after the first refuses");
        assert_eq!(doc.schema_version, 1);
    }

    /// MAN-44 code review CR-1 regression: a black-holed first candidate
    /// (SYNs silently dropped, not refused) must not consume the WHOLE
    /// overall timeout budget before a later, live candidate is ever
    /// tried -- same shape and same reasoning as
    /// `uplink::connect_first_reachable_bounded_stops_at_the_overall_deadline`:
    /// a real SYN black hole depends on undocumented host/network behavior
    /// (a host with no route to a given block gets an immediate
    /// `NetworkUnreachable` instead of a hang), so this fakes an
    /// unconditionally hanging first attempt under paused tokio time
    /// instead of dialing a real address. `connect_any`'s previous bare
    /// `TcpStream::connect` with no per-candidate bound would have let
    /// address 1 alone eat the entire `overall_timeout` here, never
    /// reaching address 2.
    #[tokio::test(start_paused = true)]
    async fn connect_any_bounded_falls_through_a_stalled_first_candidate() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let overall_timeout = std::time::Duration::from_secs(10);
        let addrs: Vec<std::net::SocketAddr> = vec![
            "127.0.0.1:1".parse().unwrap(),
            "127.0.0.1:2".parse().unwrap(),
        ]; // never actually dialed -- `connect` below is faked

        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_connect = attempts.clone();
        let connect = move |addr: std::net::SocketAddr| {
            let attempts = attempts_for_connect.clone();
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                if addr.port() == 1 {
                    std::future::pending::<std::io::Result<()>>().await
                } else {
                    Ok(())
                }
            }
        };

        let started = tokio::time::Instant::now();
        let (_stream, addr) = connect_any_bounded(&addrs, overall_timeout, connect)
            .await
            .expect("must fall through to the live second candidate");
        assert_eq!(addr.port(), 2);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(
            started.elapsed() < overall_timeout,
            "the stalled first candidate must not consume the whole overall budget: elapsed {:?}",
            started.elapsed()
        );
    }

    fn doc_with(
        health: manta_server::metrics::OverallUplinkHealth,
    ) -> manta_server::status::StatusDoc {
        manta_server::status::StatusDoc {
            schema_version: 1,
            version: "test".to_string(),
            uptime_seconds: 0,
            spots_total: 0,
            telnet_clients: 0,
            json_clients: 0,
            ws_clients: 0,
            active_tracks: None,
            uplink: manta_server::status::UplinkStatus {
                health,
                connected_targets: 0,
                enabled_targets: 0,
                sent_total: 0,
                suppressed_total: 0,
                reconnects_total: 0,
                reconnect_window_seconds: 300,
                flapping_threshold: 3,
                targets: vec![],
            },
        }
    }

    #[test]
    fn status_exit_code_is_zero_when_healthy_one_when_degraded() {
        use manta_server::metrics::OverallUplinkHealth;
        assert_eq!(status_exit_code(&doc_with(OverallUplinkHealth::Ok)), 0);
        assert_eq!(
            status_exit_code(&doc_with(OverallUplinkHealth::Disabled)),
            0
        );
        assert_eq!(
            status_exit_code(&doc_with(OverallUplinkHealth::Degraded)),
            1
        );
        assert_eq!(status_exit_code(&doc_with(OverallUplinkHealth::Down)), 1);
    }

    #[tokio::test]
    async fn fetch_status_parses_a_chunk_split_http_response() {
        // A body assembled from one read() would pass trivially and hide
        // a real framing bug -- the status line, headers, and body are
        // deliberately written in three separate writes here.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let body = doc_with(manta_server::metrics::OverallUplinkHealth::Disabled).to_json();

        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let (mut socket, _) = listener.accept().await.unwrap();
            socket.write_all(b"HTTP/1.1 200 OK\r\n").await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            socket
                .write_all(
                    format!(
                        "Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            socket.write_all(body.as_bytes()).await.unwrap();
            let _ = socket.shutdown().await;
        });

        let doc = fetch_status(&[addr], std::time::Duration::from_secs(5))
            .await
            .expect("must parse a response split across several writes");
        assert_eq!(doc.schema_version, 1);
    }

    #[tokio::test]
    async fn fetch_status_errors_cleanly_on_a_404_and_on_a_non_json_body() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let (mut socket, _) = listener.accept().await.unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            let _ = socket.shutdown().await;
        });
        let err = fetch_status(&[addr], std::time::Duration::from_secs(5))
            .await
            .expect_err("a 404 must be a clean error, not a panic");
        assert!(format!("{err:#}").contains("404"));

        let listener2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr2 = listener2.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let (mut socket, _) = listener2.accept().await.unwrap();
            let body = "not json";
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let _ = socket.shutdown().await;
        });
        let err = fetch_status(&[addr2], std::time::Duration::from_secs(5))
            .await
            .expect_err("a non-JSON body must be a clean error, not a panic");
        assert!(format!("{err:#}").contains("status document"));
    }

    #[tokio::test]
    async fn fetch_status_times_out_instead_of_hanging_on_a_silent_server() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (_socket, _peer) = listener.accept().await.unwrap();
            // Accepts, then never writes anything -- the socket stays open.
            std::future::pending::<()>().await
        });

        let started = std::time::Instant::now();
        let err = fetch_status(&[addr], std::time::Duration::from_millis(200))
            .await
            .expect_err("a silent server must time out, not hang forever");
        assert!(err.to_string().contains("timed out"));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "must not have hung past the configured timeout"
        );
    }

    // MAN-56: input-layer health counters wiring.

    /// A wrapper `IqSource` that forgets to forward `health_counters`
    /// silently swallows the inner source's counters via the trait's
    /// `None` default -- the metrics would just be absent, with nothing
    /// failing loudly. Same hazard `confirmed_live_handle` carries; both
    /// are asserted here.
    #[test]
    fn fixed_center_freq_source_forwards_both_optional_trait_signals() {
        use manta_input::InputHealthCounters;
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        struct StubSource {
            counters: Arc<InputHealthCounters>,
            live: Arc<AtomicBool>,
        }
        impl IqSource for StubSource {
            fn sample_rate(&self) -> f64 {
                48_000.0
            }
            fn center_freq_hz(&self) -> f64 {
                0.0
            }
            fn read(&mut self, _buf: &mut [num_complex::Complex32]) -> Result<usize> {
                Ok(0)
            }
            fn confirmed_live_handle(&self) -> Option<Arc<AtomicBool>> {
                Some(self.live.clone())
            }
            fn health_counters(&self) -> Option<Arc<InputHealthCounters>> {
                Some(self.counters.clone())
            }
        }

        let counters = Arc::new(InputHealthCounters::new());
        let live = Arc::new(AtomicBool::new(false));
        let wrapped = FixedCenterFreqSource {
            inner: Box::new(StubSource {
                counters: counters.clone(),
                live: live.clone(),
            }),
            freq_hz: 14_025_000.0,
        };

        assert!(Arc::ptr_eq(&wrapped.health_counters().unwrap(), &counters));
        assert!(Arc::ptr_eq(
            &wrapped.confirmed_live_handle().unwrap(),
            &live
        ));
    }

    #[test]
    fn input_health_of_snapshots_all_three_counters_without_transposing_them() {
        // Three same-typed u64s: a transposition would be invisible to any
        // test that used equal values (MAN-56 D7).
        let c = manta_input::InputHealthCounters::new();
        c.record_dropped(7);
        c.record_gap();
        c.record_gap();
        c.record_malformed();
        let h = input_health_of(&c);
        assert_eq!(h.dropped_packets, 7);
        assert_eq!(h.gaps_detected, 2);
        assert_eq!(h.malformed_packets, 1);
    }

    #[test]
    fn a_source_without_counters_publishes_no_input_health_series() {
        struct StubSourceNoCounters;
        impl IqSource for StubSourceNoCounters {
            fn sample_rate(&self) -> f64 {
                48_000.0
            }
            fn center_freq_hz(&self) -> f64 {
                0.0
            }
            fn read(&mut self, _buf: &mut [num_complex::Complex32]) -> Result<usize> {
                Ok(0)
            }
        }

        let m = manta_server::metrics::Metrics::new();
        // Mirrors the wiring above: `None` means we never call
        // set_input_health.
        let src: Box<dyn IqSource> = Box::new(StubSourceNoCounters);
        if let Some(c) = src.health_counters() {
            m.set_input_health("file", input_health_of(&c));
        }
        assert!(!m
            .render_prometheus_text()
            .contains("manta_input_malformed_packets_total{"));
    }

    // MAN-136 round-1 validate code-review finding 1: the increment
    // condition for `manta_spots_unresolved_geography_total` must match the
    // condition under which `SpotMessage::from_spot` emits the `UNKNOWN_*`
    // sentinels -- the RESOLVED ADIF entity number, not merely whether
    // `cty.lookup` returned an entry.

    const GEOGRAPHY_CTY_FIXTURE: &str = "\
United States:    5:  8: NA:  40.0:  75.0:  5.0:  K:
    K,W,N;
";
    /// One `dxcc.tsv` row for the fixture above, in the vendored file's
    /// `<primary-prefix>\t<adif-number>\t<name>` shape.
    const GEOGRAPHY_DXCC_FIXTURE: &str = "K\t291\tUnited States\n";

    #[test]
    fn a_callsign_with_a_resolved_entity_number_is_not_counted_as_unresolved() {
        let cty =
            manta_spot::cty::Table::parse_with_dxcc(GEOGRAPHY_CTY_FIXTURE, GEOGRAPHY_DXCC_FIXTURE);
        assert_eq!(cty.lookup("W1AW").and_then(|e| e.dxcc), Some(291));
        assert!(!geography_is_unresolved(&cty, "W1AW"));
    }

    #[test]
    fn an_unresolvable_callsign_is_counted_as_unresolved() {
        let cty =
            manta_spot::cty::Table::parse_with_dxcc(GEOGRAPHY_CTY_FIXTURE, GEOGRAPHY_DXCC_FIXTURE);
        assert!(cty.lookup("QQ1AAA").is_none(), "test premise");
        assert!(geography_is_unresolved(&cty, "QQ1AAA"));
    }

    #[test]
    fn a_maritime_or_aeronautical_mobile_callsign_is_counted_as_unresolved() {
        // /MM and /AM resolve through the base prefix, so the entity-number
        // test alone reads them as resolved -- but `SpotMessage::from_spot`
        // emits UNKNOWN_CONTINENT/UNKNOWN_CQ_ZONE and null lat/lon for them,
        // so the counter must not sit at zero while those go out.
        let cty =
            manta_spot::cty::Table::parse_with_dxcc(GEOGRAPHY_CTY_FIXTURE, GEOGRAPHY_DXCC_FIXTURE);
        assert_eq!(
            cty.lookup("W1AW/MM").and_then(|e| e.dxcc),
            Some(291),
            "test premise: the base prefix still resolves"
        );
        assert!(geography_is_unresolved(&cty, "W1AW/MM"));
        assert!(geography_is_unresolved(&cty, "W1AW/AM"));
        assert!(!geography_is_unresolved(&cty, "W1AW/P"));
    }

    #[test]
    fn a_cty_resolvable_callsign_with_no_dxcc_row_is_still_counted_as_unresolved() {
        // The cty.dat/dxcc.tsv drift state: `cty.dat` was hand-refreshed
        // (data/SOURCES.md has no refresh automation) without regenerating
        // the TSV, so geography resolves -- non-null dxLat/dxLon -- while
        // the entity number does not, and the spot goes out with
        // `dxDxcc: -1`. Counting `lookup().is_none()` missed exactly this.
        let cty = manta_spot::cty::Table::parse_with_dxcc(GEOGRAPHY_CTY_FIXTURE, "");
        let entry = cty.lookup("W1AW").expect("geography still resolves");
        assert_eq!(entry.dxcc, None, "test premise: only the number is missing");
        assert_eq!(entry.continent, "NA");
        assert!(
            geography_is_unresolved(&cty, "W1AW"),
            "a spot emitted with UNKNOWN_DXCC must be counted, even though cty.dat resolved it"
        );
    }
}
