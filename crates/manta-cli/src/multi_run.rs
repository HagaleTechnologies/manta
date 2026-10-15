//! MAN-13: `manta run` with several `[[input]]` entries. Every input opens
//! and decodes on its own thread (`manta_engine::open_inputs`/
//! `listen_inputs`) and all of them feed one validator, so a station heard
//! by two inputs is spotted once on the one spot output. A live input that
//! drops out reconnects on its own thread (`ReconnectingSource`) and logs
//! its health edges; an input that ends in error stops only itself.
//! A daemon's SIGHUP reload (MAN-78), the scheduled list refresh (MAN-316),
//! RTTY marks (MAN-314), each input's `manta_input_*` counters (MAN-56) and
//! the live-state stream (MAN-324, one `bandState` per input) work as with
//! one input.

use crate::reconnect::{HealthSink, InputHealthTotals, ReconnectingSource};
use crate::{
    fmt, logging, text_lines, wrap_file_replay, LiveSourceSpec, ReplayMode, ResolvedInput,
    RunLists, SpotResolved,
};
use anyhow::{bail, Result};
use manta_engine::{InputSpec, PipelineConfig, RttyObserver, SessionClock, SharedObservers};
use manta_server::metrics::Metrics;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

/// What `run` hands the multi-input path.
pub(crate) struct MultiRun<'a> {
    pub inputs: &'a [ResolvedInput],
    pub replay: ReplayMode,
    pub cfg: &'a PipelineConfig,
    pub json: bool,
    /// `--decoded-text`: with a `[server]` table, decoded text is logged
    /// only when asked (MAN-123), as in the one-input path.
    pub decoded_text: bool,
    /// `Some` with a `[server]` table: one telnet/JSON/metrics server for
    /// every input.
    pub server: Option<ServerSetup>,
    /// MAN-316: the lists every input's spots are checked against, and the
    /// refresher to start once the servers are up.
    pub run_lists: RunLists,
    pub spot: &'a SpotResolved,
    /// MAN-78: what a SIGHUP reload re-resolves with; `Some` on Unix.
    pub reload: Option<ReloadSetup>,
}

/// MAN-78: startup's view of the config, for `reload::ReloadContext`.
/// Built on Unix only: elsewhere there is no SIGHUP.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) struct ReloadSetup {
    pub config_path: Option<std::path::PathBuf>,
    pub cli: crate::CliOverrides,
    pub cli_engine: Option<manta_decode::decoder::Engine>,
    pub vars: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    pub startup_raw: toml::Table,
}

/// The `[server]` side of a multi-input daemon.
pub(crate) struct ServerSetup {
    pub cfg: manta_server::config::ServerConfig,
    pub rbn_uplink: Vec<manta_server::config::RbnUplinkConfig>,
    pub rbn_compare: Option<manta_server::config::RbnCompareConfig>,
    pub replay_epoch: Option<i64>,
}

/// The session nonce for several replayed files: each file's own nonce,
/// folded in config order, so the same files give the same spot ids.
fn replay_nonce(paths: &[&std::path::Path]) -> Result<u128> {
    let mut nonce: u128 = 0;
    for path in paths {
        nonce = nonce.rotate_left(1) ^ crate::session_nonce_for_replay_path(path)?;
    }
    Ok(nonce)
}

/// `input "b" (b.wav) carries no frequency of its own`: with several
/// inputs every one needs an RF reference, or its frequencies cannot be
/// compared with the others'.
pub(crate) fn check_rf_references(inputs: &[ResolvedInput]) -> Result<()> {
    for input in inputs {
        if input.dial_freq_hz.is_none() && !input.spec.is_rf_aware() {
            let what = match &input.spec {
                LiveSourceSpec::File { path, .. } => format!(" ({})", path.display()),
                _ => String::new(),
            };
            bail!(
                "input {}{what} carries no frequency of its own: set center_freq_hz in its \
                 [[input]] entry",
                fmt::quoted(&input.name)
            );
        }
    }
    Ok(())
}

/// The sum of per-input counters (`manta_active_tracks`, D9).
fn sum(counters: &[Arc<AtomicU64>]) -> u64 {
    counters.iter().map(|c| c.load(Ordering::Relaxed)).sum()
}

