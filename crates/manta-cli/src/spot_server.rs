//! Starting the telnet, JSON-Lines/WebSocket and metrics servers for a
//! daemon run (`start_spot_server`) and the types its parameters name.
//! MAN-548 moved them out of `main.rs`.

use crate::build_info;
use crate::daemon_build_info;
use crate::fmt;
use crate::logging;
use crate::relay_socket;
use crate::spawn_tracked_listener;
use crate::spot_bus_for;
use crate::spot_outputs;
use crate::station_profile;
use crate::LISTENER_JSON;
use crate::LISTENER_LIVE;
use crate::LISTENER_METRICS;
use crate::LISTENER_RTTY_TELNET;
use crate::LISTENER_TELNET;
use anyhow::{anyhow, Context, Result};
use manta_engine::PipelineConfig;

/// Handles what the `Run` on-spot closure needs to feed a running spot server.
pub(crate) struct SpotServer {
    pub(crate) bus: std::sync::Arc<manta_server::bus::SpotBus>,
    pub(crate) metrics: std::sync::Arc<manta_server::metrics::Metrics>,
    /// Signals the telnet/JSON/WS client tasks to drain their already-
    /// queued spots and exit, instead of being forcibly cut off by
    /// `Runtime::shutdown_timeout`'s raw deadline with no chance to finish
    /// an in-flight write. `signal_spot_server_shutdown` first calls
    /// `stop_status_line`, then sends here before shutting the runtime down.
    pub(crate) shutdown_tx: tokio::sync::watch::Sender<bool>,
    /// Every spawned telnet/JSON/WS per-client connection task, tracked so
    /// shutdown can genuinely AWAIT their completion (bounded by
    /// `SHUTDOWN_DRAIN_DEADLINE`) instead of guessing a fixed sleep
    /// duration -- see `shutdown_runtime_after_drain`.
    pub(crate) tasks: manta_server::tasks::ClientTasks,
    /// The run's reference lists, shared with the validator and
    /// `JsonStreamConfig`, kept here too so the publish callback can check
    /// resolvability once per spot for `manta_spots_unresolved_geography_total`
    /// -- checking inside `SpotMessage::from_spot` would scale with connected
    /// client count instead of spot count. MAN-316: a list refresh may swap
    /// the cty table, so the callback loads it once per spot.
    pub(crate) lists: std::sync::Arc<manta_engine::ReferenceLists>,
    /// The operator's OWN station callsign (config, not decoder output --
    /// and not required to be cty-resolvable), whose de-side `UNKNOWN_*`
    /// sentinels are checked per spot against the current table: MAN-316's
    /// refresh can change the answer, at the cost of one more lookup.
    /// MAN-324: also `deCall` on every live record, and the station half of
    /// a spotted decision's `spotId`.
    pub(crate) station_callsign: String,
    /// The periodic status task's retained handle. `stop_status_line` sends
    /// its dedicated stop signal and joins it before `shutdown_tx` wakes
    /// any client (MAN-256). The task's own guards leave a check-to-log
    /// window, so joining after a shared broadcast would be too late.
    /// `None` when `status_interval_secs = 0`. The mutex lets shutdown
    /// take the handle while holding only `&SpotServer`.
    pub(crate) status_line: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// The status task's own stop signal, sent and joined before
    /// `shutdown_tx` wakes clients (MAN-256).
    pub(crate) status_shutdown_tx: tokio::sync::watch::Sender<bool>,
    /// MAN-301: the live RBN comparator's task, `None` when `[rbn_compare]`
    /// is off. Joined after the client broadcast, bounded by
    /// `SHUTDOWN_DRAIN_DEADLINE`; its exit is not ordered against the
    /// start of the client drain.
    pub(crate) rbn_compare: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// The metrics/status HTTP listener's real bound address (MAN-44),
    /// from `TcpListener::local_addr()`, so a port-0 bind is reachable by
    /// tests without guessing or binding a fixed port.
    #[cfg_attr(not(test), allow(dead_code))]
    metrics_addr: std::net::SocketAddr,
    /// The JSON listener's real bound address, so a port-0 bind is reachable
    /// by tests -- `metrics_addr`'s MAN-44 precedent.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) json_addr: std::net::SocketAddr,
    /// MAN-324: the live-state listener's bound address; `None` when
    /// `[server].live_port` is unset, in which case the decode loop runs
    /// no live tap.
    pub(crate) live_addr: Option<std::net::SocketAddr>,
    /// MAN-78: each `[[rbn_uplink]]` task's live `dry_run` flag, in config
    /// order, for the SIGHUP reload thread (Unix only) to flip.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) uplink_dry_run: Vec<UplinkDryRun>,
}

