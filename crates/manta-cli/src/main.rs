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
        /// HPSDR/Hermes (Metis) device hostname or IP, feature `hpsdr`.
        /// Requires --hpsdr-freq and --hpsdr-rate.
        #[cfg(feature = "hpsdr")]
        #[arg(long, conflicts_with_all = ["device", "source"])]
        hpsdr_host: Option<String>,
        /// HPSDR/Hermes control port (default 1024, the standard Metis
        /// discovery/control port).
        #[cfg(feature = "hpsdr")]
        #[arg(long, default_value_t = manta_input::hpsdr::CONTROL_PORT, requires = "hpsdr_host")]
        hpsdr_port: u16,
        /// RF center frequency in Hz. Required with --hpsdr-host.
        #[cfg(feature = "hpsdr")]
        #[arg(long, requires = "hpsdr_host")]
        hpsdr_freq: Option<f64>,
        /// Sample rate in Hz. Required with --hpsdr-host.
        #[cfg(feature = "hpsdr")]
        #[arg(long, requires = "hpsdr_host")]
        hpsdr_rate: Option<f64>,
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
        /// HPSDR/Hermes (Metis) device hostname or IP, feature `hpsdr`.
        /// Requires --hpsdr-freq and --hpsdr-rate.
        #[cfg(feature = "hpsdr")]
        #[arg(long, conflicts_with_all = ["device", "source"])]
        hpsdr_host: Option<String>,
        /// HPSDR/Hermes control port (default 1024, the standard Metis
        /// discovery/control port).
        #[cfg(feature = "hpsdr")]
        #[arg(long, default_value_t = manta_input::hpsdr::CONTROL_PORT, requires = "hpsdr_host")]
        hpsdr_port: u16,
        /// RF center frequency in Hz. Required with --hpsdr-host.
        #[cfg(feature = "hpsdr")]
        #[arg(long, requires = "hpsdr_host")]
        hpsdr_freq: Option<f64>,
        /// Sample rate in Hz. Required with --hpsdr-host.
        #[cfg(feature = "hpsdr")]
        #[arg(long, requires = "hpsdr_host")]
        hpsdr_rate: Option<f64>,
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

fn start_spot_server(
    config_path: &std::path::Path,
    sample_rate_hz: f64,
    epoch: std::time::SystemTime,
    session_nonce: u128,
) -> Result<(tokio::runtime::Runtime, SpotServer)> {
    let cfg_text = std::fs::read_to_string(config_path)?;
    let file: manta_server::config::DaemonConfigFile = toml::from_str(&cfg_text)?;
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
            shutdown_rx.clone(),
            tasks.clone(),
            manta_server::tasks::new_connection_limiter(
                manta_server::telnet::MAX_TELNET_CONNECTIONS,
            ),
        ));
        tokio::spawn(manta_server::json_stream::serve(
            json_listener,
            manta_server::json_stream::JsonStreamConfig {
                bus: bus.clone(),
                metrics: metrics.clone(),
                cty,
                station_call: cfg.station_callsign.clone(),
                decoder_version,
                shutdown: shutdown_rx,
            },
            tasks.clone(),
            manta_server::tasks::new_connection_limiter(
                manta_server::json_stream::MAX_JSON_STREAM_CONNECTIONS,
            ),
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
        ));

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
            server_config,
            dial_freq_hz,
            replay_epoch,
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
            let src: Box<dyn IqSource> = match dial_freq_hz {
                Some(freq_hz) => Box::new(FixedCenterFreqSource {
                    inner: src,
                    freq_hz,
                }),
                None => src,
            };

            // Kept alive for the process lifetime: dropping it would stop
            // the spawned server tasks. `None` when --server-config wasn't
            // given, in which case `spot_server` stays None too. `epoch`/
            // `session_nonce` are deliberately computed IN this branch, not
            // above it -- `--source`-only replay (no --server-config) never
            // consumes either, and computing `session_nonce` means hashing
            // the entire replayed file a second time after it's already
            // been opened; skip that full-file pass entirely when nothing
            // downstream needs it (round-7 review finding).
            let (server_runtime, spot_server) = match server_config {
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
        } => {
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
            let report = manta_engine::soak(src, &cfg, std::time::Duration::from_secs(duration))?;
            eprintln!("{report:?}");
            if !manta_engine::soak_passed(&report) {
                std::process::exit(1);
            }
        }
    }
    Ok(())
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
}
