//! `manta` CLI: decode CW from a recorded file or a live receiver, and run
//! the spotting daemon.

// MAN-156: this binary's `///` comments are clap `--help` and man-page text,
// printed verbatim, so they keep plain-text placeholders such as `<TABLE>`
// and `[spot]` that rustdoc would parse as HTML tags and links. CI's
// `RUSTDOCFLAGS=-D warnings` doc build still applies these lints to every
// library crate.
#![allow(
    rustdoc::broken_intra_doc_links,
    rustdoc::invalid_html_tags,
    rustdoc::bare_urls
)]

use anyhow::{anyhow, bail, Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use cli_parse::parse_capture_rate_hz;
use cli_parse::parse_capture_start;
use cli_parse::parse_center_freq_hz;
use cli_parse::parse_dial_freq_hz;
use cli_parse::parse_engine;
use cli_parse::parse_freq_correction_ppm;
use cli_parse::parse_funnel_bypass;
use cli_parse::parse_funnel_tol;
use cli_parse::parse_hpsdr_freq_hz;
use cli_parse::parse_hpsdr_mac;
use cli_parse::parse_hpsdr_rate_hz;
use cli_parse::parse_iq_sample_rate_hz;
use cli_parse::parse_ka9q_group;
use cli_parse::parse_prior;
use cli_parse::parse_rbn_spotter;
use cli_parse::parse_reference_hz;
use cli_parse::parse_search_ppm;
use cli_parse::parse_tune_hz;
use cli_parse::parse_window_s;
use cli_parse::HPSDR_CONTROL_PORT;
use daemon_client::run_status;
use daemon_client::status_exit_code;
use daemon_client::status_failure_message;
use daemon_client::strict_exit_code;
use manta_decode::decoder::Engine;
use manta_engine::PipelineConfig;
use manta_input::IqSource;
use resolve::needs_dial_freq;
use resolve::record_iq_ignored_note;
use resolve::replay_mode_ignored_note;
use resolve::resolve;
use resolve::resolve_spot_pinned;
use resolve::select_input;
use resolve::CliOverrides;
use resolve::CliPins;
use resolve::Resolved;
use resolve::SpotResolved;
use session_epoch::cw_replay_epoch;
use source_spec::recording_center_warning;
use source_spec::trusted_band_notice;
use source_spec::warn_if_audio_source_has_no_rf_reference;
use source_spec::HpsdrOpts;
use source_spec::HpsdrProtocolArg;
use source_spec::Ka9qOpts;
use source_spec::KiwiOpts;
use source_spec::LiveSourceSpec;
use spot_server::SpotServer;
use std::path::{Path, PathBuf};

mod band_streams;
mod bench;
mod build_info;
mod calibrate_cmd;
mod callsign_lookup;
mod cli_docs;
mod cli_parse;
mod clock_check;
mod config;
mod config_cmd;
mod config_edit;
mod daemon_client;
mod devices;
mod doctor_checks;
mod fmt;
mod ftx_run;
mod funnel_cmd;
mod grade_cmd;
mod grade_paired;
mod grade_transcript;
mod health_cmd;
mod hpsdr_group;
mod list_cache;
mod list_http;
mod list_refresh;
mod live_state;
mod logging;
mod multi_run;
mod observations;
mod qrz;
mod reconnect;
mod recording;
mod relay_socket;
mod reload;
mod resolve;
mod run_cmd;
mod session_epoch;
mod silent;
mod soapy_opts;
mod source_check;
mod source_spec;
mod spectrum_feed;
mod spot_outputs;
mod spot_server;
mod stdio;
#[cfg(test)]
mod test_ports;
#[cfg(test)]
mod test_support;
mod text_lines;
use list_cache::ListKind;
use stdio::{errln, out, outln};

#[derive(Parser)]
#[command(
    name = "manta",
    // Without an explicit bin_name, clap falls back to the runtime argv[0]
    // for "Usage: ..." lines -- on Windows that's "manta.exe" (the actual
    // executable filename), not "manta". Pin it so help/usage text (and
    // tests that assert against it, e.g. crates/manta-cli/tests/cli.rs) is
    // identical across platforms.
    bin_name = "manta",
    // MAN-83: commit + features, so a support request needs nothing else
    version = build_info::VERSION_LINE,
    about = "Open-source wideband CW skimmer: every CW signal in an SDR passband, decoded at once, emitted as RBN-compatible spots",
    after_help = "\
Examples:
  manta gen v1 --out /tmp/v1          Make a synthetic test recording
  manta decode /tmp/v1/v1.wav         Decode it, no radio needed
  manta listen                        Copy from the default sound card
  manta listen --kiwi-host rx.example.org --kiwi-freq-hz 7030000
                                      Copy from a public KiwiSDR on 40 m

Run `manta <command> --help` for a command's full options."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List audio inputs and available SDR devices.
    Devices {
        /// Print one JSON report.
        #[arg(long)]
        json: bool,
    },
    /// Read a short source sample and report its level without decoding.
    #[command(
        long_about = "Read a short source sample and report its level without decoding. SOURCE is an audio WAV path, equivalent to --source PATH; use --source-iq for complex IQ. Without a selector, use the configured input or default audio input. The sampling deadline is duration + 5 seconds after opening, checked between reads. Native open/read calls can exceed it."
    )]
    Check(source_check::Args),
    /// Decode CW from a recorded IQ file.
    ///
    /// Reads a stereo WAV file (channel 0 = I, channel 1 = Q), a manta IQ
    /// recording, or headerless IQ, decodes the CW in it, and prints the
    /// copied text plus a spot summary. Fully deterministic: the same file
    /// always produces the same output.
    Decode {
        /// Stereo IQ WAV file: 32-bit float, or 16-, 24- or 32-bit integer
        /// samples. Its centre frequency is read from the matching
        /// `<name>.json` sidecar next to it, unless --center-freq-hz is given.
        /// A manta IQ recording (`run --record-iq`) is found by its
        /// PATH.meta.jsonl, which supplies its rate and centre.
        path: PathBuf,
        /// RF centre frequency of the recording, in Hz, overriding its sidecar.
        ///
        /// Without this flag or a sidecar, reported frequencies are baseband
        /// offsets from the recording's centre, and manta prints a warning.
        #[arg(long, value_name = "HZ", value_parser = parse_center_freq_hz)]
        center_freq_hz: Option<f64>,
        /// Read headerless little-endian IQ: cf32 or ci16.
        ///
        /// Requires a sample rate and centre frequency (--sample-rate-hz,
        /// --center-freq-hz). Omit for WAV or a manta recording with
        /// metadata.
        #[arg(long, value_name = "FORMAT")]
        iq_format: Option<manta_input::RawIqFormat>,
        /// Sample rate of headerless IQ.
        #[arg(long, value_name = "HZ", value_parser = parse_iq_sample_rate_hz, requires = "iq_format")]
        sample_rate_hz: Option<f64>,
        /// Print the full decode report as one JSON object.
        #[arg(long, help_heading = "Output")]
        json: bool,
        /// Also print the main signal's learned dot, dash and gap lengths
        /// (in dits) on stderr. `--json` always includes them.
        #[arg(long, help_heading = "Output")]
        timing: bool,
        /// Write why each callsign candidate was or was not spotted to PATH as
        /// JSON Lines (schema manta-observations.v1, see docs/OBSERVATIONS.md).
        ///
        /// PATH must not exist. Holds callsigns, reasons and scores, not the
        /// decoded text; add --observation-text for that.
        #[arg(long, value_name = "PATH", help_heading = "Output")]
        observations: Option<PathBuf>,
        /// Also write each candidate's characters, confidences, erasures and
        /// decoder alternatives. Requires --observations.
        #[arg(long, requires = "observations", help_heading = "Output")]
        observation_text: bool,
        /// TOML config file; the whole file is validated.
        ///
        /// Its `[decode]` values are the baseline; an explicit --engine
        /// overrides just the engine setting. The environment (MANTA_CONFIG,
        /// MANTA_*) is not read here, so output never depends on it.
        // `--server-config` stays as a hidden alias for the flag's old
        // name; help advertises the canonical spelling only (D11/MAN-77).
        #[arg(long, alias = "server-config")]
        config: Option<PathBuf>,
        /// Decode engine: `legacy` (the default), `edge-legacy`, or `hsmm`.
        ///
        /// `hsmm` is experimental: it was tested and did not pass the accuracy
        /// and CPU-cost checks needed to become the default. Left unset
        /// rather than defaulting, so that an explicit choice can be told
        /// apart from no choice at all: when --config also names an engine,
        /// this flag wins if given, and the file's value is used otherwise.
        // hsmm's promotion gate was measured and failed on 2026-09-09, on both
        // accuracy and CPU budget:
        // docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md.
        #[arg(long, value_parser = parse_engine)]
        engine: Option<Engine>,
        /// Reweight acoustically viable word readings: on or off. Overrides [decode].
        #[arg(long, value_parser = parse_prior)]
        prior: Option<manta_decode::config_file::PriorSwitch>,
        #[command(flatten)]
        filters: FilterOpts,
    },
    /// Score the decoder against a reference recording and a spot log.
    ///
    /// Decodes each station a reference skimmer reported, one channel at a
    /// time with the tracker bypassed, and reports how many callsigns came
    /// back. For measuring decode quality, not for day-to-day operating.
    Oracle {
        /// Stereo IQ WAV file: 32-bit float, or 16-, 24- or 32-bit integer
        /// samples. Its centre frequency is read from the matching
        /// `<name>.json` sidecar next to it. A gap-free manta IQ recording
        /// (with its PATH.meta.jsonl) is read from its metadata instead.
        path: PathBuf,
        /// Reverse Beacon Network daily-dump CSV, pre-filtered to the
        /// recording's time window.
        rbn_csv: PathBuf,
        /// Spotter callsign whose spots are the reference set -- normally
        /// the skimmer that sat next to this receiver. Required: the right
        /// call depends on the recording.
        #[arg(long)]
        spotter: String,
        /// When the recording starts, as ISO-8601 UTC (for example
        /// 2025-11-29T00:00:00Z).
        ///
        /// Anchors the reference spot times to the recording instead of
        /// assuming it began exactly on the hour.
        #[arg(long, value_parser = parse_capture_start)]
        capture_start: i64,
        /// How many seconds around each reference spot to decode.
        #[arg(long, default_value_t = 40.0, value_parser = parse_window_s)]
        window_s: f64,
        /// TOML config file; the whole file is validated.
        ///
        /// Its `[decode]` values are the baseline; an explicit --engine
        /// overrides just the engine setting. The environment (MANTA_CONFIG,
        /// MANTA_*) is not read here, so output never depends on it.
        // `--server-config` stays as a hidden alias for the flag's old
        // name; help advertises the canonical spelling only (D11/MAN-77).
        #[arg(long, alias = "server-config")]
        config: Option<PathBuf>,
        /// Decode engine: `legacy` (the default), `edge-legacy`, or `hsmm`.
        ///
        /// `hsmm` is experimental: it was tested and did not pass the accuracy
        /// and CPU-cost checks needed to become the default. Left unset
        /// rather than defaulting, so that an explicit choice can be told
        /// apart from no choice at all: when --config also names an engine,
        /// this flag wins if given, and the file's value is used otherwise.
        // hsmm's promotion gate was measured and failed on 2026-09-09, on both
        // accuracy and CPU budget:
        // docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md.
        #[arg(long, value_parser = parse_engine)]
        engine: Option<Engine>,
        /// Reweight acoustically viable word readings: on or off. Overrides [decode].
        #[arg(long, value_parser = parse_prior)]
        prior: Option<manta_decode::config_file::PriorSwitch>,
        /// Write one JSON object per reference spot to this file. The
        /// summary always goes to stdout as well.
        #[arg(long)]
        jsonl: Option<PathBuf>,
        /// Also run the full pipeline once and write a stage-attributed loss
        /// funnel (JSON Lines) to PATH. Diagnostic only: stdout and --jsonl
        /// are unchanged
        #[arg(long, value_name = "PATH")]
        funnel: Option<PathBuf>,
        /// Add oracle-bypass experiments to the funnel file, comma-separated:
        /// known-frequency, known-timing. Each forces stages with truth and
        /// reports an upper bound after the production summary; never a
        /// score. Requires --funnel
        #[arg(
            long,
            value_name = "BYPASS",
            value_delimiter = ',',
            requires = "funnel",
            value_parser = parse_funnel_bypass
        )]
        funnel_bypass: Vec<manta_engine::funnel::Bypass>,
        /// Frequency tolerance for matching a candidate or track to a truth
        /// row, plus half a channel width
        #[arg(long, value_name = "HZ", default_value_t = 150.0, value_parser = parse_funnel_tol)]
        funnel_track_tol_hz: f64,
        /// Frequency tolerance for matching a decision or spot to a truth row
        #[arg(long, value_name = "HZ", default_value_t = 500.0, value_parser = parse_funnel_tol)]
        funnel_spot_tol_hz: f64,
        /// Time tolerance for matching a decision or spot to a truth row's
        /// first and last report
        #[arg(long, value_name = "SECONDS", default_value_t = 90.0, value_parser = parse_funnel_tol)]
        funnel_spot_window_s: f64,
    },
    /// Generate a synthetic CW test recording.
    ///
    /// Writes a WAV file plus its sidecar and manifest, for testing a
    /// decode without any radio hardware. Each named recording is a fixed,
    /// reproducible scenario -- clean signal, weak signal, fading, high
    /// speed, two stations sharing one channel -- so the same name always
    /// produces the same file.
    Gen {
        /// Which test recording to generate: v1 to v6, vr1 to vr5, vr6a,
        /// vr6b, vr7, vr8, or vw1 to vw5.
        vector: String,
        /// Directory to write `<name>.wav`, `<name>.json` and
        /// `<name>.manifest.json` into.
        #[arg(long)]
        out: PathBuf,
    },
    /// Decode CW live from a receiver or sound card, or run the spotting
    /// daemon.
    ///
    /// Prints a SPOT: line on stdout for each confirmed spot, and decoded
    /// text on stderr, one line per track. With --config, also runs the
    /// full spotting daemon: the telnet cluster server, the JSON Lines /
    /// WebSocket stream, and the metrics endpoint; decoded text is then
    /// off unless --decoded-text is given.
    // `run` is the daemon entry point; `listen` stays as a visible alias
    // for ad hoc audio/dev testing (D11/MAN-77, see
    // docs/DECISIONS/2026-09-06-broad-review-decisions.md).
    // MAN-306: --hpsdr-protocol needs an HPSDR selector: a host, a MAC, or
    // the flags of a hostless (discovered) radio.
    #[command(visible_alias = "listen", group(clap::ArgGroup::new("hpsdr_selector").args(["hpsdr_host", "hpsdr_mac", "hpsdr_port", "hpsdr_freq_hz", "hpsdr_rate_hz"]).multiple(true)))]
    Run(run_cmd::RunArgs),
    /// Run the live decoder for a fixed duration as a stability check.
    ///
    /// Reads from the same sources `listen` does, runs for the full --duration
    /// seconds, and exits non-zero if the pipeline panics or its memory
    /// use keeps growing. A --source recording that ends sooner does not end
    /// the run: soak waits out the rest of --duration before it reports.
    /// Intended for unattended stability runs, not for day-to-day operating.
    // MAN-306: --hpsdr-protocol needs an HPSDR selector: a host, a MAC, or
    // the flags of a hostless (discovered) radio.
    #[command(group(clap::ArgGroup::new("hpsdr_selector").args(["hpsdr_host", "hpsdr_mac", "hpsdr_port", "hpsdr_freq_hz", "hpsdr_rate_hz"]).multiple(true)))]
    Soak {
        /// With several [[input]] entries in the config, the one to examine,
        /// by its name. Optional with one input.
        #[arg(long, value_name = "NAME")]
        input: Option<String>,
        /// How long to run, in seconds. The run lasts this long even when a
        /// --source recording ends sooner.
        #[arg(long)]
        duration: u64,
        /// Sound-card input device; matched by substring. Defaults to the
        /// system default input.
        #[arg(long, conflicts_with = "source", help_heading = "Audio input")]
        device: Option<String>,
        /// Replay a WAV file instead of listening to a live device.
        ///
        /// Decoded as fast as the machine manages, not in real time, but the
        /// run still lasts the full --duration: given a 60-second file and
        /// --duration 3600, soak decodes the file, then waits, decoding nothing,
        /// until the hour is up and prints its report. A live source keeps the
        /// decoder busy for the whole run.
        #[arg(long, conflicts_with = "device", help_heading = "Audio input")]
        source: Option<PathBuf>,
        /// KiwiSDR receiver hostname. Requires --kiwi-freq-hz.
        #[arg(help_heading = "KiwiSDR source")]
        #[arg(long, conflicts_with_all = crate::soapy_opts::and_driver(["device", "source", "ka9q_group", "hpsdr_host", "hpsdr_mac", "hpsdr_freq_hz", "hpsdr_rate_hz", "hpsdr_port"]), requires = "kiwi_freq_hz")]
        kiwi_host: Option<String>,
        /// KiwiSDR receiver port.
        #[arg(
            long,
            default_value = "8073",
            requires = "kiwi_host",
            help_heading = "KiwiSDR source"
        )]
        kiwi_port: u16,
        /// Receiver centre frequency, in Hz. Required with --kiwi-host.
        #[arg(
            long = "kiwi-freq-hz",
            alias = "kiwi-freq",
            requires = "kiwi_host",
            help_heading = "KiwiSDR source"
        )]
        kiwi_freq_hz: Option<f64>,
        /// KiwiSDR password. Leave empty for public receivers that do not
        /// ask for one.
        #[arg(
            long,
            requires = "kiwi_host",
            default_value = "",
            help_heading = "KiwiSDR source"
        )]
        kiwi_password: String,
        #[command(flatten)]
        soapy: crate::soapy_opts::SoapyArgs,
        /// openHPSDR radio hostname or IP address (protocol 1: Hermes-Lite 2, Hermes; protocol 2: ANAN).
        ///
        /// Requires --hpsdr-freq-hz and --hpsdr-rate-hz. Included in every build.
        #[arg(help_heading = "HPSDR source")]
        #[arg(long, conflicts_with_all = crate::soapy_opts::and_driver(["device", "source", "ka9q_group", "kiwi_host"]))]
        #[arg(conflicts_with_all = ["device", "source"])]
        #[arg(conflicts_with = "hpsdr_mac")]
        hpsdr_host: Option<String>,
        /// Discover and select a radio by MAC address; omit host to discover.
        #[arg(long, value_parser = parse_hpsdr_mac, help_heading = "HPSDR source", conflicts_with_all = ["hpsdr_host", "device", "source"])]
        hpsdr_mac: Option<[u8; 6]>,
        /// openHPSDR control port.
        ///
        /// Protocol 2 also sends to the three ports above it and receives IQ from this port + 11 (1035 by default).
        #[arg(long, help_heading = "HPSDR source")]
        #[arg(conflicts_with_all = ["device", "source"])]
        #[arg(value_parser = clap::value_parser!(u16).range(1..))]
        hpsdr_port: Option<u16>,
        /// openHPSDR protocol the radio speaks.
        ///
        /// auto asks the radio and uses protocol 2 when it answers as protocol 2, protocol 1 otherwise; 1 and 2 skip the question.
        #[arg(
            long,
            value_enum,
            default_value_t = HpsdrProtocolArg::Auto,
            requires = "hpsdr_selector",
            help_heading = "HPSDR source"
        )]
        hpsdr_protocol: HpsdrProtocolArg,
        /// Receiver centre frequency, in Hz. Required with the hpsdr source.
        #[arg(
            long = "hpsdr-freq-hz",
            alias = "hpsdr-freq",
            value_parser = parse_hpsdr_freq_hz,
            help_heading = "HPSDR source"
        )]
        #[arg(conflicts_with_all = ["device", "source"])]
        hpsdr_freq_hz: Option<f64>,
        /// Sample rate, in Hz. Required with the hpsdr source.
        #[arg(
            long = "hpsdr-rate-hz",
            alias = "hpsdr-rate",
            value_parser = parse_hpsdr_rate_hz,
            help_heading = "HPSDR source"
        )]
        #[arg(conflicts_with_all = ["device", "source"])]
        hpsdr_rate_hz: Option<f64>,
        /// Multicast data group of a ka9q-radio (radiod) IQ channel
        ///
        /// An IPv4 multicast address, or the name in the channel's `data =`
        /// line (for example cw-20m.local) when this machine resolves mDNS
        /// names. manta joins the group and reads the channel's sample rate,
        /// encoding and frequency from radiod's status packets; it does not
        /// create or retune channels.
        #[arg(help_heading = "ka9q-radio source", value_parser = parse_ka9q_group)]
        #[arg(long, conflicts_with_all = crate::soapy_opts::and_driver(["device", "source", "kiwi_host", "hpsdr_host", "hpsdr_mac", "hpsdr_freq_hz", "hpsdr_rate_hz", "hpsdr_port"]))]
        ka9q_group: Option<String>,
        /// RTP data port of the group.
        #[arg(
            long,
            default_value_t = manta_input::ka9q::wire::DATA_PORT,
            value_parser = clap::value_parser!(u16).range(1..),
            requires = "ka9q_group",
            help_heading = "ka9q-radio source"
        )]
        ka9q_port: u16,
        /// SSRC of the channel to read, when the group carries more than one.
        #[arg(long, requires = "ka9q_group", help_heading = "ka9q-radio source")]
        ka9q_ssrc: Option<u32>,
        /// IPv4 address of the local interface to join the group on. manta
        /// also joins on 127.0.0.1, for a radiod on this machine.
        #[arg(
            long,
            requires = "ka9q_group",
            value_parser = clap::value_parser!(std::net::Ipv4Addr),
            help_heading = "ka9q-radio source"
        )]
        ka9q_interface: Option<std::net::Ipv4Addr>,
        /// Decimate the source to this sample rate, in Hz, before decoding.
        ///
        /// Must divide the source's own rate by a power of two, and the
        /// result must itself be a rate the channelizer supports. Omit to
        /// decode at the source's native rate.
        #[arg(long, value_parser = parse_capture_rate_hz)]
        capture_rate_hz: Option<f64>,
        /// Read --source as a raw complex-IQ recording rather than ordinary
        /// mono audio.
        ///
        /// Without this, --source is taken as mono real audio at 48000 Hz.
        /// A stereo audio recording and a two-channel IQ capture cannot be
        /// told apart from the WAV header alone, so say which you have.
        #[arg(long)]
        source_iq: bool,
        #[command(flatten)]
        filters: FilterOpts,
        /// Radio dial frequency, in Hz. See `run --dial-freq-hz`; without it
        /// an audio source's reported frequencies are baseband offsets.
        #[arg(long, value_parser = parse_dial_freq_hz, help_heading = "Audio input")]
        dial_freq_hz: Option<f64>,
        /// TOML config file whose `[input]`, `[spot]`, `[detector]` and
        /// `[decode]` tables supply what the flags leave unset.
        ///
        /// Falls back to $MANTA_CONFIG. Flags override the file, and
        /// MANTA_<TABLE>_<KEY> environment variables sit between the two.
        /// `soak` never starts the spot servers.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Ask a running daemon whether its RBN uplink is healthy.
    ///
    // MAN-44.
    /// Reads the `GET /status` document the daemon's metrics listener
    /// serves and prints a one-screen summary, or the raw JSON
    /// with --json. Exits 0 when every enabled uplink target is connected
    /// (or none is configured), 1 when any is down or flapping, and 2 when
    /// the daemon cannot be reached or its answer cannot be read.
    Status {
        /// Daemon config to read the metrics `bind_addr`/`metrics_port`
        /// from, with the same `MANTA_*` overlay `run` applies (falls back
        /// to `MANTA_CONFIG`). A wildcard `bind_addr` (`0.0.0.0`/`::`)
        /// dials loopback.
        // `--server-config` stays a hidden alias, as on `run`: the
        // deprecation notice names `--config`, so `status` must accept it.
        #[arg(long, alias = "server-config", conflicts_with = "addr")]
        config: Option<PathBuf>,
        /// Explicit `host:port` of the daemon's metrics listener (default
        /// 127.0.0.1:7302, the documented default metrics port).
        #[arg(long)]
        addr: Option<String>,
        /// Print the raw status JSON instead of the human-readable summary.
        #[arg(long)]
        json: bool,
        /// Give up after this many seconds if the daemon doesn't answer.
        #[arg(long, default_value_t = 5)]
        timeout_secs: u64,
    },
    /// Ask a running daemon whether it is healthy.
    ///
    /// Prints the daemon's health report, one check per line. Exits 0 when
    /// the daemon reports healthy, and 1 when it reports unhealthy, cannot
    /// be reached, or does not answer within --timeout-secs. These are the
    /// exit codes a container health check expects.
    // MAN-81: the image's HEALTHCHECK. Never exits 2 -- Docker reserves it.
    Health {
        /// Daemon config to read metrics_bind_addr/metrics_port from, with the
        /// same MANTA_* overrides `run` applies (falls back to MANTA_CONFIG). A
        /// wildcard address (0.0.0.0/::) is dialled on loopback.
        #[arg(long, conflicts_with = "addr")]
        config: Option<PathBuf>,
        /// Explicit host:port of the daemon's metrics listener (default
        /// 127.0.0.1:7302).
        #[arg(long)]
        addr: Option<String>,
        /// Give up after this many seconds if the daemon doesn't answer.
        #[arg(long, default_value_t = 3)]
        timeout_secs: u64,
    },
    /// Check that this machine is ready to run manta, then whether the
    /// receiver hears anything.
    ///
    /// First checks the config, the audio library, that the server ports
    /// are free, the clock against an NTP server, and that each enabled RBN
    /// uplink target accepts a connection. Then opens the receiver, runs the
    /// real decode pipeline for --duration seconds and reports track, SNR
    /// and spot counts plus a verdict. Each check prints PASS, WARN, FAIL or
    /// SKIP, with a fix for every problem. Exits 1 when a check fails.
    ///
    /// Exit codes: 0 when no check fails, whatever the verdict; 1 when a
    /// check fails or doctor cannot start; 2 for a command-line usage error.
    /// With --strict, a run whose checks pass exits 3 unless the verdict is
    /// DECODING.
    // MAN-126: the setup checks live in doctor_checks.rs and clock_check.rs.
    // MAN-235: `strict_exit_code` maps a verdict to its --strict code.
    // MAN-306: --hpsdr-protocol needs an HPSDR selector: a host, a MAC, or
    // the flags of a hostless (discovered) radio.
    #[command(group(clap::ArgGroup::new("hpsdr_selector").args(["hpsdr_host", "hpsdr_mac", "hpsdr_port", "hpsdr_freq_hz", "hpsdr_rate_hz"]).multiple(true)))]
    Doctor {
        /// With several [[input]] entries in the config, the one to examine,
        /// by its name. Optional with one input.
        #[arg(long, value_name = "NAME")]
        input: Option<String>,
        /// How long to listen for, in seconds (3 to 3600).
        #[arg(long, default_value_t = 10)]
        duration: u64,
        /// Sound-card input device; matched by substring. Defaults to the
        /// system default input.
        #[arg(long, conflicts_with = "source", help_heading = "Audio input")]
        device: Option<String>,
        /// Replay a WAV file instead of listening to a live device.
        ///
        /// Decoded as fast as the machine manages, not in real time, so a
        /// 60-second file usually finishes sooner than that. Useful for
        /// demos and repeatable testing.
        #[arg(long, conflicts_with = "device", help_heading = "Audio input")]
        source: Option<PathBuf>,
        /// KiwiSDR receiver hostname. Requires --kiwi-freq-hz.
        #[arg(help_heading = "KiwiSDR source")]
        #[arg(long, conflicts_with_all = crate::soapy_opts::and_driver(["device", "source", "ka9q_group", "hpsdr_host", "hpsdr_mac", "hpsdr_freq_hz", "hpsdr_rate_hz", "hpsdr_port"]), requires = "kiwi_freq_hz")]
        kiwi_host: Option<String>,
        /// KiwiSDR receiver port.
        #[arg(
            long,
            default_value = "8073",
            requires = "kiwi_host",
            help_heading = "KiwiSDR source"
        )]
        kiwi_port: u16,
        /// Receiver centre frequency, in Hz. Required with --kiwi-host.
        #[arg(
            long = "kiwi-freq-hz",
            alias = "kiwi-freq",
            requires = "kiwi_host",
            help_heading = "KiwiSDR source"
        )]
        kiwi_freq_hz: Option<f64>,
        /// KiwiSDR password. Leave empty for public receivers that do not
        /// ask for one.
        #[arg(
            long,
            requires = "kiwi_host",
            default_value = "",
            help_heading = "KiwiSDR source"
        )]
        kiwi_password: String,
        #[command(flatten)]
        soapy: crate::soapy_opts::SoapyArgs,
        /// openHPSDR radio hostname or IP address (protocol 1: Hermes-Lite 2, Hermes; protocol 2: ANAN).
        ///
        /// Requires --hpsdr-freq-hz and --hpsdr-rate-hz. Included in every build.
        #[arg(help_heading = "HPSDR source")]
        #[arg(long, conflicts_with_all = crate::soapy_opts::and_driver(["device", "source", "ka9q_group", "kiwi_host"]))]
        #[arg(conflicts_with_all = ["device", "source"])]
        #[arg(conflicts_with = "hpsdr_mac")]
        hpsdr_host: Option<String>,
        /// Discover and select a radio by MAC address; omit host to discover.
        #[arg(long, value_parser = parse_hpsdr_mac, help_heading = "HPSDR source", conflicts_with_all = ["hpsdr_host", "device", "source"])]
        hpsdr_mac: Option<[u8; 6]>,
        /// openHPSDR control port.
        ///
        /// Protocol 2 also sends to the three ports above it and receives IQ from this port + 11 (1035 by default).
        #[arg(long, help_heading = "HPSDR source")]
        #[arg(conflicts_with_all = ["device", "source"])]
        #[arg(value_parser = clap::value_parser!(u16).range(1..))]
        hpsdr_port: Option<u16>,
        /// openHPSDR protocol the radio speaks.
        ///
        /// auto asks the radio and uses protocol 2 when it answers as protocol 2, protocol 1 otherwise; 1 and 2 skip the question.
        #[arg(
            long,
            value_enum,
            default_value_t = HpsdrProtocolArg::Auto,
            requires = "hpsdr_selector",
            help_heading = "HPSDR source"
        )]
        hpsdr_protocol: HpsdrProtocolArg,
        /// Receiver centre frequency, in Hz. Required with the hpsdr source.
        #[arg(
            long = "hpsdr-freq-hz",
            alias = "hpsdr-freq",
            value_parser = parse_hpsdr_freq_hz,
            help_heading = "HPSDR source"
        )]
        #[arg(conflicts_with_all = ["device", "source"])]
        hpsdr_freq_hz: Option<f64>,
        /// Sample rate, in Hz. Required with the hpsdr source.
        #[arg(
            long = "hpsdr-rate-hz",
            alias = "hpsdr-rate",
            value_parser = parse_hpsdr_rate_hz,
            help_heading = "HPSDR source"
        )]
        #[arg(conflicts_with_all = ["device", "source"])]
        hpsdr_rate_hz: Option<f64>,
        /// Multicast data group of a ka9q-radio (radiod) IQ channel
        ///
        /// An IPv4 multicast address, or the name in the channel's `data =`
        /// line (for example cw-20m.local) when this machine resolves mDNS
        /// names. manta joins the group and reads the channel's sample rate,
        /// encoding and frequency from radiod's status packets; it does not
        /// create or retune channels.
        #[arg(help_heading = "ka9q-radio source", value_parser = parse_ka9q_group)]
        #[arg(long, conflicts_with_all = crate::soapy_opts::and_driver(["device", "source", "kiwi_host", "hpsdr_host", "hpsdr_mac", "hpsdr_freq_hz", "hpsdr_rate_hz", "hpsdr_port"]))]
        ka9q_group: Option<String>,
        /// RTP data port of the group.
        #[arg(
            long,
            default_value_t = manta_input::ka9q::wire::DATA_PORT,
            value_parser = clap::value_parser!(u16).range(1..),
            requires = "ka9q_group",
            help_heading = "ka9q-radio source"
        )]
        ka9q_port: u16,
        /// SSRC of the channel to read, when the group carries more than one.
        #[arg(long, requires = "ka9q_group", help_heading = "ka9q-radio source")]
        ka9q_ssrc: Option<u32>,
        /// IPv4 address of the local interface to join the group on. manta
        /// also joins on 127.0.0.1, for a radiod on this machine.
        #[arg(
            long,
            requires = "ka9q_group",
            value_parser = clap::value_parser!(std::net::Ipv4Addr),
            help_heading = "ka9q-radio source"
        )]
        ka9q_interface: Option<std::net::Ipv4Addr>,
        /// Decimate the source to this sample rate, in Hz, before decoding.
        ///
        /// Must divide the source's own rate by a power of two, and the
        /// result must itself be a rate the channelizer supports. Omit to
        /// decode at the source's native rate.
        #[arg(long, value_parser = parse_capture_rate_hz)]
        capture_rate_hz: Option<f64>,
        /// Read --source as a raw complex-IQ recording rather than ordinary
        /// mono audio.
        ///
        /// Without this, --source is taken as mono real audio at 48000 Hz.
        /// A stereo audio recording and a two-channel IQ capture cannot be
        /// told apart from the WAV header alone, so say which you have.
        #[arg(long)]
        source_iq: bool,
        /// Radio dial frequency, in Hz. See `run --dial-freq-hz`; without it
        /// an audio source's reported frequencies, and the report's center
        /// frequency, are baseband offsets.
        #[arg(long, value_parser = parse_dial_freq_hz, help_heading = "Audio input")]
        dial_freq_hz: Option<f64>,
        /// TOML config file whose `[input]`, `[spot]`, `[detector]` and
        /// `[decode]` tables supply what the flags leave unset.
        ///
        /// Falls back to $MANTA_CONFIG. Flags override the file, and
        /// MANTA_<TABLE>_<KEY> environment variables sit between the two.
        /// doctor checks [server]'s ports and each [[rbn_uplink]] target,
        /// but never starts a server or logs in to a collector.
        #[arg(long)]
        config: Option<PathBuf>,
        /// NTP server the clock check asks for the time, as HOST or
        /// HOST:PORT.
        ///
        /// doctor sends it one time request over UDP port 123. Name a server
        /// on your own network when outbound NTP is blocked.
        #[arg(
            long,
            default_value = clock_check::DEFAULT_NTP_SERVER,
            value_parser = clock_check::parse_ntp_server,
            help_heading = "Setup checks"
        )]
        ntp_server: clock_check::NtpServer,
        /// Print the full report as one JSON object instead of a readable
        /// summary.
        #[arg(long, help_heading = "Output")]
        json: bool,
        /// Exit 3 unless the verdict is DECODING, so a script can act on it.
        ///
        /// Without --strict every verdict exits 0: a quiet band is not a
        /// broken setup. With --strict, DECODING exits 0, and NO_SIGNAL,
        /// NOT_CW, ACTIVITY_NO_SNR, NOISY_NO_DECODE and WEAK_NO_DECODE exit
        /// 3. A failed check still exits 1, whatever the verdict.
        // MAN-235: the codes come from `strict_exit_code`.
        #[arg(long, help_heading = "Output")]
        strict: bool,
        #[command(flatten)]
        filters: FilterOpts,
    },
    /// Measure the receiver's frequency error against a known signal.
    ///
    /// Listens for --duration seconds to a carrier whose frequency is known
    /// exactly -- a time-signal station (WWV, WWVH, BPM, RWM) or an NCDXF
    /// beacon -- and reports the freq_correction_ppm that corrects the
    /// receiver. It picks a reference inside the receiver's passband by
    /// itself; --tune-hz points the configured receiver at one, and
    /// --reference-hz names any other carrier you know. Offers to save the
    /// value to the config file.
    // MAN-127: see calibrate_cmd.rs and
    // docs/DECISIONS/2026-10-11-man127-calibrate-command.md. The source flag
    // block is copied from `Doctor`, as run/soak/doctor each copy it, except
    // the `--soapy-*` flags, which are one flattened `soapy_opts::SoapyArgs`
    // (MAN-550).
    // MAN-306: --hpsdr-protocol needs an HPSDR selector: a host, a MAC, or
    // the flags of a hostless (discovered) radio.
    #[command(group(clap::ArgGroup::new("hpsdr_selector").args(["hpsdr_host", "hpsdr_mac", "hpsdr_port", "hpsdr_freq_hz", "hpsdr_rate_hz"]).multiple(true)))]
    Calibrate {
        /// With several [[input]] entries in the config, the one to examine,
        /// by its name. Optional with one input.
        #[arg(long, value_name = "NAME")]
        input: Option<String>,
        /// How long to measure, in seconds (10 to 3600). NCDXF beacons take
        /// turns on each frequency, so give them 180.
        #[arg(long, default_value_t = 60)]
        duration: u64,
        /// Measure against this carrier, in Hz, instead of the built-in list.
        /// Use only a signal whose frequency you know to a fraction of a
        /// hertz.
        #[arg(long, value_parser = parse_reference_hz)]
        reference_hz: Option<f64>,
        /// How far from the reference to look, in parts per million (1 to
        /// 1000). Widen it for a receiver without a temperature-compensated
        /// oscillator.
        #[arg(long, default_value_t = manta_engine::calibrate::DEFAULT_SEARCH_PPM, value_parser = parse_search_ppm)]
        search_ppm: f64,
        /// Retune the configured receiver to this centre frequency, in Hz,
        /// for this measurement only. KiwiSDR, SoapySDR and HPSDR sources.
        /// Keep the reference 1 to 2 kHz from the centre, e.g. 9998500 for
        /// the 10 MHz time signal.
        #[arg(long, value_parser = parse_tune_hz)]
        #[arg(conflicts_with_all = crate::soapy_opts::and_driver(["kiwi_host", "hpsdr_host", "hpsdr_mac", "hpsdr_freq_hz", "hpsdr_rate_hz", "hpsdr_port"]))]
        tune_hz: Option<f64>,
        /// Sound-card input device; matched by substring. Defaults to the
        /// system default input.
        #[arg(long, conflicts_with = "source", help_heading = "Audio input")]
        device: Option<String>,
        /// Replay a WAV file instead of listening to a live device.
        ///
        /// Decoded as fast as the machine manages, not in real time, so a
        /// 60-second file usually finishes sooner than that. Useful for
        /// demos and repeatable testing.
        #[arg(long, conflicts_with = "device", help_heading = "Audio input")]
        source: Option<PathBuf>,
        /// KiwiSDR receiver hostname. Requires --kiwi-freq-hz.
        #[arg(help_heading = "KiwiSDR source")]
        #[arg(long, conflicts_with_all = crate::soapy_opts::and_driver(["device", "source", "ka9q_group", "hpsdr_host", "hpsdr_mac", "hpsdr_freq_hz", "hpsdr_rate_hz", "hpsdr_port"]), requires = "kiwi_freq_hz")]
        kiwi_host: Option<String>,
        /// KiwiSDR receiver port.
        #[arg(
            long,
            default_value = "8073",
            requires = "kiwi_host",
            help_heading = "KiwiSDR source"
        )]
        kiwi_port: u16,
        /// Receiver centre frequency, in Hz. Required with --kiwi-host.
        #[arg(
            long = "kiwi-freq-hz",
            alias = "kiwi-freq",
            requires = "kiwi_host",
            help_heading = "KiwiSDR source"
        )]
        kiwi_freq_hz: Option<f64>,
        /// KiwiSDR password. Leave empty for public receivers that do not
        /// ask for one.
        #[arg(
            long,
            requires = "kiwi_host",
            default_value = "",
            help_heading = "KiwiSDR source"
        )]
        kiwi_password: String,
        #[command(flatten)]
        soapy: crate::soapy_opts::SoapyArgs,
        /// openHPSDR radio hostname or IP address (protocol 1: Hermes-Lite 2, Hermes; protocol 2: ANAN).
        ///
        /// Requires --hpsdr-freq-hz and --hpsdr-rate-hz. Included in every build.
        #[arg(help_heading = "HPSDR source")]
        #[arg(long, conflicts_with_all = crate::soapy_opts::and_driver(["device", "source", "ka9q_group", "kiwi_host"]))]
        #[arg(conflicts_with_all = ["device", "source"])]
        #[arg(conflicts_with = "hpsdr_mac")]
        hpsdr_host: Option<String>,
        /// Discover and select a radio by MAC address; omit host to discover.
        #[arg(long, value_parser = parse_hpsdr_mac, help_heading = "HPSDR source", conflicts_with_all = ["hpsdr_host", "device", "source"])]
        hpsdr_mac: Option<[u8; 6]>,
        /// openHPSDR control port.
        ///
        /// Protocol 2 also sends to the three ports above it and receives IQ from this port + 11 (1035 by default).
        #[arg(long, help_heading = "HPSDR source")]
        #[arg(conflicts_with_all = ["device", "source"])]
        #[arg(value_parser = clap::value_parser!(u16).range(1..))]
        hpsdr_port: Option<u16>,
        /// openHPSDR protocol the radio speaks.
        ///
        /// auto asks the radio and uses protocol 2 when it answers as protocol 2, protocol 1 otherwise; 1 and 2 skip the question.
        #[arg(
            long,
            value_enum,
            default_value_t = HpsdrProtocolArg::Auto,
            requires = "hpsdr_selector",
            help_heading = "HPSDR source"
        )]
        hpsdr_protocol: HpsdrProtocolArg,
        /// Receiver centre frequency, in Hz. Required with the hpsdr source.
        #[arg(
            long = "hpsdr-freq-hz",
            alias = "hpsdr-freq",
            value_parser = parse_hpsdr_freq_hz,
            help_heading = "HPSDR source"
        )]
        #[arg(conflicts_with_all = ["device", "source"])]
        hpsdr_freq_hz: Option<f64>,
        /// Sample rate, in Hz. Required with the hpsdr source.
        #[arg(
            long = "hpsdr-rate-hz",
            alias = "hpsdr-rate",
            value_parser = parse_hpsdr_rate_hz,
            help_heading = "HPSDR source"
        )]
        #[arg(conflicts_with_all = ["device", "source"])]
        hpsdr_rate_hz: Option<f64>,
        /// Multicast data group of a ka9q-radio (radiod) IQ channel
        ///
        /// An IPv4 multicast address, or the name in the channel's `data =`
        /// line (for example cw-20m.local) when this machine resolves mDNS
        /// names. manta joins the group and reads the channel's sample rate,
        /// encoding and frequency from radiod's status packets; it does not
        /// create or retune channels.
        #[arg(help_heading = "ka9q-radio source", value_parser = parse_ka9q_group)]
        #[arg(long, conflicts_with_all = crate::soapy_opts::and_driver(["device", "source", "kiwi_host", "hpsdr_host", "hpsdr_mac", "hpsdr_freq_hz", "hpsdr_rate_hz", "hpsdr_port"]))]
        ka9q_group: Option<String>,
        /// RTP data port of the group.
        #[arg(
            long,
            default_value_t = manta_input::ka9q::wire::DATA_PORT,
            value_parser = clap::value_parser!(u16).range(1..),
            requires = "ka9q_group",
            help_heading = "ka9q-radio source"
        )]
        ka9q_port: u16,
        /// SSRC of the channel to read, when the group carries more than one.
        #[arg(long, requires = "ka9q_group", help_heading = "ka9q-radio source")]
        ka9q_ssrc: Option<u32>,
        /// IPv4 address of the local interface to join the group on. manta
        /// also joins on 127.0.0.1, for a radiod on this machine.
        #[arg(
            long,
            requires = "ka9q_group",
            value_parser = clap::value_parser!(std::net::Ipv4Addr),
            help_heading = "ka9q-radio source"
        )]
        ka9q_interface: Option<std::net::Ipv4Addr>,
        /// Decimate the source to this sample rate, in Hz, before decoding.
        ///
        /// Must divide the source's own rate by a power of two, and the
        /// result must itself be a rate the channelizer supports. Omit to
        /// decode at the source's native rate.
        #[arg(long, value_parser = parse_capture_rate_hz)]
        capture_rate_hz: Option<f64>,
        /// Read --source as a raw complex-IQ recording rather than ordinary
        /// mono audio.
        ///
        /// Without this, --source is taken as mono real audio at 48000 Hz.
        /// A stereo audio recording and a two-channel IQ capture cannot be
        /// told apart from the WAV header alone, so say which you have.
        #[arg(long)]
        source_iq: bool,
        /// Radio dial frequency, in Hz. See `run --dial-freq-hz`; without it
        /// an audio source's reported frequencies, and the report's
        /// `center_freq_hz`, are baseband offsets.
        #[arg(long, value_parser = parse_dial_freq_hz, help_heading = "Audio input")]
        dial_freq_hz: Option<f64>,
        /// TOML config file whose [input] table names the receiver. The
        /// measured value is saved to that table.
        ///
        /// Falls back to $MANTA_CONFIG. Flags override the file, and
        /// MANTA_INPUT_<KEY> environment variables sit between the two.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Save the measured value to the config file without asking.
        #[arg(long)]
        write: bool,
        /// Print the report as one JSON object. Never asks before saving.
        #[arg(long, help_heading = "Output")]
        json: bool,
    },
    /// Check or create a config file, without starting anything.
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Measure decode quality on synthetic signals, no radio needed.
    #[command(subcommand)]
    Bench(BenchCommand),
    /// Grade a node's spot log against RBN reports, offline.
    ///
    /// Reads SpotMessage JSON Lines (the `field-node.py record-spots`
    /// archive) and RBN daily-dump CSVs, and grades every spot the way
    /// `[rbn_compare]` grades it live: match, disagree, elsewhere, none or
    /// unknown. Prints a per-band summary, or the grade records with
    /// --json. --reference grades against a skimmer's saved `DX de` lines
    /// (a co-located CW Skimmer Server) instead of RBN CSVs, and --paired
    /// grades both ways: recall, the reports manta missed and its candidate
    /// false spots. Never reads MANTA_* variables: the output depends only
    /// on the inputs and flags.
    Grade {
        #[command(flatten)]
        args: grade_cmd::Args,
    },
    /// Print a tab-completion script for your shell.
    ///
    /// Load it from your shell's startup file, for example by adding
    /// `eval "$(manta completions bash)"` to ~/.bashrc. The script covers
    /// this build's commands and flags, so load it again after an upgrade.
    Completions {
        /// Shell to print the script for.
        shell: clap_complete::aot::Shell,
    },
    /// Write manta's man pages into a directory.
    ///
    /// Writes manta.1 and one page per command (manta-run.1,
    /// manta-config-check.1, ...), replacing older copies. Release archives
    /// already carry these pages in man/man1.
    ManPages {
        /// Directory to write the pages into, created if missing, for
        /// example /usr/local/share/man/man1.
        #[arg(long, value_name = "DIR")]
        out: PathBuf,
    },
}

