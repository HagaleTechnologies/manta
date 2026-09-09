//! `manta` CLI. M0 surface: decode a WAV fixture, generate golden vectors.
//! The daemon (SDR input, servers) arrives at M2/M3 (ROADMAP).

mod config;

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
    ///
    /// `run` is the D11/MAN-77 daemon verb name; shipped here as a
    /// `visible_alias` so `manta run --config manta.toml` works today
    /// (MAN-74) without waiting on MAN-77's full promotion of `run` to
    /// canonical and retirement of `listen`.
    #[command(visible_alias = "run")]
    Listen {
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
        /// `None` (flag not passed) falls through to `input.freq_correction_ppm`
        /// in `--config`'s file, then to 0.0 (MAN-74) -- unlike `decode`
        /// (which has no config-file tier), this can't be a bare `f64`
        /// with `default_value_t`, since that makes "typed 0" and "typed
        /// nothing" indistinguishable.
        #[arg(
            long,
            value_parser = parse_freq_correction_ppm,
            allow_negative_numbers = true
        )]
        freq_correction_ppm: Option<f64>,
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
        /// The daemon TOML config (SPEC §9: `[server]`, `[[rbn_uplink]]`,
        /// `[input]`, `[spot]`, `[detector]`, `[decode]`) -- MAN-74. The
        /// telnet cluster server, JSON Lines/WebSocket stream, and metrics
        /// endpoint (ARCHITECTURE §7-§8) start alongside the decode loop
        /// iff the resolved config has a `[server]` table; a
        /// `[server]`-free file is a valid decode-only tuning config.
        /// `--server-config` is kept as a deprecated alias for existing
        /// unit files/scripts.
        #[arg(long, alias = "server-config")]
        config: Option<PathBuf>,
        /// RF dial frequency in Hz, overriding the source's own
        /// `center_freq_hz()`. Required with --server-config when the
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
        /// and --server-config; ignored for a live source. Without this,
        /// the epoch is the file's mtime, which is real and reproducible
        /// for an untouched file but changes if the file is copied,
        /// downloaded, or restored without preserving filesystem metadata
        /// -- pass this explicitly when byte-identical JSON `timestamp`/
        /// RBN Zulu output across environments matters more than "whatever
        /// this machine's copy of the file happens to say."
        #[arg(long, value_parser = parse_replay_epoch)]
        replay_epoch: Option<i64>,
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
        /// `None` falls through to `input.freq_correction_ppm` in
        /// `--config`'s file, then 0.0 (MAN-74) -- see `Listen`'s copy of
        /// this field for why it's `Option<f64>` rather than
        /// `default_value_t`.
        #[arg(
            long,
            value_parser = parse_freq_correction_ppm,
            allow_negative_numbers = true
        )]
        freq_correction_ppm: Option<f64>,
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
        /// The daemon TOML config (SPEC §9 `[input]`/`[spot]`/`[detector]`/
        /// `[decode]` tables; `soak` never starts the telnet/JSON/metrics
        /// servers, so `[server]`/`[[rbn_uplink]]` are parsed but unused
        /// here). MAN-74.
        #[arg(long)]
        config: Option<PathBuf>,
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

/// All CLI flags that select or configure a source, for `Listen` and `Soak`
/// alike -- built once per subcommand instead of each match arm repeating
/// the hpsdr > kiwi > soapy > audio/file priority chain and per-driver
/// required-field checks. Replaces the `KiwiOpts`/`SoapyOpts`/`HpsdrOpts`
/// flag-bag structs and the parallel `open_source`/`open_hpsdr_source`
/// bodies they existed to feed (MAN-74 Phase 3/5 refactor): `into_spec`
/// funnels every CLI source into the same `config::SourceSpec` the
/// `[input]`-table path already produces, so `open_source_spec` is the one
/// place that actually opens a connection.
struct SourceFlags {
    device: Option<String>,
    source: Option<PathBuf>,
    kiwi_host: Option<String>,
    kiwi_port: u16,
    kiwi_freq: Option<f64>,
    kiwi_password: String,
    #[cfg(feature = "soapy")]
    soapy_driver: Option<String>,
    #[cfg(feature = "soapy")]
    soapy_freq: Option<f64>,
    #[cfg(feature = "soapy")]
    soapy_rate: Option<f64>,
    #[cfg(feature = "soapy")]
    soapy_gain: Option<f64>,
    #[cfg(feature = "hpsdr")]
    hpsdr_host: Option<String>,
    #[cfg(feature = "hpsdr")]
    hpsdr_port: u16,
    #[cfg(feature = "hpsdr")]
    hpsdr_freq: Option<f64>,
    #[cfg(feature = "hpsdr")]
    hpsdr_rate: Option<f64>,
}

