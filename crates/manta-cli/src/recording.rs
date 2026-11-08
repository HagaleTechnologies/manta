//! `--record-iq`: the destination checks before any source opens, starting
//! the recordings, and finishing and reporting them inside the shutdown
//! budget. MAN-548 moved it out of `main.rs`.

use crate::logging;
use crate::resolve::ResolvedInput;
use anyhow::{anyhow, bail, Result};
use manta_input::IqSource;
use std::path::Path;

/// MAN-309: `--record-iq`'s startup check, before any listener or source
/// opens: the payload, its metadata and the partial marker must not exist,
/// and the directory must. `start_recording` creates them exclusively.
pub(crate) fn check_record_destination(path: &Path) -> Result<()> {
    for p in [
        path.to_path_buf(),
        manta_input::metadata_path(path),
        manta_input::partial_metadata_path(path),
    ] {
        if p.exists() {
            bail!("IQ recording destination already exists: {}", p.display());
        }
    }
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    if !parent.is_dir() {
        bail!(
            "IQ recording directory does not exist: {}",
            parent.display()
        );
    }
    Ok(())
}

/// MAN-309: each `[[input]]`'s `record_iq` passes `check_record_destination`
/// and no payload, metadata or partial marker overlaps another input's.
pub(crate) fn check_record_destinations(inputs: &[ResolvedInput]) -> Result<()> {
    let mut seen: Vec<String> = Vec::new();
    for input in inputs {
        let Some(path) = input.record_iq.as_deref() else {
            continue;
        };
        check_record_destination(path)?;
        // The directory exists (checked above), so its canonical form
        // catches two spellings of one destination (`a/../x`, a symlinked
        // directory). Compared without ASCII case, since macOS and Windows
        // filesystems ignore it by default: on a case-sensitive one this
        // refuses two names that differ only in case, which is harmless.
        let canonical = match (path.parent(), path.file_name()) {
            (Some(dir), Some(name)) => {
                let dir = if dir.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    dir
                };
                dir.canonicalize()
                    .map_or_else(|_| path.to_path_buf(), |d| d.join(name))
            }
            _ => path.to_path_buf(),
        };
        for reserved in [
            manta_input::metadata_path(&canonical),
            manta_input::partial_metadata_path(&canonical),
            canonical,
        ] {
            let reserved = reserved.to_string_lossy().to_ascii_lowercase();
            if seen.contains(&reserved) {
                bail!(
                    "two [[input]] entries record to {}; give each its own record_iq",
                    path.display()
                );
            }
            seen.push(reserved);
        }
    }
    Ok(())
}