// MAN-116: `manta bench sensitivity`; see bench.rs and
// docs/DECISIONS/2026-10-10-man116-sensitivity-benchmark.md.
#[derive(Subcommand)]
enum BenchCommand {
    /// Measure how recall and copy accuracy fall off as SNR drops.
    ///
    /// Generates synthetic CW recordings for every combination of channel
    /// condition, speed and SNR, decodes each one with the same pipeline
    /// `manta decode` uses, and prints a recall and character-error-rate
    /// table. Each recording holds ten stations sending "CQ CQ DE <call>
    /// <call> K" for its whole length. The same manta version and flags
    /// always print the same table, so a published result can be
    /// regenerated and checked.
    ///
    /// SNR is quoted the way RBN and CW Skimmer quote it: the transmitted
    /// carrier against the noise in a 500 Hz bandwidth.
    Sensitivity(bench::SensitivityArgs),
}

// MAN-76: `manta config check` / `manta config init`; see config_cmd.rs and
// docs/DECISIONS/2026-10-07-man76-config-check-init.md.
#[derive(Subcommand)]
enum ConfigCommand {
    /// Validate a config file and print the settings it resolves to.
    ///
    /// Runs every check `manta run` applies to its config, including the
    /// MANTA_* environment variables, without opening the receiver,
    /// binding a port or starting a server. Exits 0 when the config is
    /// valid and 1, naming the setting and the problem, when it is not.
    Check {
        /// Config file to check. Defaults to $MANTA_CONFIG, then to
        /// manta.toml in the current directory.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Write a new config file listing every setting at its default.
    ///
    /// Every setting is commented out, so the new file changes nothing
    /// until you edit it. Refuses to replace an existing file without
    /// --force.
    Init {
        /// Where to write the file; `-` prints it instead.
        #[arg(long, default_value = config_cmd::DEFAULT_PATH)]
        out: PathBuf,
        /// Replace the file if it already exists.
        #[arg(long)]
        force: bool,
    },
}

// Provenance for the flags below (contributor-facing, deliberately `//` and
// not `///`: clap republishes `///` verbatim into operator-facing `--help`,
// and MAN-135 is specifically about not doing that):
//   - freq_correction_ppm: config key `input.freq_correction_ppm`,
//     SPEC-decode-core.md §1.4. Legacy precedent is CW Skimmer/SkimSrv's
//     `FreqCalibration=` .ini key, which is a raw multiplier; this flag is
//     ppm per the spec's contract.
//   - allowlist: the Operator Watch List of ARCHITECTURE §6 / MAN-28.
//     Legacy precedent: CW Skimmer's Watch List (Aggregator manual App. A2).
//   - blocklist / notch: MAN-31. cty / scp: MAN-79.
/// MAN-301: `run`'s live RBN comparison flags, merged over
/// `[rbn_compare]` in `resolve`.
#[derive(clap::Args, Clone, Debug, Default)]
#[command(next_help_heading = "RBN comparison")]
struct RbnCompareOpts {
    // `help`, not a doc comment: clap drops a one-paragraph doc comment's
    // final period, and this text is the design's (MAN-301 §2).
    #[arg(
        long,
        help = "Grade each spot against RBN's public telnet feed and publish the grades on the \
                JSON stream and /metrics. Needs --config with a [server] table. Same as \
                [rbn_compare].enabled = true."
    )]
    rbn_compare: bool,
    #[arg(
        long = "rbn-spotter",
        value_name = "CALL",
        value_parser = parse_rbn_spotter,
        help = "Count only this RBN skimmer's reports when grading, for example a co-located \
                skimmer on the same antenna. Repeatable. Replaces [rbn_compare].spotters."
    )]
    rbn_spotter: Vec<String>,
}

