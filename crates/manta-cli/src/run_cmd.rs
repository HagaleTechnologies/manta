//! `manta run` (alias `listen`): the `RunArgs` flags and `run`, the live and
//! replay daemon wiring that used to be `run_command`'s `Command::Run` arm.
//! MAN-548 moved it out of `main.rs`.

use crate::attach_language_prior;
use crate::cli_parse::parse_capture_rate_hz;
use crate::cli_parse::parse_dial_freq_hz;
use crate::cli_parse::parse_engine;
use crate::cli_parse::parse_hpsdr_freq_hz;
use crate::cli_parse::parse_hpsdr_mac;
use crate::cli_parse::parse_hpsdr_rate_hz;
use crate::cli_parse::parse_iq_sample_rate_hz;
use crate::cli_parse::parse_ka9q_group;
use crate::cli_parse::parse_prior;
use crate::cli_parse::parse_replay_epoch;
use crate::clock_check;
use crate::config;
use crate::config_cmd;
use crate::decode_quality_of;
use crate::fmt;
use crate::ftx_run;

use crate::latency_histogram_of;
use crate::live_state;
use crate::logging;
use crate::multi_run;
use crate::prepare_live;
use crate::prepare_run_lists;
use crate::reconnect;
use crate::reconnect::ReconnectingSource;
use crate::record_terminal_source_health;
use crate::recording::check_record_destination;
use crate::recording::check_record_destinations;
use crate::recording::close_recordings;
use crate::recording::finish_recordings;
use crate::recording::start_input_recording;
use crate::refuse_rbn_compare_replay;
use crate::reload;
use crate::resolve::needs_dial_freq;
use crate::resolve::report_iq_open_error;
use crate::resolve::wrap_file_replay;
use crate::resolve::CliOverrides;
use crate::session_epoch::resolve_epoch;
use crate::session_epoch::resolve_ftx_epoch;
use crate::session_epoch::session_nonce_for_replay_path;
use crate::shutdown_runtime_after_drain;
use crate::signal_spot_server_shutdown;
use crate::source_health_sink;
use crate::source_loss_sink;
use crate::source_spec::apply_raw_iq;
use crate::source_spec::trusted_band_notice;
use crate::source_spec::warn_if_audio_source_has_no_rf_reference;
use crate::source_spec::HpsdrOpts;
use crate::source_spec::HpsdrProtocolArg;
use crate::source_spec::Ka9qOpts;
use crate::source_spec::KiwiOpts;
use crate::source_spec::LiveSourceSpec;
use crate::spawn_attenuation_poller;
use crate::spectrum_feed;
use crate::spot_outputs;
use crate::spot_server::start_spot_server;
use crate::spot_server::RelayConfigs;
use crate::spot_server::SourceInfo;
use crate::spot_server::SpotServerParams;
use crate::stale_cty_warning;
use crate::start_callsign_lookup;
use crate::start_list_refresh;
use crate::stdio;
use crate::stdio::errln;
use crate::text_lines;
use crate::warn_iq_mismatch;
use crate::FilterOpts;
use crate::Prepared;
use crate::RbnCompareOpts;
use crate::RttyLogState;
use crate::ACTIVE_TRACKS_POLL_INTERVAL;
use crate::DECODE_LATENCY_POLL_INTERVAL;
use crate::INPUT_HEALTH_POLL_INTERVAL;
use anyhow::{anyhow, bail, Result};
use manta_decode::decoder::Engine;
use manta_input::IqSource;
use std::path::PathBuf;

// MAN-89 (PR #131 review, round 7): the "would `from_spot` emit the
// `UNKNOWN_*` sentinels?" predicate that
// `manta_spots_unresolved_geography_total` is defined over used to be
// defined HERE, one crate away from the `SpotMessage::from_spot` it has to
// agree with -- and it drifted: the station side was classified from the raw
// configured `station_callsign` while `from_spot` resolved the SSID-stripped
// one. It now lives beside that code in `manta-server::spot_message`, with
// the station-side entry point doing the stripping itself so no call site
// can forget it.
use manta_server::spot_message::{geography_is_unresolved, station_geography_unresolved};

