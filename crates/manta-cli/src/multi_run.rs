//! MAN-13: `manta run` with several `[[input]]` entries. Every input opens
//! and decodes on its own thread (`manta_engine::open_inputs`/
//! `listen_inputs`) and all of them feed one validator, so a station heard
//! by two inputs is spotted once on the one spot output. A live input that
//! drops out reconnects on its own thread (`ReconnectingSource`) and logs
//! its health edges; an input that ends in error stops only itself.

use crate::reconnect::{HealthSink, ReconnectingSource};
use crate::{
    fmt, logging, text_lines, wrap_file_replay, LiveSourceSpec, ReplayMode, ResolvedInput,
};
use anyhow::{bail, Result};
use manta_engine::{InputSpec, PipelineConfig, SessionClock, SharedObservers};
use manta_server::metrics::Metrics;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Instant, SystemTime};

/// What `run` hands the multi-input path.
pub(crate) struct MultiRun<'a> {
    pub inputs: &'a [ResolvedInput],
    pub replay: ReplayMode,
    pub cfg: &'a PipelineConfig,
    pub json: bool,
    /// `Some` with a `[server]` table: one telnet/JSON/metrics server for
    /// every input.
    pub server: Option<ServerSetup>,
}

/// The `[server]` side of a multi-input daemon.
pub(crate) struct ServerSetup {
    pub cfg: manta_server::config::ServerConfig,
    pub rbn_uplink: Vec<manta_server::config::RbnUplinkConfig>,
    pub rbn_compare: Option<manta_server::config::RbnCompareConfig>,
    pub lists: Arc<manta_engine::ReferenceLists>,
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
                "input {:?}{what} carries no frequency of its own: set center_freq_hz in its \
                 [[input]] entry",
                input.name
            );
        }
    }
    Ok(())
}

/// `{decoding} of {configured} inputs decoding`.
fn decoding_line(healthy: &AtomicUsize, n: usize) -> String {
    format!(
        "{} of {n} inputs decoding",
        healthy.load(Ordering::Relaxed).min(n)
    )
}