/// Flags that decide which decodes become spots. Shared by `decode`,
/// `listen` and `soak`.
#[derive(clap::Args, Clone, Debug)]
#[command(next_help_heading = "Filtering")]
struct FilterOpts {
    /// Correct a receiver whose clock reads off-frequency, in parts per
    /// million.
    ///
    /// Decoded and spotted frequencies are scaled by 1 + ppm/1000000, so
    /// a receiver reading about 20 Hz high on 14 MHz is corrected with
    /// roughly -1.4. 0 disables the correction. `doctor` still prints the
    /// source's own centre frequency uncorrected; the correction only
    /// affects the frequencies it validates. This is the same idea as CW
    /// Skimmer's FreqCalibration setting, expressed in ppm rather than as
    /// a raw multiplier.
    ///
    /// Defaults to 0, or to `input.freq_correction_ppm` from --config.
    #[arg(
        long,
        value_parser = parse_freq_correction_ppm,
        allow_negative_numbers = true
    )]
    freq_correction_ppm: Option<f64>,
    /// Spot this callsign even when the usual validity checks reject it.
    ///
    /// Repeat the flag for more than one call. Use it for a station you
    /// know is on the air but that manta keeps rejecting -- an unusual
    /// prefix, a special-event call. Bypasses callsign-grammar and
    /// country checks and the repeat-before-spotting rule. --blocklist and
    /// --notch still win: a call named by both is not spotted.
    #[arg(long)]
    allowlist: Vec<String>,
    /// File of callsigns never to spot, one per line.
    #[arg(long)]
    blocklist: Option<PathBuf>,
    /// File of frequency ranges never to spot, one `low_hz-high_hz` range
    /// per line.
    #[arg(long)]
    notch: Option<PathBuf>,
    /// Country-prefix file (AD1C's cty.dat) to check callsigns against, in place of the copy built into manta.
    ///
    /// A call whose prefix this file does not list is not spotted unless --allowlist names it,
    /// so a new prefix or DXpedition is rejected until the file lists it. Download the current
    /// file from https://www.country-files.com/cty/cty.dat to accept such calls without waiting
    /// for a new manta release. Defaults to the built-in copy, or to `spot.cty_path` from --config.
    #[arg(long)]
    cty: Option<PathBuf>,
    /// Known-callsign list (MASTER.SCP) to use in place of the copy built into manta.
    ///
    /// A call on the list is spotted with more confidence; a call missing from it can still
    /// be spotted. Download the current list from https://www.supercheckpartial.com/MASTER.SCP.
    /// Defaults to the built-in copy, or to `spot.scp_path` from --config.
    #[arg(long)]
    scp: Option<PathBuf>,
    /// Use exactly the cty.dat whose SHA-256 is HEX (64 hex digits, as in manta's `lists:` log
    /// line). With --cty (or spot.cty_path) set, that file must have this SHA-256 and nothing
    /// else is tried; without one, manta looks in the built-in copy, then spot.list_cache_dir.
    /// manta stops before reading the source if no copy matches. Defaults to `spot.cty_sha256`
    /// from --config.
    #[arg(long, value_name = "HEX", value_parser = config::parse_sha256)]
    cty_sha256: Option<String>,
    /// Use exactly the MASTER.SCP whose SHA-256 is HEX. Looked up like --cty-sha256. Defaults to
    /// `spot.scp_sha256` from --config.
    #[arg(long, value_name = "HEX", value_parser = config::parse_sha256)]
    scp_sha256: Option<String>,
    /// Callsign-pattern list used in place of the built-in one ([spot] pattern_path).
    #[arg(long, value_name = "PATH")]
    patterns: Option<PathBuf>,
}