// The flags of `manta run`; the `Command::Run` variant's doc comment is
// the command's help text, so this struct carries none (MAN-548).
#[derive(clap::Args)]
pub(crate) struct RunArgs {
    /// Sound-card input device; matched by substring. Defaults to the
    /// system default input.
    #[arg(long, conflicts_with = "source", help_heading = "Audio input")]
    device: Option<String>,
    /// Replay a WAV file instead of listening to a live device.
    ///
    /// Decoded as fast as the machine manages, so a 60-second file usually
    /// finishes sooner than that; add --realtime to replay it at its own
    /// pace, or --loop to keep it playing. Useful for demos and repeatable
    /// testing.
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
    pub(crate) ka9q_group: Option<String>,
    /// RTP data port of the group.
    #[arg(
            long,
            default_value_t = manta_input::ka9q::wire::DATA_PORT,
            value_parser = clap::value_parser!(u16).range(1..),
            requires = "ka9q_group",
            help_heading = "ka9q-radio source"
        )]
    pub(crate) ka9q_port: u16,
    /// SSRC of the channel to read, when the group carries more than one.
    #[arg(long, requires = "ka9q_group", help_heading = "ka9q-radio source")]
    pub(crate) ka9q_ssrc: Option<u32>,
    /// IPv4 address of the local interface to join the group on. manta
    /// also joins on 127.0.0.1, for a radiod on this machine.
    #[arg(
            long,
            requires = "ka9q_group",
            value_parser = clap::value_parser!(std::net::Ipv4Addr),
            help_heading = "ka9q-radio source"
        )]
    pub(crate) ka9q_interface: Option<std::net::Ipv4Addr>,
    /// Decimate the source to this sample rate, in Hz, before decoding.
    ///
    /// Must divide the source's own rate by a power of two, and the
    /// result must itself be a rate the channelizer supports. Omit to
    /// decode at the source's native rate.
    #[arg(long, value_parser = parse_capture_rate_hz)]
    capture_rate_hz: Option<f64>,
    /// Read --source as complex IQ rather than ordinary mono audio.
    ///
    /// IQ is a two-channel IQ WAV, a manta IQ recording (found by its
    /// PATH.meta.jsonl), or headerless IQ described by --iq-format,
    /// --sample-rate-hz and --dial-freq-hz. Without this, --source is
    /// taken as mono real audio at 48000 Hz. A stereo audio recording
    /// and a two-channel IQ capture cannot be told apart from the WAV
    /// header alone, so say which you have.
    #[arg(long)]
    source_iq: bool,
    /// Read headerless little-endian IQ: cf32 or ci16.
    ///
    /// Requires --source-iq, a sample rate and centre frequency
    /// (--sample-rate-hz, --dial-freq-hz). Omit for WAV or a manta
    /// recording with metadata.
    #[arg(long, value_name = "FORMAT", requires = "source_iq")]
    iq_format: Option<manta_input::RawIqFormat>,
    /// Sample rate of headerless IQ before any capture-rate decimation.
    #[arg(long, value_name = "HZ", value_parser = parse_iq_sample_rate_hz, requires = "iq_format")]
    sample_rate_hz: Option<f64>,
    /// Record the input IQ while decoding.
    ///
    /// Writes little-endian complex-float32 samples to PATH and its
    /// metadata to PATH.meta.jsonl. Existing files are never
    /// overwritten. Recording stops if storage cannot keep up; decoding
    /// continues.
    #[arg(long, value_name = "PATH")]
    record_iq: Option<PathBuf>,
    /// Replay the file at the recording's own pace instead of as fast as
    /// possible.
    ///
    /// A 120-second file takes 120 seconds, so telnet and JSON clients have
    /// time to connect and watch spots arrive. The decoded output is the same,
    /// byte for byte, as an unpaced replay; only its timing changes. Only for
    /// file replay: a live receiver already runs in real time. Also set by
    /// input.realtime in the config file.
    // No `requires = "source"`: the file source can come from the config
    // file, so the check runs after `resolve` (MAN-269 D4).
    #[arg(long)]
    realtime: bool,
    /// Start the file again from the beginning each time it ends, until
    /// stopped.
    ///
    /// Implies --realtime. Keeps a short recording playing for a demo; stop it
    /// with Ctrl-C or SIGTERM. The jump from the last sample back to the first
    /// is not smoothed, and a station already spotted is not spotted again for
    /// 10 minutes, so a looped recording yields a fresh spot about every 10
    /// minutes (type sh/dx on the telnet port to list earlier ones). Only for
    /// file replay. Also set by input.loop in the config file.
    #[arg(long = "loop")]
    loop_replay: bool,
    /// Print each decode as a JSON object, one per line, instead of
    /// plain text.
    #[arg(long, help_heading = "Output")]
    json: bool,
    /// Also print decoded text while the spotting daemon runs.
    ///
    /// Decoded text goes to stderr, one line per track, labelled with
    /// the track number, frequency and speed. It is printed by default,
    /// except when a `[server]` table starts the spotting daemon, so
    /// that a service log holds spots and diagnostics only. This flag
    /// prints it there too.
    #[arg(long, conflicts_with = "json", help_heading = "Output")]
    decoded_text: bool,
    /// TOML config file that turns this into the full spotting daemon.
    ///
    /// Needs a `[server]` table with the station callsign and ports.
    /// Starts the telnet cluster server, the JSON Lines / WebSocket
    /// stream, and the metrics endpoint alongside the decode loop.
    ///
    /// On Linux and macOS, SIGHUP makes the running daemon re-read this
    /// file and apply its [spot] lists and each [[rbn_uplink]] dry_run
    /// without a restart; other settings need a restart.
    // `--server-config` stays as a hidden alias for the flag's old
    // name; help advertises the canonical spelling only (D11/MAN-77).
    #[arg(long, alias = "server-config", help_heading = "Server")]
    config: Option<PathBuf>,
    /// Radio dial frequency, in Hz.
    ///
    /// For a sound card or an ordinary (real-audio) --source WAV this
    /// becomes the source's centre frequency; for KiwiSDR, SoapySDR, HPSDR
    /// and ka9q-radio inputs, and for a --source-iq recording whose
    /// sidecar already carries a centre frequency, it overrides what the
    /// source reports.
    /// Required with --config when the input is a sound card or a
    /// real-audio --source WAV, because neither knows what frequency the
    /// radio was on -- without it, spots would be published at the audio
    /// tone frequency (for example 700 Hz) instead of the real one. A
    /// --source-iq recording with a sidecar centre frequency does not need
    /// it. Outside --config it is optional, but omitting it for an audio
    /// source prints a warning and reported frequencies stay baseband
    /// offsets.
    ///
    /// Enter the suppressed-carrier (USB) dial reading: manta adds the
    /// decoded tone offset to this value as-is. On a rig in CW mode the
    /// displayed dial frequency is usually already offset by your sidetone
    /// pitch -- subtract it (for example 700-800 Hz) first, or spots read
    /// high by that amount (CW-R inverts the sign and doubles the error).
    #[arg(long, value_parser = parse_dial_freq_hz, help_heading = "Server")]
    dial_freq_hz: Option<f64>,
    /// Timestamp to treat as the start of a replayed file, in Unix
    /// seconds.
    ///
    /// Only meaningful with --source and --config; ignored for a live
    /// input. Without it, the file's modification time is used, which
    /// is stable for an untouched file but changes when the file is
    /// copied or downloaded. Set it explicitly when spot timestamps
    /// must be identical across machines.
    #[arg(long, value_parser = parse_replay_epoch, help_heading = "Server")]
    replay_epoch: Option<i64>,
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
    #[command(flatten)]
    pub(crate) rbn: RbnCompareOpts,
    #[command(flatten)]
    pub(crate) logging: logging::LogOpts,
}