/// Runs every input until each has ended. Exit 0 when every input ended
/// cleanly; an input that ended in error makes the run fail, naming it.
pub(crate) fn run(m: MultiRun<'_>) -> Result<std::process::ExitCode> {
    check_rf_references(m.inputs)?;
    let n = m.inputs.len();
    let names: Vec<String> = m.inputs.iter().map(|i| i.name.clone()).collect();
    let live = m.inputs.iter().any(|i| i.spec.is_reconnectable());
    let stop = Arc::new(AtomicBool::new(false));
    let stop_handler = stop.clone();
    ctrlc::set_handler(move || stop_handler.store(true, Ordering::Relaxed))?;
    #[cfg(unix)]
    crate::install_unix_signal_handlers(&stop, false)?;
    let healthy = Arc::new(AtomicUsize::new(n));
    let opened_at: Arc<Mutex<Vec<Option<Instant>>>> = Arc::new(Mutex::new(vec![None; n]));
    // Set once the server is up; the health sinks run on input threads.
    let metrics_slot: Arc<OnceLock<Arc<Metrics>>> = Arc::new(OnceLock::new());
    let crashes: Vec<Arc<AtomicU64>> = (0..n).map(|_| Arc::new(AtomicU64::new(0))).collect();

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
            let metrics_slot = metrics_slot.clone();
            InputSpec {
                name: input.name.clone(),
                freq_correction_ppm: input.freq_correction_ppm,
                observers: manta_engine::InputObservers {
                    static_crashes: Some(crashes[index].clone()),
                    ..Default::default()
                },
                open: Box::new(move || {
                    let first = spec.open(capture, dial)?;
                    if let Ok(mut at) = opened_at.lock() {
                        at[index] = Some(Instant::now());
                    }
                    let reopen = spec.clone();
                    if spec.is_reconnectable() {
                        let on_health: HealthSink = Box::new(move |up| {
                            if let Some(metrics) = metrics_slot.get() {
                                metrics.set_source_health(&name, up);
                            }
                            if up {
                                healthy.fetch_add(1, Ordering::Relaxed);
                                tracing::info!(
                                    "input {name}: back; {}",
                                    decoding_line(&healthy, n)
                                );
                            } else {
                                let _ = healthy.fetch_update(
                                    Ordering::Relaxed,
                                    Ordering::Relaxed,
                                    |h| h.checked_sub(1),
                                );
                                tracing::warn!(
                                    "input {name}: lost; retrying, {}",
                                    decoding_line(&healthy, n)
                                );
                            }
                        });
                        Ok(Box::new(ReconnectingSource::new(
                            spec.name(),
                            first,
                            Box::new(move || reopen.open(capture, dial)),
                            stop,
                            true,
                            on_health,
                        )) as Box<dyn manta_input::IqSource>)
                    } else {
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
    for input in opened.inputs() {
        tracing::info!(
            "manta {} input {}: sample_rate_hz={} dial_frequency={:.1} kHz",
            env!("CARGO_PKG_VERSION"),
            input.name,
            input.sample_rate_hz,
            input.center_freq_hz / 1e3
        );
    }
    let clock = if live {
        let at = opened_at.lock().map(|a| a.clone()).unwrap_or_default();
        let epoch = at.iter().flatten().min().copied();
        SessionClock::Live {
            offsets: at
                .iter()
                .map(|t| match (t, epoch) {
                    (Some(t), Some(e)) => t.saturating_duration_since(e),
                    _ => std::time::Duration::ZERO,
                })
                .collect(),
        }
    } else {
        SessionClock::Replay
    };
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
                },
                epoch,
                nonce,
                setup.lists,
                m.cfg,
            )?;
            server.metrics.arm_decode_watchdog();
            for name in &names {
                server.metrics.set_source_health(name, true);
            }
            let _ = metrics_slot.set(server.metrics.clone());
            Some((rt, server))
        }
    };
    logging::note(&format!(
        "manta: listening on {n} inputs ({}); send SIGINT or SIGTERM to stop",
        names.join(", ")
    ));
    let mut tracks = vec![0usize; n];

    let json = m.json;
    let mut text = text_lines::TrackLines::default();
    let mut ready = vec![false; n];
    let stop_ready = stop.clone();
    let ends = manta_engine::listen_inputs(
        opened,
        m.cfg,
        clock,
        SharedObservers::default(),
        |ev| {
            if json {
                println!("{}", serde_json::to_string(ev).unwrap_or_default());
            } else if let Some(line) = text.ingest(ev) {
                eprintln!("{line}");
            }
        },
        |spot| {
            if let Some((_, server)) = &server {
                let truncated = server.bus.publish(spot.clone());
                if truncated > 0 {
                    server.metrics.record_history_truncated(truncated as u64);
                }
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
            tracks[input] = n_tracks;
            if let Some((_, server)) = &server {
                server
                    .metrics
                    .set_active_tracks(tracks.iter().sum::<usize>() as u64);
                server.metrics.set_static_crashes_total(
                    crashes.iter().map(|c| c.load(Ordering::Relaxed)).sum(),
                );
                server.metrics.record_pipeline_batch();
            }
            if !ready[input] && !stop_ready.load(Ordering::Relaxed) {
                ready[input] = true;
                tracing::info!(
                    "manta {} ready: decoding source={}",
                    env!("CARGO_PKG_VERSION"),
                    names[input]
                );
            }
        },
        |end| {
            if let Some((_, server)) = &server {
                crate::record_terminal_source_health(&server.metrics, &end.name, &end.result);
            }
            if let Err(e) = &end.result {
                let _ = healthy
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |h| h.checked_sub(1));
                tracing::error!(
                    "input {}: stopped ({e:#}); {}",
                    end.name,
                    decoding_line(&healthy, n)
                );
            }
        },
    )?;
    if !json {
        for line in text.finish() {
            eprintln!("{line}");
        }
    }
    if let Some((rt, server)) = server {
        server.metrics.mark_decode_stopped();
        server.metrics.set_active_tracks(0);
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
            "input {:?} ended in error: {}",
            failed.name,
            e.unwrap_or_default()
        );
    }
    Ok(std::process::ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoding_line_counts_healthy_inputs() {
        let healthy = AtomicUsize::new(2);
        assert_eq!(decoding_line(&healthy, 3), "2 of 3 inputs decoding");
        healthy.store(5, Ordering::Relaxed);
        assert_eq!(decoding_line(&healthy, 3), "3 of 3 inputs decoding");
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