/// Strips a leading UTF-8 BOM (`\u{feff}`), common in Windows-authored text
/// files -- `str::trim` does not remove it, so left unstripped it corrupts
/// the first line's parse (a blocklist callsign that never matches, or a
/// notch range silently rejected).
fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

/// Loads a reference table once, with consistent path context and BOM handling.
fn read_reference<T>(
    path: &Path,
    label: &str,
    parse: impl FnOnce(&str) -> Result<T>,
) -> Result<std::sync::Arc<T>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {label} {}", path.display()))?;
    parse(strip_bom(&text)).map(std::sync::Arc::new)
}

/// Builds a `PipelineConfig` from the CLI's shared flags: the MAN-29
/// frequency-calibration correction, the MAN-28 operator Watch List, and
/// the MAN-31 operator suppression lists. Each is optional/repeatable; an
/// absent one leaves that list empty, matching `PipelineConfig`'s own
/// defaults. MAN-316: also resolves the cty.dat/MASTER.SCP hash pins, before
/// any source opens, and returns the version of each list it chose.
fn build_pipeline_config(
    freq_correction_ppm: f64,
    spot: &SpotResolved,
    detector: manta_engine::DetectorConfig,
    engine: Engine,
) -> Result<(PipelineConfig, [list_refresh::InUse; 2])> {
    let mut cfg = PipelineConfig {
        freq_correction_ppm,
        allowlist: spot.allowlist.clone(),
        validation_level: spot.validation_level,
        repetition_fusion: spot.repetition_fusion,
        detector,
        ..Default::default()
    };
    cfg.decode.engine = engine;
    if let Some(path) = &spot.blocklist {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading blocklist file {}", path.display()))?;
        cfg.blocklist = manta_engine::Blocklist::parse(strip_bom(&text));
    }
    if let Some(path) = &spot.notch {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading notch file {}", path.display()))?;
        cfg.notch = manta_engine::NotchList::parse(strip_bom(&text));
    }
    let cache = spot.list_cache_dir.clone().map(list_cache::Cache::new);
    let cty_in_use = match resolve_list_bytes(
        ListKind::Cty,
        spot.cty.as_deref(),
        spot.cty_sha256.as_deref(),
        cache.as_ref(),
    )? {
        None => list_refresh::InUse::bundled(ListKind::Cty),
        Some((text, in_use)) => {
            let table = manta_spot::cty::Table::parse(strip_bom(&text));
            // MAN-79's rule for an explicit file; a pin is the operator's
            // exact choice of bytes, so it skips every check (D5).
            if let (list_refresh::Origin::File(path), true) = (&in_use.origin, table.is_empty()) {
                bail!(
                    "cty.dat file {} lists no callsign prefixes; is it AD1C's cty.dat (not cty.csv or a saved web page)?",
                    path.display()
                );
            }
            cfg.cty = Some(std::sync::Arc::new(table));
            in_use
        }
    };
    let scp_in_use = match resolve_list_bytes(
        ListKind::Scp,
        spot.scp.as_deref(),
        spot.scp_sha256.as_deref(),
        cache.as_ref(),
    )? {
        None => list_refresh::InUse::bundled(ListKind::Scp),
        Some((text, in_use)) => {
            let set = manta_spot::scp::Set::parse(strip_bom(&text));
            if let (list_refresh::Origin::File(path), true) = (&in_use.origin, set.is_empty()) {
                bail!("master.scp file {} lists no callsigns", path.display());
            }
            cfg.scp = Some(std::sync::Arc::new(set));
            in_use
        }
    };
    if let Some(path) = &spot.patterns {
        cfg.patterns = Some(read_reference(path, "the pattern list", |text| {
            manta_spot::pattern::Table::parse(text).map_err(|error| match error {
                manta_spot::pattern::ParseError::Empty => {
                    anyhow!("the pattern list {} contains no patterns", path.display())
                }
                _ => anyhow!("reading the pattern list {}: {error}", path.display()),
            })
        })?);
    }
    Ok((cfg, [cty_in_use, scp_in_use]))
}

/// Shares live country/SCP cells with the running validator. No list generates text.
fn attach_language_prior(
    cfg: &mut PipelineConfig,
    spot: &SpotResolved,
    lists: Option<std::sync::Arc<manta_engine::ReferenceLists>>,
) -> Result<()> {
    if !cfg.decode.prior_enabled {
        return Ok(());
    }
    let heard = match &spot.calls_heard_path {
        Some(path) => manta_spot::prior::CallsHeard::load(path)
            .with_context(|| format!("reading calls-heard file {}", path.display()))?,
        None => manta_spot::prior::CallsHeard::default(),
    };
    let lists = lists.unwrap_or_else(|| {
        std::sync::Arc::new(manta_engine::ReferenceLists::new(
            cfg.cty_table(),
            cfg.scp_set(),
        ))
    });
    cfg.decode.prior = Some(std::sync::Arc::new(manta_spot::prior::LanguagePrior::new(
        lists,
        heard,
        cfg.decode.prior_weight,
    )));
    Ok(())
}

/// MAN-316 D3 steps 1 and 2 for one list: the text an explicit path or a
/// hash pin chooses, or `None` for the built-in copy (no path and no pin).
/// A pin matching the built-in copy returns the built-in text, reported as
/// pinned. A set path is the only place a pin is checked: a
/// path whose SHA-256 differs from the pin is an error. Without a path, a
/// pin is looked up in the built-in copy, then `list_cache_dir`; a miss is
/// an error naming where it looked.
fn resolve_list_bytes(
    kind: ListKind,
    path: Option<&std::path::Path>,
    pin: Option<&str>,
    cache: Option<&list_cache::Cache>,
) -> Result<Option<(String, list_refresh::InUse)>> {
    let (file_name, flag, path_flag, label) = match kind {
        ListKind::Cty => ("cty.dat", "--cty-sha256", "--cty", "cty.dat"),
        ListKind::Scp => ("MASTER.SCP", "--scp-sha256", "--scp", "master.scp"),
    };
    let utf8 = |bytes: Vec<u8>, what: &std::path::Path| -> Result<String> {
        String::from_utf8(bytes).map_err(|_| {
            anyhow!(
                "reading {label} file {}: stream did not contain valid UTF-8",
                what.display()
            )
        })
    };
    if let Some(path) = path {
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading {label} file {}", path.display()))?;
        let sha = list_cache::sha256_hex(&bytes);
        let origin = match pin {
            Some(pin) if pin != sha => bail!(
                "{file_name} file {} has SHA-256 {sha}, not the pinned {pin} ({flag})",
                path.display()
            ),
            Some(_) => list_refresh::Origin::Pinned(list_refresh::PinSource::File(path.into())),
            None => list_refresh::Origin::File(path.into()),
        };
        let text = utf8(bytes, path)?;
        return Ok(Some((text, list_refresh::InUse::new(kind, sha, origin))));
    }
    let Some(pin) = pin else {
        return Ok(None);
    };
    let bundled = list_refresh::bundled_sha256(kind);
    if pin == bundled {
        let in_use = list_refresh::InUse::new(
            kind,
            bundled,
            list_refresh::Origin::Pinned(list_refresh::PinSource::Bundled),
        );
        let text = match kind {
            ListKind::Cty => manta_spot::CTY_DAT,
            ListKind::Scp => manta_spot::MASTER_SCP,
        };
        return Ok(Some((text.to_string(), in_use)));
    }
    let Some(cache) = cache else {
        bail!(
            "{flag} {pin}: no {file_name} has that SHA-256: the built-in copy is {bundled} and \
             spot.list_cache_dir is not set; pass the file itself with {path_flag}"
        );
    };
    let version_path = cache.version_path(kind, pin);
    match cache
        .load_version(kind, pin)
        .map_err(|e| anyhow!("{flag} {pin}: {e}"))?
    {
        None => bail!(
            "{flag} {pin}: no {file_name} has that SHA-256: the built-in copy is {bundled} and \
             {} does not exist; pass the file itself with {path_flag}",
            version_path.display()
        ),
        Some(bytes) => {
            let text = utf8(bytes, &version_path)?;
            let in_use = list_refresh::InUse::new(
                kind,
                pin.to_string(),
                list_refresh::Origin::Pinned(list_refresh::PinSource::Cache(version_path)),
            );
            Ok(Some((text, in_use)))
        }
    }
}

/// MAN-316 Design §5: every validating command names the lists it uses on
/// stderr when a pin is set (`run` with refresh on prints its own line).
fn note_pinned_lists(spot: &SpotResolved, in_use: &[list_refresh::InUse; 2]) {
    if spot.cty_sha256.is_some() || spot.scp_sha256.is_some() {
        logging::note(&list_refresh::lists_line(&in_use[0], &in_use[1]));
    }
}

/// Range-checks a `[decode]` table (SPEC v2 §7) loaded by `config::load`
/// from `origin` (the config file's path, or "environment"). Every message
/// names `origin`; `config::load` adds no context layer on top.
fn validate_decode_config(
    cfg: manta_decode::decoder::DecodeConfig,
    origin: &str,
) -> Result<manta_decode::decoder::DecodeConfig> {
    // MAN-208: the `[decode.refine]` sub-table's ranges, in the same
    // single-choke-point style as every check below.
    if let Err(note) = cfg.refine.validate() {
        bail!("[decode.refine] {note} in {origin}");
    }
    if !cfg.prior_weight.is_finite() || cfg.prior_weight < 0.0 {
        bail!("[decode] prior_weight must be finite and >= 0 in {origin}");
    }
    if !cfg.reading_margin.is_finite() || cfg.reading_margin < 0.1 {
        bail!("[decode] reading_margin must be finite and >= 0.1 in {origin}");
    }
    if !(1..=16).contains(&cfg.held_readings) {
        bail!("[decode] held_readings must be in 1..=16 in {origin}");
    }
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
            origin
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
            origin
        );
    }
    if !cfg.demod.tau_lo_ms.is_finite() || cfg.demod.tau_lo_ms <= 0.0 {
        bail!(
            "[decode] tau_lo_ms must be a finite, positive number of milliseconds in {} \
             (got {})",
            origin,
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
            origin,
            cfg.beam.sigma
        );
    }
    if cfg.beam.width == 0 {
        bail!(
            "[decode] beam_width must be nonzero in {} (0 disables the beam decoder entirely)",
            origin
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
            origin
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
            origin,
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
            origin
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
            origin
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
            origin
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
            origin,
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
            origin,
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
            origin,
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
            origin,
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
            origin,
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
            origin,
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
            origin,
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
            origin,
            cfg.noise.noise_min_bias_db
        );
    }
    // Codex review, PR #178: MAN-168's engine wiring is what gives
    // `NoiseTracker::push`'s spectral-reference branch its first real
    // (non-`None`) input, so `spectral_beta`/`spectral_min_bias_db =
    // inf` -- previously harmless dead config, since the branch never
    // activated -- now makes `spectral_beta * b_spec * r` infinite the
    // same way `noise_min_bias_db = inf` does above, permanently closing
    // the keying-present gate.
    if !cfg.noise.spectral_min_bias_db.is_finite() {
        bail!(
            "[decode] spectral_min_bias_db must be finite in {} (got {}; a non-finite value \
             makes every spectral noise estimate infinite, silently closing the evidence gate)",
            origin,
            cfg.noise.spectral_min_bias_db
        );
    }
    if !cfg.noise.spectral_beta.is_finite() {
        bail!(
            "[decode] spectral_beta must be finite in {} (got {}; a non-finite value makes \
             every spectral noise estimate infinite, silently closing the evidence gate)",
            origin,
            cfg.noise.spectral_beta
        );
    }
    // Codex review, PR #178 round 4: a finite NEGATIVE spectral_beta
    // (e.g. a `-0.5` sign typo) makes NoiseTracker::push's spectral term
    // (`beta * b_spec * r`) negative, so `max(n_temp, ...)` always
    // discards it -- silently disabling the QRM/click discount just
    // wired in, the same way `speed_alpha < 0.0` silently broke the
    // speed estimate elsewhere in this file. 0.0 stays legal: it's a
    // valid, explicit "no spectral discount" value, not a sign error.
    if cfg.noise.spectral_beta < 0.0 {
        bail!(
            "[decode] spectral_beta must be nonnegative in {} (got {}; a negative value makes \
             the spectral term always lose to max(), silently disabling the spectral \
             noise discount)",
            origin,
            cfg.noise.spectral_beta
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
            origin,
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
            origin,
            cfg.noise.noise_window_ms
        );
    }
    // Codex review, PR #161 round 4: the remaining newly-exposed v1 §9
    // fields have the same class of gap -- `hyst_frac` reaching
    // `Demod::decision_band`'s `hyst_frac * (e_hi - lo)` with NaN makes
    // every keying comparison false (Legacy silently emits nothing at all,
    // no error, no panic); a non-positive or too-wide band (hyst_frac <= 0
    // collapses the band to a single point, no hysteresis at all;
    // hyst_frac >= 0.5 pushes the band's outer edges to or past the rails
    // themselves) breaks the open/close asymmetry hysteresis exists for
    // (MAN-103 replaced the multiplicative `hyst_up`/`hyst_down` pair with
    // this single additive fraction; see `envelope.rs`'s
    // `DemodConfig::hyst_frac` doc comment); `flush_gap_dits <= 0` forces
    // an instant/premature word flush on every hop. Validate all of them
    // here too, for the same reason as every check above: this is the one
    // place a `[decode]` table from disk enters the process.
    if !cfg.demod.hyst_frac.is_finite() {
        bail!(
            "[decode] hyst_frac must be finite in {} (got {})",
            origin,
            cfg.demod.hyst_frac
        );
    }
    if cfg.demod.hyst_frac <= 0.0 || cfg.demod.hyst_frac >= 0.5 {
        bail!(
            "[decode] hyst_frac must be > 0.0 and < 0.5 in {} (got {})",
            origin,
            cfg.demod.hyst_frac
        );
    }
    if !cfg.demod.debounce_ms.is_finite() || cfg.demod.debounce_ms <= 0.0 {
        bail!(
            "[decode] debounce_ms must be finite and positive in {} (got {})",
            origin,
            cfg.demod.debounce_ms
        );
    }
    if !cfg.flush_gap_dits.is_finite() || cfg.flush_gap_dits <= 0.0 {
        bail!(
            "[decode] flush_gap_dits must be finite and positive in {} (got {})",
            origin,
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
            origin,
            cfg.evidence.llr_clip
        );
    }
    // Codex review, MAN-168: `refine_bw_hz` is a newly-activated (default
    // 0.0/disabled) setting -- a NaN or infinite value is neither `<=
    // 0.0` (so refinement isn't bypassed) nor a usable bandwidth,
    // reaching `Refiner::new`'s own `debug_assert!(bw_hz > 0.0)` (a debug
    // panic; a release build instead designs an all-NaN/degenerate FIR
    // that then poisons every refined amplitude). 0.0 itself must stay
    // legal -- it's the documented "disabled" sentinel, not an error.
    //
    // Codex review, PR #178: a NEGATIVE value (e.g. a `-30` sign typo)
    // also isn't `> 0.0`, so `decoder_input`'s own `refine_bw_hz <= 0.0`
    // check silently treats it as the disabled bypass instead of
    // reporting the operator's config error -- the documented disabled
    // sentinel is specifically `0.0`, not "anything non-positive".
    if !cfg.refine_bw_hz.is_finite() || cfg.refine_bw_hz < 0.0 {
        bail!(
            "[decode] refine_bw_hz must be finite and nonnegative in {} (got {}; use 0.0 to \
             disable refinement, not a negative value)",
            origin,
            cfg.refine_bw_hz
        );
    }
    Ok(cfg)
}