pub(crate) fn run(args: RunArgs) -> Result<std::process::ExitCode> {
    let RunArgs {
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
        iq_format,
        sample_rate_hz,
        record_iq,
        realtime,
        loop_replay,
        filters,
        json,
        decoded_text,
        config,
        dial_freq_hz,
        replay_epoch,
        engine,
        prior,
        rbn,
        logging: log_opts,
    } = args;
    // MAN-124: first, so every later stderr line of `run` (startup
    // notes, a fatal config error) exists after the subscriber does.
    logging::init(&log_opts);
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
    let cli = CliOverrides {
        prior,
        device,
        source,
        source_iq,
        realtime,
        loop_replay,
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
        replay_epoch,
        allowlist,
        blocklist,
        notch,
        cty,
        scp,
        cty_sha256,
        scp_sha256,
        patterns,
        rbn,
    };
    // MAN-78: a SIGHUP reload re-resolves with the same flags.
    #[cfg(unix)]
    let reload_cli = cli.clone();
    #[cfg(unix)]
    let vars: Vec<_> = std::env::vars_os().collect();
    let source_flag = cli.source.is_some();
    let Prepared {
        config_path,
        loaded,
        mut resolved,
        pipeline: cfg,
        lists_in_use,
    } = prepare_live(cli, config, engine, false, None)?;
    // MAN-309: headerless IQ is described on the command line.
    apply_raw_iq(&mut resolved, iq_format, sample_rate_hz)?;
    // MAN-309: one recording per input; `--record-iq` names one.
    if record_iq.is_some() && !resolved.inputs.is_empty() {
        bail!(
            "--record-iq records one input; this run has {} inputs: set record_iq in \
                     each [[input]] entry to record",
            resolved.inputs.len()
        );
    }
    // CLI > environment > file (`input.record_iq`). A source flag
    // that replaced the configured input(s) drops their record_iq
    // with their other keys, as `resolve` does.
    let record_iq = record_iq.or_else(|| match resolved.replaced_file_source {
        Some(_) => None,
        None => loaded.input.shared.record_iq.clone(),
    });
    // MAN-141: `input.spectrum_udp`, dropped with the rest of the
    // file's input when a source flag replaced it.
    let spectrum_out = match resolved.replaced_file_source {
        Some(_) => None,
        None => loaded.input.shared.spectrum.clone(),
    };
    // An existing --source is an existing destination too.
    if let Some(path) = &record_iq {
        check_record_destination(path)?;
    }
    check_record_destinations(&resolved.inputs)?;
    // MAN-79's stale built-in cty.dat warning, before any I/O,
    // unless the cache may replace that copy: `prepare_run_lists`
    // warns then, once it has chosen (MAN-316).
    let cache_may_choose_cty = resolved.spec.is_reconnectable() && resolved.spot.refresh_lists;
    if !cache_may_choose_cty {
        if let Some(warning) = stale_cty_warning(&lists_in_use[0], std::time::SystemTime::now()) {
            logging::warning(&warning);
        }
    }
    refuse_rbn_compare_replay(&resolved, source_flag)?;
    // MAN-268: an unedited manta.example.toml must not start a
    // daemon that spots, or logs in to a collector, as N0CALL.
    // Checked first, on the identities after the MANTA_* overlay:
    // before `is_rf_aware()` (which opens IQ WAVs), the
    // dial-frequency guard, and any source or listener. Callsigns
    // only; `config check` keeps the broad placeholder scan. See
    // docs/DECISIONS/2026-10-10-man268-unattended-packaging.md.
    config_cmd::reject_example_callsigns(&loaded)?;
    // MAN-13: several [[input]] entries decode on their own threads
    // into one validator and one spot output.
    if !resolved.inputs.is_empty() {
        // MAN-316: as with one input; mixing file and live inputs
        // is refused, so the first input's kind is every input's,
        // and `prepare_run_lists` warns of a stale copy once the
        // cache has had its say.
        let live = resolved.inputs.iter().any(|i| i.spec.is_reconnectable());
        let run_lists = prepare_run_lists(live, &resolved.spot, lists_in_use, &cfg)?;
        let mut cfg = cfg;
        attach_language_prior(&mut cfg, &resolved.spot, Some(run_lists.lists.clone()))?;
        if run_lists.refresh.is_some() {
            cfg.cty = Some(run_lists.lists.cty.load());
            cfg.scp = Some(run_lists.lists.scp.load());
        }
        let server = match loaded.server.clone() {
            None => None,
            Some(server_cfg) => Some(multi_run::ServerSetup {
                cfg: server_cfg,
                rbn_uplink: loaded.rbn_uplink.clone(),
                relays: RelayConfigs {
                    skimmer: loaded.skimmer_relay.clone(),
                    wsjtx: loaded.wsjtx_relay.clone(),
                },
                // MAN-142 (D10): a run counts as a replay when any
                // input is a file.
                outputs: spot_outputs::SpotOutputs::for_run(
                    loaded.pskreporter.as_ref(),
                    &loaded.wsjtx_output,
                    resolved.inputs.iter().any(|i| !i.spec.is_reconnectable()),
                ),
                rbn_compare: resolved.rbn_compare.clone(),
                replay_epoch: resolved.replay_epoch,
            }),
        };
        #[cfg(unix)]
        let reload = Some(multi_run::ReloadSetup {
            config_path: config_path.clone(),
            cli: reload_cli,
            cli_engine: engine,
            vars,
            startup_raw: loaded.raw.clone(),
        });
        #[cfg(not(unix))]
        let reload = None;
        return multi_run::run(multi_run::MultiRun {
            inputs: &resolved.inputs,
            replay_epoch: resolved.replay_epoch,
            hpsdr_allocations: config::hpsdr_allocations(&loaded.inputs),
            replay: resolved.replay,
            cfg: &cfg,
            json,
            decoded_text,
            server,
            run_lists,
            spot: &resolved.spot,
            callsign_lookup: resolved.callsign_lookup.as_ref(),
            reload,
        });
    }
    let spec = &resolved.spec;
    let dial_freq_hz = resolved.dial_freq_hz;
    // The resolved (CLI > env > file) capture rate, for the first open
    // and every reopen below: binding the CLI's raw value here dropped a
    // config/env capture rate on a MAN-73 reconnect.
    let capture_rate_hz = resolved.capture_rate_hz;
    // Needed to derive a recording-specific replay epoch/nonce.
    let replay_path = match spec {
        LiveSourceSpec::File { path, .. } => Some(path.clone()),
        _ => None,
    };
    let has_rf_aware_source = spec.is_rf_aware();
    let source_name = spec.name();
    // MAN-234: how the bad-sample warnings name this input.
    let input_label = spec.input_label();

    // Servers start iff the resolved config has a [server] table
    // (D7); the guard runs after the load and before any source I/O.
    if needs_dial_freq(loaded.server.is_some(), has_rf_aware_source, dial_freq_hz) {
        report_iq_open_error(spec, dial_freq_hz)?;
        bail!(
            "--dial-freq-hz is required with --config when using a plain \
                     audio device or --source WAV file -- neither reports a real RF \
                     frequency (KiwiSDR/SoapySDR already know theirs from \
                     --kiwi-freq/--soapy-freq); set it with --dial-freq-hz, \
                     MANTA_INPUT_CENTER_FREQ_HZ, or input.center_freq_hz"
        );
    }
    // MAN-141: a spectrum feed needs a real RF frequency too, with
    // or without [server], and its socket is bound before any
    // source I/O (as `[[wsjtx_relay]]` sockets are).
    if spectrum_out.is_some() && needs_dial_freq(true, has_rf_aware_source, dial_freq_hz) {
        report_iq_open_error(spec, dial_freq_hz)?;
        bail!(spectrum_feed::NEEDS_RF_MESSAGE);
    }
    // The input's name: its `name` key, else its type (MAN-13).
    let spectrum_input = loaded
        .inputs
        .first()
        .map_or(source_name, |n| n.name.as_str());
    let spectrum_bound = spectrum_out
        .as_ref()
        .map(|out| spectrum_feed::bind(spectrum_input, out))
        .transpose()?;
    if config_path.is_some() && loaded.server.is_none() {
        logging::note(&format!(
            "note: {} has no [server] table; the telnet/JSON/metrics servers are not \
                     started",
            loaded.origin
        ));
    }
    warn_if_audio_source_has_no_rf_reference(has_rf_aware_source, dial_freq_hz);
    // MAN-78: a daemon's SIGHUP is a reload, so it must not land on
    // this thread, the decode loop's: see `reload::mask_sighup`.
    // Blocked before the source, the servers and the reload thread
    // start, so each inherits the mask and only the reload thread
    // unblocks it. SIGINT/SIGTERM still reach this thread; they end
    // the run anyway.
    #[cfg(unix)]
    if loaded.server.is_some() {
        anyhow::Context::context(
            reload::mask_sighup(libc::SIG_BLOCK),
            "blocking SIGHUP on the decode thread",
        )?;
    }
    let opening_at = std::time::SystemTime::now();
    let first = spec.open(capture_rate_hz, dial_freq_hz)?;
    let acquired_at = opening_at + first.acquisition_start_offset();
    let iq_warned = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let iq_name = spectrum_input.to_string();
    warn_iq_mismatch(
        &cfg.conditioning,
        first.center_freq_hz(),
        Some(&iq_name),
        &iq_warned,
    );
    let replay_epoch = resolved.replay_epoch;
    // MAN-312 (SPEC §2.7): read here, while `first` is in hand;
    // logged just before decoding starts (below).
    let crash_live = manta_engine::crash_live_channels(first.sample_rate(), first.rf_passband_hz());
    // MAN-196: the source's trusted band, read once here while
    // `first` is in hand (before `wrap_file_replay` and `listen`
    // take ownership); `Some` only when a decimator is in the
    // path (`--capture-rate-hz`/`input.capture_rate_hz`). Logged
    // beside the crash notice below, before the readiness marker.
    // Round-1 review (PR #300): the notice states the band the
    // source actually delivers -- `rf_passband_hz()`, the inner
    // passband intersected with the trusted band -- so a
    // decimation wrapping an already narrower source (a KiwiSDR's
    // +/-5 kHz filter, a one-sided rig-audio passband) never
    // claims a wider usable passband than `manta check` and SETT
    // advertise for the same source.
    let trusted_band = first.trusted_band_hz().map(|_| first.rf_passband_hz());
    let trusted_band_fs_hz = first.sample_rate();

    // MAN-122 review round 6 (P2): installed HERE -- before
    // `start_spot_server` binds the sockets and logs the
    // `listening:` startup banner naming them -- and not after it,
    // as an earlier revision did. That banner is an advertisement:
    // a supervisor or operator that reacts to it by immediately
    // sending SIGINT/SIGTERM must not hit the signal's DEFAULT
    // disposition, which terminates the daemon outright and skips
    // the client/status shutdown drain that MAN-85's handler
    // exists to guarantee. Installing the handler first makes the
    // whole observable window -- banner included -- covered by the
    // drain. A signal landing before the decode loop starts is
    // observed by `listen_with_observers` before its next startup-
    // calibration read (review round 7; before that round it was
    // observed only once the two-second calibration buffer had
    // filled); it processes only what it has already read and
    // returns `Ok` without reading again. A reconnect's backoff or
    // reopen in flight ends within ~100 ms (MAN-252: the reopen runs
    // on its own thread, which a stop abandons). A paced replay's
    // wait, including one through a recorded gap, ends within
    // ~100 ms too (MAN-453). A read already in
    // flight is not interrupted: it returns with whatever the source
    // yields next, or fails after that source's own stall bound, and
    // the shutdown sequence below runs on both the `Ok` and the error
    // path, exactly as it does for a signal arriving mid-decode. The
    // banner itself still precedes the listener
    // tasks (see `start_spot_server`), so its ordering against
    // per-connection lines is unchanged.
    //
    // MAN-78/MAN-232: on Unix, `install_unix_signal_handlers` owns
    // every stop signal (SIGINT, SIGTERM, and SIGHUP without a
    // `[server]` table) through signal-hook, at the same point, so
    // the same covered window holds for every signal; ctrlc is
    // Windows-only below, for console events.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    #[cfg(unix)]
    let reload_signals = crate::install_unix_signal_handlers(&stop, loaded.server.is_some())?;
    // MAN-232: Windows' Ctrl-C/Ctrl-Break; on Unix every stop signal
    // is signal-hook's (`install_unix_signal_handlers` above). A
    // second event exits at once, as a second SIGINT does on Unix.
    #[cfg(windows)]
    {
        let stop_handler = stop.clone();
        let signalled = std::sync::atomic::AtomicBool::new(false);
        ctrlc::set_handler(move || {
            if signalled.swap(true, std::sync::atomic::Ordering::Relaxed) {
                std::process::exit(130);
            }
            stop_handler.store(true, std::sync::atomic::Ordering::Relaxed);
        })?;
    }
    // MAN-316: the cty.dat/MASTER.SCP every consumer shares (D3);
    // with refresh on and a live source, the cache's last good
    // download replaces the built-in copy here, before the
    // validator and servers are built.
    let run_lists = prepare_run_lists(spec.is_reconnectable(), &resolved.spot, lists_in_use, &cfg)?;
    let mut cfg = cfg;
    attach_language_prior(&mut cfg, &resolved.spot, Some(run_lists.lists.clone()))?;
    if run_lists.refresh.is_some() {
        cfg.cty = Some(run_lists.lists.cty.load());
        cfg.scp = Some(run_lists.lists.scp.load());
    }
    let lists = run_lists.lists.clone();

    // Kept alive for the process lifetime: dropping it would stop
    // the spawned server tasks. `None` when --config wasn't
    // given, in which case `spot_server` stays None too. `epoch`/
    // `session_nonce` are deliberately computed IN this branch, not
    // above it -- `--source`-only replay (no --config) never
    // consumes either, and computing `session_nonce` means hashing
    // the entire replayed file a second time after it's already
    // been opened; skip that full-file pass entirely when nothing
    // downstream needs it (round-7 review finding).
    // MAN-309: one epoch for the spot servers and the recording, so
    // a live recording starts at the instant its spots are timed
    // from; resolved only when one of them consumes it.
    let session_epoch = if loaded.server.is_some() || record_iq.is_some() {
        Some(resolve_epoch(replay_path.as_deref(), replay_epoch)?)
    } else {
        None
    };
    // Coverage is reported before the server's listening banner.
    let digital_modes = if resolved.replaced_file_source.is_none() {
        loaded.input.shared.digital_modes.as_slice()
    } else {
        &[]
    };
    let ftx = (!digital_modes.is_empty()).then(|| {
        std::sync::Arc::new(ftx_run::Session::new(vec![(
            source_name.to_string(),
            resolved.freq_correction_ppm,
        )]))
    });
    if ftx.is_some() {
        let (windows, lines) = ftx_run::plan(
            source_name,
            digital_modes,
            first.center_freq_hz(),
            first.rf_passband_hz(),
            first.trusted_band_hz(),
            first.sample_rate(),
        );
        for line in lines {
            logging::note(&line);
        }
        if replay_path.is_none() && !windows.is_empty() {
            if let Some(line) = ftx_run::clock_line(clock_check::kernel_synced()) {
                logging::note(&line);
            }
        }
    }
    let (
        server_runtime,
        spot_server,
        active_tracks,
        mut active_tracks_poller,
        decode_latency,
        mut decode_latency_poller,
        decode_quality,
    ) = match loaded.server.clone() {
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
            let epoch = match session_epoch {
                Some(epoch) => epoch,
                None => resolve_epoch(replay_path.as_deref(), replay_epoch)?,
            };
            let session_nonce: u128 = match &replay_path {
                Some(replay_path) => session_nonce_for_replay_path(replay_path)?,
                // Live session: `epoch` above is already SystemTime::now().
                None => epoch
                    .duration_since(std::time::SystemTime::UNIX_EPOCH)
                    .expect("epoch predates the Unix epoch")
                    .as_nanos(),
            };

            // The resolved (CLI > env > file, MAN-261) value, already
            // validated by `check_freq_correction_ppm`; re-derived
            // here because the factor, not the ppm, is what the
            // advertised SETT bounds are scaled by (MAN-86 review).
            let freq_calibration =
                manta_spot::calibration_factor_from_ppm(resolved.freq_correction_ppm)
                    .map_err(|e| anyhow!(e))?;
            // MAN-122: the banner `start_spot_server` logs names
            // the source, its sample rate and its dial frequency,
            // so all three are read HERE, from `first` (the
            // startup connection), before it is moved into the
            // pipeline below.
            let (rt, server) = start_spot_server(SpotServerParams {
                cfg: server_cfg,
                rbn_uplink_cfgs: loaded.rbn_uplink.clone(),
                rbn_compare_cfg: resolved.rbn_compare.clone(),
                source: SourceInfo {
                    name: source_name,
                    sample_rate_hz: first.sample_rate(),
                    dial_freq_hz: first.center_freq_hz(),
                    rf_passband_hz: first.rf_passband_hz(),
                    freq_calibration,
                    coverage: &[],
                    inputs: None,
                },
                epoch,
                session_nonce,
                lists: lists.clone(),
                pipeline: &cfg,
                relays: RelayConfigs {
                    skimmer: loaded.skimmer_relay.clone(),
                    wsjtx: loaded.wsjtx_relay.clone(),
                },
                outputs: spot_outputs::SpotOutputs::for_run(
                    loaded.pskreporter.as_ref(),
                    &loaded.wsjtx_output,
                    replay_path.is_some(),
                ),
            })?;
            // MAN-45 (round-9 finding): the daemon's own copy of the
            // gauge `manta_engine::listen_with_observers` updates as
            // it runs (on the MAIN thread, outside this tokio
            // runtime) -- polled into `Metrics` below, the same
            // bridge shape MAN-55's `confirmed_live_handle` watcher
            // uses for source liveness. MAN-122 additionally takes
            // the same count synchronously off `listen`'s per-batch
            // `on_tracks` callback, for the status line's
            // decode-progress heartbeat, which a polled gauge value
            // cannot express (a steady count and a wedged decode
            // loop look identical through the atomic alone).
            let active_tracks = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
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
                        track_metrics
                            .set_active_tracks(gauge.load(std::sync::atomic::Ordering::Relaxed));
                        tokio::time::sleep(ACTIVE_TRACKS_POLL_INTERVAL).await;
                    }
                })
            };
            // MAN-128: same bridge shape as `active_tracks`
            // above -- the engine's decode loop runs on the
            // main thread, outside this tokio runtime, so a
            // poller copies its lock-free observer into
            // `Metrics` on a timer.
            let decode_latency = std::sync::Arc::new(manta_engine::DecodeLatencyObserver::new());
            server
                .metrics
                .set_decode_latency(latency_histogram_of(&decode_latency.snapshot()));
            // MAN-118: the decode-quality observer rides the same
            // poller; its all-zero snapshot is published now so
            // the families carry samples from startup.
            let decode_quality = std::sync::Arc::new(manta_engine::DecodeQualityObserver::new());
            server
                .metrics
                .set_decode_quality(decode_quality_of(&decode_quality.snapshot()));
            let decode_latency_poller = {
                let obs = decode_latency.clone();
                let quality = decode_quality.clone();
                let latency_metrics = server.metrics.clone();
                rt.spawn(async move {
                    loop {
                        tokio::time::sleep(DECODE_LATENCY_POLL_INTERVAL).await;
                        latency_metrics.set_decode_latency(latency_histogram_of(&obs.snapshot()));
                        latency_metrics.set_decode_quality(decode_quality_of(&quality.snapshot()));
                    }
                })
            };
            // MAN-128: armed right after the listeners are up
            // and before the decode loop starts below, so
            // `/healthz`'s decode check is live for the entire
            // run -- `mark_decode_stopped()` on the shutdown
            // path (below) is what makes it catch a source
            // that has died during the drain that follows.
            server.metrics.arm_decode_watchdog();
            // MAN-73: for every reconnectable source kind, ALL
            // health reporting (including this initial value) is
            // now owned by `ReconnectingSource` below, which reads
            // the same `confirmed_live_handle()` signal the old
            // MAN-55 watcher task used to, and additionally flips
            // health false/true across every later reconnect, not
            // just the first connection. File replay is the only
            // kind that bypasses `ReconnectingSource` entirely
            // (determinism; see `is_reconnectable`'s doc comment),
            // so its health is set true here, once, immediately.
            if !spec.is_reconnectable() {
                server.metrics.set_source_health(source_name, true);
            }

            (
                Some(rt),
                Some(server),
                Some(active_tracks),
                Some(active_tracks_poller),
                Some(decode_latency),
                Some(decode_latency_poller),
                Some(decode_quality),
            )
        }
        None => (None, None, None, None, None, None, None),
    };
    // MAN-316: after the `listening:` banner, before `manta:
    // listening;`: the `lists:` lines, `manta_list_info`, and the
    // `manta-lists` thread. `None` (file replay, refresh off) keeps
    // the decode loop's check off entirely.
    let reference_lists = start_list_refresh(
        run_lists,
        spec.is_reconnectable(),
        &resolved.spot,
        spot_server.as_ref().map(|server| server.metrics.clone()),
        stop.clone(),
    )?;
    // MAN-318: live sources only; a replay notes it and looks
    // nothing up.
    let callsign_lookup = start_callsign_lookup(
        resolved.callsign_lookup.as_ref(),
        spec.is_reconnectable(),
        spot_server.as_ref().map(|server| server.metrics.clone()),
    )?;

    // MAN-78: with a `[server]` table, SIGHUP reloads the `[spot]`
    // lists and each `[[rbn_uplink]]` `dry_run` (D2). The thread
    // starts after `start_spot_server`, so its log lines have a
    // subscriber; a SIGHUP that arrived earlier is buffered by
    // `Signals` and handled as soon as it starts.
    let operator_lists = std::sync::Arc::new(manta_engine::OperatorListsUpdate::default());
    let drain_gate = std::sync::Arc::new(reload::DrainGate::default());
    #[cfg(unix)]
    let reload_enabled = match (reload_signals, &spot_server) {
        (Some(signals), Some(server)) => {
            reload::spawn(
                signals,
                reload::ReloadContext {
                    config_path: config_path.clone(),
                    cli: reload_cli,
                    cli_engine: engine,
                    vars,
                    startup_raw: loaded.raw.clone(),
                    lists: operator_lists.clone(),
                    uplinks: server.uplink_dry_run.clone(),
                    drain_gate: drain_gate.clone(),
                    stop: stop.clone(),
                },
            )?;
            true
        }
        _ => false,
    };
    #[cfg(not(unix))]
    let reload_enabled = false;

    // MAN-73: wrap every reconnectable source so a later read
    // error/EOF is retried with `manta-server::backoff`'s policy
    // instead of propagating out of `listen()` and ending the
    // process. File replay is passed through unwrapped -- its
    // errors and EOF must reach `listen()` unchanged for
    // byte-identical replay.
    let input_health: Option<std::sync::Arc<reconnect::InputHealthTotals>>;
    let src: Box<dyn IqSource> = if spec.is_reconnectable() {
        let initial_healthy = first.confirmed_live_handle().is_none();
        let name = spec.name();
        let on_health = source_health_sink(
            name,
            spot_server.as_ref().map(|server| server.metrics.clone()),
            active_tracks.clone(),
        );
        let reopen_spec = spec.clone();
        let iq_cfg = cfg.conditioning.clone();
        let wrapped = ReconnectingSource::new(
            name,
            first,
            Box::new(move || {
                let src = reopen_spec.open(capture_rate_hz, dial_freq_hz)?;
                warn_iq_mismatch(&iq_cfg, src.center_freq_hz(), Some(&iq_name), &iq_warned);
                Ok(src)
            }),
            stop.clone(),
            initial_healthy,
            on_health,
        )
        .with_loss_sink(source_loss_sink(
            name,
            spot_server.as_ref().map(|server| server.metrics.clone()),
        ));
        input_health = wrapped.input_health();
        Box::new(wrapped)
    } else {
        input_health = reconnect::InputHealthTotals::for_source(first.as_ref());
        // MAN-269: `--realtime`/`--loop` pace and loop a file replay;
        // every loop pass reopens through `spec.open`, so it gets a
        // fresh decimator and the same dial override.
        let reopen_spec = spec.clone();
        let label = replay_path
            .as_ref()
            .map_or_else(String::new, |p| p.display().to_string());
        wrap_file_replay(
            first,
            resolved.replay,
            label,
            Box::new(move || {
                Ok(reopen_spec.open(capture_rate_hz, dial_freq_hz)? as Box<dyn IqSource>)
            }),
            stop.clone(),
        )?
    };

    // MAN-56: a source's packet loss/malformed counters are
    // input-layer state manta-server cannot compute itself (it
    // has no manta-input dependency). Sample them into Metrics on
    // a timer, the same wiring-layer-injection shape
    // `set_source_health` uses -- and take the handle HERE, before
    // `listen(src, ..)` below takes ownership of the source for
    // the rest of the run. MAN-228: the handle is the
    // `InputHealthTotals` summed over every connection, so the
    // series keep counting across a MAN-73 reconnect instead of
    // freezing at the startup connection's values. Sources with
    // no wire-packet loss model publish no series at all, which
    // is deliberate: a permanently-zero counter reads as "no
    // loss" rather than "not measured" (ARCHITECTURE §8's
    // "absent means not measured" distinction).
    if let (Some(rt), Some(server), Some(counters)) = (&server_runtime, &spot_server, input_health)
    {
        let metrics = server.metrics.clone();
        // Published once eagerly so the series exists (at 0) from
        // the very first scrape rather than only after one poll
        // interval.
        metrics.set_input_health(source_name, counters.snapshot());
        rt.spawn(async move {
            loop {
                tokio::time::sleep(INPUT_HEALTH_POLL_INTERVAL).await;
                metrics.set_input_health(source_name, counters.snapshot());
            }
        });
    }
    // MAN-308: automatic attenuation's level and change count, from
    // the handle every reopen shares.
    if let (Some(rt), Some(server), Some(attenuation)) =
        (&server_runtime, &spot_server, spec.auto_attenuation())
    {
        spawn_attenuation_poller(
            rt,
            server.metrics.clone(),
            source_name.to_string(),
            attenuation,
        );
    }

    // A plain stderr line in text mode rather than `tracing::info!`,
    // so `-q`/`RUST_LOG` never hide it there; under `--log-format
    // json` it is a leveled INFO record (MAN-124). Two jobs: `listen` otherwise prints
    // nothing at startup (2026-09-05 review, lens 1 #4/#7), and it
    // is the readiness handshake `tests/signal_shutdown.rs` waits
    // for. It remains AFTER the signal handlers are installed, which as of
    // review round 6 runs further above (before `start_spot_server`
    // and its `listening:` banner), so every startup line this
    // daemon emits -- this marker and the banner alike -- is now
    // published with the handler already installed; no line
    // advertises a daemon that would still die on the signal's
    // default disposition. If the fuller startup banner (lens 1 #7)
    // ever replaces this line, `READY_MARKER` must be updated to
    // match. stdout stays pure JSON under `--json` (MAN-59
    // round 6); this goes to stderr. MAN-78: a daemon that reloads
    // on SIGHUP says so; the `manta: listening;` prefix the tests
    // wait for is the same either way.
    //
    // MAN-312 (SPEC §2.7): the static-crash filter fails open on a
    // narrow source; say so once rather than leave an operator who
    // set `crash_filter` believing it acts. Logged after the servers
    // bound, so a failed start still prints exactly one `error:`
    // line (MAN-270), and before the readiness marker, so a reader
    // that stops at the marker never leaves it unread.
    if cfg.detector.crash_filter != manta_engine::CrashFilter::Off
        && crash_live < manta_engine::CRASH_MIN_LIVE_CHANNELS
    {
        tracing::info!(
            target: "manta",
            "static-crash filter inactive: the source passband holds {crash_live} channels, fewer than the {} it needs",
            manta_engine::CRASH_MIN_LIVE_CHANNELS
        );
    }
    // MAN-196: a decimated capture states its usable passband
    // once, at the same point as the crash notice -- after the
    // servers bound, before the readiness marker -- so an
    // operator who chose `--capture-rate-hz` for its nominal
    // Nyquist width learns the channels beyond the decimator's
    // <= 1 dB trusted band are not tracked. No line at all
    // without a decimator: the non-decimated path is unchanged
    // in every observable respect.
    if let Some((lo, hi)) = trusted_band {
        tracing::info!(target: "manta", "{}", trusted_band_notice(trusted_band_fs_hz, lo, hi));
    }
    if reload_enabled {
        logging::note(
            "manta: listening; send SIGINT or SIGTERM to stop, SIGHUP to reload \
                     [spot] lists and dry_run",
        );
    } else {
        logging::note("manta: listening; send SIGINT or SIGTERM to stop");
    }
    // MAN-309: the exact samples, gaps and source properties handed
    // to the engine are recorded; the decoder never waits on the
    // writer. The recording starts at the session's epoch: a live
    // run's spot-server epoch, or a replay's own.
    let (src, recording) = match &record_iq {
        Some(path) => {
            let start = match session_epoch {
                Some(epoch) => epoch,
                None => resolve_epoch(replay_path.as_deref(), replay_epoch)?,
            };
            let (src, handle) = start_input_recording(src, path, start, stop.clone())?;
            (src, Some(handle))
        }
        None => (src, None),
    };
    let src = if let Some(ftx) = &ftx {
        if let Some(server) = &spot_server {
            ftx.bind(server.bus.clone(), server.metrics.clone());
        }
        ftx.wrap(
            0,
            src,
            resolve_ftx_epoch(spec, replay_epoch, acquired_at)?,
            digital_modes,
            replay_path.is_none(),
        )
    } else {
        src
    };
    // Captured before `src` is moved into the pipeline, for the
    // readiness event below.
    let source_sample_rate_hz = src.sample_rate();
    let mut pipeline_ready_logged = false;
    // MAN-122 review round 5 (P2): `stop` is moved into
    // `listen_with_observers` below, so the readiness observer needs
    // its own handle to read the cancellation flag. The engine runs
    // the padding and calibration `on_tracks` callbacks BEFORE its
    // loop first examines `stop` (manta-engine/src/listen.rs: the
    // two `on_tracks(n_tracks)` calls above `loop { if
    // stop.load(..) { break } }`) -- also when a stop request cut
    // the calibration fill short (review round 7), since what was
    // read is still processed -- so a SIGINT arriving during the
    // two-second startup calibration would otherwise publish
    // `ready: decoding` for a run that shuts down without ever
    // decoding a chunk.
    let stop_ready = stop.clone();
    // MAN-232: run's stdout lines go through `StopOnClose`: the
    // first failed write sets `stop` -- starting the same drain a
    // signal starts, so connected clients still drain -- and the
    // stored error becomes exit 141 at `finish()`, after the drain.
    let stdout_lines = stdio::StopOnClose::stdout(stop.clone());
    // MAN-123: decoded text is grouped per track (text_lines.rs) and
    // goes to stderr; stdout carries spots. A daemon -- servers
    // started from a [server] table -- logs no decoded text unless
    // asked, so its journal holds spots and diagnostics only.
    let print_text = !json && (spot_server.is_none() || decoded_text);
    let mut text_lines = text_lines::TrackLines::default();
    // MAN-312: `listen` stores its running crash count here before
    // each `on_tracks` call, which copies it into `Metrics`.
    let static_crashes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    // MAN-234: `listen` stores its running count of replaced input
    // samples here before each `on_tracks` call, which warns once
    // at the first one and copies the count into `Metrics`, where
    // it is published at 0 from startup.
    let bad_samples = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut bad_samples_warned: u64 = 0;
    cfg.conditioning = if resolved.replaced_file_source.is_some() {
        Default::default()
    } else {
        loaded.input.shared.conditioning()
    };
    let conditioned = std::sync::Arc::new(manta_engine::ConditioningObserver::default());
    if let Some(server) = &spot_server {
        server.metrics.set_input_bad_samples(source_name, 0);
        publish_conditioned(
            &server.metrics,
            source_name,
            &cfg.conditioning,
            &conditioned,
        );
    }
    // MAN-314: the engine's marked RTTY set, logged on change and
    // copied into `manta_rtty_signals` from the per-batch callback.
    let rtty_observer = std::sync::Arc::new(manta_engine::RttyObserver::default());
    let mut rtty_log = RttyLogState::new(source_sample_rate_hz);
    // MAN-141: the feed's span comes from the opened source, the
    // same numbers the engine's tap uses.
    let (spectrum_sink, spectrum_thread) = match spectrum_bound {
        Some(feed) => spectrum_feed::open(
            feed,
            src.sample_rate(),
            src.center_freq_hz(),
            src.rf_passband_hz(),
            resolved.freq_correction_ppm,
            spot_server.as_ref().map(|server| server.metrics.as_ref()),
        )?
        .unzip(),
        None => (None, None),
    };
    let listen_result = manta_engine::listen_with_observers(
        src,
        &cfg,
        stop,
        manta_engine::ListenObservers {
            active_tracks: active_tracks.clone(),
            decode_latency: decode_latency.clone(),
            decode_quality: decode_quality.clone(),
            operator_lists: reload_enabled.then(|| operator_lists.clone()),
            reference_lists,
            callsign_lookup,
            static_crashes: Some(static_crashes.clone()),
            bad_samples: Some(bad_samples.clone()),
            conditioned: Some(conditioned.clone()),
            // MAN-495: the full-report observer is doctor's; the
            // daemon's metric reads the counts observer above.
            conditioning_report: None,
            rtty: Some(rtty_observer.clone()),
            // MAN-324: only with a live-state listener.
            live_state: spot_server
                .as_ref()
                .filter(|server| server.live_addr.is_some())
                .map(|server| {
                    std::sync::Arc::new(live_state::LiveSink::new(
                        server.bus.clone(),
                        server.metrics.clone(),
                        &server.station_callsign,
                    )) as std::sync::Arc<dyn manta_engine::LiveStateSink>
                }),
            detector_confirmations: None,
            track_lifecycle: None,
            spectrum: spectrum_sink,
            funnel_trace: None,
        },
        |ev| {
            if json {
                stdout_lines.line(format_args!("{}", serde_json::to_string(ev).unwrap()));
                return;
            }
            if !print_text {
                return;
            }
            if let Some(line) = text_lines.ingest(ev) {
                if logging::is_json() {
                    tracing::info!(target: "manta::text", "{line}");
                } else {
                    errln!("{line}");
                }
            }
        },
        // Provisional CLI-debugging text/JSON printed below is NOT
        // the ecosystem wire contract -- that's `spot_server`
        // (manta-server's telnet/JSON-Lines/WebSocket fan-out,
        // ARCHITECTURE §7), fed here when --config is set. The
        // human `SPOT:` line is stdout's product in text mode
        // (MAN-123); decoded text and diagnostics go to stderr.
        |spot| {
            if let Some(server) = &spot_server {
                let truncated = server.bus.publish(spot.clone());
                server
                    .metrics
                    .record_history_truncated(truncated.history_truncated as u64);
                server.metrics.record_json_stream_history_truncated(
                    truncated.stream_history_truncated as u64,
                );
                server.metrics.record_spot(spot);
                // MAN-136/MAN-45: counted ONCE per spot here, NOT
                // inside `SpotMessage::from_spot` -- that runs once
                // per connected JSON/WS client (json_stream.rs:126),
                // so counting there would scale with client count
                // instead of spot count. Checks BOTH sides: the
                // operator's own station_callsign is config, not
                // decoder output, and isn't required to resolve --
                // MAN-316: both sides use the table in use now,
                // loaded once, since a list refresh can swap it.
                // MAN-89: `station_geography_unresolved` strips the
                // RBN `-N` per-band SSID itself, as `from_spot` does.
                let cty = server.lists.cty.load();
                if geography_is_unresolved(&cty, &spot.callsign)
                    || station_geography_unresolved(&cty, &server.station_callsign)
                {
                    server.metrics.record_unresolved_geography();
                }
            }
            if json {
                stdout_lines.line(format_args!("{}", fmt::spot_json(spot)));
                return;
            }
            stdout_lines.line(format_args!("{}", fmt::spot_line(spot)));
        },
        // MAN-122 review round 1: the live track gauge comes from
        // `TrackManager`'s own lifecycle -- how many tracks are
        // promoted and holding a decoder right now -- not from the
        // `DecoderEvent` stream above. A promoted track whose
        // demodulator has not latched emits nothing for up to the
        // ~30 s silent-GC window, so an event-derived count reports
        // `tracks=0` on a node that is genuinely decoding weak
        // signals.
        //
        // Review round 2: this observer fires once per processed
        // batch (repeats included), so it is also the daemon's
        // decode-progress heartbeat. Two relaxed atomics per
        // ~43 ms chunk, immediately after that chunk's channelizer
        // + TrackManager work -- unmeasurable against it, and the
        // only thing that lets the status line say "stalled"
        // instead of republishing a frozen `tracks=N` forever.
        |n_tracks| {
            let n_bad = bad_samples.load(std::sync::atomic::Ordering::Relaxed);
            if n_bad > 0 && bad_samples_warned == 0 {
                logging::warning(&fmt::bad_samples_live_warning(
                    &input_label,
                    n_bad,
                    spot_server.is_some(),
                ));
                bad_samples_warned = n_bad;
            }
            if let Some(server) = &spot_server {
                server.metrics.set_input_bad_samples(source_name, n_bad);
                publish_conditioned(
                    &server.metrics,
                    source_name,
                    &cfg.conditioning,
                    &conditioned,
                );
            }
            if let Some(marks_hz) = rtty_log.poll(&rtty_observer) {
                if let Some(server) = &spot_server {
                    server.metrics.set_rtty_signals(&marks_hz);
                }
            }
            if let Some(server) = &spot_server {
                server.metrics.set_active_tracks(n_tracks as u64);
                server.metrics.set_static_crashes_total(
                    static_crashes.load(std::sync::atomic::Ordering::Relaxed),
                );
                server.metrics.record_pipeline_batch();
                // The first batch is the earliest moment the daemon
                // can honestly claim to be decoding: `listen`'s
                // calibration read has returned, the channelizer is
                // built and the TrackManager has processed real
                // hops. The startup banner above only ever claimed
                // bound sockets (review round 2).
                // Checked here rather than only at first-batch
                // time so a cancelled startup cannot publish a
                // false readiness event: `stop` is already `true`
                // by the time the calibration callbacks run, and
                // the decode loop breaks out immediately after
                // them. Not latching `pipeline_ready_logged` on
                // this path is deliberate -- the flag means "we
                // have claimed readiness", and no claim was made.
                if !pipeline_ready_logged && !stop_ready.load(std::sync::atomic::Ordering::Relaxed)
                {
                    pipeline_ready_logged = true;
                    tracing::info!(
                        target: "manta",
                        "{}",
                        manta_server::status::format_pipeline_ready(
                            env!("CARGO_PKG_VERSION"),
                            source_name,
                            source_sample_rate_hz,
                        )
                    );
                }
            }
        },
    );
    // MAN-78 D7: the decode loop has returned, so the daemon is
    // draining from here on. Closing the gate (under the lock a
    // reload holds while it applies) and setting `stop` tell the
    // reload thread to ignore SIGHUP instead of flipping `dry_run`
    // mid-drain.
    if let Some(ftx) = &ftx {
        ftx.stop();
    }
    drain_gate.close();
    stop_ready.store(true, std::sync::atomic::Ordering::Relaxed);
    // MAN-309: the source is gone, so the queue is closed; the
    // writer finalizes during the drain and is collected after it.
    let recording_deadline = close_recordings(recording.as_slice());
    // MAN-141: the engine dropped its sink, so the sender thread has
    // sent what was queued and ends.
    if let Some(thread) = spectrum_thread {
        let _ = thread.join();
    }
    // MAN-234: the run's total, only when it grew past the count
    // the first-sample line reported (success and error alike).
    let bad_total = bad_samples.load(std::sync::atomic::Ordering::Relaxed);
    if bad_total > bad_samples_warned {
        logging::warning(&fmt::bad_samples_warning(&input_label, bad_total));
    }

    // A source read error returns from listen without closing its
    // tracks, so their partial lines are printed here, on both paths.
    if print_text {
        for line in text_lines.finish() {
            if logging::is_json() {
                tracing::info!(target: "manta::text", "{line}");
            } else {
                errln!("{line}");
            }
        }
    }

    // Run the same server-shutdown sequence on BOTH the success and
    // error paths -- an SDR disconnect or WAV read failure from
    // `listen` must not skip draining already-published spots or
    // abort in-flight client writes with no chance to finish, which
    // a bare `listen(...)?` before this block used to do on any
    // error (round-7 review finding). Explicitly signal the client
    // tasks to drain (e.g. spots from TrackManager::finish() just
    // before `listen` returned) before tearing the runtime down.
    if let Some(server) = &spot_server {
        // MAN-128: `listen_with_observers` has just returned, which
        // means the decode loop has genuinely stopped (success or
        // error alike -- `listen_result` is captured above rather
        // than `?`-ed for exactly this reason). This is the first
        // thing done in this block, before touching any poller or
        // sending `shutdown_tx`, so `/healthz` reports unhealthy for
        // the entire up-to-`SHUTDOWN_DRAIN_DEADLINE` drain that
        // follows -- the window in which `source_health` alone
        // would otherwise still read healthy for a source that has
        // already died. One final snapshot is published first so a
        // scrape during the drain sees the decode loop's true last
        // latency distribution rather than a stale mid-run one.
        if let Some(obs) = &decode_latency {
            server
                .metrics
                .set_decode_latency(latency_histogram_of(&obs.snapshot()));
        }
        if let Some(obs) = &decode_quality {
            server
                .metrics
                .set_decode_quality(decode_quality_of(&obs.snapshot()));
        }
        server.metrics.mark_decode_stopped();
        if let Some(poller) = decode_latency_poller.take() {
            poller.abort();
            if let Some(rt) = server_runtime.as_ref() {
                let _ = rt.block_on(poller);
            }
        }
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
        server.metrics.set_rtty_signals(&[]);
        // Before `shutdown_tx.send(true)`, not after: ordering is
        // what makes this observable at all (MAN-64) -- writing it
        // after the drain would guarantee no scraper still
        // connected to the (by-then torn-down) metrics
        // listener could ever see it. Even before the drain, this
        // is a best-effort window, not a guarantee: the metrics
        // listener only outlives this call for as long as a
        // telnet/JSON/WS client is genuinely draining underneath
        // `shutdown_runtime_after_drain`'s `await_all` (up to
        // `SHUTDOWN_DRAIN_DEADLINE`); with none connected the
        // runtime tears down microseconds later and no scrape can
        // land (see `record_terminal_source_health`'s doc comment).
        record_terminal_source_health(&server.metrics, source_name, &listen_result);
        // Finish the status task before the broadcast wakes clients
        // to drain, including any line already past its last guard.
        if let Some(rt) = server_runtime.as_ref() {
            signal_spot_server_shutdown(rt, server);
        }
    }
    // `server_runtime`/`spot_server` are always constructed as a
    // matched pair (both `Some` or both `None`, see their
    // construction above) -- `zip` makes that invariant explicit
    // instead of a defensive branch for a case that can't happen.
    if let Some((rt, server)) = server_runtime.zip(spot_server.as_ref()) {
        shutdown_runtime_after_drain(rt, &server.tasks);
    }
    let recording_result = finish_recordings(recording, recording_deadline);
    listen_result?;
    // MAN-232: a closed (or otherwise failed) stdout reader stopped
    // the loop above through `stop`; only now, with the drain done,
    // does it become the run's exit status (141 / 1). A source
    // failure still wins, because it is reported first.
    stdout_lines.finish()?;
    // MAN-309: a recording that did not complete fails the run last.
    recording_result?;
    Ok(std::process::ExitCode::SUCCESS)
}

