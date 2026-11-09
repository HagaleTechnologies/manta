use super::*;
use crate::test_support::*;
use std::path::PathBuf;

#[test]
fn epoch_for_replay_path_rejects_a_pre_unix_epoch_mtime() {
    // Regression (round-8 review): a Unix filesystem can represent an
    // mtime before 1970 (rare, but real). Left unvalidated, that
    // SystemTime flows all the way to SpotBus::unix_ts_for, whose
    // `.duration_since(UNIX_EPOCH).expect(...)` panics on the very
    // first spot delivered to any client -- a crash discovered at
    // spot-delivery time instead of a clean error at startup.
    let f = write_temp_file(b"pre-epoch mtime fixture");
    let pre_epoch = std::time::SystemTime::UNIX_EPOCH - std::time::Duration::from_secs(1);
    // Windows requires FILE_WRITE_ATTRIBUTES on the handle to call
    // SetFileTime; a read-only `File::open` handle (as used previously)
    // fails with PermissionDenied there regardless of the target time
    // value -- open with write access instead.
    std::fs::OpenOptions::new()
        .write(true)
        .open(f.path())
        .unwrap()
        .set_modified(pre_epoch)
        .expect("this platform must support setting mtime for the test to be meaningful");

    let result = epoch_for_replay_path(f.path());
    assert!(
        result.is_err(),
        "a pre-1970 mtime must be rejected at startup, not deferred to a later panic"
    );
}

#[test]
fn resolve_epoch_ignores_replay_epoch_for_a_live_session() {
    // Regression (round-8 review): --replay-epoch's own doc comment
    // says "ignored for a live source," but the old match applied it
    // unconditionally on `Some(secs)` regardless of `replay_path`. A
    // live session given the flag would then publish spots with a
    // fabricated historical timestamp AND derive its session_nonce
    // from that same fixed value instead of a fresh nanosecond-
    // precision now -- breaking the "two live sessions started within
    // the same wall-clock second don't collide" guarantee entirely,
    // since every live start with the same flag value would collide.
    let before = std::time::SystemTime::now();
    let epoch = resolve_epoch(None, Some(1_751_635_200)).unwrap();
    let after = std::time::SystemTime::now();
    assert!(
        epoch >= before && epoch <= after,
        "a live session (no replay_path) must ignore --replay-epoch and use now(), got {epoch:?}"
    );
}

#[test]
fn resolve_epoch_prefers_an_explicit_replay_epoch_over_file_mtime() {
    // --replay-epoch must win even when a replay path is also given --
    // it's the escape hatch for exactly the case where mtime isn't
    // trustworthy (a copy/download that didn't preserve it).
    let f = write_temp_file(b"resolve_epoch fixture");
    let explicit =
        std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_751_635_200);
    assert_eq!(
        resolve_epoch(Some(f.path()), Some(1_751_635_200)).unwrap(),
        explicit
    );
}

#[test]
fn resolve_epoch_falls_back_to_file_mtime_for_replay_without_an_explicit_epoch() {
    let f = write_temp_file(b"resolve_epoch fixture");
    assert_eq!(
        resolve_epoch(Some(f.path()), None).unwrap(),
        epoch_for_replay_path(f.path()).unwrap()
    );
}

/// MAN-309: a manta IQ recording pair in `dir`, `gap` its one gap.
fn native_pair(dir: &Path, name: &str, start_secs: i64, gap: u64) -> PathBuf {
    use manta_input::recording_format::MetadataRecord;
    let path = dir.join(name);
    std::fs::write(&path, [0u8; 80]).unwrap();
    let header = manta_input::RecordingHeader {
        schema: "manta-iq.v1".into(),
        sample_format: "cf32_le".into(),
        sample_rate_hz: 48_000.0,
        center_freq_hz: 14_000_000.0,
        start_unix_seconds: start_secs,
        start_nanoseconds: 500,
        rf_passband_hz: [-24_000.0, 24_000.0],
        trusted_band_hz: None,
        analytic_guard_hz: 0.0,
    };
    let meta = MetadataRecord::Header(header).to_line()
        + &MetadataRecord::Gap {
            before_sample: 5,
            missing_samples: gap,
        }
        .to_line()
        + &MetadataRecord::End {
            sample_count: 10,
            reason: manta_input::EndReason::Stopped,
        }
        .to_line();
    std::fs::write(manta_input::metadata_path(&path), meta).unwrap();
    path
}

#[test]
fn ftx_epoch_preserves_live_acquisition_before_startup_finishes() {
    let opened = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_792_407_602);
    let acquired = opened - std::time::Duration::from_millis(1500);
    assert_eq!(
        resolve_ftx_epoch(&LiveSourceSpec::AudioDevice(None), Some(123), acquired).unwrap(),
        acquired
    );
}