fn merge_cli_prior(
    prior: Option<manta_decode::config_file::PriorSwitch>,
    mut cfg: manta_decode::decoder::DecodeConfig,
    origin: &str,
) -> Result<manta_decode::decoder::DecodeConfig> {
    if let Some(prior) = prior {
        cfg.prior_enabled = prior == manta_decode::config_file::PriorSwitch::On;
    }
    if cfg.prior_enabled && cfg.engine == Engine::Hsmm {
        bail!("[decode] prior requires legacy or edge-legacy in {origin}");
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

/// MAN-78 (supersedes MAN-85's ctrlc `termination` feature on Unix), MAN-232:
/// SIGINT and SIGTERM always drain; SIGHUP drains too unless this is a daemon
/// (`[server]`), where it requests a reload instead -- the returned `Signals`
/// buffers it for `reload::spawn`. Each stop signal gets ONE handler action:
/// store `stop` (the drain watches it), then atomically `swap` `signalled`
/// to `true` and `_exit(128 + signal)` if it was already set. One action,
/// not separate store/check/arm registrations, because (a) a signal
/// delivered between separate registrations would run only some of them,
/// and (b) handlers for *different* signals nest (signal-hook installs with
/// an empty `sa_mask`), so a load-then-store pair lets two concurrent stop
/// signals both see "not yet signalled" and neither exit. The swap makes
/// "first or second?" a single atomic decision. So the first stop signal
/// only requests the drain, and any second one ends the process from
/// inside the handler -- the one place that still runs while the decode
/// thread is blocked in a stdout write (SA_RESTART restarts that write, and
/// std retries EINTR anyway). Only these handlers set `signalled`: a closed
/// stdout and the post-loop `stop_ready` store set `stop` without arming it.
/// Installed before the `listening:` banner (MAN-122 round 6).
#[cfg(unix)]
fn install_unix_signal_handlers(
    stop: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    daemon: bool,
) -> Result<Option<signal_hook::iterator::Signals>> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    use std::sync::atomic::Ordering;
    let signalled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut stop_signals = vec![(SIGINT, "SIGINT"), (SIGTERM, "SIGTERM")];
    if !daemon {
        stop_signals.push((SIGHUP, "SIGHUP"));
    }
    for (signal, name) in stop_signals {
        let stop = stop.clone();
        let signalled = signalled.clone();
        // SAFETY: the action runs in signal context, so it may only do
        // async-signal-safe work. It does: two lock-free atomic operations
        // on `AtomicBool`s it owns (via `Arc` clones, never freed while
        // registered) and `low_level::exit`, which is `_exit(2)` --
        // async-signal-safe and skipping at-exit hooks. No allocation, no
        // locks, no I/O.
        unsafe {
            signal_hook::low_level::register(signal, move || {
                stop.store(true, Ordering::SeqCst);
                if signalled.swap(true, Ordering::SeqCst) {
                    signal_hook::low_level::exit(128 + signal);
                }
            })
        }
        .with_context(|| format!("installing the {name} handler"))?;
    }
    if daemon {
        let signals = signal_hook::iterator::Signals::new([SIGHUP])
            .context("installing the SIGHUP handler")?;
        Ok(Some(signals))
    } else {
        Ok(None)
    }
}

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

/// MAN-73: the `manta_source_health` sink a `ReconnectingSource` reports
/// through. The unhealthy transition also zeroes the shared active-track
/// gauge (PR #207 review): for the whole outage `listen()` is blocked in
/// the reconnecting `read()`, so it can't publish a count itself, and the
/// poller above would keep exporting the pre-drop tracks -- MAN-45's ghost
/// count. `listen()` publishes the real count again after its first
/// post-reconnect chunk. MAN-251 (PR #330 review): the EXPORTED
/// `manta_active_tracks` is cleared here too, synchronously -- the poller
/// loads the shared gauge and publishes it as two separate steps, so a
/// tick that loaded the pre-drop count could republish it after the
/// transition, and without this clear the zero would wait for the next
/// full `ACTIVE_TRACKS_POLL_INTERVAL`. MAN-314: `manta_rtty_signals` is
/// zeroed here for the same reason; it is published only from the
/// per-batch callback, which cannot run during the outage, and the
/// reconnect's discontinuity restarts the RTTY set that callback
/// publishes next.
fn source_health_sink(
    name: &'static str,
    metrics: Option<std::sync::Arc<manta_server::metrics::Metrics>>,
    active_tracks: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
) -> reconnect::HealthSink {
    Box::new(move |healthy| {
        if !healthy {
            if let Some(gauge) = &active_tracks {
                gauge.store(0, std::sync::atomic::Ordering::Relaxed);
            }
            if let Some(metrics) = &metrics {
                metrics.set_active_tracks(0);
                metrics.set_rtty_signals(&[]);
            }
        }
        if let Some(metrics) = &metrics {
            metrics.set_source_health(name, healthy);
        }
    })
}

/// MAN-533: the loss sink a one-input `ReconnectingSource` reports every
/// lost connection's reason through, for `/status` `sources[]`.
fn source_loss_sink(
    name: &'static str,
    metrics: Option<std::sync::Arc<manta_server::metrics::Metrics>>,
) -> reconnect::LossSink {
    Box::new(move |reason| {
        if let Some(metrics) = &metrics {
            metrics.record_source_loss(name, reason);
        }
    })
}

/// How often the daemon copies the engine's decode-latency observer into
/// `Metrics` (MAN-128). Same bridge shape and tuning rationale as
/// `ACTIVE_TRACKS_POLL_INTERVAL`.
const DECODE_LATENCY_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// Stops status emission completely before clients can begin draining.
fn stop_status_line(rt: &tokio::runtime::Runtime, server: &SpotServer) {
    let _ = server.status_shutdown_tx.send(true);
    if let Some(status_line) = server
        .status_line
        .lock()
        .expect("status-line handle mutex poisoned")
        .take()
    {
        let _ = rt.block_on(status_line);
    }
}

/// MAN-256: join the status task before broadcasting client shutdown.
/// The comparator still joins after the broadcast, with its existing bound.
fn signal_spot_server_shutdown(rt: &tokio::runtime::Runtime, server: &SpotServer) {
    stop_status_line(rt, server);
    let _ = server.shutdown_tx.send(true);
    if let Some(rbn_compare) = server
        .rbn_compare
        .lock()
        .expect("rbn_compare handle mutex poisoned")
        .take()
    {
        let _ =
            rt.block_on(async { tokio::time::timeout(SHUTDOWN_DRAIN_DEADLINE, rbn_compare).await });
    }
}

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

/// MAN-64: the source-health FAILURE transition for a fatal `listen` exit.
/// Since MAN-73, a reconnectable source's read error no longer reaches
/// here -- `ReconnectingSource` retries it and reports `false`/`true`
/// itself through `source_health_sink` -- so `manta_engine::listen`
/// returning `Err` now means file replay failed, a reopened live source
/// came back with a different sample rate or centre frequency, or the
/// pipeline behind the source failed. Either way the process is about to
/// exit; recording it here -- BEFORE `shutdown_tx.send(true)` -- gives a
/// scraper a chance to see `manta_source_health{...} 0` before the daemon
/// is gone, PROVIDED the metrics listener is still alive to answer it.
/// That is conditional, not guaranteed (validate-plan round, V-1):
/// `metrics_http::serve` runs outside `ClientTasks` with no shutdown watch,
/// so it survives past this point only for as long as
/// `shutdown_runtime_after_drain`'s `tasks::await_all` keeps the runtime
/// alive -- the whole `SHUTDOWN_DRAIN_DEADLINE` window when a
/// telnet/JSON/WS client is genuinely still draining, but almost no time
/// (the runtime is torn down microseconds later) when no such client is
/// connected, the ordinary state for a scrape-only deployment. Written
/// through `set_source_health_terminal` (round 7, V-2), not the regular
/// `set_source_health`, so no later regular write can flip it back; see
/// `SourceHealthEntry` in `manta-server/src/metrics.rs` for which writers
/// exist today.
///
/// Deliberately silent on the `Ok` path: a clean end of stream (file replay
/// finished, operator Ctrl-C) is normal termination, not a source failure,
/// and reporting it as unhealthy would make the gauge lie in the opposite
/// direction. See ARCHITECTURE.md §8 and
/// `docs/DECISIONS/2026-09-04-man64-metrics-request-rate-and-source-health.md`.
fn record_terminal_source_health(
    metrics: &manta_server::metrics::Metrics,
    source_name: &str,
    listen_result: &Result<()>,
) {
    if listen_result.is_err() {
        metrics.set_source_health_terminal(source_name, false);
    }
}

/// MAN-128: the engine's `DecodeLatencySnapshot` and `manta-server`'s
/// `LatencyHistogram` are deliberately disjoint types (the same dependency-
/// free-`manta-server` boundary `input_health_of` crosses above) -- this is
/// where the engine's bucket bounds get attached to its own snapshot.
fn latency_histogram_of(
    s: &manta_engine::DecodeLatencySnapshot,
) -> manta_server::metrics::LatencyHistogram {
    manta_server::metrics::LatencyHistogram {
        bounds_seconds: manta_engine::DECODE_LATENCY_BUCKETS_SECONDS.to_vec(),
        bucket_counts: s.bucket_counts.clone(),
        sum_seconds: s.sum_seconds,
    }
}

/// Emit one calibration mismatch warning for the lifetime of an input.
fn warn_iq_mismatch(
    cfg: &manta_dsp::condition::ConditioningConfig,
    center: f64,
    name: Option<&str>,
    warned: &std::sync::atomic::AtomicBool,
) {
    use manta_dsp::condition::StageMode;
    let Some(cal) = &cfg.iq_calibration else {
        return;
    };
    if cfg.iq != StageMode::Apply
        || (center.is_finite() && center > 0.0 && (cal.center_freq_hz - center).abs() <= 1.0)
    {
        return;
    }
    if warned.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let prefix = name.map_or_else(String::new, |n| format!("input {}: ", fmt::quoted(n)));
    logging::warning(&format!(
        "{prefix}IQ correction withheld: calibrated at {:.1} kHz, receiver at {:.1} kHz; measure a new calibration",
        cal.center_freq_hz / 1000.0,
        center / 1000.0
    ));
}

/// `manta decode`'s decode: a manta IQ recording or headerless IQ through
/// `decode_source_observed` (MAN-309), a WAV otherwise. MAN-131: says so
/// before decoding when the reported frequencies will be baseband offsets,
/// only for an existing file, so a missing WAV still fails with just its
/// `open WAV` error.
fn decode_for_cli(
    path: &Path,
    mut center_freq_hz: Option<f64>,
    iq_format: Option<manta_input::RawIqFormat>,
    sample_rate_hz: Option<f64>,
    cfg: &PipelineConfig,
    policy: manta_engine::ObservationPolicy,
) -> Result<(manta_engine::DecodeReport, manta_engine::ObservedRun)> {
    let native = manta_input::is_native_recording(path);
    if native || iq_format.is_some() {
        if !native && (sample_rate_hz.is_none() || center_freq_hz.is_none()) {
            bail!("headerless IQ requires --sample-rate-hz and --center-freq-hz");
        }
        let mut src = manta_input::open_iq_file(
            path,
            manta_input::IqFileOptions {
                raw_format: iq_format,
                sample_rate_hz,
                center_freq_hz,
            },
        )?;
        if native && center_freq_hz.is_none() && src.center_freq_hz() <= 0.0 {
            errln!(
                "warning: IQ recording {} has no RF centre frequency in its \
                 metadata -- reported frequencies are baseband offsets from the \
                 recording's centre, not absolute RF frequencies. Pass the centre \
                 frequency, e.g. --center-freq-hz 14000000.",
                path.display()
            );
        }
        warn_iq_mismatch(
            &cfg.conditioning,
            src.center_freq_hz(),
            None,
            &std::sync::atomic::AtomicBool::new(false),
        );
        // `decode_source` segments the decode at each recorded gap.
        manta_engine::decode_source_observed(src.as_mut(), cfg, policy)
    } else {
        if center_freq_hz.is_none() && path.is_file() {
            let sidecar = manta_input::read_sidecar(path)?;
            if let Some(warning) =
                recording_center_warning(path, sidecar.map(|sc| sc.center_freq_hz))
            {
                errln!("{warning}");
                // Keep the warning true: a negative sidecar centre
                // decodes at 0.0, not shifted by itself.
                center_freq_hz = Some(0.0);
            }
        }
        let mut src = match center_freq_hz {
            Some(hz) => manta_input::WavIqSource::open_with_center_freq_hz(path, hz)?,
            None => manta_input::WavIqSource::open(path)?,
        };
        warn_iq_mismatch(
            &cfg.conditioning,
            src.center_freq_hz(),
            None,
            &std::sync::atomic::AtomicBool::new(false),
        );
        manta_engine::decode_source_observed(&mut src, cfg, policy)
    }
}

/// MAN-207: creates `--observations PATH`, refusing an existing path so a
/// typo can never truncate the recording itself.
fn create_observation_file(path: &Path) -> Result<std::fs::File> {
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(file) => Ok(file),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => bail!(
            "--observations {} already exists; remove it or choose another path",
            path.display()
        ),
        Err(e) => Err(e).with_context(|| format!("create --observations {}", path.display())),
    }
}

/// MAN-118: as `latency_histogram_of`, attaching the engine's rail-ratio
/// bounds and the repetition gate's bucket bounds to their snapshots.
fn decode_quality_of(
    s: &manta_engine::DecodeQualitySnapshot,
) -> manta_server::metrics::DecodeQuality {
    manta_server::metrics::DecodeQuality {
        refine: s.refine,
        chars_total: s.chars_total,
        garbles_total: s.garbles_total,
        rail_ratio: manta_server::metrics::ValueHistogram {
            bounds: manta_engine::RAIL_RATIO_BUCKETS.to_vec(),
            bucket_counts: s.rail_ratio_bucket_counts.clone(),
            sum: s.rail_ratio_sum,
        },
        repetitions: manta_server::metrics::ValueHistogram {
            bounds: manta_spot::REPETITION_BUCKETS
                .iter()
                .map(|&b| f64::from(b))
                .collect(),
            bucket_counts: s.repetition_bucket_counts.clone(),
            sum: s.repetition_sum as f64,
        },
        // MAN-207: the live-state stream's labels, `none` for a spot.
        decisions: s
            .decisions
            .iter()
            .map(|&(verdict, reason, n)| {
                (
                    manta_server::live_state::decision_label(verdict),
                    reason.map_or("none", manta_server::live_state::reason_label),
                    n,
                )
            })
            .collect(),
        // MAN-207: the MAN-201 role labels.
        not_candidates: s
            .not_candidates
            .iter()
            .map(|&(role, n)| (role.as_str(), n))
            .collect(),
    }
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
        queue_dropped_samples: counters.queue_dropped_samples(),
        dropped_packets: counters.dropped_packets(),
        gaps_detected: counters.gaps_detected(),
        malformed_packets: counters.malformed_packets(),
    }
}

/// MAN-308: `manta-input`'s attenuation snapshot as `manta-server`'s type,
/// the one place the two meet (as `input_health_of`).
fn input_attenuation_of(
    s: manta_input::hpsdr::AttenuationSnapshot,
) -> manta_server::metrics::InputAttenuation {
    manta_server::metrics::InputAttenuation {
        db: s.db,
        changes: s.changes,
    }
}

/// MAN-308: publish `a` under `source` once a protocol-2 open has engaged
/// it; before that (or on a protocol-1 radio) the input has no series.
fn publish_attenuation(
    metrics: &manta_server::metrics::Metrics,
    source: &str,
    a: &manta_input::hpsdr::AutoAttenuation,
) {
    if let Some(s) = a.snapshot() {
        metrics.set_input_attenuation(source, input_attenuation_of(s));
    }
}

/// MAN-308: sample `a` into `/metrics` now and then every
/// `INPUT_HEALTH_POLL_INTERVAL`, the MAN-56 poller shape.
fn spawn_attenuation_poller(
    rt: &tokio::runtime::Runtime,
    metrics: std::sync::Arc<manta_server::metrics::Metrics>,
    source: String,
    a: std::sync::Arc<manta_input::hpsdr::AutoAttenuation>,
) {
    publish_attenuation(&metrics, &source, &a);
    rt.spawn(async move {
        loop {
            tokio::time::sleep(INPUT_HEALTH_POLL_INTERVAL).await;
            publish_attenuation(&metrics, &source, &a);
        }
    });
}

/// MAN-128: feeds `manta_build_info`. `git_sha` comes from `build.rs`
/// (`MANTA_GIT_SHA`, "unknown" with no `.git` -- e.g. a Docker build
/// context, which `.dockerignore` excludes it from). MAN-83: the values are
/// `build_info`'s constants, the same ones `--version` and the JSON
/// stream's `decoderVersion` read, so the gauge, the version line and the
/// spots always name the same build.
fn daemon_build_info() -> manta_server::metrics::BuildInfo {
    manta_server::metrics::BuildInfo {
        version: build_info::VERSION.to_string(),
        git_sha: build_info::GIT_SHA.to_string(),
        features: build_info::FEATURES.to_string(),
    }
}

/// The identity and `SKIMMER/SETT` settings every telnet client is told
/// about this station. Split out of `start_spot_server` (which can only be
/// exercised through a real bound listener) so the source-to-SETT wiring
/// itself is unit-testable -- MAN-86 review found two ways for it to
/// advertise coverage manta cannot actually hear, and neither was visible
/// from `sett.rs`'s own tests.
fn station_profile(
    cfg: &manta_server::config::ServerConfig,
    center_freq_hz: f64,
    rf_passband_hz: (f64, f64),
    freq_calibration: f64,
    validation_level: manta_spot::ValidationLevel,
) -> manta_server::telnet::StationProfile {
    manta_server::telnet::StationProfile {
        call: cfg.station_callsign.clone(),
        operator_name: cfg.operator_name.clone(),
        operator_qth: cfg.operator_qth.clone(),
        operator_grid: cfg.operator_grid.clone(),
        sett: manta_server::sett::SettSettings {
            validation_level: match validation_level {
                manta_spot::ValidationLevel::Minimal => {
                    manta_server::sett::ValidationLevel::Minimal
                }
                manta_spot::ValidationLevel::Normal => manta_server::sett::ValidationLevel::Normal,
            },
            cq_only: false,
            segments: manta_server::sett::segments_for_passband(
                center_freq_hz,
                rf_passband_hz,
                freq_calibration,
            ),
        },
    }
}

/// The three listener names `/healthz`/`manta_listener_up` track (MAN-128)
/// -- named constants so `set_listener_up`'s and `spawn_tracked_listener`'s
/// call sites below can't drift apart via a typo in one of the two paired
/// string literals (code-review finding, round 1).
const LISTENER_TELNET: &str = "telnet";
const LISTENER_JSON: &str = "json";
const LISTENER_METRICS: &str = "metrics";
/// MAN-324: the live-state listener, registered only when
/// `[server].live_port` is set.
const LISTENER_LIVE: &str = "live";
/// MAN-322: the RTTY-only telnet listener, registered only when
/// `[server].rtty_telnet_port` is set.
const LISTENER_RTTY_TELNET: &str = "rtty_telnet";