/// Register configured conditioning series at zero, then publish exact running counts.
pub(crate) fn publish_conditioned(
    metrics: &manta_server::metrics::Metrics,
    source: &str,
    cfg: &manta_dsp::condition::ConditioningConfig,
    observer: &manta_engine::ConditioningObserver,
) {
    use manta_dsp::condition::{Mark, Stage, StageMode};
    let counts = observer.snapshot();
    for (stage, name, mode) in [
        (Stage::Impulse, "impulse", cfg.impulse),
        (Stage::Dc, "dc", cfg.dc),
    ] {
        for (mark, label) in [
            (Mark::Repaired, "repaired"),
            (Mark::Flagged, "flagged"),
            (Mark::Corrected, "corrected"),
            (Mark::Uncertain, "uncertain"),
        ] {
            let enabled = matches!(
                (stage, mode, mark),
                (Stage::Dc, StageMode::Apply, Mark::Corrected)
                    | (Stage::Impulse, StageMode::Apply, Mark::Repaired)
                    | (
                        Stage::Impulse,
                        StageMode::Observe | StageMode::Apply,
                        Mark::Flagged | Mark::Uncertain
                    )
            );
            if enabled {
                metrics.set_input_conditioned_samples(
                    source,
                    name,
                    label,
                    counts[stage as usize][mark as usize],
                );
            }
        }
    }
}

#[cfg(test)]
mod tests;