/// MAN-78: one `[[rbn_uplink]]` target's live `dry_run` flag, read per
/// spot by `uplink::serve_with_live_dry_run`, and the same `host:port`
/// label `/metrics` uses (MAN-128 D7) for the reload's log line.
#[derive(Clone)]
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) struct UplinkDryRun {
    pub(crate) label: String,
    /// The entry's `enabled`: a disabled target runs no task, so nothing
    /// reads its flag.
    pub(crate) enabled: bool,
    pub(crate) flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// What the MAN-122 startup banner names about the live source; carried as
/// one struct, which is one field of `SpotServerParams`.
pub(crate) struct SourceInfo<'a> {
    pub(crate) name: &'a str,
    pub(crate) sample_rate_hz: f64,
    /// Also the RF centre the MAN-86 `SKIMMER/SETT` segments are derived
    /// around (`IqSource::center_freq_hz`, after any `--dial-freq-hz`
    /// override).
    pub(crate) dial_freq_hz: f64,
    /// MAN-86 review: deliberately SEPARATE from `sample_rate_hz`, not
    /// derived from it. `sample_rate_hz` is the per-sample timing quantity
    /// `SpotBus` needs to turn a sample index into wall clock;
    /// `rf_passband_hz` is the `(lo, hi)` offsets from `dial_freq_hz` of
    /// the spectrum the receiver actually delivers, and only that may be
    /// advertised to Aggregator as decodable coverage. The two differ for
    /// any resampling source, and the passband is not even symmetric for a
    /// rig-audio source -- see `IqSource::rf_passband_hz`.
    pub(crate) rf_passband_hz: (f64, f64),
    /// The same multiplicative factor `manta-engine::listen` applies to
    /// every emitted spot frequency (`--freq-correction-ppm`). MAN-86
    /// review: the advertised segments have to move with the spots, or at
    /// the supported +/-1000 ppm limit manta advertises bounds that exclude
    /// frequencies from its own spot stream.
    pub(crate) freq_calibration: f64,
    /// MAN-13 (D15): with several inputs, every input's `(centre,
    /// passband, calibration)`; `SKIMMER/SETT` advertises their union.
    /// Empty: the one source above.
    pub(crate) coverage: &'a [(f64, (f64, f64), f64)],
    /// MAN-13: the `[[input]]` names, set only with two or more inputs;
    /// the startup banner then prints the `inputs=` form.
    pub(crate) inputs: Option<&'a [String]>,
}

/// MAN-320: the `[[skimmer_relay]]`/`[[wsjtx_relay]]` blocks
/// `start_spot_server` runs (`Default` = none).
#[derive(Debug, Clone, Default)]
pub(crate) struct RelayConfigs {
    pub skimmer: Vec<manta_server::config::SkimmerRelayConfig>,
    pub wsjtx: Vec<manta_server::config::WsjtxRelayConfig>,
}