#[test]
fn ftx_epoch_uses_each_native_input_and_keeps_live_time_in_a_mixed_session() {
    let dir = tempfile::tempdir().unwrap();
    let acquired = std::time::SystemTime::now();
    for (name, seconds) in [("a.cf32", 1_792_407_600), ("b.cf32", 1_792_407_615)] {
        let path = native_pair(dir.path(), name, seconds, 0);
        let spec = LiveSourceSpec::File {
            path,
            source_iq: true,
            raw: None,
        };
        assert_eq!(
            resolve_ftx_epoch(&spec, None, acquired).unwrap(),
            std::time::UNIX_EPOCH + std::time::Duration::new(seconds as u64, 500)
        );
    }
    assert_eq!(
        resolve_ftx_epoch(&LiveSourceSpec::AudioDevice(None), None, acquired).unwrap(),
        acquired
    );
}

#[test]
fn a_native_recording_replays_at_its_recorded_start_not_its_mtime() {
    let dir = tempfile::tempdir().unwrap();
    let rec = native_pair(dir.path(), "a.cf32", 1_792_407_600, 7);
    let recorded = std::time::UNIX_EPOCH + std::time::Duration::new(1_792_407_600, 500);
    assert_eq!(resolve_epoch(Some(&rec), None).unwrap(), recorded);
    // --replay-epoch still wins.
    assert_eq!(
        resolve_epoch(Some(&rec), Some(1_751_635_200)).unwrap(),
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_751_635_200)
    );
    // A pre-1970 recorded start is refused, as a pre-1970 mtime is.
    let old = native_pair(dir.path(), "old.cf32", -5, 7);
    assert!(resolve_epoch(Some(&old), None).is_err());
}

#[test]
fn a_native_recordings_nonce_covers_its_metadata_but_not_its_name() {
    let dir = tempfile::tempdir().unwrap();
    let a = native_pair(dir.path(), "a.cf32", 1_792_407_600, 7);
    let renamed = native_pair(dir.path(), "renamed.cf32", 1_792_407_600, 7);
    let other_gap = native_pair(dir.path(), "gap.cf32", 1_792_407_600, 8);
    let other_start = native_pair(dir.path(), "start.cf32", 1_792_407_601, 7);
    let nonce = session_nonce_for_replay_path(&a).unwrap();
    assert_eq!(session_nonce_for_replay_path(&renamed).unwrap(), nonce);
    assert_ne!(session_nonce_for_replay_path(&other_gap).unwrap(), nonce);
    assert_ne!(session_nonce_for_replay_path(&other_start).unwrap(), nonce);
    // A plain file's nonce is still its bytes' hash alone.
    let plain = dir.path().join("plain.cf32");
    std::fs::write(&plain, [0u8; 80]).unwrap();
    assert_eq!(
        session_nonce_for_replay_path(&plain).unwrap(),
        u128::from(fnv1a_file(0xcbf29ce484222325, &plain).unwrap())
    );
    assert_ne!(session_nonce_for_replay_path(&plain).unwrap(), nonce);
}

/// MAN-309: copying a recording pair and touching its mtime keeps the
/// server's epoch and spot identity; how the capture ended (the end
/// reason) and the metadata's formatting are not part of the identity.
#[test]
fn a_copied_recording_with_a_new_mtime_keeps_its_epoch_and_nonce() {
    let dir = tempfile::tempdir().unwrap();
    let a = native_pair(dir.path(), "a.cf32", 1_792_407_600, 7);
    let copy = dir.path().join("copy.cf32");
    std::fs::copy(&a, &copy).unwrap();
    std::fs::copy(
        manta_input::metadata_path(&a),
        manta_input::metadata_path(&copy),
    )
    .unwrap();
    let touched = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
    for p in [copy.clone(), manta_input::metadata_path(&copy)] {
        std::fs::File::options()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(touched)
            .unwrap();
    }
    assert_eq!(
        std::fs::metadata(&copy).unwrap().modified().unwrap(),
        touched
    );
    assert_eq!(
        resolve_epoch(Some(&copy), None).unwrap(),
        resolve_epoch(Some(&a), None).unwrap()
    );
    let nonce = session_nonce_for_replay_path(&a).unwrap();
    assert_eq!(session_nonce_for_replay_path(&copy).unwrap(), nonce);

    // Same samples, start and gaps, but ended `eof` and written with
    // the keys spaced out: the same recording.
    let meta = manta_input::metadata_path(&copy);
    let text = std::fs::read_to_string(&meta)
        .unwrap()
        .replace("\"stopped\"", "\"eof\"")
        .replace("\":", "\": ");
    std::fs::write(&meta, text).unwrap();
    assert_eq!(
        manta_input::read_recording_summary(&copy).unwrap().reason,
        manta_input::EndReason::Eof
    );
    assert_eq!(session_nonce_for_replay_path(&copy).unwrap(), nonce);
}