/// Spawns `fut` as a tracked listener task: `metrics` reports `name` down
/// the moment that task ends, whether by panic or by returning (the
/// daemon's own listener loops only return via `shutdown_tx`, but a
/// probe reading `manta_listener_up`/`/healthz` during the drain that
/// follows should see the true state, not a stale "up"). `name` is
/// `&'static str` because it's always one of the three fixed listener
/// names below, never operator-supplied.
fn spawn_tracked_listener(
    name: &'static str,
    metrics: std::sync::Arc<manta_server::metrics::Metrics>,
    fut: impl std::future::Future<Output = ()> + Send + 'static,
) {
    let handle = tokio::spawn(fut);
    tokio::spawn(async move {
        if let Err(err) = handle.await {
            tracing::error!(listener = name, error = %err, "listener task panicked");
        }
        metrics.set_listener_up(name, false);
    });
}

fn spot_bus_for(
    cfg: &manta_server::config::ServerConfig,
    sample_rate_hz: f64,
    epoch: std::time::SystemTime,
    session_nonce: u128,
) -> manta_server::bus::SpotBus {
    manta_server::bus::SpotBus::new(sample_rate_hz, epoch, session_nonce).with_history_minutes(
        cfg.history_minutes
            .unwrap_or(manta_server::bus::DEFAULT_HISTORY_MINUTES),
    )
}

/// MAN-316: the reference lists a `run` shares between its validator and
/// spot servers, and the refresher to start once the servers are up.
struct RunLists {
    lists: std::sync::Arc<manta_engine::ReferenceLists>,
    /// cty first, then scp.
    in_use: [list_refresh::InUse; 2],
    /// `Some` only for a live source with `refresh_lists` on (D2).
    refresh: Option<list_refresh::RefreshSetup>,
}

/// Builds `run`'s shared lists from `cfg`'s tables. With a live source and
/// `refresh_lists` on, a list no pin or path chose may start from the cache
/// (D3) and is handed to the refresher, and the stale built-in cty.dat
/// warning is printed here, judged on the copy chosen; otherwise the tables
/// are `cfg`'s, and file replay never reads the cache (D4).
fn prepare_run_lists(
    live: bool,
    spot: &SpotResolved,
    in_use: [list_refresh::InUse; 2],
    cfg: &PipelineConfig,
) -> Result<RunLists> {
    if !(live && spot.refresh_lists) {
        return Ok(RunLists {
            lists: std::sync::Arc::new(manta_engine::ReferenceLists::new(
                cfg.cty_table(),
                cfg.scp_set(),
            )),
            in_use,
            refresh: None,
        });
    }
    let cache = spot.list_cache_dir.clone().map(list_cache::Cache::new);
    let [cty_in_use, scp_in_use] = in_use;
    let (lists, in_use) = list_refresh::resolve_startup(
        cfg.cty_table(),
        cty_in_use,
        cfg.scp_set(),
        scp_in_use,
        true,
        cache.as_ref(),
    );
    if let Some(warning) = stale_cty_warning(&in_use[0], std::time::SystemTime::now()) {
        logging::warning(&warning);
    }
    let mut managed = Vec::new();
    for (list, url) in in_use.iter().zip([&spot.cty_url, &spot.scp_url]) {
        if !list.is_operator_choice() {
            managed.push(list_refresh::ListSetting {
                url: list_http::HttpUrl::parse(url).map_err(|e| anyhow!("[spot]: {e}"))?,
                in_use: list.clone(),
            });
        }
    }
    Ok(RunLists {
        lists: std::sync::Arc::new(lists),
        in_use,
        refresh: Some(list_refresh::RefreshSetup {
            lists: managed,
            cache,
            limits: list_http::FetchLimits::default(),
        }),
    })
}

/// MAN-316: publishes the lists in use to `manta_list_info` (every daemon),
/// prints the startup lines, and starts the `manta-lists` thread when a
/// MAN-318: starts the `manta-lookup` thread when `[callsign_lookup]` is
/// enabled and the source is live; a file replay notes that lookups are off
/// (D3), so it stays repeatable. `None` skips the validator's lookup step.
fn start_callsign_lookup(
    cfg: Option<&config::CallsignLookupConfig>,
    live: bool,
    metrics: Option<std::sync::Arc<manta_server::metrics::Metrics>>,
) -> Result<Option<std::sync::Arc<dyn manta_spot::CallsignLookup>>> {
    let Some(cfg) = cfg.filter(|c| c.enabled) else {
        return Ok(None);
    };
    if !live {
        logging::note(callsign_lookup::FILE_REPLAY_NOTE);
        return Ok(None);
    }
    logging::note(&callsign_lookup::startup_line(cfg));
    let lookup = callsign_lookup::start(cfg, metrics)?;
    Ok(Some(lookup))
}

/// live source has refresh on and a list to manage. Returns the handle the
/// decode loop watches, `None` when nothing can change the lists.
fn start_list_refresh(
    run_lists: RunLists,
    live: bool,
    spot: &SpotResolved,
    metrics: Option<std::sync::Arc<manta_server::metrics::Metrics>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<Option<std::sync::Arc<manta_engine::ReferenceLists>>> {
    let RunLists {
        lists,
        in_use,
        refresh,
    } = run_lists;
    let managed: Vec<ListKind> = refresh.as_ref().map_or_else(Vec::new, |setup| {
        setup.lists.iter().map(|list| list.in_use.kind).collect()
    });
    if let Some(metrics) = &metrics {
        list_refresh::publish(metrics, &in_use, &managed);
    }
    let Some(setup) = refresh else {
        if spot.refresh_lists && !live {
            logging::note(&list_refresh::file_replay_note(&in_use[0], &in_use[1]));
        } else {
            note_pinned_lists(spot, &in_use);
        }
        return Ok(None);
    };
    logging::note(&list_refresh::lists_line(&in_use[0], &in_use[1]));
    if !setup.lists.is_empty() {
        logging::note(&setup.startup_line());
    }
    for list in &in_use {
        if let Some(note) = list_refresh::left_alone_note(list) {
            logging::note(&note);
        }
    }
    if setup.lists.is_empty() {
        return Ok(None);
    }
    list_refresh::spawn(setup, lists.clone(), metrics, stop)
        .context("starting the manta-lists thread")?;
    Ok(Some(lists))
}

/// How `manta doctor` names the configured receiver. Never includes the
/// KiwiSDR password.
fn receiver_desc(spec: &LiveSourceSpec) -> doctor_checks::ReceiverDesc {
    use doctor_checks::{ReceiverDesc, ReceiverKind};
    let (kind, description) = match spec {
        LiveSourceSpec::Kiwi(kiwi) => (
            ReceiverKind::Kiwi,
            format!(
                "KiwiSDR {}:{}",
                kiwi.host.as_deref().unwrap_or_default(),
                kiwi.port
            ),
        ),
        LiveSourceSpec::Soapy(soapy) => (
            ReceiverKind::Soapy,
            format!(
                "SoapySDR device \"{}\"",
                soapy.driver.as_deref().unwrap_or_default()
            ),
        ),
        LiveSourceSpec::Hpsdr(hpsdr) => (
            ReceiverKind::Hpsdr,
            match &hpsdr.host {
                Some(host) => format!(
                    "HPSDR radio {host}:{}",
                    hpsdr.port.unwrap_or(HPSDR_CONTROL_PORT)
                ),
                None => format!(
                    "HPSDR radio via discovery{}",
                    hpsdr
                        .mac
                        .as_ref()
                        .map(|mac| format!(
                            " (MAC {})",
                            manta_input::hpsdr::discovery::format_mac(mac)
                        ))
                        .unwrap_or_default()
                ),
            },
        ),
        LiveSourceSpec::Ka9q(ka9q) => (
            ReceiverKind::Ka9q,
            format!(
                "ka9q-radio group {}:{}{}",
                ka9q.group.as_deref().unwrap_or_default(),
                ka9q.port,
                ka9q.ssrc.map(|s| format!(" ssrc {s}")).unwrap_or_default()
            ),
        ),
        LiveSourceSpec::AudioDevice(None) => (
            ReceiverKind::SoundCard,
            "the system default sound-card input".to_string(),
        ),
        LiveSourceSpec::AudioDevice(Some(name)) => (
            ReceiverKind::SoundCard,
            format!("the sound-card input matching \"{name}\""),
        ),
        LiveSourceSpec::File { path, .. } => (
            ReceiverKind::File,
            format!("the WAV file {}", path.display()),
        ),
    };
    ReceiverDesc { kind, description }
}

/// `run`/`soak`/`doctor`'s shared start: load the config (`--config`, else
/// `MANTA_CONFIG`) with the `MANTA_*` overlay, merge the CLI over it, print
/// the merge notes, and build the pipeline config. The environment is read
/// here and only here, with `vars_os` so a non-UTF-8 variable is never a
/// panic (MAN-261 scenario 4).
struct Prepared {
    config_path: Option<PathBuf>,
    loaded: config::Loaded,
    resolved: Resolved,
    pipeline: PipelineConfig,
    /// MAN-316: the version of cty.dat and MASTER.SCP `pipeline` holds.
    lists_in_use: [list_refresh::InUse; 2],
}

/// MAN-79 scenario 2: one stderr line when the built-in cty.dat is in use
/// and older than manta_spot::vintage::CTY_DAT_STALE_AFTER_DAYS. `now` is a
/// parameter so tests never depend on today's date.
fn bundled_cty_warning(cty_overridden: bool, now: std::time::SystemTime) -> Option<String> {
    if cty_overridden {
        return None;
    }
    let days = manta_spot::vintage::stale_cty_dat_age_days(now)?;
    Some(format!(
        "warning: the built-in cty.dat is {days} days old (retrieved {}), so calls from \
         prefixes allocated since then are not spotted. Download the current file from \
         https://www.country-files.com/cty/cty.dat and pass it with --cty or set spot.cty_path.",
        manta_spot::vintage::CTY_DAT_RETRIEVED
    ))
}

/// [`bundled_cty_warning`] for `cty`, the cty.dat a command decodes with:
/// silent unless it is the built-in copy because nothing chose another (a
/// path, a pin, even of the built-in bytes, or the cache). A live `run`
/// with refresh on may start from the cache's copy (MAN-316), so
/// `prepare_run_lists` asks once it has chosen.
fn stale_cty_warning(cty: &list_refresh::InUse, now: std::time::SystemTime) -> Option<String> {
    bundled_cty_warning(cty.origin != list_refresh::Origin::Bundled, now)
}

/// Shared typed config and source resolution, with no pipeline assets or I/O:
/// the part of `prepare_live` that `check` and `calibrate` share.
struct SourcePrepared {
    config_path: Option<PathBuf>,
    loaded: config::Loaded,
    resolved: Resolved,
}

/// MAN-13: a command that examines one receiver, and the `--input` it was
/// given (see `select_input`).
#[derive(Debug, Clone, Copy)]
struct OneInput<'a> {
    command: &'a str,
    name: Option<&'a str>,
}

/// A source flag names a standalone receiver and `resolve` ignores the
/// configured entries for it, so only an explicit `--input` still narrows
/// them (Codex, PR 313).
fn needs_input_selection(cli: &CliOverrides, one: OneInput<'_>) -> bool {
    one.name.is_some() || cli.source_selector().is_none()
}

fn prepare_source(
    cli: CliOverrides,
    config_flag: Option<PathBuf>,
    one: OneInput<'_>,
) -> Result<SourcePrepared> {
    let vars: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os().collect();
    let config_path = config_flag.or_else(|| config::config_path_from_env(&vars));
    let mut loaded = config::load(config_path.as_deref(), config::Env::Read(&vars))?;
    if needs_input_selection(&cli, one) {
        select_input(&mut loaded, one.name, one.command)?;
    }
    let resolved = resolve(cli, &loaded)?;
    for note in &resolved.notes {
        logging::note(note);
    }
    Ok(SourcePrepared {
        config_path,
        loaded,
        resolved,
    })
}

/// `warn_stale_cty`: print [`stale_cty_warning`] here. `run` passes false
/// and prints it itself once it knows which cty.dat it starts with.
fn prepare_live(
    cli: CliOverrides,
    config_flag: Option<PathBuf>,
    cli_engine: Option<Engine>,
    warn_stale_cty: bool,
    one: Option<OneInput<'_>>,
) -> Result<Prepared> {
    let vars: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os().collect();
    let config_path = config_flag.or_else(|| config::config_path_from_env(&vars));
    let prepared = prepare_config_for(cli, config_path, cli_engine, &vars, one)?;
    for note in &prepared.resolved.notes {
        logging::note(note);
    }
    if warn_stale_cty {
        if let Some(warning) =
            stale_cty_warning(&prepared.lists_in_use[0], std::time::SystemTime::now())
        {
            logging::warning(&warning);
        }
    }
    Ok(prepared)
}

/// `prepare_live`'s non-printing core, given the already-chosen config
/// path and the environment: load + `MANTA_*` overlay, CLI merge, and the
/// blocklist/notch file reads. A SIGHUP reload (MAN-78) re-runs exactly
/// this, so `manta config check` passing means a reload passes too.
fn prepare_config(
    cli: CliOverrides,
    config_path: Option<PathBuf>,
    cli_engine: Option<Engine>,
    vars: &[(std::ffi::OsString, std::ffi::OsString)],
) -> Result<Prepared> {
    prepare_config_for(cli, config_path, cli_engine, vars, None)
}

/// `prepare_config`, narrowed to one input for a one-receiver command
/// (MAN-13).
fn prepare_config_for(
    cli: CliOverrides,
    config_path: Option<PathBuf>,
    cli_engine: Option<Engine>,
    vars: &[(std::ffi::OsString, std::ffi::OsString)],
    one: Option<OneInput<'_>>,
) -> Result<Prepared> {
    let mut loaded = config::load(config_path.as_deref(), config::Env::Read(vars))?;
    if let Some(one) = one.filter(|one| needs_input_selection(&cli, *one)) {
        select_input(&mut loaded, one.name, one.command)?;
    }
    let cli_prior = cli.prior;
    let rbn_opts = cli.rbn.clone();
    let resolved = resolve(cli, &loaded)?;
    check_rbn_compare(&rbn_opts, &loaded, &resolved)?;
    let decode = merge_cli_prior(
        cli_prior,
        merge_cli_engine(cli_engine, loaded.decode.clone()),
        &loaded.origin,
    )?;
    let (mut pipeline, lists_in_use) = build_pipeline_config(
        resolved.freq_correction_ppm,
        &resolved.spot,
        loaded.detector,
        decode.engine,
    )?;
    pipeline.conditioning = resolved.conditioning.clone();
    pipeline.cw_policy = resolved.cw_policy.clone();
    pipeline.cw_clock = if resolved.spec.is_reconnectable()
        || !matches!(resolved.spec, LiveSourceSpec::File { .. })
    {
        manta_engine::cw_segments::Clock::Live
    } else {
        manta_engine::cw_segments::Clock::Samples
    };
    if let LiveSourceSpec::File { ref path, .. } = resolved.spec {
        (pipeline.cw_epoch, pipeline.cw_epoch_nanoseconds) = cw_replay_epoch(
            path,
            resolved.replay_epoch,
            pipeline.cw_policy.scheduled()
                || resolved.inputs.iter().any(|i| i.cw_policy.scheduled()),
            loaded.inputs.first().map_or("file", |i| i.name.as_str()),
        )?;
    }
    // Every input's pipeline shares `pipeline`'s epoch (`listen_inputs`
    // replaces only policy and clock), so scheduled native recordings must
    // agree on their start; differing ones would run another recording's
    // schedule against the wrong UTC.
    let mut shared_epoch = matches!(resolved.spec, LiveSourceSpec::File { .. }).then(|| {
        (
            pipeline.cw_epoch,
            pipeline.cw_epoch_nanoseconds,
            String::from("[input]"),
        )
    });
    for input in &resolved.inputs {
        if let LiveSourceSpec::File { ref path, .. } = input.spec {
            let epoch = cw_replay_epoch(
                path,
                resolved.replay_epoch,
                input.cw_policy.scheduled(),
                &input.name,
            )?;
            if !input.cw_policy.scheduled() || resolved.replay_epoch.is_some() {
                continue;
            }
            match &shared_epoch {
                None => {
                    (pipeline.cw_epoch, pipeline.cw_epoch_nanoseconds) = epoch;
                    shared_epoch = Some((epoch.0, epoch.1, input.name.clone()));
                }
                Some((secs, nanos, first)) if (*secs, *nanos) != epoch => bail!(
                    "inputs {} and {} are recordings with different start times; a scheduled \
                     replay applies one epoch to every input: set replay_epoch",
                    fmt::quoted(first),
                    fmt::quoted(&input.name)
                ),
                Some(_) => {}
            }
        }
    }
    pipeline.decode = decode;
    attach_language_prior(&mut pipeline, &resolved.spot, None)?;
    Ok(Prepared {
        config_path,
        loaded,
        resolved,
        pipeline,
        lists_in_use,
    })
}