/// MAN-309: starts recording `src` to `path` and logs where.
pub(crate) fn start_input_recording(
    src: Box<dyn IqSource>,
    path: &Path,
    start: std::time::SystemTime,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<(Box<dyn IqSource>, manta_input::RecordingHandle)> {
    let (rec, handle) = manta_input::start_recording(
        src,
        path,
        start,
        Some(Box::new(|msg: &str| {
            logging::warning(&format!("warning: {msg}"))
        })),
    )?;
    logging::note(&format!(
        "recording IQ to {}; metadata: {}",
        path.display(),
        manta_input::metadata_path(path).display()
    ));
    Ok((Box::new(rec.with_stop_flag(stop)), handle))
}

/// MAN-309: how long `run` gives its recordings, in total, to finalize
/// once decoding ends. The writers finalize while the clients drain, and
/// only what the drain left of this is waited out after it, so recording
/// adds nothing to the stop budget the service manager allows
/// (`TimeoutStopSec=60` against the 50 s `SHUTDOWN_DRAIN_DEADLINE` plus
/// 2 s of runtime shutdown) beyond this bound when the drain is shorter.
/// With `RECORDING_PUBLICATION_GRACE` after it, it still ends inside that
/// drain (MAN-453).
pub(crate) const RECORDING_FINISH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// MAN-453: how long after `RECORDING_FINISH_TIMEOUT` a writer that has
/// already begun publishing may take to finish. One window for every
/// recording of the run, as the timeout is one deadline, so recording's
/// part of a stop ends 7 s after `close_recordings`, inside the drain.
pub(crate) const RECORDING_PUBLICATION_GRACE: std::time::Duration =
    std::time::Duration::from_secs(2);

/// MAN-453: what `finish` gets at `now` for a recording closed with
/// `deadline`: the rest of the timeout, then the rest of the shared grace.
pub(crate) fn recording_finish_budget(
    deadline: std::time::Instant,
    now: std::time::Instant,
) -> (std::time::Duration, std::time::Duration) {
    let grace_end = deadline + RECORDING_PUBLICATION_GRACE;
    (
        deadline.saturating_duration_since(now),
        grace_end.saturating_duration_since(deadline.max(now)),
    )
}

/// MAN-453: finishes one recording with what is left of `deadline` and of
/// the publication grace after it.
fn finish_by(
    handle: manta_input::RecordingHandle,
    deadline: std::time::Instant,
) -> manta_input::RecordingOutcome {
    let (timeout, grace) = recording_finish_budget(deadline, std::time::Instant::now());
    handle.finish(timeout, grace)
}

/// MAN-309: closes every recording's queue once decoding has ended, so
/// each writer finalizes during the client drain. Returns the one deadline
/// `finish_recordings` waits until, however many inputs record.
pub(crate) fn close_recordings(handles: &[manta_input::RecordingHandle]) -> std::time::Instant {
    for handle in handles {
        handle.close();
    }
    std::time::Instant::now() + RECORDING_FINISH_TIMEOUT
}

/// MAN-309: collects every recording of `run` by `deadline` (from
/// `close_recordings`) and logs each outcome. `Err` (the first failure)
/// when any recording did not complete, which `run` turns into its exit
/// status after the drain; one failing recording does not stop the others
/// from finishing.
pub(crate) fn finish_recordings(
    handles: impl IntoIterator<Item = manta_input::RecordingHandle>,
    deadline: std::time::Instant,
) -> Result<()> {
    let mut result = Ok(());
    for handle in handles {
        let finished = finish_recording(handle, deadline);
        if result.is_ok() {
            result = finished;
        }
    }
    result
}

/// MAN-309: finalizes one recording, waiting no later than `deadline`.
fn finish_recording(
    handle: manta_input::RecordingHandle,
    deadline: std::time::Instant,
) -> Result<()> {
    let recorded = handle.path().to_path_buf();
    let outcome = finish_by(handle, deadline);
    report_recording(&recorded, outcome)
}

/// MAN-309: on an early return from a multi-input `run`, finalizes one
/// recording by `deadline` and removes the published pair when it holds no
/// samples: nothing was recorded, and the destination would refuse the
/// next start. Any other outcome is kept and logged as `finish_recording`
/// reports it; the run is failing already, so nothing is returned.
pub(crate) fn discard_empty_recording(
    handle: manta_input::RecordingHandle,
    deadline: std::time::Instant,
) {
    let recorded = handle.path().to_path_buf();
    match finish_by(handle, deadline) {
        manta_input::RecordingOutcome::Finished { samples: 0, .. } => {
            let removed = std::fs::remove_file(manta_input::metadata_path(&recorded))
                .and_then(|()| std::fs::remove_file(&recorded));
            match removed {
                Ok(()) => logging::note(&format!(
                    "removed IQ recording {}: it holds no samples",
                    recorded.display()
                )),
                Err(e) => logging::warning(&format!(
                    "warning: cannot remove the empty IQ recording {}: {e}",
                    recorded.display()
                )),
            }
        }
        outcome => {
            if let Err(e) = report_recording(&recorded, outcome) {
                logging::warning(&format!("warning: {e:#}"));
            }
        }
    }
}

/// MAN-309: logs how the recording at `recorded` ended; `Err` when it did
/// not complete.
fn report_recording(recorded: &Path, outcome: manta_input::RecordingOutcome) -> Result<()> {
    match outcome {
        manta_input::RecordingOutcome::Finished {
            path,
            samples,
            reason,
        } => {
            if reason == manta_input::EndReason::Backpressure {
                bail!(
                    "IQ recording {} holds only the first {samples} samples: the writer fell \
                     behind",
                    path.display()
                );
            }
            logging::note(&format!(
                "recording finished: {}, {samples} samples",
                path.display()
            ));
            Ok(())
        }
        // Its cause was reported once, as a warning, when it happened.
        manta_input::RecordingOutcome::Failed(_) => Err(anyhow!(
            "IQ recording {} is incomplete: it stopped early (see the warning above)",
            recorded.display()
        )),
        manta_input::RecordingOutcome::TimedOut(path) => {
            logging::warning(&format!(
                "warning: IQ recording finalization timed out: {}; completion is unconfirmed",
                path.display()
            ));
            Err(anyhow!(
                "IQ recording {} is unconfirmed: it did not finalize in time (see the warning \
                 above)",
                path.display()
            ))
        }
    }
}