#[test]
fn resolve_epoch_is_now_for_a_live_session_with_no_replay_path() {
    let before = std::time::SystemTime::now();
    let epoch = resolve_epoch(None, None).unwrap();
    let after = std::time::SystemTime::now();
    assert!(epoch >= before && epoch <= after);
}

#[test]
fn epoch_for_replay_path_is_a_real_deterministic_timestamp() {
    // Regression (round-6 review): the epoch fed into SpotBus (and
    // from there into every JSON `timestamp`/RBN Zulu field) must be
    // BOTH a genuine wall-clock instant (not a content-hash reinterpreted
    // as nanoseconds, which produced dates spanning 1970-2554) AND
    // stable across reruns of the same replay file (a fresh
    // SystemTime::now() every run broke reproducible replay output,
    // the specific regression this round's finding flagged). A file's
    // own mtime satisfies both: it's a real filesystem fact, and it
    // doesn't change between two reads of the same untouched file.
    let f = write_temp_file(b"replay epoch fixture");
    let a = epoch_for_replay_path(f.path()).unwrap();
    let b = epoch_for_replay_path(f.path()).unwrap();
    assert_eq!(a, b, "must be stable across reruns of the same file");

    let now = std::time::SystemTime::now();
    let drift = now
        .duration_since(a)
        .or_else(|_| a.duration_since(now))
        .unwrap();
    assert!(
        drift < std::time::Duration::from_secs(60),
        "must be a genuine near-present timestamp, not a fabricated far date; drift was {drift:?}"
    );
}

#[test]
fn session_nonce_for_replay_path_matches_the_published_fnv_1a_algorithm() {
    // Regression (round-12 review): std::collections::hash_map::
    // DefaultHasher's algorithm is explicitly documented as
    // UNSPECIFIED across Rust releases, so the same replay file could
    // hash differently across builds/toolchains -- and this value
    // feeds every JSON spot `id`. FNV-1a-64 is a small, independently
    // published, versioned algorithm with no dependency on any std or
    // compiler internals: `hash = (hash XOR byte) * FNV_PRIME`,
    // starting from the published offset basis. Recomputed here from
    // the same published constants via a separate expression (not
    // just self-consistency) to pin the implementation against the
    // actual formula, catching e.g. an accidentally swapped
    // XOR/multiply order or wrong constant.
    const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let expected_empty = OFFSET_BASIS as u128;
    let expected_a = (OFFSET_BASIS ^ 0x61u64).wrapping_mul(PRIME) as u128;
    let expected_ab =
        (((OFFSET_BASIS ^ 0x61u64).wrapping_mul(PRIME) ^ 0x62u64).wrapping_mul(PRIME)) as u128;

    assert_eq!(
        session_nonce_for_replay_path(write_temp_file(b"").path()).unwrap(),
        expected_empty
    );
    assert_eq!(
        session_nonce_for_replay_path(write_temp_file(b"a").path()).unwrap(),
        expected_a
    );
    assert_eq!(
        session_nonce_for_replay_path(write_temp_file(b"ab").path()).unwrap(),
        expected_ab
    );
}

#[test]
fn session_nonce_for_replay_path_is_deterministic_for_the_same_content() {
    let f = write_temp_file(b"same recording bytes");
    assert_eq!(
        session_nonce_for_replay_path(f.path()).unwrap(),
        session_nonce_for_replay_path(f.path()).unwrap()
    );
}

#[test]
fn session_nonce_for_replay_path_is_stable_across_different_paths_for_the_same_content() {
    // The exact bug this fix exists to prevent: the same recording,
    // re-read from a different path (a rename, a different mount, a
    // different checkout) must derive the SAME replay session nonce.
    let a = write_temp_file(b"identical recording bytes");
    let b = write_temp_file(b"identical recording bytes");
    assert_eq!(
        session_nonce_for_replay_path(a.path()).unwrap(),
        session_nonce_for_replay_path(b.path()).unwrap(),
        "the same content at two different paths must derive the same nonce"
    );
}

#[test]
fn session_nonce_for_replay_path_differs_across_different_recordings() {
    let a =
        session_nonce_for_replay_path(write_temp_file(b"contest-weekend bytes").path()).unwrap();
    let b =
        session_nonce_for_replay_path(write_temp_file(b"quiet-weeknight bytes").path()).unwrap();
    assert_ne!(
        a, b,
        "two different recordings must not collide on the same replay session nonce"
    );
}