/// MAN-301's refusals that need the merged command line: `--rbn-spotter`
/// without comparison, comparison turned on by the flag without `[server]`
/// (the loader refuses the file's own `enabled = true`), and `--rbn-spotter`
/// naming this station. Run before any source or listener opens, and by
/// `manta config check`. The replayed-recording refusal is
/// [`refuse_rbn_compare_replay`], which only `run` (and its SIGHUP reload)
/// and `config check` call: `soak` and `doctor` share this path but never
/// start the comparator.
fn check_rbn_compare(
    cli: &RbnCompareOpts,
    loaded: &config::Loaded,
    resolved: &Resolved,
) -> Result<()> {
    let enabled = resolved.rbn_compare.as_ref().is_some_and(|c| c.enabled);
    if !cli.rbn_spotter.is_empty() && !enabled {
        bail!(
            "--rbn-spotter needs RBN comparison: add --rbn-compare or set \
             [rbn_compare].enabled = true"
        );
    }
    let Some(rbn_compare) = resolved.rbn_compare.as_ref().filter(|c| c.enabled) else {
        return Ok(());
    };
    let Some(server) = &loaded.server else {
        bail!("{}", config::rbn_compare_needs_server(&loaded.origin));
    };
    if !cli.rbn_spotter.is_empty() {
        if let Some(call) = rbn_compare.own_call_spotter(&server.station_callsign) {
            bail!(
                "--rbn-spotter {call} is this station's own callsign: its reports are always \
                 excluded, so every spot would grade none"
            );
        }
        if let Some(call) = rbn_compare.excluded_spotter() {
            bail!(
                "--rbn-spotter {call} is in [rbn_compare].exclude_spotters: its reports would \
                 never count"
            );
        }
    }
    Ok(())
}

/// MAN-301: RBN comparison enabled on a run that replays a recording is
/// refused, since the live feed's reports would never line up with the
/// recording's spot times. `run` calls this after `prepare_live`, before any
/// source or listener opens; its SIGHUP reload and `manta config check` call
/// it too. `soak` and `doctor` do not, because neither starts the comparator.
/// `source_flag` is whether `--source` chose the file.
fn refuse_rbn_compare_replay(resolved: &Resolved, source_flag: bool) -> Result<()> {
    if !resolved.rbn_compare.as_ref().is_some_and(|c| c.enabled) {
        return Ok(());
    }
    if let LiveSourceSpec::File { .. } = resolved.spec {
        let how = if source_flag {
            "--source"
        } else {
            "[input] type = \"file\""
        };
        bail!(
            "RBN comparison grades spots against the live RBN feed, but this run replays a \
             recording ({how}). Grade a recorded node's spot log with `manta grade` instead."
        );
    }
    Ok(())
}

/// `decode`/`oracle`: one stderr note naming the tables present in the file
/// that the command does not apply (D7).
fn note_ignored_tables(loaded: &config::Loaded, command: &str, applied: &[&str]) {
    if let Some(note) = ignored_tables_note(loaded, command, applied) {
        errln!("{note}");
    }
}

/// The text of `note_ignored_tables`'s note, or `None` when every present
/// table applies (`bench sensitivity` returns it instead of printing it).
fn ignored_tables_note(loaded: &config::Loaded, command: &str, applied: &[&str]) -> Option<String> {
    let ignored: Vec<&str> = loaded
        .present
        .iter()
        .map(String::as_str)
        .filter(|t| !applied.contains(t))
        .collect();
    (!ignored.is_empty()).then(|| {
        format!(
            "note: {command} ignores [{}] from {}",
            ignored.join("], ["),
            loaded.origin
        )
    })
}

fn main() -> std::process::ExitCode {
    match run_command() {
        Ok(code) => code,
        // MAN-232: stdout's reader went away; the failing command already
        // said what it needed to (run's drain logs `stdout closed; stopping`),
        // so exit 141 silently -- what a shell reports for a SIGPIPE'd
        // producer.
        Err(err) if err.downcast_ref::<stdio::StdoutClosed>().is_some() => {
            std::process::ExitCode::from(stdio::EXIT_STDOUT_CLOSED)
        }
        Err(err) => {
            // MAN-124: under --log-format json the error is a log record too,
            // whatever the level filter; exit status stays 1. MAN-270: in
            // text mode it is one escaped `error:` line.
            if logging::is_json() {
                logging::fatal(&fmt::cause_text(&err));
            } else {
                errln!("{}", fmt::render_error(&err));
            }
            std::process::ExitCode::FAILURE
        }
    }
}

fn run_command() -> Result<std::process::ExitCode> {
    warn_deprecations();
    match Cli::parse().command {
        Command::Devices { json } => std::process::exit(devices::run(json)?),
        Command::Check(args) => {
            let code = match source_check::run(args) {
                Ok(code) => code,
                Err(error) => {
                    errln!("{}", fmt::render_error(&error));
                    1
                }
            };
            std::process::exit(code);
        }
        Command::Decode {
            path,
            center_freq_hz,
            iq_format,
            sample_rate_hz,
            json,
            timing,
            observations,
            observation_text,
            filters,
            config,
            engine,
            prior,
        } => {
            let FilterOpts {
                freq_correction_ppm,
                allowlist,
                blocklist,
                notch,
                cty,
                scp,
                cty_sha256,
                scp_sha256,
                patterns,
            } = filters;
            // D7/D8: the whole file is validated, the environment is never
            // read; [decode], [detector], [spot] and input.freq_correction_ppm
            // apply, with the CLI over the file.
            let loaded = config::load(config.as_deref(), config::Env::Ignore)?;
            note_ignored_tables(&loaded, "decode", &["decode", "detector", "spot", "input"]);
            // MAN-13: a recording came from one receiver, and decode cannot
            // tell which entry that was: entries that disagree on the
            // correction need --freq-correction-ppm, never the first one's.
            let several = loaded.inputs.len() >= 2;
            let ppm_of = |n: &config::NamedInput| n.input.shared.freq_correction_ppm.unwrap_or(0.0);
            if several
                && freq_correction_ppm.is_none()
                && loaded
                    .inputs
                    .iter()
                    .any(|n| ppm_of(n) != ppm_of(&loaded.inputs[0]))
            {
                let names: Vec<&str> = loaded.inputs.iter().map(|n| n.name.as_str()).collect();
                bail!(
                    "manta decode cannot tell which of {}'s {} inputs ({}) recorded {}: pass \
                     --freq-correction-ppm with that receiver's correction (0 for none)",
                    loaded.origin,
                    names.len(),
                    names.join(", "),
                    path.display()
                );
            }
            if several
                && loaded
                    .inputs
                    .iter()
                    .any(|n| n.input.shared.cw_policy != loaded.inputs[0].input.shared.cw_policy)
            {
                bail!("manta decode cannot choose between inputs with different CW coverage in {}: use a config containing only the recording's input",loaded.origin);
            }
            // MAN-496: the conditioning keys follow the ppm rule: a
            // recording came from one receiver, so entries that disagree
            // are ambiguous, never silently the first one's.
            let cond_of = |n: &config::NamedInput| {
                let c = n.input.shared.conditioning();
                (c.dc, c.impulse)
            };
            let conditioning = loaded.inputs[0].input.shared.conditioning();
            if several
                && loaded
                    .inputs
                    .iter()
                    .any(|n| cond_of(n) != cond_of(&loaded.inputs[0]))
            {
                let names: Vec<&str> = loaded.inputs.iter().map(|n| n.name.as_str()).collect();
                bail!(
                    "manta decode cannot tell which of {}'s {} inputs ({}) recorded {}: set \
                     dc_block and impulse_suppress alike in every [[input]] entry",
                    loaded.origin,
                    names.len(),
                    names.join(", "),
                    path.display()
                );
            }
            let shared = &loaded.input.shared;
            if several
                && loaded
                    .inputs
                    .iter()
                    .any(|n| n.input.shared.conditioning != shared.conditioning)
            {
                bail!(
                    "manta decode cannot tell which input recorded {}: the [[input]] entries have conflicting IQ conditioning; supply a config for the recording's receiver",
                    path.display()
                );
            }
            if several {
                errln!(
                    "note: decode applies freq_correction_ppm and dc_block, impulse_suppress and IQ \
                     conditioning{} from the {} [[input]] entries in {}",
                    if shared.cw_policy == Default::default() {
                        ""
                    } else {
                        " and CW coverage"
                    },
                    loaded.inputs.len(),
                    loaded.origin
                );
            } else if loaded.input.source.is_some()
                || shared.center_freq_hz.is_some()
                || shared.capture_rate_hz.is_some()
                || shared.replay_epoch.is_some()
                || shared.dc_block.is_some()
                || shared.impulse_suppress.is_some()
            {
                errln!(
                    "note: decode applies input.freq_correction_ppm and dc_block, impulse_suppress \
                     and IQ conditioning{} from [input] in {}",
                    if shared.cw_policy == Default::default() {
                        ""
                    } else {
                        " and CW coverage"
                    },
                    loaded.origin
                );
            }
            let spot = resolve_spot_pinned(
                allowlist,
                blocklist,
                notch,
                cty,
                scp,
                CliPins {
                    cty_sha256,
                    scp_sha256,
                },
                patterns,
                &loaded.spot,
            );
            let decode_cfg = merge_cli_prior(
                prior,
                merge_cli_engine(engine, loaded.decode.clone()),
                &loaded.origin,
            )?;
            let (mut cfg, lists_in_use) = build_pipeline_config(
                freq_correction_ppm
                    .or(loaded.input.shared.freq_correction_ppm)
                    .unwrap_or(0.0),
                &spot,
                loaded.detector,
                decode_cfg.engine,
            )?;
            note_pinned_lists(&spot, &lists_in_use);
            cfg.conditioning = conditioning;
            cfg.cw_policy = loaded.input.shared.cw_policy.clone();
            (cfg.cw_epoch, cfg.cw_epoch_nanoseconds) = cw_replay_epoch(
                &path,
                loaded.input.shared.replay_epoch,
                cfg.cw_policy.scheduled(),
                loaded.inputs.first().map_or("file", |i| i.name.as_str()),
            )?;
            cfg.decode = decode_cfg;
            attach_language_prior(&mut cfg, &spot, None)?;
            // MAN-207: the observation file is created before decoding, so an
            // existing path fails fast and is never truncated.
            let policy = match (&observations, observation_text) {
                (None, _) => manta_engine::ObservationPolicy::summary(),
                (Some(_), false) => manta_engine::ObservationPolicy::records(),
                (Some(_), true) => manta_engine::ObservationPolicy::text(),
            };
            let observation_file = match &observations {
                Some(obs_path) => Some(create_observation_file(obs_path)?),
                None => None,
            };
            let decoded = decode_for_cli(
                &path,
                center_freq_hz,
                iq_format,
                sample_rate_hz,
                &cfg,
                policy,
            );
            let (report, observed) = match decoded {
                Ok(decoded) => decoded,
                Err(e) => {
                    if let Some(obs_path) = &observations {
                        // Best effort: the empty file must not look like a record.
                        let _ = std::fs::remove_file(obs_path);
                    }
                    return Err(e);
                }
            };
            // Written before any output, so a failed stdout write cannot
            // leave the file empty.
            let written = match (&observations, observation_file) {
                (Some(obs_path), Some(file)) => {
                    match observations::write(file, policy.detail, report.sample_rate_hz, &observed)
                    {
                        Ok(n) => Some((obs_path, n)),
                        Err(e) => {
                            // Best effort: a file cut short has no summary
                            // trailer and must not look like a record.
                            let _ = std::fs::remove_file(obs_path);
                            return Err(e).with_context(|| {
                                format!("write --observations {}", obs_path.display())
                            });
                        }
                    }
                }
                _ => None,
            };
            if json {
                outln!("{}", serde_json::to_string(&report)?)?;
            } else {
                outln!("{}", report.text)?;
                errln!("{}", fmt::decode_summary(&report));
                // MAN-314: only when RTTY was marked, so a CW-only decode
                // keeps MAN-270's one-line summary.
                if !report.rtty.is_empty() {
                    errln!("{}", rtty_line(&report.rtty, report.sample_rate_hz));
                }
                for event in &report.events {
                    if let manta_decode::events::DecoderEvent::ReadingsHeld { readings, .. } = event
                    {
                        errln!("{}", text_lines::readings_line(readings));
                    }
                }
                // MAN-315: the primary track's learned sender timing, on
                // request only, so the default stderr stays the summary.
                if timing {
                    if let Some(estimates) = &report.timing {
                        errln!("timing (dits, mean ± sd): {}", estimates.display_line());
                    }
                }
            }
            // MAN-234: after the output, in both modes, so the decoded text
            // stays first and `--json` stdout stays one record.
            if report.bad_samples > 0 {
                logging::warning(&fmt::bad_samples_warning(
                    &path.display().to_string(),
                    report.bad_samples,
                ));
            }
            if let Some((obs_path, n)) = written {
                errln!(
                    "observations: {n} record{} written to {}",
                    if n == 1 { "" } else { "s" },
                    obs_path.display()
                );
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
            prior,
            jsonl,
            funnel,
            funnel_bypass,
            funnel_track_tol_hz,
            funnel_spot_tol_hz,
            funnel_spot_window_s,
        } => {
            let loaded = config::load(config.as_deref(), config::Env::Ignore)?;
            note_ignored_tables(&loaded, "oracle", &["decode", "spot"]);
            // MAN-309: a manta IQ recording opens through the same reader as
            // `decode`; the oracle reads one continuous capture, so a
            // recording with gaps is refused rather than spliced.
            if manta_input::is_native_recording(&path)
                && manta_input::read_recording_summary(&path)?.has_gaps()
            {
                bail!(
                    "IQ recording {} has input gaps; manta oracle needs one continuous capture",
                    path.display()
                );
            }
            let mut src = manta_input::open_iq_file(&path, manta_input::IqFileOptions::default())?;
            let (fs, center) = (src.sample_rate(), src.center_freq_hz());
            let iq = manta_input::read_all(src.as_mut())?;
            let spots = manta_testkit::oracle::parse_rbn_spots(&rbn_csv, &spotter, capture_start)?;
            let decode = merge_cli_prior(
                prior,
                merge_cli_engine(engine, loaded.decode.clone()),
                &loaded.origin,
            )?;
            let spot = resolve_spot_pinned(
                vec![],
                None,
                None,
                None,
                None,
                CliPins::default(),
                None,
                &loaded.spot,
            );
            let (mut pipeline, _) =
                build_pipeline_config(0.0, &spot, loaded.detector, decode.engine)?;
            pipeline.decode = decode;
            attach_language_prior(&mut pipeline, &spot, None)?;
            let cfg = pipeline.decode.clone();
            let (results, summary) =
                manta_testkit::oracle::run_oracle(&iq, fs, center, &spots, window_s, &cfg)?;
            if let Some(p) = jsonl {
                let mut w = std::io::BufWriter::new(std::fs::File::create(p)?);
                for r in &results {
                    use std::io::Write;
                    writeln!(w, "{}", serde_json::to_string(r)?)?;
                }
            }
            outln!("{}", serde_json::to_string_pretty(&summary)?)?;
            // MAN-198: after stdout, so the oracle's own output is unchanged.
            if let Some(out) = funnel {
                let args = funnel_cmd::FunnelArgs {
                    recording: &path,
                    rbn_csv: &rbn_csv,
                    spotter: &spotter,
                    capture_start_unix_s: capture_start,
                    config: config.as_deref(),
                    out: &out,
                    bypasses: &funnel_bypass,
                    iq: &iq,
                    tol: manta_engine::funnel::Tolerances {
                        track_tol_hz: funnel_track_tol_hz,
                        spot_tol_hz: funnel_spot_tol_hz,
                        window_s,
                        spot_window_s: funnel_spot_window_s,
                        ..Default::default()
                    },
                };
                funnel_cmd::run(&args, &pipeline, iq.len() as f64 / fs)?;
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
                "vr1" => manta_testkit::vectors::vr1(),
                "vr2" => manta_testkit::vectors::vr2(),
                "vr3" => manta_testkit::vectors::vr3(),
                "vr4" => manta_testkit::vectors::vr4(),
                "vr5" => manta_testkit::vectors::vr5(),
                "vr6a" => manta_testkit::vectors::vr6a(),
                "vr6b" => manta_testkit::vectors::vr6b(),
                "vr7" => manta_testkit::vectors::vr7(),
                "vr8" => manta_testkit::vectors::vr8(),
                "vw1" => manta_testkit::vectors::vw1(),
                "vw2" => manta_testkit::vectors::vw2(),
                "vw3" => manta_testkit::vectors::vw3(),
                "vw4" => manta_testkit::vectors::vw4(),
                "vw5" => manta_testkit::vectors::vw5(),
                other => bail!(
                    "unknown vector '{}' (available: v1-v6, vr1-vr5, vr6a, vr6b, vr7-vr8, vw1-vw5)",
                    manta_server::status_doc::escape_for_terminal(other).replace('\'', "\\'")
                ),
            };
            std::fs::create_dir_all(&out)?;
            let manifest = manta_testkit::vectors::write_fixture_set(&spec, &out)?;
            errln!(
                "wrote {}/{{{}.wav,{}.json,{}.manifest.json}} (expected frequency {})",
                out.display(),
                spec.name,
                spec.name,
                spec.name,
                manta_server::human::khz(manifest.expected_freq_hz)
            );
        }
        Command::Run(args) => return run_cmd::run(args),
        Command::Soak {
            input,
            duration,
            device,
            source,
            kiwi_host,
            kiwi_port,
            kiwi_freq_hz,
            kiwi_password,
            soapy,
            hpsdr_host,
            hpsdr_mac,
            hpsdr_port,
            hpsdr_protocol,
            hpsdr_freq_hz,
            hpsdr_rate_hz,
            ka9q_group,
            ka9q_port,
            ka9q_ssrc,
            ka9q_interface,
            capture_rate_hz,
            source_iq,
            filters,
            dial_freq_hz,
            config,
        } => {
            let FilterOpts {
                freq_correction_ppm,
                allowlist,
                blocklist,
                notch,
                cty,
                scp,
                cty_sha256,
                scp_sha256,
                patterns,
            } = filters;
            let Prepared {
                loaded,
                resolved,
                pipeline: cfg,
                lists_in_use,
                ..
            } = prepare_live(
                CliOverrides {
                    prior: None,
                    device,
                    source,
                    source_iq,
                    realtime: false,
                    loop_replay: false,
                    kiwi: KiwiOpts {
                        host: kiwi_host,
                        port: kiwi_port,
                        freq: kiwi_freq_hz,
                        password: kiwi_password,
                    },
                    soapy: soapy.into(),
                    hpsdr: HpsdrOpts {
                        host: hpsdr_host,
                        mac: hpsdr_mac,
                        port: hpsdr_port,
                        protocol: hpsdr_protocol.into(),
                        freq: hpsdr_freq_hz,
                        rate: hpsdr_rate_hz,
                        auto_attenuation: None,
                    },
                    ka9q: Ka9qOpts {
                        group: ka9q_group,
                        port: ka9q_port,
                        ssrc: ka9q_ssrc,
                        interface: ka9q_interface,
                        ..Ka9qOpts::unset()
                    },
                    freq_correction_ppm,
                    dial_freq_hz,
                    capture_rate_hz,
                    replay_epoch: None,
                    allowlist,
                    blocklist,
                    notch,
                    cty,
                    scp,
                    cty_sha256,
                    scp_sha256,
                    patterns,
                    rbn: RbnCompareOpts::default(),
                },
                config,
                None,
                true,
                Some(OneInput {
                    command: "soak",
                    name: input.as_deref(),
                }),
            )?;
            let mut cfg = cfg;
            cfg.conditioning = if resolved.replaced_file_source.is_some() {
                Default::default()
            } else {
                loaded.input.shared.conditioning()
            };
            if loaded.server.is_some() {
                errln!(
                    "note: soak does not start the spot servers; ignoring [server] from {}",
                    loaded.origin
                );
            }
            if let Some(note) = replay_mode_ignored_note("soak", resolved.replay, &loaded.origin) {
                errln!("{note}");
            }
            if let Some(note) = spectrum_feed::ignored_note("soak", &loaded) {
                errln!("{note}");
            }
            if let Some(note) = record_iq_ignored_note("soak", &loaded) {
                errln!("{note}");
            }
            note_pinned_lists(&resolved.spot, &lists_in_use);
            // MAN-316: soak never refreshes; for a file it says so, as `run` does.
            if resolved.spot.refresh_lists && !resolved.spec.is_reconnectable() {
                logging::note(&list_refresh::file_replay_note(
                    &lists_in_use[0],
                    &lists_in_use[1],
                ));
            }
            let spec = &resolved.spec;
            warn_if_audio_source_has_no_rf_reference(spec.is_rf_aware(), resolved.dial_freq_hz);
            let src: Box<dyn IqSource> =
                spec.open(resolved.capture_rate_hz, resolved.dial_freq_hz)?;
            warn_iq_mismatch(
                &cfg.conditioning,
                src.center_freq_hz(),
                Some(
                    loaded
                        .inputs
                        .first()
                        .map_or(spec.name(), |input| input.name.as_str()),
                ),
                &std::sync::atomic::AtomicBool::new(false),
            );
            // MAN-196: the same usable-passband line `run` logs, once,
            // before the soak starts; `Some` only with a decimator in the
            // path. Plain stderr: `soak` installs no tracing subscriber.
            // The values are the intersected `rf_passband_hz()`, exactly
            // as `run`'s notice above (round-1 review, PR #300).
            if let Some((lo, hi)) = src.trusted_band_hz().map(|_| src.rf_passband_hz()) {
                errln!("{}", trusted_band_notice(src.sample_rate(), lo, hi));
            }
            let report = manta_engine::soak(src, &cfg, std::time::Duration::from_secs(duration))?;
            out!("{}", fmt::soak_report(&report))?;
            if !manta_engine::soak_passed(&report) {
                return Ok(std::process::ExitCode::FAILURE);
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
            let reply = match run_status(config.as_deref(), addr.as_deref(), timeout_secs) {
                Ok(reply) => reply,
                Err(e) => {
                    errln!("{}", status_failure_message(&e));
                    return Ok(std::process::ExitCode::from(2));
                }
            };
            if json {
                out!("{}", manta_server::status_doc::value_to_json(&reply.raw))?;
            } else {
                out!("{}", manta_server::status_doc::render_human(&reply.doc))?;
            }
            return Ok(std::process::ExitCode::from(status_exit_code(&reply.doc)));
        }
        Command::Health {
            config,
            addr,
            timeout_secs,
        } => {
            std::process::exit(health_cmd::run(
                config.as_deref(),
                addr.as_deref(),
                timeout_secs,
            ));
        }
        Command::Doctor {
            input,
            duration,
            device,
            source,
            kiwi_host,
            kiwi_port,
            kiwi_freq_hz,
            kiwi_password,
            filters,
            soapy,
            hpsdr_host,
            hpsdr_mac,
            hpsdr_port,
            hpsdr_protocol,
            hpsdr_freq_hz,
            hpsdr_rate_hz,
            ka9q_group,
            ka9q_port,
            ka9q_ssrc,
            ka9q_interface,
            capture_rate_hz,
            source_iq,
            dial_freq_hz,
            config,
            ntp_server,
            json,
            strict,
        } => {
            let FilterOpts {
                freq_correction_ppm,
                allowlist,
                blocklist,
                notch,
                cty,
                scp,
                cty_sha256,
                scp_sha256,
                patterns,
            } = filters;
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
            let Prepared {
                config_path,
                loaded,
                resolved,
                pipeline: cfg,
                lists_in_use,
            } = prepare_live(
                CliOverrides {
                    prior: None,
                    device,
                    source,
                    source_iq,
                    realtime: false,
                    loop_replay: false,
                    kiwi: KiwiOpts {
                        host: kiwi_host,
                        port: kiwi_port,
                        freq: kiwi_freq_hz,
                        password: kiwi_password,
                    },
                    soapy: soapy.into(),
                    hpsdr: HpsdrOpts {
                        host: hpsdr_host,
                        mac: hpsdr_mac,
                        port: hpsdr_port,
                        protocol: hpsdr_protocol.into(),
                        freq: hpsdr_freq_hz,
                        rate: hpsdr_rate_hz,
                        auto_attenuation: None,
                    },
                    ka9q: Ka9qOpts {
                        group: ka9q_group,
                        port: ka9q_port,
                        ssrc: ka9q_ssrc,
                        interface: ka9q_interface,
                        ..Ka9qOpts::unset()
                    },
                    freq_correction_ppm,
                    dial_freq_hz,
                    capture_rate_hz,
                    replay_epoch: None,
                    allowlist,
                    blocklist,
                    notch,
                    cty,
                    scp,
                    cty_sha256,
                    scp_sha256,
                    patterns,
                    rbn: RbnCompareOpts::default(),
                },
                config,
                None,
                true,
                Some(OneInput {
                    command: "doctor",
                    name: input.as_deref(),
                }),
            )?;
            note_pinned_lists(&resolved.spot, &lists_in_use);
            if let Some(note) = replay_mode_ignored_note("doctor", resolved.replay, &loaded.origin)
            {
                errln!("{note}");
            }
            if let Some(note) = spectrum_feed::ignored_note("doctor", &loaded) {
                errln!("{note}");
            }
            if let Some(note) = record_iq_ignored_note("doctor", &loaded) {
                errln!("{note}");
            }
            // MAN-126: named setup checks first, each printed as it is
            // known; the receiver and signal stages become check lines too,
            // so a failure is named rather than ending in a generic error.
            let spec = &resolved.spec;
            let rf_aware = spec.is_rf_aware();
            let receiver = receiver_desc(spec);
            let mut sink = doctor_checks::Sink::new(json);
            doctor_checks::run_setup_checks(
                &doctor_checks::SetupInputs {
                    config_path: config_path.as_deref(),
                    loaded: &loaded,
                    needs_dial: needs_dial_freq(
                        loaded.server.is_some(),
                        rf_aware,
                        resolved.dial_freq_hz,
                    ),
                    receiver: &receiver,
                    ntp_server: &ntp_server,
                },
                &mut sink,
            );
            // MAN-232: a failed setup-check write stops doctor here, so
            // `doctor | true` does not open the receiver and run the signal
            // check for a reader that left.
            sink.written()?;
            warn_if_audio_source_has_no_rf_reference(rf_aware, resolved.dial_freq_hz);
            let report = match spec.open(resolved.capture_rate_hz, resolved.dial_freq_hz) {
                Err(e) => {
                    sink.push(doctor_checks::receiver_check(&receiver, Err(&e)));
                    None
                }
                Ok(src) => {
                    sink.push(doctor_checks::receiver_check(
                        &receiver,
                        Ok(src.sample_rate()),
                    ));
                    // MAN-232: a failed receiver-line write stops doctor
                    // before the signal check runs for its full duration
                    // for a reader that already left.
                    sink.written()?;
                    match manta_engine::doctor(src, &cfg, std::time::Duration::from_secs(duration))
                    {
                        Ok(report) => {
                            if !json {
                                outln!()?;
                                out!("{}", fmt::doctor_report(&report))?;
                                let requested = std::time::Duration::from_secs(duration);
                                if let Some(hint) = fmt::doctor_rerun_hint(&report, requested) {
                                    out!("{hint}")?;
                                }
                            }
                            Some(report)
                        }
                        Err(e) => {
                            sink.push(doctor_checks::signal_check(&e));
                            None
                        }
                    }
                }
            };
            // MAN-232: again after the receiver/signal stage, for the same
            // reason as the first `written()` call.
            sink.written()?;
            let checks = sink.checks;
            if json {
                // `verdict()` is computed, not a stored field, so a plain
                // `serde_json::to_string(&report)` omits the command's
                // primary health classification entirely -- merge it in as
                // an extra key rather than making JSON consumers duplicate
                // the classification policy themselves. A struct, not a
                // `Map`, so the keys keep declaration order (MAN-139).
                #[derive(serde::Serialize)]
                #[serde(rename_all = "camelCase")]
                struct DoctorJson<'a> {
                    #[serde(flatten)]
                    report: Option<&'a manta_engine::DoctorReport>,
                    #[serde(skip_serializing_if = "Option::is_none")]
                    verdict: Option<manta_engine::Verdict>,
                    checks: &'a [doctor_checks::Check],
                    checks_status: doctor_checks::Status,
                }
                let doctor_json = DoctorJson {
                    report: report.as_ref(),
                    verdict: report.as_ref().map(|r| r.verdict()),
                    checks: &checks,
                    checks_status: doctor_checks::worst(&checks),
                };
                outln!("{}", serde_json::to_string(&doctor_json)?)?;
            } else {
                outln!()?;
                outln!("{}", doctor_checks::summary(&checks))?;
            }
            // As `manta status` does: the specific lines above are the
            // whole story, so no generic `Error:` line follows them (every
            // write above flushed, so no trailing flush is needed either).
            if doctor_checks::worst(&checks) == doctor_checks::Status::Fail {
                return Ok(std::process::ExitCode::FAILURE);
            }
            // A run with no report always pushed a FAIL row, so it has
            // already returned 1 above.
            if let (true, Some(report)) = (strict, &report) {
                return Ok(std::process::ExitCode::from(strict_exit_code(
                    report.verdict(),
                )));
            }
        }
        Command::Calibrate {
            input,
            duration,
            reference_hz,
            search_ppm,
            tune_hz,
            device,
            source,
            kiwi_host,
            kiwi_port,
            kiwi_freq_hz,
            kiwi_password,
            soapy,
            hpsdr_host,
            hpsdr_mac,
            hpsdr_port,
            hpsdr_protocol,
            hpsdr_freq_hz,
            hpsdr_rate_hz,
            ka9q_group,
            ka9q_port,
            ka9q_ssrc,
            ka9q_interface,
            capture_rate_hz,
            source_iq,
            dial_freq_hz,
            config,
            write,
            json,
        } => calibrate_cmd::run(calibrate_cmd::CalibrateArgs {
            input,
            duration_secs: duration,
            reference_hz,
            search_ppm,
            tune_hz,
            write,
            json,
            config,
            cli: CliOverrides {
                prior: None,
                device,
                source,
                source_iq,
                kiwi: KiwiOpts {
                    host: kiwi_host,
                    port: kiwi_port,
                    freq: kiwi_freq_hz,
                    password: kiwi_password,
                },
                soapy: soapy.into(),
                hpsdr: HpsdrOpts {
                    host: hpsdr_host,
                    mac: hpsdr_mac,
                    port: hpsdr_port,
                    protocol: hpsdr_protocol.into(),
                    freq: hpsdr_freq_hz,
                    rate: hpsdr_rate_hz,
                    auto_attenuation: None,
                },
                ka9q: Ka9qOpts {
                    group: ka9q_group,
                    port: ka9q_port,
                    ssrc: ka9q_ssrc,
                    interface: ka9q_interface,
                    ..Ka9qOpts::unset()
                },
                capture_rate_hz,
                dial_freq_hz,
                ..CliOverrides::none()
            },
        })?,
        Command::Config(ConfigCommand::Check { config }) => config_cmd::check(config)?,
        Command::Config(ConfigCommand::Init { out, force }) => config_cmd::init(&out, force)?,
        Command::Bench(BenchCommand::Sensitivity(args)) => bench::sensitivity(args)?,
        Command::Grade { args } => grade_cmd::run(args)?,
        Command::Completions { shell } => {
            cli_docs::write_completions(shell, &mut Cli::command(), &mut std::io::stdout().lock())?
        }
        Command::ManPages { out } => {
            let n = cli_docs::write_man_pages(Cli::command(), &out)?;
            errln!("wrote {n} man pages to {}", out.display());
        }
    }
    Ok(std::process::ExitCode::SUCCESS)
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

/// stderr, not `tracing::warn!`: this runs before `Cli::parse()`, and the
/// only `tracing_subscriber` init in this binary (`logging::init`, MAN-124)
/// runs after it at the top of `run`, so a parse-time `warn!` would be
/// dropped. stderr is also the stream `--json`'s JSON Lines consumer never
/// reads (see `logging`'s MAN-59 round-6 note), so this cannot
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
            Deprecation::ListenVerb => errln!(
                "warning: starting the daemon with `manta listen` is deprecated and will be \
                 removed in a future release; use `manta run --config` instead."
            ),
            Deprecation::ServerConfigFlag => errln!(
                "warning: `--server-config` is deprecated and will be removed in a future \
                 release; use `--config` instead."
            ),
        }
    }
}

