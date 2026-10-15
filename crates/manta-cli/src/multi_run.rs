//! MAN-13: `manta run` with several `[[input]]` entries. Every input opens
//! and decodes on its own thread (`manta_engine::open_inputs`/
//! `listen_inputs`) and all of them feed one validator, so a station heard
//! by two inputs is spotted once on the one spot output. A live input that
//! drops out reconnects on its own thread (`ReconnectingSource`) and logs
//! its health edges; an input that ends in error stops only itself.

use crate::reconnect::{HealthSink, ReconnectingSource};
use crate::{fmt, logging, text_lines, wrap_file_replay, LiveSourceSpec, ReplayMode, ResolvedInput};
use anyhow::{bail, Result};
use manta_engine::{InputSpec, PipelineConfig, SessionClock, SharedObservers};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// What `run` hands the multi-input path.
pub(crate) struct MultiRun<'a> {
    pub inputs: &'a [ResolvedInput],
    pub replay: ReplayMode,
    pub cfg: &'a PipelineConfig,
    pub json: bool,
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
    let healthy = Arc::new(AtomicUsize::new(n));
    let opened_at: Arc<Mutex<Vec<Option<Instant>>>> = Arc::new(Mutex::new(vec![None; n]));

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
            InputSpec {
                name: input.name.clone(),
                freq_correction_ppm: input.freq_correction_ppm,
                observers: Default::default(),
                open: Box::new(move || {
                    let first = spec.open(capture, dial)?;
                    if let Ok(mut at) = opened_at.lock() {
                        at[index] = Some(Instant::now());
                    }
                    let reopen = spec.clone();
                    if spec.is_reconnectable() {
                        let on_health: HealthSink = Box::new(move |up| {
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
    logging::note(&format!(
        "manta: listening on {n} inputs ({}); send SIGINT or SIGTERM to stop",
        names.join(", ")
    ));

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
            if json {
                println!("{}", serde_json::json!({ "spot": spot }));
            } else {
                println!("{}", fmt::spot_line(spot));
            }
        },
        |input, _n_tracks| {
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
            if let Err(e) = &end.result {
                let _ = healthy.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |h| {
                    h.checked_sub(1)
                });
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
    if let Some(failed) = ends.iter().find(|e| e.result.is_err()) {
        let e = failed.result.as_ref().err().map(|e| format!("{e:#}"));
        bail!("input {:?} ended in error: {}", failed.name, e.unwrap_or_default());
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
