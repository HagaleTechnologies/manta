//! Prints `TrackManager`'s close-reason breakdown (`unconfirmed`/
//! `hang_expired`/`silent`/`merged`/`evicted`) plus peak concurrent active
//! tracks and total events/spots for a WAV file, via the already-public
//! `soak_with_metrics` API. Built for MAN-166's real-recording diagnosis
//! (`crates/manta-testkit/audio-corpus/README.md`) -- these close reasons
//! aren't visible from `manta decode --json`'s `DecodeReport` (which has
//! no equivalent field), so this is the way to see *why* tracks are
//! dying, not just how many spots came out. Usage: `close_counts <wav>`
//! (needs a same-stem `.json` sidecar for a real center frequency, same
//! as `manta decode`).
use manta_engine::{soak_with_metrics, PipelineConfig};
use manta_input::WavIqSource;
use std::path::Path;
use std::time::Duration;

/// Generous enough that no real recording plausible for this corpus
/// (ARCHITECTURE.md's audio-corpus scope) should ever hit it, but a hard
/// cap on a WAV file that hangs is still real: without one, this tool
/// would just block forever instead of failing loudly (Codex review, PR
/// #152, on the previous 1h cap silently truncating a slower-than-1h
/// run's report with no indication it had happened at all).
const MAX_DURATION: Duration = Duration::from_secs(24 * 3600);

fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("usage: close_counts <wav>");
    let src = WavIqSource::open(Path::new(&path))?;
    let cfg = PipelineConfig::default();
    let report = soak_with_metrics(
        Box::new(src),
        &cfg,
        MAX_DURATION,
        MAX_DURATION,
        |_sample| {},
    )?;
    // Two distinct ways this report can be a partial one, neither
    // obvious from the printed counts alone -- both must fail loudly
    // (Codex review, PR #152) rather than let a truncated or panicked
    // run be read as if it were a clean, complete count.
    if report.panicked {
        anyhow::bail!(
            "the decode pipeline panicked before finishing -- every count above is a \
             PARTIAL report of whatever ran before the panic, not the whole file"
        );
    }
    if report.duration_actual >= MAX_DURATION {
        anyhow::bail!(
            "hit the {MAX_DURATION:?} processing cap before the file finished -- \
             every count above is a PARTIAL report of a truncated prefix, not the whole file"
        );
    }
    println!("{:#?}", report);
    Ok(())
}