/// Everything `start_spot_server` needs, by name (MAN-548: replaces its
/// ten-parameter list and the clippy argument-count allow it needed).
pub(crate) struct SpotServerParams<'a> {
    pub(crate) cfg: manta_server::config::ServerConfig,
    pub(crate) rbn_uplink_cfgs: Vec<manta_server::config::RbnUplinkConfig>,
    pub(crate) rbn_compare_cfg: Option<manta_server::config::RbnCompareConfig>,
    pub(crate) source: SourceInfo<'a>,
    pub(crate) epoch: std::time::SystemTime,
    pub(crate) session_nonce: u128,
    pub(crate) lists: std::sync::Arc<manta_engine::ReferenceLists>,
    pub(crate) pipeline: &'a PipelineConfig,
    pub(crate) relays: RelayConfigs,
    pub(crate) outputs: spot_outputs::SpotOutputs,
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
pub(crate) fn start_spot_server(
    params: SpotServerParams<'_>,
) -> Result<(tokio::runtime::Runtime, SpotServer)> {
    let SpotServerParams {
        cfg,
        rbn_uplink_cfgs,
        rbn_compare_cfg,
        source,
        epoch,
        session_nonce,
        lists,
        pipeline,
        relays,
        outputs,
    } = params;
    let validation_level = pipeline.validation_level;
    // MAN-320 (D11): every enabled `[[wsjtx_relay]]` socket is bound before
    // anything is spawned, so a taken UDP port fails startup with the
    // relay's own message instead of leaving a half-started daemon.
    let wsjtx_sockets = relays
        .wsjtx
        .iter()
        .map(|r| {
            r.enabled
                .then(|| relay_socket::bind_wsjtx_socket(r.listen))
                .transpose()
        })
        .collect::<Result<Vec<Option<std::net::UdpSocket>>>>()?;
    // MAN-142 (D9, D13): the spot outputs' sockets are bound here too, for
    // the same reason.
    let pskreporter_socket = outputs
        .pskreporter
        .as_ref()
        .map(|p| spot_outputs::bind_pskreporter_socket(&p.host))
        .transpose()?;
    let wsjtx_output_sockets = outputs
        .wsjtx
        .iter()
        .map(|w| {
            w.enabled
                .then(|| spot_outputs::bind_wsjtx_output_socket(w.destination))
                .transpose()
        })
        .collect::<Result<Vec<Option<std::net::UdpSocket>>>>()?;
    // MAN-59 connection/rejection events below are logged at `info`/
    // `warn`. `start_spot_server` no longer installs the subscriber:
    // `logging::init` does, at the top of `run` (MAN-124), and its module
    // doc carries the stderr-not-stdout reasoning (MAN-59 round 6).

    // MAN-261: `[server]`/`[[rbn_uplink]]` arrive already parsed and
    // validated by `config::load`, before any source was opened.
    //
    // MAN-138: `config::load` already rejected an inconsistent location
    // before I/O; this re-resolution obtains the value every JSON spot
    // carries, and still rejects one for a caller that built its
    // `ServerConfig` directly.
    let station_location = cfg
        .station_location()
        .map_err(|e| anyhow!("[server]: {e}"))?;
    // MAN-142: the grid the spot outputs name the station at (empty
    // without a location).
    let station_grid = station_location
        .as_ref()
        .map(|l| l.grid.clone())
        .unwrap_or_default();
    let bus = std::sync::Arc::new(spot_bus_for(
        &cfg,
        source.sample_rate_hz,
        epoch,
        session_nonce,
    ));
    let metrics = std::sync::Arc::new(manta_server::metrics::Metrics::new());
    metrics.set_build_info(daemon_build_info());
    // MAN-83: the commit rides as SemVer build metadata, so every spot names
    // the binary that produced it (`manta-<version>+<sha>`). Same-binary
    // byte-identity (SPEC §6 item 4) holds because this is a compile-time
    // constant, and the outputs CI hashes (`decode --json`, `run --json`)
    // never carry it. Supersedes MAN-128's note keeping this string free of
    // the commit; see
    // docs/DECISIONS/2026-10-10-man83-build-identity-and-decoder-versioning.md.
    let decoder_version = build_info::DECODER_VERSION.to_string();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let (status_shutdown_tx, status_shutdown_rx) = tokio::sync::watch::channel(false);
    let tasks = manta_server::tasks::new_client_tasks();

    let rt = tokio::runtime::Runtime::new()?;
    let (status_line, metrics_addr, json_addr, live_addr, uplink_dry_run, rbn_task) =
        rt.block_on(async {
            // MAN-132: metrics binds its own `metrics_bind_addr` (loopback by
            // default), never `bind_addr`. With two configurable addresses
            // "which listener failed?" is a real question, so each bind names
            // its listener, address key and port.
            let telnet_listener =
                tokio::net::TcpListener::bind((cfg.bind_addr.as_str(), cfg.telnet_port))
                    .await
                    .with_context(|| {
                        format!(
                            "binding the telnet server (bind_addr = {}, telnet_port = {})",
                            fmt::quoted(&cfg.bind_addr),
                            cfg.telnet_port
                        )
                    })?;
            let json_listener =
                tokio::net::TcpListener::bind((cfg.bind_addr.as_str(), cfg.json_port))
                    .await
                    .with_context(|| {
                        format!(
                            "binding the JSON server (bind_addr = {}, json_port = {})",
                            fmt::quoted(&cfg.bind_addr),
                            cfg.json_port
                        )
                    })?;
            let metrics_listener =
                tokio::net::TcpListener::bind((cfg.metrics_bind_addr.as_str(), cfg.metrics_port))
                    .await
                    .with_context(|| {
                        format!(
                    "binding the metrics server (metrics_bind_addr = {}, metrics_port = {})",
                    fmt::quoted(&cfg.metrics_bind_addr),
                    cfg.metrics_port
                )
                    })?;
            let metrics_addr = metrics_listener.local_addr()?;
            let json_addr = json_listener.local_addr()?;
            // MAN-324: the live-state listener, only when `live_port` is set.
            let live_listener = match cfg.live_port {
                Some(live_port) => Some(
                    tokio::net::TcpListener::bind((cfg.live_bind_addr.as_str(), live_port))
                        .await
                        .with_context(|| {
                            format!(
                            "binding the live-state server (live_bind_addr = {}, live_port = {})",
                            fmt::quoted(&cfg.live_bind_addr),
                            live_port
                        )
                        })?,
                ),
                None => None,
            };
            let live_addr = live_listener
                .as_ref()
                .map(tokio::net::TcpListener::local_addr)
                .transpose()?;
            // MAN-322 D5: the RTTY-only telnet listener, only when
            // `rtty_telnet_port` is set. It binds `bind_addr`, like
            // `telnet_port`.
            let rtty_telnet_listener = match cfg.rtty_telnet_port {
                Some(rtty_telnet_port) => Some(
                    tokio::net::TcpListener::bind((cfg.bind_addr.as_str(), rtty_telnet_port))
                        .await
                        .with_context(|| {
                            format!(
                                "binding the RTTY telnet server (bind_addr = {}, \
                                 rtty_telnet_port = {})",
                                fmt::quoted(&cfg.bind_addr),
                                rtty_telnet_port
                            )
                        })?,
                ),
                None => None,
            };
            let rtty_telnet_addr = rtty_telnet_listener
                .as_ref()
                .map(tokio::net::TcpListener::local_addr)
                .transpose()?;

            // MAN-122 scenario 1. Emitted after every bind succeeds (so a bind
            // failure never produces a banner at all) but BEFORE any listener task is
            // spawned -- on a multi-thread runtime a spawned accept loop can admit a
            // client immediately, and its per-connection line would otherwise be able
            // to land ahead of the banner. `local_addr()`, not the configured port,
            // so a `*_port = 0` (ephemeral) config still names the real address.
            //
            // Review round 2: this line says `listening:`, not `ready:`. Three
            // bound sockets are not evidence that anything will ever be decoded --
            // a replay shorter than `manta_engine`'s two-second calibration window,
            // or a live source that fails its first reads, exits here with the
            // pipeline never having started. The readiness event proper is emitted
            // from the decode loop's first batch (see `format_pipeline_ready`'s
            // call site).
            tracing::info!(
                target: "manta",
                "{}",
                manta_server::status::format_startup_banner(&manta_server::status::StartupInfo {
                    version: env!("CARGO_PKG_VERSION"),
                    source: source.name,
                    sample_rate_hz: source.sample_rate_hz,
                    dial_freq_hz: source.dial_freq_hz,
                    station_callsign: &cfg.station_callsign,
                    telnet_addr: telnet_listener.local_addr()?,
                    json_addr: json_listener.local_addr()?,
                    metrics_addr: metrics_listener.local_addr()?,
                    live_addr,
                    rtty_telnet_addr,
                    inputs: source.inputs,
                })
            );

            // MAN-128: all three binds above already succeeded (a failed bind
            // returns via `?` before this point), so every listener starts
            // "up" -- `spawn_tracked_listener` below flips each back to "down"
            // the moment its own task actually ends.
            metrics.set_listener_up(LISTENER_TELNET, true);
            metrics.set_listener_up(LISTENER_JSON, true);
            metrics.set_listener_up(LISTENER_METRICS, true);
            if live_listener.is_some() {
                metrics.enable_live();
                metrics.set_listener_up(LISTENER_LIVE, true);
            }
            if rtty_telnet_listener.is_some() {
                metrics.set_listener_up(LISTENER_RTTY_TELNET, true);
            }

            let telnet_ip_command_limiter =
                manta_server::rate_limit::IpRateLimiter::new_with_override(
                    manta_server::telnet::MAX_TELNET_COMMANDS,
                    manta_server::telnet::COMMAND_RATE_WINDOW,
                    cfg.telnet_max_commands_per_ip,
                );
            manta_server::rate_limit::spawn_stale_entry_reaper(telnet_ip_command_limiter.clone());
            // MAN-86: Aggregator will not forward spots from a source that
            // never answers SKIMMER/SETT (Aggregator manual v6.0 §9.2) --
            // `profile` carries the operator identity for the greeting banner
            // and the live-passband segments for the SETT reply.
            if cfg.operator_grid.is_none() || cfg.operator_qth.is_none() {
                tracing::warn!(
                    target: "manta",
                    "telnet greeting will omit QTH/grid -- set [server].operator_qth and \
                 operator_grid so RBN Aggregator can record this node's location \
                 (Aggregator manual v6.0 §9.2)"
                );
            }
            // MAN-138: without a configured position every JSON spot's
            // deGrid/deLat/deLon is null -- legal, but a map shows nothing.
            if station_location.is_none() {
                tracing::warn!(
                    target: "manta",
                    "JSON spots will carry no station position -- set [server].operator_grid, or \
                 operator_lat and operator_lon, so deGrid/deLat/deLon name this node's location"
                );
            }
            let mut profile = station_profile(
                &cfg,
                source.dial_freq_hz,
                source.rf_passband_hz,
                source.freq_calibration,
                validation_level,
            );
            if !source.coverage.is_empty() {
                profile.sett.segments = manta_server::sett::segments_for_inputs(source.coverage);
            }
            let profile = std::sync::Arc::new(profile);
            // MAN-322 D5: an RTTY telnet listener speaks the CW listener's
            // protocol and serves RTTY spots only, with its own client cap,
            // per-IP quota and command budget (built from the same
            // `telnet_*` keys).
            if let Some(rtty_telnet_listener) = rtty_telnet_listener {
                let rtty_telnet_ip_command_limiter =
                    manta_server::rate_limit::IpRateLimiter::new_with_override(
                        manta_server::telnet::MAX_TELNET_COMMANDS,
                        manta_server::telnet::COMMAND_RATE_WINDOW,
                        cfg.telnet_max_commands_per_ip,
                    );
                manta_server::rate_limit::spawn_stale_entry_reaper(
                    rtty_telnet_ip_command_limiter.clone(),
                );
                spawn_tracked_listener(
                    LISTENER_RTTY_TELNET,
                    metrics.clone(),
                    manta_server::telnet::serve(
                        rtty_telnet_listener,
                        bus.clone(),
                        metrics.clone(),
                        profile.clone(),
                        shutdown_rx.clone(),
                        tasks.clone(),
                        manta_server::tasks::new_connection_limiter(
                            manta_server::telnet::MAX_TELNET_CONNECTIONS,
                        ),
                        manta_server::tasks::IpQuota::new_with_override(
                            manta_server::telnet::MAX_TELNET_CONNECTIONS_PER_IP,
                            cfg.telnet_max_connections_per_ip,
                        ),
                        rtty_telnet_ip_command_limiter,
                        manta_server::tasks::CLIENT_DRAIN_DEADLINE,
                        cfg.line_format,
                        manta_spot::SpotMode::Rtty,
                    ),
                );
            }
            spawn_tracked_listener(
                LISTENER_TELNET,
                metrics.clone(),
                manta_server::telnet::serve(
                    telnet_listener,
                    bus.clone(),
                    metrics.clone(),
                    profile,
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
                    cfg.line_format,
                    manta_spot::SpotMode::Cw,
                ),
            );
            let json_ip_ping_limiter = manta_server::rate_limit::IpRateLimiter::new_with_override(
                manta_server::json_stream::MAX_INBOUND_PINGS,
                manta_server::json_stream::PING_RATE_WINDOW,
                cfg.json_max_pings_per_ip,
            );
            manta_server::rate_limit::spawn_stale_entry_reaper(json_ip_ping_limiter.clone());
            // MAN-324: the live-state listener is the same fan-out code on the
            // bus's live channel, with its own client cap, per-IP quota and
            // ping budget (built from the same `json_*` keys).
            if let Some(live_listener) = live_listener {
                let live_ip_ping_limiter =
                    manta_server::rate_limit::IpRateLimiter::new_with_override(
                        manta_server::json_stream::MAX_INBOUND_PINGS,
                        manta_server::json_stream::PING_RATE_WINDOW,
                        cfg.json_max_pings_per_ip,
                    );
                manta_server::rate_limit::spawn_stale_entry_reaper(live_ip_ping_limiter.clone());
                spawn_tracked_listener(
                    LISTENER_LIVE,
                    metrics.clone(),
                    manta_server::json_stream::serve(
                        live_listener,
                        manta_server::json_stream::JsonStreamConfig {
                            bus: bus.clone(),
                            metrics: metrics.clone(),
                            lists: lists.clone(),
                            station_call: cfg.station_callsign.clone(),
                            station_location: station_location.clone(),
                            decoder_version: decoder_version.clone(),
                            shutdown: shutdown_rx.clone(),
                            drain_deadline: manta_server::tasks::CLIENT_DRAIN_DEADLINE,
                            stream: manta_server::json_stream::StreamKind::Live,
                        },
                        tasks.clone(),
                        manta_server::tasks::new_connection_limiter(
                            manta_server::json_stream::MAX_LIVE_STREAM_CONNECTIONS,
                        ),
                        manta_server::tasks::IpQuota::new_with_override(
                            manta_server::json_stream::MAX_JSON_STREAM_CONNECTIONS_PER_IP,
                            cfg.json_max_connections_per_ip,
                        ),
                        live_ip_ping_limiter,
                    ),
                );
            }
            spawn_tracked_listener(
                LISTENER_JSON,
                metrics.clone(),
                manta_server::json_stream::serve(
                    json_listener,
                    manta_server::json_stream::JsonStreamConfig {
                        bus: bus.clone(),
                        metrics: metrics.clone(),
                        lists: lists.clone(),
                        station_call: cfg.station_callsign.clone(),
                        station_location,
                        decoder_version,
                        // .clone(): MAN-32/MAN-42's uplink::serve spawns below also
                        // need shutdown_rx -- can't let this be the moving consumer
                        // anymore now that there are more consumers.
                        shutdown: shutdown_rx.clone(),
                        drain_deadline: manta_server::tasks::CLIENT_DRAIN_DEADLINE,
                        stream: manta_server::json_stream::StreamKind::Spots,
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
                ),
            );
            // Reaps completed per-client tasks continuously, independent of
            // shutdown -- without this, `tasks` only ever shrinks at
            // shutdown_runtime_after_drain's one-time `await_all`, so ordinary
            // connect/disconnect churn grows it without bound for the life of
            // the process (round-11 review finding).
            manta_server::tasks::spawn_reaper(tasks.clone());
            // MAN-44: the WHOLE uplink registry is published before the
            // metrics/status endpoint is spawned below. On this multi-thread
            // runtime that endpoint accepts on another worker the instant its
            // task is spawned, so a probe racing a later registration loop
            // could read a half-registered (or empty) registry and be told
            // `disabled` -- `manta status` exit 0 -- for a daemon whose
            // configured targets are in fact down. Registered-but-unconnected
            // reads as down, the honest startup answer. `target_labels`
            // assigns the Prometheus label (`host:port`, `#N`-suffixed only on
            // an exact duplicate; MAN-128 D7).
            let uplink_labels = manta_server::uplink::target_labels(&rbn_uplink_cfgs);
            let enabled_uplinks = rbn_uplink_cfgs.iter().filter(|u| u.enabled).count();
            let uplink_tasks: Vec<_> = rbn_uplink_cfgs
                .into_iter()
                .zip(uplink_labels)
                .map(|(uplink_cfg, label)| {
                    let target = metrics.register_uplink_target(label.clone(), uplink_cfg.enabled);
                    // MAN-78: one live flag per entry, seeded from the file.
                    let dry_run = UplinkDryRun {
                        label,
                        enabled: uplink_cfg.enabled,
                        flag: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
                            uplink_cfg.dry_run,
                        )),
                    };
                    (uplink_cfg, target, dry_run)
                })
                .collect();
            let uplink_dry_run: Vec<UplinkDryRun> =
                uplink_tasks.iter().map(|(_, _, d)| d.clone()).collect();
            // MAN-301: enabled before the metrics endpoint spawns below, for
            // MAN-44's reason: a scrape racing startup must already see the
            // `manta_rbn_compare_*` families of a comparator that is starting.
            let rbn_compare_cfg = rbn_compare_cfg.filter(|c| c.enabled);
            if rbn_compare_cfg.is_some() {
                metrics.enable_rbn_compare();
            }
            // MAN-320: one relay series per configured block (disabled ones
            // included, reading 0), registered before the metrics endpoint
            // spawns for MAN-44's reason. Workers spawn after the uplinks.
            let skimmer_relays: Vec<_> = relays
                .skimmer
                .into_iter()
                .map(|r| {
                    let stats = metrics
                        .register_relay(r.label(), manta_server::metrics::RelayKind::Skimmer);
                    (r, stats)
                })
                .collect();
            let wsjtx_relays: Vec<_> = relays
                .wsjtx
                .into_iter()
                .zip(wsjtx_sockets)
                .map(|(r, socket)| {
                    let stats =
                        metrics.register_relay(r.label(), manta_server::metrics::RelayKind::Wsjtx);
                    (r, socket, stats)
                })
                .collect();
            // MAN-142 (D15): the spot outputs' series, registered before the
            // metrics endpoint spawns for MAN-44's reason: a configured
            // table shows its zeros, enabled or not.
            let pskreporter_stats = outputs
                .pskreporter_registered
                .then(|| metrics.register_pskreporter());
            let wsjtx_outputs: Vec<_> = outputs
                .wsjtx
                .iter()
                .zip(wsjtx_output_sockets)
                .map(|(w, socket)| (w.clone(), socket, metrics.register_wsjtx_output(w.label())))
                .collect();
            let metrics_ip_request_limiter =
                manta_server::rate_limit::IpRateLimiter::new_with_override(
                    manta_server::metrics_http::MAX_METRICS_REQUESTS_PER_IP,
                    manta_server::metrics_http::METRICS_REQUEST_RATE_WINDOW,
                    cfg.metrics_max_requests_per_ip,
                );
            manta_server::rate_limit::spawn_stale_entry_reaper(metrics_ip_request_limiter.clone());
            spawn_tracked_listener(
                LISTENER_METRICS,
                metrics.clone(),
                manta_server::metrics_http::serve(
                    metrics_listener,
                    metrics.clone(),
                    manta_server::tasks::new_connection_limiter(
                        manta_server::metrics_http::MAX_METRICS_CONNECTIONS,
                    ),
                    manta_server::tasks::IpQuota::new_with_override(
                        manta_server::metrics_http::MAX_METRICS_CONNECTIONS_PER_IP,
                        cfg.metrics_max_connections_per_ip,
                    ),
                    metrics_ip_request_limiter,
                ),
            );
            // MAN-32/MAN-42: one independent uplink::serve task per configured
            // [[rbn_uplink]] entry -- the common case for existing single-node
            // operators is no [[rbn_uplink]] tables at all (empty Vec, loop
            // body never runs), and uplink::serve itself also no-ops when
            // `enabled = false` (belt-and-suspenders, not a duplicate check:
            // this loop additionally avoids spawning a task at all when the
            // Vec is empty). Each task owns its own SpotBus subscription and
            // backoff state, so one target being down never affects another's
            // delivery or retry timing.
            // MAN-128 D6/D7: each target was registered above, before both its
            // `serve` task and the metrics endpoint, so a scrape landing before
            // the first connect attempt still sees it.
            for (uplink_cfg, target, dry_run) in uplink_tasks {
                tokio::spawn(manta_server::uplink::serve_with_live_dry_run(
                    uplink_cfg,
                    cfg.station_callsign.clone(),
                    bus.clone(),
                    target,
                    dry_run.flag,
                    shutdown_rx.clone(),
                ));
            }
            // MAN-320: relay workers for the enabled blocks only. They
            // publish onto the bus's relay channel, which only the uplinks
            // read (D4).
            for (r, stats) in skimmer_relays {
                if !r.enabled {
                    continue;
                }
                tokio::spawn(manta_server::relay::skimmer::run(
                    manta_server::relay::skimmer::SkimmerRelay {
                        host: r.host,
                        port: r.port,
                        callsign: r.callsign,
                    },
                    manta_server::rbn_compare::system_clock(),
                    bus.clone(),
                    stats,
                    shutdown_rx.clone(),
                ));
            }
            for (r, socket, stats) in wsjtx_relays {
                let Some(socket) = socket else {
                    continue;
                };
                // The label names the address actually bound, so
                // `listen = "127.0.0.1:0"` logs its real port (devex 4).
                let label = socket
                    .local_addr()
                    .map_or_else(|_| r.label(), |a| a.to_string());
                tokio::spawn(manta_server::relay::wsjtx::run(
                    manta_server::relay::wsjtx::WsjtxRelay {
                        label,
                        callsign: r.callsign,
                    },
                    socket,
                    manta_server::rbn_compare::system_clock(),
                    bus.clone(),
                    stats,
                    shutdown_rx.clone(),
                ));
            }
            // MAN-142 (D3): the spot outputs read the bus the uplinks read.
            // Each receiver is subscribed here, before the decoder starts
            // publishing, and each worker runs in the tracked task set, so
            // the shutdown drain awaits its final flush.
            if let (Some(p), Some(socket), Some(stats)) =
                (outputs.pskreporter, pskreporter_socket, pskreporter_stats)
            {
                // `config::load` refused an enabled table without a grid.
                let grid = station_grid.clone();
                let call =
                    manta_server::config::strip_ssid(&cfg.station_callsign).to_ascii_uppercase();
                logging::note(&spot_outputs::pskreporter_startup_line(&p, &call, &grid));
                let nonce = bus.session_nonce();
                let worker = manta_server::pskreporter::run(
                    manta_server::pskreporter::PskReporter {
                        host: p.host.clone(),
                        port: p.port,
                        receiver: manta_server::pskreporter::ipfix::Receiver {
                            callsign: call,
                            locator: grid,
                            software: format!("manta {}", build_info::VERSION),
                            antenna: p.antenna.clone(),
                        },
                        spot_types: p.spot_types,
                        flush_offset: manta_server::pskreporter::flush_offset(nonce),
                        flush_interval: manta_server::pskreporter::batch::FLUSH_INTERVAL,
                        observation_domain: manta_server::pskreporter::observation_domain(nonce),
                    },
                    socket,
                    manta_server::rbn_compare::system_clock(),
                    bus.clone(),
                    bus.subscribe(),
                    stats,
                    shutdown_rx.clone(),
                );
                tasks.lock().await.spawn(worker);
            }
            for (w, socket, stats) in wsjtx_outputs {
                let Some(socket) = socket else {
                    continue;
                };
                logging::note(&spot_outputs::wsjtx_output_startup_line(w.destination));
                let worker = manta_server::wsjtx_output::run(
                    manta_server::wsjtx_output::WsjtxOutput {
                        label: w.label(),
                        destination: w.destination,
                        de_call: manta_server::config::strip_ssid(&cfg.station_callsign)
                            .to_ascii_uppercase(),
                        de_grid: station_grid.clone(),
                        version: build_info::VERSION.to_string(),
                        revision: build_info::GIT_SHA.to_string(),
                        off_air: outputs.replay,
                    },
                    socket,
                    bus.clone(),
                    bus.subscribe(),
                    stats,
                    shutdown_rx.clone(),
                );
                tasks.lock().await.spawn(worker);
            }
            // MAN-301: the live comparator reads the same bus the uplinks do
            // and publishes grades back onto it. Its metric families were
            // enabled above, before the metrics endpoint was spawned.
            let rbn_compare = rbn_compare_cfg.map(|rc| {
                let station_callsign = cfg.station_callsign.clone();
                manta_server::rbn_compare::live::spawn(
                    manta_server::rbn_compare::live::LiveConfig {
                        feed: manta_server::rbn_compare::feed::FeedConfig {
                            host: rc.feed_host.clone(),
                            port: rc.feed_port,
                            login_callsign: rc
                                .effective_login_callsign(&station_callsign)
                                .to_string(),
                        },
                        grade: rc.grade_config(&station_callsign),
                        station_callsign,
                        tick: manta_server::rbn_compare::live::TICK,
                    },
                    bus.clone(),
                    metrics.clone(),
                    manta_server::rbn_compare::system_clock(),
                    shutdown_rx.clone(),
                )
            });

            // MAN-122 scenario 2. The handle travels out of this block and
            // into `SpotServer::status_line` so shutdown can join the task --
            // see that field's doc comment.
            let status_line = manta_server::status::spawn_status_line(
                metrics.clone(),
                cfg.status_interval_secs
                    .map_or(manta_server::status::DEFAULT_STATUS_INTERVAL, |secs| {
                        std::time::Duration::from_secs(secs)
                    }),
                enabled_uplinks,
                status_shutdown_rx,
            );

            anyhow::Ok((
                status_line,
                metrics_addr,
                json_addr,
                live_addr,
                uplink_dry_run,
                rbn_compare,
            ))
        })?;

    Ok((
        rt,
        SpotServer {
            bus,
            metrics,
            shutdown_tx,
            tasks,
            station_callsign: cfg.station_callsign.clone(),
            lists,
            status_line: std::sync::Mutex::new(status_line),
            status_shutdown_tx,
            rbn_compare: std::sync::Mutex::new(rbn_task),
            metrics_addr,
            json_addr,
            live_addr,
            uplink_dry_run,
        },
    ))
}

#[cfg(test)]
mod tests;
