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

fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("usage: close_counts <wav>");
    let src = WavIqSource::open(Path::new(&path))?;
    let cfg = PipelineConfig::default();
    let report = soak_with_metrics(
        Box::new(src),
        &cfg,
        Duration::from_secs(3600),
        Duration::from_secs(3600),
        |_sample| {},
    )?;
    println!("{:#?}", report);
    Ok(())
}