impl SourceFlags {
    #[cfg(feature = "soapy")]
    fn has_soapy(&self) -> bool {
        self.soapy_driver.is_some()
    }
    #[cfg(not(feature = "soapy"))]
    fn has_soapy(&self) -> bool {
        false
    }

    #[cfg(feature = "hpsdr")]
    fn has_hpsdr(&self) -> bool {
        self.hpsdr_host.is_some()
    }
    #[cfg(not(feature = "hpsdr"))]
    fn has_hpsdr(&self) -> bool {
        false
    }

    /// True if any source-selection flag was given -- MAN-74: a CLI source
    /// flag wins over `[input]` entirely (Decision 5), since mixing a CLI
    /// source into a file source field-by-field has no coherent meaning.
    fn has_source(&self) -> bool {
        self.device.is_some()
            || self.source.is_some()
            || self.kiwi_host.is_some()
            || self.has_soapy()
            || self.has_hpsdr()
    }

    /// True if the CLI source in effect reports its own real RF center
    /// frequency, unlike a plain audio device or WAV replay.
    fn is_rf_aware(&self) -> bool {
        self.kiwi_host.is_some() || self.has_soapy() || self.has_hpsdr()
    }

    /// hpsdr > kiwi > soapy > audio/file -- the same priority
    /// `open_hpsdr_source`/`open_source`'s call chain used: in practice
    /// only one of kiwi/soapy/hpsdr is ever set, since each already
    /// `conflicts_with_all` `device`/`source`.
    fn into_spec(self) -> Result<config::SourceSpec> {
        #[cfg(feature = "hpsdr")]
        if let Some(host) = self.hpsdr_host {
            let freq_hz = self
                .hpsdr_freq
                .ok_or_else(|| anyhow!("--hpsdr-freq is required with --hpsdr-host"))?;
            let rate_hz = self
                .hpsdr_rate
                .ok_or_else(|| anyhow!("--hpsdr-rate is required with --hpsdr-host"))?;
            return Ok(config::SourceSpec::Hpsdr {
                host,
                port: self.hpsdr_port,
                freq_hz,
                rate_hz,
            });
        }
        if let Some(host) = self.kiwi_host {
            let freq_hz = self
                .kiwi_freq
                .ok_or_else(|| anyhow!("--kiwi-freq is required with --kiwi-host"))?;
            return Ok(config::SourceSpec::Kiwi {
                host,
                port: self.kiwi_port,
                freq_hz,
                password: self.kiwi_password,
            });
        }
        #[cfg(feature = "soapy")]
        if let Some(driver) = self.soapy_driver {
            let freq_hz = self
                .soapy_freq
                .ok_or_else(|| anyhow!("--soapy-freq is required with --soapy-driver"))?;
            let rate_hz = self
                .soapy_rate
                .ok_or_else(|| anyhow!("--soapy-rate is required with --soapy-driver"))?;
            return Ok(config::SourceSpec::Soapy {
                driver,
                freq_hz,
                rate_hz,
                gain_db: self.soapy_gain,
            });
        }
        Ok(match self.source {
            Some(path) => config::SourceSpec::File { path },
            None => config::SourceSpec::Audio {
                device: self.device,
            },
        })
    }
}

fn open_audio_source(device: Option<String>, source: Option<PathBuf>) -> Result<Box<dyn IqSource>> {
    Ok(match source {
        Some(path) => Box::new(manta_input::AudioIqSource::from_wav_file(&path)?),
        None => Box::new(manta_input::AudioIqSource::from_device(device.as_deref())?),
    })
}

