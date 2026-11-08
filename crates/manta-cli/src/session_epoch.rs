//! The session epoch and spot-`id` nonce for a session, including a replayed
//! recording's (`resolve_epoch`, `session_nonce_for_replay_path`, the FNV-1a
//! helpers). MAN-548 moved them out of `main.rs`.

use crate::cli_parse::check_replay_epoch;
use crate::fmt;
use crate::source_spec::LiveSourceSpec;
use anyhow::{bail, Context, Result};
use std::path::Path;

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
pub(crate) fn epoch_for_replay_path(path: &std::path::Path) -> Result<std::time::SystemTime> {
    // MAN-309: a manta IQ recording knows when its sample zero was taken,
    // so a copy with a new mtime keeps its timestamps.
    if manta_input::is_native_recording(path) {
        let start = manta_input::read_recording_summary(path)?
            .header
            .start_time()
            .filter(|t| *t >= std::time::SystemTime::UNIX_EPOCH);
        return start.with_context(|| {
            format!(
                "{} records a start time before the Unix epoch (1970-01-01), which can't be \
                 used as a replay epoch -- pass --replay-epoch explicitly instead",
                path.display()
            )
        });
    }
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

/// A scheduled replay must use a recorded or explicit epoch, never file mtime.
pub(crate) fn cw_replay_epoch(
    path: &Path,
    explicit: Option<i64>,
    scheduled: bool,
    input_name: &str,
) -> Result<(i64, u32)> {
    if let Some(epoch) = explicit {
        return Ok((epoch, 0));
    }
    if !scheduled {
        return Ok((0, 0));
    }
    if !manta_input::is_native_recording(path) {
        bail!(
            "input {}: scheduled file input requires replay_epoch or a native recording epoch",
            fmt::quoted(input_name)
        );
    }
    let epoch = epoch_for_replay_path(path)?.duration_since(std::time::UNIX_EPOCH)?;
    check_replay_epoch("recording epoch", i64::try_from(epoch.as_secs())?)
        .map(|secs| (secs, epoch.subsec_nanos()))
        .map_err(|e| anyhow::anyhow!(e))
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
pub(crate) fn resolve_epoch(
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

/// Epoch of sample zero for an individual FTx input, captured when it opens.
pub(crate) fn resolve_ftx_epoch(
    spec: &LiveSourceSpec,
    replay_epoch: Option<i64>,
    acquired_at: std::time::SystemTime,
) -> Result<std::time::SystemTime> {
    match spec {
        LiveSourceSpec::File { path, .. } => resolve_epoch(Some(path), replay_epoch),
        _ => Ok(acquired_at),
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
pub(crate) fn session_nonce_for_replay_path(path: &std::path::Path) -> Result<u128> {
    const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    let mut hash = fnv1a_file(OFFSET_BASIS, path)?;
    // MAN-309: a manta IQ recording's metadata (start, gaps, guards) is part
    // of the recording, so it continues the same hash. Its name is not.
    if manta_input::is_native_recording(path) {
        hash = fnv1a_recording_metadata(hash, path)?;
    }
    Ok(hash as u128)
}

/// MAN-309: continues `hash` over the replay-relevant content of `payload`'s
/// metadata: the header and every gap, each re-serialized canonically, and
/// the stored sample count. The end reason (`eof`, `stopped`,
/// `backpressure`) says how the capture ended, not what it replays, so it
/// is left out, as is the formatting of the file itself.
fn fnv1a_recording_metadata(mut hash: u64, payload: &std::path::Path) -> Result<u64> {
    use std::io::BufRead;

    // Validates every line (and bounds its length) before the pass below.
    manta_input::read_recording_summary(payload)?;
    let meta = manta_input::metadata_path(payload);
    let file = std::fs::File::open(&meta)
        .with_context(|| format!("opening {} to derive its replay identity", meta.display()))?;
    for line in std::io::BufReader::new(file).lines() {
        let line = line
            .with_context(|| format!("reading {} to derive its replay identity", meta.display()))?;
        let record: manta_input::recording_format::MetadataRecord =
            serde_json::from_str(&line).with_context(|| format!("parsing {}", meta.display()))?;
        let canonical = match record {
            manta_input::recording_format::MetadataRecord::End { sample_count, .. } => {
                format!("end {sample_count}\n")
            }
            other => other.to_line(),
        };
        hash = fnv1a_bytes(hash, canonical.as_bytes());
    }
    Ok(hash)
}

/// FNV-1a-64 over `bytes`, continuing from `hash`.
fn fnv1a_bytes(mut hash: u64, bytes: &[u8]) -> u64 {
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// FNV-1a-64 over `path`'s bytes, continuing from `hash`.
pub(crate) fn fnv1a_file(mut hash: u64, path: &std::path::Path) -> Result<u64> {
    use std::io::Read;

    let mut file = std::fs::File::open(path)
        .with_context(|| format!("opening {} to derive its replay identity", path.display()))?;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("reading {} to derive its replay identity", path.display()))?;
        if n == 0 {
            break;
        }
        hash = fnv1a_bytes(hash, &buf[..n]);
    }
    Ok(hash)
}