/// Each input's open-completion time after the earliest one (the session
/// epoch), in input order; an input with no time reads zero.
fn live_offsets(opened_at: &[Option<Instant>]) -> Vec<Duration> {
    let epoch = opened_at.iter().flatten().min().copied();
    opened_at
        .iter()
        .map(|t| match (t, epoch) {
            (Some(t), Some(e)) => t.saturating_duration_since(e),
            _ => Duration::ZERO,
        })
        .collect()
}

/// Two inputs' marks closer than this are one RTTY signal heard twice (a
/// station in two receivers' overlap): well under the narrowest standard
/// shift, 170 Hz.
const SAME_RTTY_MARK_HZ: f64 = 50.0;

/// MAN-314: every input's marked RTTY frequencies, in input order;
/// `manta_rtty_signals` counts their union.
#[derive(Default)]
struct RttyMarks(Mutex<Vec<Vec<f64>>>);

impl RttyMarks {
    fn new(n: usize) -> Self {
        Self(Mutex::new(vec![Vec::new(); n]))
    }

    /// Replaces input `index`'s marks; returns the union to publish,
    /// ascending, a mark another input also reports counted once.
    fn set(&self, index: usize, marks_hz: Vec<f64>) -> Vec<f64> {
        let mut all = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = all.get_mut(index) {
            *slot = marks_hz;
        }
        let mut marks: Vec<f64> = all.iter().flatten().copied().collect();
        marks.sort_by(f64::total_cmp);
        marks.dedup_by(|b, a| *b - *a < SAME_RTTY_MARK_HZ);
        marks
    }
}

/// What one input's health sink updates besides its own flag.
struct InputHealthState {
    healthy: Arc<Vec<AtomicBool>>,
    tracks: Vec<Arc<AtomicU64>>,
    rtty: Arc<RttyMarks>,
    metrics: Arc<OnceLock<Arc<Metrics>>>,
}

/// MAN-73 per input: the `ReconnectingSource` health sink of input
/// `index`. A down edge zeroes only this input's track count and RTTY
/// marks (its thread is blocked reconnecting, so it cannot publish them
/// itself, MAN-45) and republishes the sums; every edge after the first
/// report logs the `k of n inputs decoding` line.
fn input_health_sink(index: usize, name: String, state: InputHealthState) -> HealthSink {
    let InputHealthState {
        healthy,
        tracks,
        rtty,
        metrics,
    } = state;
    // The first healthy report of a source that started unconfirmed is
    // its start, not a recovery.
    let mut ever_up = false;
    // The report at construction is the initial state.
    let mut constructed = false;
    Box::new(move |up| {
        let initial = !std::mem::replace(&mut constructed, true);
        let union = (!up).then(|| {
            tracks[index].store(0, Ordering::Relaxed);
            rtty.set(index, Vec::new())
        });
        let was = healthy[index].swap(up, Ordering::Relaxed);
        if let Some(metrics) = metrics.get() {
            if let Some(union) = &union {
                metrics.set_active_tracks(sum(&tracks));
                metrics.set_rtty_signals(union);
            }
            metrics.set_source_health(&name, up);
        }
        let recovered = !initial && up && !was && ever_up;
        let lost = !initial && !up && was;
        ever_up |= up;
        if recovered {
            tracing::info!("input {name}: back; {}", decoding_line(&healthy));
        } else if lost {
            tracing::warn!("input {name}: lost; retrying, {}", decoding_line(&healthy));
        }
    })
}

/// `{decoding} of {configured} inputs decoding`.
fn decoding_line(healthy: &[AtomicBool]) -> String {
    let decoding = healthy.iter().filter(|h| h.load(Ordering::Relaxed)).count();
    format!("{decoding} of {} inputs decoding", healthy.len())
}

/// An input that has ended while others may still run delivers no more
/// samples: `manta_source_health` reads 0 for it from here on, and the
/// inputs still running keep their own state. A clean end after a stop
/// request is the daemon's shutdown, not a failure, so it is left alone as
/// the one-input path's `record_terminal_source_health` leaves it (no
/// outage counted).
fn publish_input_end(metrics: &Metrics, name: &str, result: &Result<()>, stopping: bool) {
    if result.is_err() || !stopping {
        metrics.set_source_health_terminal(name, false);
    }
}