/// `--config`'s fallback: `MANTA_CONFIG`, read directly here rather than
/// through `apply_env_overlay`'s per-key table splice, since it names which
/// file to read rather than a value inside one (`config::ENV_CONFIG_PATH`'s
/// own doc comment). An explicit `--config` always wins, matching the
/// documented CLI-flag > env var precedence.
fn resolve_config_path(cli_config: Option<PathBuf>) -> Option<PathBuf> {
    cli_config.or_else(|| {
        std::env::var(config::ENV_CONFIG_PATH)
            .ok()
            .map(PathBuf::from)
    })
}

/// Opens a `config::SourceSpec`, whether it came from the `[input]` table
/// (`config::resolve_source`) or from CLI flags (`SourceFlags::into_spec`)
/// -- MAN-74 collapses what used to be `open_source`/`open_hpsdr_source`/
/// `open_audio_source`'s parallel, feature-cfg-gated chains into this one
/// function. `InputSource`/`SourceSpec` are never feature-gated (a config
/// naming `type = "soapy"`/`"hpsdr"` always parses, on every build -- MAN-74
/// Decision 3), so a build missing the matching Cargo feature fails HERE, at
/// source-open time, with a message naming the required `--features` flag,
/// rather than being unable to represent the variant at all.
fn open_source_spec(spec: config::SourceSpec) -> Result<Box<dyn IqSource>> {
    match spec {
        config::SourceSpec::Audio { device } => open_audio_source(device, None),
        config::SourceSpec::File { path } => open_audio_source(None, Some(path)),
        config::SourceSpec::Kiwi {
            host,
            port,
            freq_hz,
            password,
        } => Ok(Box::new(manta_input::kiwi::KiwiIqSource::connect(
            &host, port, freq_hz, &password,
        )?)),
        config::SourceSpec::Soapy {
            driver,
            freq_hz,
            rate_hz,
            gain_db,
        } => {
            #[cfg(feature = "soapy")]
            {
                Ok(Box::new(manta_input::soapy::SoapySdrIqSource::open(
                    &driver, rate_hz, freq_hz, gain_db,
                )?))
            }
            #[cfg(not(feature = "soapy"))]
            {
                let _ = (driver, freq_hz, rate_hz, gain_db);
                bail!("[input] type = \"soapy\" requires building manta with --features soapy");
            }
        }
        config::SourceSpec::Hpsdr {
            host,
            port,
            freq_hz,
            rate_hz,
        } => {
            #[cfg(feature = "hpsdr")]
            {
                let hpsdr_cfg = manta_input::hpsdr::HpsdrConfig {
                    host,
                    port,
                    ddc_count: 1,
                    sample_rate_hz: rate_hz,
                    center_freq_hz: vec![freq_hz],
                };
                let mut sources = manta_input::hpsdr::HpsdrDevice::open(hpsdr_cfg)?;
                Ok(Box::new(sources.remove(0)))
            }
            #[cfg(not(feature = "hpsdr"))]
            {
                let _ = (host, port, freq_hz, rate_hz);
                bail!("[input] type = \"hpsdr\" requires building manta with --features hpsdr");
            }
        }
    }
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
pub(crate) fn strip_bom(text: &str) -> &str {
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
) -> Result<PipelineConfig> {
    let mut cfg = PipelineConfig {
        freq_correction_ppm,
        allowlist,
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
const SHUTDOWN_DRAIN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(25);

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

// MAN-59: the daemon's only durable record of connection events/rejections
// was the live Prometheus counters (no history, reset on restart) --
// nothing to reconstruct WHAT happened or FROM WHERE after an abuse
// incident. `try_init` (not `init`, which panics on a second call) since
// this can now run from more than one call site (the `--server-config`
// deprecation warning, then again from `start_spot_server` if the resolved
// config has a `[server]` table) -- a defensive no-op on an
// already-initialized global subscriber costs nothing. `RUST_LOG`
// overrides; unset defaults to `info`, so an operator gets useful output
// (including the deprecation warning) with zero configuration, and can
// raise verbosity for deeper debugging without a code change.
//
// MAN-59 review round 6 (P1): `fmt()` writes to stdout by default, but
// `Command::Listen --json` ALSO writes DecoderEvents/spots as JSON Lines to
// stdout -- AGENTS.md's "file input -> byte-identical spot logs" hard
// requirement means any interleaved non-JSON tracing line corrupts that
// machine-readable stream for real consumers and breaks
// deterministic-replay byte-identity. stderr is a separate stream a
// JSON-Lines consumer never reads.
fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
}

fn start_spot_server(
    cfg: manta_server::config::ServerConfig,
    rbn_uplink_cfgs: Vec<manta_server::config::RbnUplinkConfig>,
    sample_rate_hz: f64,
    epoch: std::time::SystemTime,
    session_nonce: u128,
) -> Result<(tokio::runtime::Runtime, SpotServer)> {
    init_tracing();

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
    rt.block_on(async {
        let telnet_listener =
            tokio::net::TcpListener::bind((cfg.bind_addr.as_str(), cfg.telnet_port)).await?;
        let json_listener =
            tokio::net::TcpListener::bind((cfg.bind_addr.as_str(), cfg.json_port)).await?;
        let metrics_listener =
            tokio::net::TcpListener::bind((cfg.bind_addr.as_str(), cfg.metrics_port)).await?;

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
                cty,
                station_call: cfg.station_callsign.clone(),
                decoder_version,
                // .clone(): MAN-32/MAN-42's uplink::serve spawns below also
                // need shutdown_rx -- can't let this be the moving consumer
                // anymore now that there are more consumers.
                shutdown: shutdown_rx.clone(),
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
        // MAN-32/MAN-42: one independent uplink::serve task per configured
        // [[rbn_uplink]] entry -- the common case for existing single-node
        // operators is no [[rbn_uplink]] tables at all (empty Vec, loop
        // body never runs), and uplink::serve itself also no-ops when
        // `enabled = false` (belt-and-suspenders, not a duplicate check:
        // this loop additionally avoids spawning a task at all when the
        // Vec is empty). Each task owns its own SpotBus subscription and
        // backoff state, so one target being down never affects another's
        // delivery or retry timing.
        for uplink_cfg in rbn_uplink_cfgs {
            tokio::spawn(manta_server::uplink::serve(
                uplink_cfg,
                cfg.station_callsign.clone(),
                bus.clone(),
                metrics.clone(),
                shutdown_rx.clone(),
            ));
        }

        anyhow::Ok(())
    })?;

    Ok((
        rt,
        SpotServer {
            bus,
            metrics,
            shutdown_tx,
            tasks,
        },
    ))
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Decode {
            path,
            json,
            freq_correction_ppm,
            allowlist,
            blocklist,
            notch,
        } => {
            let cfg = build_pipeline_config(freq_correction_ppm, allowlist, blocklist, notch)?;
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
            config: config_path,
            dial_freq_hz,
            replay_epoch,
        } => {
            if std::env::args().any(|arg| arg == "--server-config") {
                init_tracing();
                tracing::warn!("--server-config is deprecated; use --config instead");
            }

            let cli_source = SourceFlags {
                device,
                source,
                kiwi_host,
                kiwi_port,
                kiwi_freq,
                kiwi_password,
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
            };
            // MAN-74: any CLI source-selection flag wins over `[input]`
            // entirely -- mixing fields across source TYPES field-by-field
            // has no coherent meaning, and this leaves clap's existing
            // conflicts_with_all/requires groups untouched.
            let cli_has_source_flags = cli_source.has_source();

            let config_path = resolve_config_path(config_path);
            let base_dir = config_path
                .as_deref()
                .and_then(std::path::Path::parent)
                .map(std::path::Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            let file = config::load(config_path.as_deref())?;
            let file_source = if cli_has_source_flags {
                None
            } else {
                config::resolve_source(&file, &base_dir)
            };

            let is_file_replay = cli_source.source.is_some()
                || matches!(&file_source, Some(config::SourceSpec::File { .. }));
            // Captured before `cli_source`/`file_source` are consumed below
            // -- needed to derive a recording-specific replay epoch. A
            // `[input] type = "file"` source (MAN-74) replays exactly like
            // `--source`, so it feeds the same epoch/session-nonce
            // derivation.
            let replay_path = cli_source.source.clone().or_else(|| match &file_source {
                Some(config::SourceSpec::File { path }) => Some(path.clone()),
                _ => None,
            });
            let has_rf_aware_source = if cli_has_source_flags {
                cli_source.is_rf_aware()
            } else {
                file_source
                    .as_ref()
                    .is_some_and(config::SourceSpec::is_rf_aware)
            };
            let source_name = if cli_has_source_flags {
                if cli_source.kiwi_host.is_some() {
                    "kiwi"
                } else if cli_source.has_soapy() {
                    "soapy"
                } else if cli_source.has_hpsdr() {
                    "hpsdr"
                } else if is_file_replay {
                    "file"
                } else {
                    "audio"
                }
            } else {
                match &file_source {
                    Some(config::SourceSpec::Kiwi { .. }) => "kiwi",
                    Some(config::SourceSpec::Soapy { .. }) => "soapy",
                    Some(config::SourceSpec::Hpsdr { .. }) => "hpsdr",
                    Some(config::SourceSpec::File { .. }) => "file",
                    Some(config::SourceSpec::Audio { .. }) | None => "audio",
                }
            };

            // The resolved dial frequency: an explicit CLI flag wins over
            // `[input].dial_freq_hz` (MAN-74). The file's `dial_freq_hz`
            // only carries over when the source actually in effect is
            // non-RF-aware -- a plain audio device or WAV replay (whether
            // chosen via `[input]` or `--device`/`--source`) reports no
            // real RF frequency of its own, so the file's value legitimately
            // still applies per Decision 5. But an RF-aware CLI source flag
            // (`--kiwi-host`/`--soapy-*`/`--hpsdr-*`) discards `[input]`
            // wholesale (`file_source`/`has_rf_aware_source` above) and
            // reports its own center frequency, so a config's stale
            // `dial_freq_hz` left over for a DIFFERENT source must not
            // silently wrap it in the wrong center frequency (round-2
            // finding C-2).
            let resolved_dial_freq_hz = dial_freq_hz.or_else(|| {
                if cli_has_source_flags && has_rf_aware_source {
                    None
                } else {
                    file.input
                        .as_ref()
                        .and_then(config::InputSource::dial_freq_hz)
                }
            });

            if file.server.is_some() && !has_rf_aware_source && resolved_dial_freq_hz.is_none() {
                bail!(
                    "--dial-freq-hz (or input.dial_freq_hz in your config file) is required \
                     with a [server] table when using a plain audio device or a WAV file \
                     source -- neither reports a real RF frequency (KiwiSDR/SoapySDR already \
                     know theirs from --kiwi-freq/--soapy-freq or the [input] table)"
                );
            }

            let cli_overrides = config::CliOverrides {
                freq_correction_ppm,
                // Same condition as `resolved_dial_freq_hz` above (round-2
                // finding C-2 / code-review finding 1): an RF-aware CLI
                // source flag discards `[input]` wholesale, so its sibling
                // `freq_correction_ppm` -- calibrated for whatever hardware
                // the file described -- must not silently carry over either.
                freq_correction_ppm_file_suppressed: cli_has_source_flags && has_rf_aware_source,
                allowlist,
                blocklist,
                notch,
            };
            let cfg = config::resolve_pipeline(&file, &base_dir, &cli_overrides)?;

            let src: Box<dyn IqSource> = match (&file_source, cli_has_source_flags) {
                (Some(spec), false) => open_source_spec(spec.clone())?,
                _ => open_source_spec(cli_source.into_spec()?)?,
            };
            let src: Box<dyn IqSource> = match resolved_dial_freq_hz {
                Some(freq_hz) => Box::new(FixedCenterFreqSource {
                    inner: src,
                    freq_hz,
                }),
                None => src,
            };

            // Kept alive for the process lifetime: dropping it would stop
            // the spawned server tasks. `None` when the resolved config has
            // no `[server]` table, in which case `spot_server` stays None
            // too (MAN-74 Decision 6: servers start iff `[server]` is
            // present, not merely because `--config` was given). `epoch`/
            // `session_nonce` are deliberately computed IN this branch, not
            // above it -- a source-only replay (no `[server]`) never
            // consumes either, and computing `session_nonce` means hashing
            // the entire replayed file a second time after it's already
            // been opened; skip that full-file pass entirely when nothing
            // downstream needs it (round-7 review finding).
            let (server_runtime, spot_server) = match file.server.clone() {
                Some(server_cfg) => {
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

                    let (rt, server) = start_spot_server(
                        server_cfg,
                        file.rbn_uplink.clone(),
                        src.sample_rate(),
                        epoch,
                        session_nonce,
                    )?;
                    // Real, if coarse, health signal: this source opened
                    // and is running. `active_tracks` has no equivalent
                    // hook yet -- manta-engine exposes no live track-count
                    // API for `listen()`'s callbacks to read, so it stays
                    // at Metrics::default()'s 0 until that surface exists.
                    //
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
                                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                                }
                                metrics.set_source_health(source_name, true);
                            });
                        }
                        None => server.metrics.set_source_health(source_name, true),
                    }
                    (Some(rt), Some(server))
                }
                None => (None, None),
            };

            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let stop_handler = stop.clone();
            ctrlc::set_handler(move || {
                stop_handler.store(true, std::sync::atomic::Ordering::Relaxed);
            })?;
            let listen_result = manta_engine::listen(
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
            config: config_path,
        } => {
            let cli_source = SourceFlags {
                device,
                source,
                kiwi_host,
                kiwi_port,
                kiwi_freq,
                kiwi_password,
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
            };
            let cli_has_source_flags = cli_source.has_source();
            // Same RF-aware-source test `Listen` uses for its
            // `resolved_dial_freq_hz`/`freq_correction_ppm_file_suppressed`
            // suppression -- `soak` shares the same CLI source flags, so a
            // stale `[input]` calibration must be suppressed here too.
            let has_rf_aware_source = cli_has_source_flags && cli_source.is_rf_aware();

            let config_path = resolve_config_path(config_path);
            let base_dir = config_path
                .as_deref()
                .and_then(std::path::Path::parent)
                .map(std::path::Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            let file = config::load(config_path.as_deref())?;
            let file_source = if cli_has_source_flags {
                None
            } else {
                config::resolve_source(&file, &base_dir)
            };

            let cli_overrides = config::CliOverrides {
                freq_correction_ppm,
                freq_correction_ppm_file_suppressed: has_rf_aware_source,
                allowlist,
                blocklist,
                notch,
            };
            let cfg = config::resolve_pipeline(&file, &base_dir, &cli_overrides)?;

            let src: Box<dyn IqSource> = match (&file_source, cli_has_source_flags) {
                (Some(spec), false) => open_source_spec(spec.clone())?,
                _ => open_source_spec(cli_source.into_spec()?)?,
            };
            let report = manta_engine::soak(src, &cfg, std::time::Duration::from_secs(duration))?;
            eprintln!("{report:?}");
            if !manta_engine::soak_passed(&report) {
                std::process::exit(1);
            }
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
            let cfg = build_pipeline_config(freq_correction_ppm, allowlist, blocklist, notch)?;
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

        let parsed = config::load(Some(cfg_file.path())).unwrap();
        let (rt, _server) = start_spot_server(
            parsed.server.clone().unwrap(),
            parsed.rbn_uplink.clone(),
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

        let parsed = config::load(Some(cfg_file.path())).unwrap();
        let (rt, _server) = start_spot_server(
            parsed.server.clone().unwrap(),
            parsed.rbn_uplink.clone(),
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

        let parsed = config::load(Some(cfg_file.path())).unwrap();
        let (rt, _server) = start_spot_server(
            parsed.server.clone().unwrap(),
            parsed.rbn_uplink.clone(),
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
}