/// MAN-314: `decode`'s RTTY summary line, `rtty: 0` without RTTY;
/// `decode` prints it only when RTTY was marked.
fn rtty_line(signals: &[manta_engine::RttySignalReport], fs: f64) -> String {
    if signals.is_empty() {
        return "rtty: 0".into();
    }
    let parts: Vec<String> = signals
        .iter()
        .map(|s| {
            format!(
                "{:.3} kHz shift {} Hz from {:.1} s",
                s.mark_hz / 1000.0,
                s.shift_hz,
                s.marked_sample_ts as f64 / fs
            )
        })
        .collect();
    format!("rtty: {} ({})", signals.len(), parts.join("; "))
}

/// MAN-314: `run`'s log line when the detector marks an RTTY signal.
fn format_rtty_marked(s: &manta_engine::RttySignalReport) -> String {
    format!(
        "RTTY marked: {:.3} kHz shift {} Hz (measured {:.1} Hz); no CW tracks there while it lasts",
        s.mark_hz / 1000.0,
        s.shift_hz,
        s.measured_shift_hz
    )
}

/// MAN-314: `run`'s log line when a marked RTTY signal clears.
fn format_rtty_cleared(s: &manta_engine::RttySignalReport, marked_for_secs: u64) -> String {
    format!(
        "RTTY cleared: {:.3} kHz shift {} Hz, marked for {marked_for_secs} s",
        s.mark_hz / 1000.0,
        s.shift_hz
    )
}

/// MAN-314: whole seconds of stream time from `s`'s mark to
/// `cleared_sample_ts`, at `fs` samples per second. Stream time, not wall
/// time: a file replay without `--realtime` runs far faster than real time
/// (Codex review on PR #272).
fn rtty_marked_for_secs(
    s: &manta_engine::RttySignalReport,
    cleared_sample_ts: u64,
    fs: f64,
) -> u64 {
    (cleared_sample_ts.saturating_sub(s.marked_sample_ts) as f64 / fs) as u64
}

/// MAN-314: what `run` last saw of the engine's marked RTTY set, so a
/// change logs one line per newly marked or cleared signal.
struct RttyLogState {
    /// The source's sample rate, for the engine's sample timestamps.
    sample_rate_hz: f64,
    generation: u64,
    current: std::collections::BTreeMap<u32, manta_engine::RttySignalReport>,
}

impl RttyLogState {
    fn new(sample_rate_hz: f64) -> Self {
        Self {
            sample_rate_hz,
            generation: 0,
            current: std::collections::BTreeMap::new(),
        }
    }

    /// Logs the marks and clears since the last call. Returns the current
    /// mark frequencies when the set changed, `None` otherwise.
    fn poll(&mut self, obs: &manta_engine::RttyObserver) -> Option<Vec<f64>> {
        let generation = obs.generation();
        if generation == self.generation {
            return None;
        }
        self.generation = generation;
        let (snapshot, sample_ts) = obs.snapshot_with_sample_ts();
        let ids: Vec<u32> = snapshot.iter().map(|s| s.id).collect();
        let fs = self.sample_rate_hz;
        self.current.retain(|id, s| {
            let keep = ids.contains(id);
            if !keep {
                let secs = rtty_marked_for_secs(s, sample_ts, fs);
                tracing::info!("{}", format_rtty_cleared(s, secs));
            }
            keep
        });
        for s in &snapshot {
            match self.current.entry(s.id) {
                std::collections::btree_map::Entry::Vacant(slot) => {
                    tracing::info!("{}", format_rtty_marked(s));
                    slot.insert(s.clone());
                }
                // Still marked: keep its clear line on the current
                // frequency and shift.
                std::collections::btree_map::Entry::Occupied(mut slot) => {
                    *slot.get_mut() = s.clone();
                }
            }
        }
        Some(snapshot.iter().map(|s| s.mark_hz).collect())
    }
}

#[cfg(test)]
mod tests;