/// An ended input decodes nothing more, whether it ended cleanly or not:
/// its track count and RTTY marks leave `manta_active_tracks` and
/// `manta_rtty_signals`, which the other inputs' batches keep publishing as
/// sums. A clean end's last batch already zeroed them; an error or a panic
/// sent none.
fn retire_input_gauges(
    metrics: &Metrics,
    index: usize,
    tracks: &[Arc<AtomicU64>],
    rtty: &RttyMarks,
) {
    tracks[index].store(0, Ordering::Relaxed);
    let union = rtty.set(index, Vec::new());
    metrics.set_active_tracks(sum(tracks));
    metrics.set_rtty_signals(&union);
}

/// Runs every input until each has ended. Exit 0 when every input ended
/// cleanly; an input that ended in error makes the run fail, naming it.
pub(crate) fn run(m: MultiRun<'_>) -> Result<std::process::ExitCode> {
    check_rf_references(m.inputs)?;
    let n = m.inputs.len();
    let names: Vec<String> = m.inputs.iter().map(|i| i.name.clone()).collect();
    let live = m.inputs.iter().any(|i| i.spec.is_reconnectable());
    // MAN-78: a daemon's SIGHUP is a reload, handled on the reload thread
    // alone: blocked here, before the input threads and the servers start,
    // so each inherits the mask (`reload::mask_sighup`).
    #[cfg(unix)]
    if m.server.is_some() {
        use anyhow::Context as _;
        crate::reload::mask_sighup(libc::SIG_BLOCK)
            .context("blocking SIGHUP on the decode threads")?;
    }
    let stop = Arc::new(AtomicBool::new(false));
    let stop_handler = stop.clone();
    ctrlc::set_handler(move || stop_handler.store(true, Ordering::Relaxed))?;
    #[cfg(unix)]
    let reload_signals = crate::install_unix_signal_handlers(&stop, m.server.is_some())?;
    // Each input's last reported health; "k of n inputs decoding" counts
    // the healthy ones. A `ReconnectingSource` reports its initial state at
    // construction, so only a change of state is an edge worth a log line.
    let healthy: Arc<Vec<AtomicBool>> = Arc::new((0..n).map(|_| AtomicBool::new(true)).collect());
    let opened_at: Arc<Mutex<Vec<Option<Instant>>>> = Arc::new(Mutex::new(vec![None; n]));
    // Set once the server is up; the health sinks run on input threads.
    let metrics_slot: Arc<OnceLock<Arc<Metrics>>> = Arc::new(OnceLock::new());
    let crashes: Vec<Arc<AtomicU64>> = (0..n).map(|_| Arc::new(AtomicU64::new(0))).collect();
    // MAN-234: each input's replaced-sample count, read on the back end's
    // thread after each of its batches, as the one-input path reads one
    // counter; `bad_warned` holds what the one-time warning already named.
    let bad: Vec<Arc<AtomicU64>> = (0..n).map(|_| Arc::new(AtomicU64::new(0))).collect();
    let bad_warned: Vec<std::cell::Cell<u64>> = (0..n).map(|_| std::cell::Cell::new(0)).collect();
    // Each input's active-track count; `manta_active_tracks` is their sum
    // (D9). A lost input's health sink zeroes only its own count.
    let tracks: Vec<Arc<AtomicU64>> = (0..n).map(|_| Arc::new(AtomicU64::new(0))).collect();
    // MAN-314: each input's marked RTTY set; the metric is their union.
    let rtty_observers: Vec<Arc<RttyObserver>> =
        (0..n).map(|_| Arc::new(RttyObserver::default())).collect();
    let rtty_marks = Arc::new(RttyMarks::new(n));
    // MAN-56: each input's packet counters, taken on its own thread at open.
    let input_counters: Arc<Mutex<Vec<Option<Arc<InputHealthTotals>>>>> =
        Arc::new(Mutex::new(vec![None; n]));

    let specs = m
        .inputs
        .iter()
        .enumerate()
        .map(|(index, input)| {
            let spec = input.spec.clone();
            let (capture, dial) = (input.capture_rate_hz, input.dial_freq_hz);
            let name = input.name.clone();
            let replay = m.replay;
            let stop = stop.clone();
            let healthy = healthy.clone();
            let opened_at = opened_at.clone();
            let health = InputHealthState {
                healthy: healthy.clone(),
                tracks: tracks.clone(),
                rtty: rtty_marks.clone(),
                metrics: metrics_slot.clone(),
            };
            let input_counters = input_counters.clone();
            let bad = bad[index].clone();
            InputSpec {
                name: input.name.clone(),
                freq_correction_ppm: input.freq_correction_ppm,
                observers: manta_engine::InputObservers {
                    static_crashes: Some(crashes[index].clone()),
                    bad_samples: Some(bad),
                    rtty: Some(rtty_observers[index].clone()),
                    ..Default::default()
                },
                open: Box::new(move || {
                    let first = spec.open(capture, dial)?;
                    if let Ok(mut at) = opened_at.lock() {
                        at[index] = Some(Instant::now());
                    }
                    let reopen = spec.clone();
                    if spec.is_reconnectable() {
                        // As the one-input path: a source that confirms it
                        // is live (Kiwi, ka9q) is healthy once it has.
                        let initial_healthy = first.confirmed_live_handle().is_none();
                        let on_health = input_health_sink(index, name.clone(), health);
                        let wrapped = ReconnectingSource::new(
                            name,
                            first,
                            Box::new(move || reopen.open(capture, dial)),
                            stop,
                            initial_healthy,
                            on_health,
                        );
                        if let Ok(mut slot) = input_counters.lock() {
                            slot[index] = wrapped.input_health();
                        }
                        Ok(Box::new(wrapped) as Box<dyn manta_input::IqSource>)
                    } else {
                        if let Ok(mut slot) = input_counters.lock() {
                            slot[index] = InputHealthTotals::for_source(first.as_ref());
                        }
                        let label = match &spec {
                            LiveSourceSpec::File { path, .. } => path.display().to_string(),
                            _ => String::new(),
                        };
                        wrap_file_replay(
                            first,
                            replay,
                            label,
                            Box::new(move || reopen.open(capture, dial)),
                        )
                    }
                }),
            }
        })
        .collect();

    let opened = manta_engine::open_inputs(specs, stop.clone())?;
    for (input, resolved) in opened.inputs().iter().zip(m.inputs) {
        tracing::info!(
            "{}",
            manta_server::status::format_input_line(
                &input.name,
                resolved.spec.name(),
                input.sample_rate_hz,
                input.center_freq_hz
            )
        );
        // MAN-196: as the one-input path, a decimated input says so -- its
        // rim channels outside the decimator's trusted band are not
        // tracked, and the nominal Nyquist coverage is not coming.
        if let Some((lo, hi)) = input.trusted_band_hz.map(|_| input.rf_passband_hz) {
            tracing::info!(
                "{}",
                crate::trusted_band_notice(input.sample_rate_hz, lo, hi)
            );
        }
    }
    let clock = if live {
        let at = opened_at.lock().map(|a| a.clone()).unwrap_or_default();
        SessionClock::Live {
            offsets: live_offsets(&at),
        }
    } else {
        SessionClock::Replay
    };
    // Each input's rate, for its own ready line; `listen_inputs` takes
    // `opened` below.
    let rates: Vec<f64> = opened.inputs().iter().map(|i| i.sample_rate_hz).collect();
    let mut shared = SharedObservers::default();
    let lists = m.run_lists.lists.clone();
    let server = match m.server {
        None => None,
        Some(setup) => {
            let first = &opened.inputs()[0];
            let files: Vec<&std::path::Path> = m
                .inputs
                .iter()
                .filter_map(|i| match &i.spec {
                    LiveSourceSpec::File { path, .. } => Some(path.as_path()),
                    _ => None,
                })
                .collect();
            let (epoch, nonce) = if live {
                let earliest = opened_at
                    .lock()
                    .ok()
                    .and_then(|a| a.iter().flatten().min().copied());
                let since = earliest.map_or(std::time::Duration::ZERO, |e| e.elapsed());
                let epoch = SystemTime::now()
                    .checked_sub(since)
                    .unwrap_or_else(SystemTime::now);
                let nonce = epoch
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos());
                (epoch, nonce)
            } else {
                (
                    crate::resolve_epoch(files.first().copied(), setup.replay_epoch)?,
                    replay_nonce(&files)?,
                )
            };
            let label = names.join(",");
            // D15: SKIMMER/SETT advertises every input's segment.
            let coverage = opened
                .inputs()
                .iter()
                .zip(m.inputs)
                .map(|(o, i)| {
                    let factor = manta_spot::calibration_factor_from_ppm(i.freq_correction_ppm)
                        .map_err(|e| anyhow::anyhow!(e))?;
                    Ok((o.center_freq_hz, o.rf_passband_hz, factor))
                })
                .collect::<Result<Vec<_>>>()?;
            let factor = manta_spot::calibration_factor_from_ppm(m.inputs[0].freq_correction_ppm)
                .map_err(|e| anyhow::anyhow!(e))?;
            let (rt, server) = crate::start_spot_server(
                setup.cfg,
                setup.rbn_uplink,
                setup.rbn_compare,
                crate::SourceInfo {
                    name: &label,
                    sample_rate_hz: opened.common_rate_hz(),
                    dial_freq_hz: first.center_freq_hz,
                    rf_passband_hz: first.rf_passband_hz,
                    freq_calibration: factor,
                    coverage: &coverage,
                    inputs: Some(&names),
                },
                epoch,
                nonce,
                lists.clone(),
                m.cfg,
            )?;
            // MAN-324: one `bandState` per input per stream second, each
            // naming its input; track and decision records as with one.
            if server.live_addr.is_some() {
                shared.live_state = Some(Arc::new(
                    crate::live_state::LiveSink::new(
                        server.bus.clone(),
                        server.metrics.clone(),
                        &server.station_callsign,
                    )
                    .with_sources(&names),
                ) as Arc<dyn manta_engine::LiveStateSink>);
            }
            // MAN-56: each input's packet counters, keyed by its name;
            // published once now so every series exists from the first
            // scrape. Inputs without a wire-loss model publish none.
            let counters = input_counters.lock().map(|c| c.clone()).unwrap_or_default();
            for (name, counters) in names.iter().zip(counters) {
                let Some(counters) = counters else { continue };
                let metrics = server.metrics.clone();
                let name = name.clone();
                metrics.set_input_health(&name, counters.snapshot());
                rt.spawn(async move {
                    loop {
                        tokio::time::sleep(crate::INPUT_HEALTH_POLL_INTERVAL).await;
                        metrics.set_input_health(&name, counters.snapshot());
                    }
                });
            }
            // MAN-128: the decode watchdog progresses while any input's
            // front end processes chunks (D9); the poller copies the shared
            // observers into `Metrics`, as the one-input path does.
            let latency = Arc::new(manta_engine::DecodeLatencyObserver::new());
            let quality = Arc::new(manta_engine::DecodeQualityObserver::new());
            server
                .metrics
                .set_decode_latency(crate::latency_histogram_of(&latency.snapshot()));
            server
                .metrics
                .set_decode_quality(crate::decode_quality_of(&quality.snapshot()));
            shared.decode_latency = Some(latency.clone());
            shared.decode_quality = Some(quality.clone());
            let poll_metrics = server.metrics.clone();
            rt.spawn(async move {
                loop {
                    tokio::time::sleep(crate::DECODE_LATENCY_POLL_INTERVAL).await;
                    poll_metrics
                        .set_decode_latency(crate::latency_histogram_of(&latency.snapshot()));
                    poll_metrics.set_decode_quality(crate::decode_quality_of(&quality.snapshot()));
                }
            });
            server.metrics.arm_decode_watchdog();
            // Health edges from here on reach `Metrics` directly; each
            // input's state so far is copied in once.
            let _ = metrics_slot.set(server.metrics.clone());
            for (name, up) in names.iter().zip(healthy.iter()) {
                server
                    .metrics
                    .set_source_health(name, up.load(Ordering::Relaxed));
                // MAN-234: every input's bad-sample series exists from the
                // first scrape, as the one-input path publishes it.
                server.metrics.set_input_bad_samples(name, 0);
            }
            Some((rt, server))
        }
    };
    // MAN-316: after the `listening:` banner, as with one input.
    shared.reference_lists = crate::start_list_refresh(
        m.run_lists,
        live,
        m.spot,
        server.as_ref().map(|(_, server)| server.metrics.clone()),
        stop.clone(),
    )?;
    // MAN-78: with a `[server]` table, SIGHUP reloads the `[spot]` lists
    // every input's spots are checked against and each `dry_run`.
    let operator_lists = Arc::new(manta_engine::OperatorListsUpdate::default());
    let drain_gate = Arc::new(crate::reload::DrainGate::default());
    #[cfg(unix)]
    let reload_enabled = match (reload_signals, &server, m.reload) {
        (Some(signals), Some((_, server)), Some(setup)) => {
            crate::reload::spawn(
                signals,
                crate::reload::ReloadContext {
                    config_path: setup.config_path,
                    cli: setup.cli,
                    cli_engine: setup.cli_engine,
                    vars: setup.vars,
                    startup_raw: setup.startup_raw,
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
    let reload_enabled = {
        let _ = m.reload;
        false
    };
    if reload_enabled {
        shared.operator_lists = Some(operator_lists);
    }
    logging::note(&format!(
        "manta: listening on {n} inputs ({}); send SIGINT or SIGTERM to stop{}",
        names.join(", "),
        if reload_enabled {
            ", SIGHUP to reload [spot] lists and dry_run"
        } else {
            ""
        }
    ));

    let json = m.json;
    // MAN-123: a daemon logs decoded text only with --decoded-text.
    let print_text = !json && (server.is_none() || m.decoded_text);
    let mut text = text_lines::TrackLines::default();
    let mut ready = vec![false; n];
    let mut rtty_logs: Vec<crate::RttyLogState> = rates
        .iter()
        .map(|&fs| crate::RttyLogState::new(fs))
        .collect();
    let stop_ready = stop.clone();
    let stop_end = stop.clone();
    let ends = manta_engine::listen_inputs(
        opened,
        m.cfg,
        clock,
        shared,
        |ev| {
            if json {
                println!("{}", serde_json::to_string(ev).unwrap_or_default());
            } else if print_text {
                if let Some(line) = text.ingest(ev) {
                    // As the one-input path: `--log-format json` makes
                    // every stderr line a JSON record (MAN-124), so the
                    // grouped text rides a `manta::text` event there.
                    if logging::is_json() {
                        tracing::info!(target: "manta::text", "{line}");
                    } else {
                        eprintln!("{line}");
                    }
                }
            }
        },
        |spot| {
            if let Some((_, server)) = &server {
                let truncated = server.bus.publish(spot.clone());
                server
                    .metrics
                    .record_history_truncated(truncated.history_truncated as u64);
                server.metrics.record_json_stream_history_truncated(
                    truncated.stream_history_truncated as u64,
                );
                server.metrics.record_spot(spot);
                let cty = server.lists.cty.load();
                if crate::geography_is_unresolved(&cty, &spot.callsign)
                    || crate::station_geography_unresolved(&cty, &server.station_callsign)
                {
                    server.metrics.record_unresolved_geography();
                }
            }
            if json {
                println!("{}", serde_json::json!({ "spot": spot }));
            } else {
                println!("{}", fmt::spot_line(spot));
            }
        },
        |input, n_tracks| {
            // A batch queued before an outage, handled after the input's
            // health sink zeroed its count and marks, must not bring them
            // back for the whole outage (MAN-45); the first batch after
            // the reconnect publishes them again.
            if healthy[input].load(Ordering::Relaxed) {
                tracks[input].store(n_tracks as u64, Ordering::Relaxed);
                if let Some(marks_hz) = rtty_logs[input].poll(&rtty_observers[input]) {
                    let union = rtty_marks.set(input, marks_hz);
                    if let Some((_, server)) = &server {
                        server.metrics.set_rtty_signals(&union);
                    }
                }
            }
            // MAN-234: warn once at this input's first replaced sample and
            // keep its metric current, as the one-input path does per
            // `on_tracks`.
            let n_bad = bad[input].load(Ordering::Relaxed);
            if n_bad > 0 && bad_warned[input].get() == 0 {
                logging::warning(&fmt::bad_samples_live_warning(
                    &names[input],
                    n_bad,
                    server.is_some(),
                ));
                bad_warned[input].set(n_bad);
            }
            if let Some((_, server)) = &server {
                server.metrics.set_input_bad_samples(&names[input], n_bad);
            }
            if let Some((_, server)) = &server {
                server.metrics.set_active_tracks(sum(&tracks));
                server.metrics.set_static_crashes_total(sum(&crashes));
                server.metrics.record_pipeline_batch();
            }
            if !ready[input] && !stop_ready.load(Ordering::Relaxed) {
                ready[input] = true;
                tracing::info!(
                    "{}",
                    manta_server::status::format_pipeline_ready(
                        env!("CARGO_PKG_VERSION"),
                        &names[input],
                        rates[input]
                    )
                );
            }
        },
        |end| {
            if let Some((_, server)) = &server {
                retire_input_gauges(&server.metrics, end.index, &tracks, &rtty_marks);
                let stopping = stop_end.load(Ordering::Relaxed);
                publish_input_end(&server.metrics, &end.name, &end.result, stopping);
            }
            // MAN-234: the input's total, only when it grew past the count
            // its first-sample line named (a clean and an error end alike).
            let bad_total = bad[end.index].load(Ordering::Relaxed);
            if bad_total > bad_warned[end.index].get() {
                logging::warning(&fmt::bad_samples_warning(&end.name, bad_total));
                bad_warned[end.index].set(bad_total);
            }
            if let Some(flag) = healthy.get(end.index) {
                flag.store(false, Ordering::Relaxed);
            }
            if let Err(e) = &end.result {
                tracing::error!(
                    "input {}: stopped ({e:#}); {}",
                    end.name,
                    decoding_line(&healthy)
                );
            }
        },
    )?;
    // MAN-78 D7: draining from here on; a later SIGHUP is ignored.
    drain_gate.close();
    stop_ready.store(true, Ordering::Relaxed);
    if print_text {
        for line in text.finish() {
            if logging::is_json() {
                tracing::info!(target: "manta::text", "{line}");
            } else {
                eprintln!("{line}");
            }
        }
    }
    if let Some((rt, server)) = server {
        server.metrics.mark_decode_stopped();
        server.metrics.set_active_tracks(0);
        server.metrics.set_rtty_signals(&[]);
        let _ = server.shutdown_tx.send(true);
        let status_line = server
            .status_line
            .lock()
            .ok()
            .and_then(|mut handle| handle.take());
        if let Some(status_line) = status_line {
            let _ = rt.block_on(status_line);
        }
        let rbn_compare = server
            .rbn_compare
            .lock()
            .ok()
            .and_then(|mut handle| handle.take());
        if let Some(rbn_compare) = rbn_compare {
            let _ = rt.block_on(tokio::time::timeout(
                crate::SHUTDOWN_DRAIN_DEADLINE,
                rbn_compare,
            ));
        }
        crate::shutdown_runtime_after_drain(rt, &server.tasks);
    }
    if let Some(failed) = ends.iter().find(|e| e.result.is_err()) {
        let e = failed.result.as_ref().err().map(|e| format!("{e:#}"));
        bail!(
            "input {} ended in error: {}",
            fmt::quoted(&failed.name),
            e.unwrap_or_default()
        );
    }
    Ok(std::process::ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_nonce_for_one_file_is_todays_and_for_several_hashes_them_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.wav");
        let b = dir.path().join("b.wav");
        std::fs::write(&a, b"first recording").unwrap();
        std::fs::write(&b, b"second recording").unwrap();
        let one = crate::session_nonce_for_replay_path(&a).unwrap();
        assert_eq!(replay_nonce(&[&a]).unwrap(), one);
        let ab = replay_nonce(&[&a, &b]).unwrap();
        assert_eq!(ab, replay_nonce(&[&a, &b]).unwrap());
        assert_ne!(ab, one);
        assert_ne!(ab, replay_nonce(&[&b, &a]).unwrap());
    }

    #[test]
    fn live_offsets_are_measured_from_the_earliest_open() {
        let t0 = Instant::now();
        let at = [
            Some(t0 + Duration::from_millis(250)),
            Some(t0),
            None,
            Some(t0 + Duration::from_secs(2)),
        ];
        assert_eq!(
            live_offsets(&at),
            [
                Duration::from_millis(250),
                Duration::ZERO,
                Duration::ZERO,
                Duration::from_secs(2)
            ]
        );
        assert_eq!(live_offsets(&[None, None]), [Duration::ZERO; 2]);
    }

    fn health_state(n: usize, metrics: &Arc<Metrics>) -> InputHealthState {
        let slot = Arc::new(OnceLock::new());
        let _ = slot.set(metrics.clone());
        InputHealthState {
            healthy: Arc::new((0..n).map(|_| AtomicBool::new(true)).collect()),
            tracks: (0..n).map(|_| Arc::new(AtomicU64::new(0))).collect(),
            rtty: Arc::new(RttyMarks::new(n)),
            metrics: slot,
        }
    }

    #[test]
    fn source_health_sink_zeroes_only_its_own_inputs_tracks() {
        let metrics = Arc::new(Metrics::new());
        let state = health_state(3, &metrics);
        for (i, count) in [4, 5, 6].into_iter().enumerate() {
            state.tracks[i].store(count, Ordering::Relaxed);
        }
        state.rtty.set(0, vec![14_080_000.0]);
        state.rtty.set(1, vec![7_040_000.0]);
        let (tracks, healthy) = (state.tracks.clone(), state.healthy.clone());
        let mut sink = input_health_sink(1, "b".into(), state);
        sink(true); // the report at construction
        assert_eq!(tracks[1].load(Ordering::Relaxed), 5);
        sink(false);
        let counts: Vec<u64> = tracks.iter().map(|t| t.load(Ordering::Relaxed)).collect();
        assert_eq!(counts, [4, 0, 6]);
        assert_eq!(metrics.active_tracks(), 10);
        assert_eq!(metrics.source_health_counts(), (0, 1));
        assert_eq!(decoding_line(&healthy), "2 of 3 inputs decoding");
        sink(true);
        assert_eq!(metrics.source_health_counts(), (1, 1));
    }

    #[test]
    fn rtty_marks_publish_the_union_and_a_lost_input_clears_only_its_own() {
        let marks = RttyMarks::new(2);
        assert_eq!(marks.set(0, vec![14_080_000.0]), [14_080_000.0]);
        assert_eq!(
            marks.set(1, vec![7_040_000.0, 7_045_000.0]),
            [7_040_000.0, 7_045_000.0, 14_080_000.0]
        );
        // One signal both receivers mark counts once.
        assert_eq!(marks.set(1, vec![14_080_012.0]), [14_080_000.0]);
        assert_eq!(marks.set(1, Vec::new()), [14_080_000.0]);
    }

    #[test]
    fn an_input_that_ended_cleanly_reads_down_and_the_others_keep_their_state() {
        let metrics = Metrics::new();
        metrics.set_source_health("a", true);
        metrics.set_source_health("b", true);
        publish_input_end(&metrics, "a", &Ok(()), false);
        assert_eq!(metrics.source_health_counts(), (1, 2));
        // Terminal: a late regular write cannot bring it back.
        metrics.set_source_health("a", true);
        assert_eq!(metrics.source_health_counts(), (1, 2));
    }

    #[test]
    fn a_clean_end_at_shutdown_is_not_an_outage_but_an_error_is() {
        let metrics = Metrics::new();
        metrics.set_source_health("a", true);
        metrics.set_source_health("b", true);
        publish_input_end(&metrics, "a", &Ok(()), true);
        assert_eq!(metrics.source_health_counts(), (2, 2));
        publish_input_end(&metrics, "b", &Err(anyhow::anyhow!("boom")), true);
        assert_eq!(metrics.source_health_counts(), (1, 2));
    }

    #[test]
    fn an_ended_input_leaves_the_summed_track_and_rtty_gauges() {
        let metrics = Metrics::new();
        let tracks: Vec<Arc<AtomicU64>> = [4, 5].map(|c| Arc::new(AtomicU64::new(c))).into();
        let marks = RttyMarks::new(2);
        marks.set(0, vec![7_040_000.0]);
        metrics.set_rtty_signals(&marks.set(1, vec![14_080_000.0]));
        metrics.set_active_tracks(sum(&tracks));
        retire_input_gauges(&metrics, 1, &tracks, &marks);
        assert_eq!(metrics.active_tracks(), 4);
        let text = metrics.render_prometheus_text();
        assert!(
            text.contains(r#"manta_rtty_signals{band="20m"} 0"#),
            "{text}"
        );
        assert!(
            text.contains(r#"manta_rtty_signals{band="40m"} 1"#),
            "{text}"
        );
    }

    #[test]
    fn decoding_line_counts_healthy_inputs() {
        let healthy: Vec<AtomicBool> = [true, false, true].map(AtomicBool::new).into();
        assert_eq!(decoding_line(&healthy), "2 of 3 inputs decoding");
        healthy[1].store(true, Ordering::Relaxed);
        assert_eq!(decoding_line(&healthy), "3 of 3 inputs decoding");
    }

    #[test]
    fn an_input_without_a_frequency_is_refused_naming_it() {
        let inputs = [ResolvedInput {
            name: "b".into(),
            spec: LiveSourceSpec::AudioDevice(None),
            freq_correction_ppm: 0.0,
            dial_freq_hz: None,
            capture_rate_hz: None,
        }];
        let err = check_rf_references(&inputs).unwrap_err().to_string();
        assert_eq!(
            err,
            "input \"b\" carries no frequency of its own: set center_freq_hz in its [[input]] \
             entry"
        );
    }
}
