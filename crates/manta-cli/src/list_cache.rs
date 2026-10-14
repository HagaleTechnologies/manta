//! MAN-316: the on-disk cache of downloaded reference lists under
//! `[spot].list_cache_dir`. Version files are immutable and named by the
//! SHA-256 of their bytes (`cty/<sha>.dat`, `scp/<sha>.scp`); `state.json`
//! names the current one. Every write goes to a unique temporary name and
//! is renamed into place, so a crash or two daemons sharing the directory
//! never leave a half-written file. See the MAN-316 plan, Design §6.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Versions of each list kept by `prune`: about 3 MB of cty, about 11 MB
/// of scp.
pub(crate) const KEPT_VERSIONS: usize = 30;

const STATE_FILE: &str = "state.json";
const DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// Makes temporary names unique between threads of one process; the pid
/// separates processes.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ListKind {
    Cty,
    Scp,
}

impl ListKind {
    pub(crate) const ALL: [ListKind; 2] = [ListKind::Cty, ListKind::Scp];

    /// The cache subdirectory, metric label and log-line name.
    pub(crate) fn name(self) -> &'static str {
        match self {
            ListKind::Cty => "cty",
            ListKind::Scp => "scp",
        }
    }

    /// A version file's extension.
    pub(crate) fn extension(self) -> &'static str {
        match self {
            ListKind::Cty => "dat",
            ListKind::Scp => "scp",
        }
    }

    /// How often a live `run` checks for a new copy.
    pub(crate) fn interval(self) -> Duration {
        match self {
            ListKind::Cty => 7 * DAY,
            ListKind::Scp => DAY,
        }
    }

    /// The list's usual file name, as operator-facing text names it.
    pub(crate) fn file_name(self) -> &'static str {
        match self {
            ListKind::Cty => "cty.dat",
            ListKind::Scp => "MASTER.SCP",
        }
    }
}

/// `<list>/state.json`. Times are Unix seconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct State {
    /// The current version: `<sha256>.<ext>` beside this file.
    pub(crate) sha256: String,
    /// Where it was downloaded from; `If-Modified-Since` is sent only to
    /// the same URL.
    pub(crate) url: String,
    pub(crate) downloaded_at: u64,
    /// The last time the URL confirmed this version current (a download,
    /// a 304 or identical bytes): the schedule counts from here.
    pub(crate) checked_at: u64,
    #[serde(default)]
    pub(crate) last_modified: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct Cache {
    dir: PathBuf,
}

impl Cache {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Cache { dir }
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    fn list_dir(&self, kind: ListKind) -> PathBuf {
        self.dir.join(kind.name())
    }

    /// `<dir>/<name>/<sha256>.<ext>`.
    pub(crate) fn version_path(&self, kind: ListKind, sha256: &str) -> PathBuf {
        self.list_dir(kind)
            .join(format!("{sha256}.{}", kind.extension()))
    }

    /// `<dir>/<name>/state.json`.
    pub(crate) fn state_path(&self, kind: ListKind) -> PathBuf {
        self.list_dir(kind).join(STATE_FILE)
    }

    /// `Ok(None)` when there is no `state.json`; `Err` (naming the path)
    /// when it cannot be read, is not a state file, names something
    /// other than a SHA-256, or holds a time `SystemTime` cannot hold.
    pub(crate) fn load_state(&self, kind: ListKind) -> Result<Option<State>, String> {
        let path = self.state_path(kind);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let state: State = serde_json::from_slice(&bytes)
            .map_err(|e| format!("{} is not a valid state file: {e}", path.display()))?;
        if !is_sha256_hex(&state.sha256) {
            return Err(format!(
                "{} names \"{}\", which is not a SHA-256",
                path.display(),
                state.sha256
            ));
        }
        for (key, secs) in [
            ("downloaded_at", state.downloaded_at),
            ("checked_at", state.checked_at),
        ] {
            if from_unix_secs(secs).is_none() {
                return Err(format!(
                    "{} has {key} {secs}, which is out of range",
                    path.display()
                ));
            }
        }
        Ok(Some(state))
    }

    /// The state and the bytes of the version it names, verified against
    /// its hash. `Ok(None)` when there is no `state.json`; a state naming a
    /// missing or altered version is an `Err`.
    pub(crate) fn load_current(&self, kind: ListKind) -> Result<Option<(State, Vec<u8>)>, String> {
        let Some(state) = self.load_state(kind)? else {
            return Ok(None);
        };
        match self.load_version(kind, &state.sha256)? {
            Some(bytes) => Ok(Some((state, bytes))),
            None => Err(format!(
                "{} names {}, but {} does not exist",
                self.state_path(kind).display(),
                state.sha256,
                self.version_path(kind, &state.sha256).display()
            )),
        }
    }

    /// The bytes of one version. `Ok(None)` when its file does not exist;
    /// `Err` when it cannot be read or its bytes do not hash to its name.
    pub(crate) fn load_version(
        &self,
        kind: ListKind,
        sha256: &str,
    ) -> Result<Option<Vec<u8>>, String> {
        if !is_sha256_hex(sha256) {
            return Err(format!("\"{sha256}\" is not a SHA-256"));
        }
        let path = self.version_path(kind, sha256);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let actual = sha256_hex(&bytes);
        if actual != sha256 {
            return Err(format!(
                "{} has SHA-256 {actual}, not {sha256}",
                path.display()
            ));
        }
        Ok(Some(bytes))
    }

    /// Writes `bytes` as a version file, then `state` (which must name it)
    /// as the current one. Creates the directories as needed.
    pub(crate) fn store(&self, kind: ListKind, bytes: &[u8], state: &State) -> io::Result<()> {
        let sha256 = sha256_hex(bytes);
        if state.sha256 != sha256 {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "the state names {}, but the bytes have SHA-256 {sha256}",
                    state.sha256
                ),
            ));
        }
        fs::create_dir_all(self.list_dir(kind))?;
        write_atomic(&self.version_path(kind, &sha256), bytes)?;
        self.write_state(kind, state)
    }

    /// Replaces `state.json` (a check that changed only `checked_at`).
    pub(crate) fn write_state(&self, kind: ListKind, state: &State) -> io::Result<()> {
        let mut json = serde_json::to_vec(state).map_err(io::Error::other)?;
        json.push(b'\n');
        fs::create_dir_all(self.list_dir(kind))?;
        write_atomic(&self.state_path(kind), &json)
    }

    /// Deletes all but the `KEPT_VERSIONS` newest version files (by mtime)
    /// and `keep_sha256`'s. Only files named `<64 lowercase hex>.<ext>` are
    /// candidates: `state.json`, temporaries and anything an operator put
    /// there are never touched.
    pub(crate) fn prune(&self, kind: ListKind, keep_sha256: &str) -> io::Result<()> {
        let entries = match fs::read_dir(self.list_dir(kind)) {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let suffix = format!(".{}", kind.extension());
        let mut versions = Vec::new();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(sha) = name.to_str().and_then(|n| n.strip_suffix(suffix.as_str())) else {
                continue;
            };
            if !is_sha256_hex(sha) {
                continue;
            }
            let meta = match entry.metadata() {
                Ok(meta) => meta,
                // Another writer pruned it first.
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            if !meta.is_file() {
                continue;
            }
            let modified = meta.modified().unwrap_or(UNIX_EPOCH);
            versions.push((modified, sha.to_string(), entry.path()));
        }
        // Newest first; the name breaks mtime ties so the result is stable.
        versions.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        for (_, sha, path) in versions.into_iter().skip(KEPT_VERSIONS) {
            if sha == keep_sha256 {
                continue;
            }
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// Lowercase hex SHA-256 of `bytes`: a version file's name, and what
/// `sha256sum` prints for it.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub(crate) fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// `None` when `s` is past what `SystemTime` can hold on this platform.
pub(crate) fn from_unix_secs(s: u64) -> Option<SystemTime> {
    UNIX_EPOCH.checked_add(Duration::from_secs(s))
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Writes `.<name>.tmp.<pid>.<counter>` beside `path`, flushes it to disk
/// and renames it over `path`. On failure the temporary is removed.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(
        ".{name}.tmp.{}.{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use tempfile::TempDir;

    fn state_for(bytes: &[u8], checked_at: u64) -> State {
        State {
            sha256: sha256_hex(bytes),
            url: "http://www.country-files.com/cty/cty.dat".to_string(),
            downloaded_at: 1_791_158_400,
            checked_at,
            last_modified: Some("Tue, 15 Sep 2026 20:52:28 GMT".to_string()),
        }
    }

    /// Every file under `dir`, recursively, as paths relative to it.
    fn files(dir: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for entry in fs::read_dir(&d).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    // `/`-joined on every platform, so expectations read the same on Windows.
                    let rel = path.strip_prefix(dir).unwrap();
                    let parts: Vec<_> = rel.iter().map(|c| c.to_string_lossy()).collect();
                    out.push(parts.join("/"));
                }
            }
        }
        out.sort();
        out
    }

    #[test]
    fn store_then_load_round_trips_and_verifies_the_hash() {
        let tmp = TempDir::new().unwrap();
        // Not created yet: `store` makes it.
        let cache = Cache::new(tmp.path().join("lists"));
        assert_eq!(cache.dir(), tmp.path().join("lists"));
        assert_eq!(cache.load_state(ListKind::Cty), Ok(None));
        assert_eq!(cache.load_current(ListKind::Cty), Ok(None));

        let bytes = b"United States: 05: 08: NA: 43.00: 87.90: 5.0: K:\n K,W;\n";
        let state = state_for(bytes, 1_791_795_600);
        cache.store(ListKind::Cty, bytes, &state).unwrap();

        let sha = sha256_hex(bytes);
        assert_eq!(sha.len(), 64);
        assert_eq!(
            cache.version_path(ListKind::Cty, &sha),
            tmp.path()
                .join("lists")
                .join("cty")
                .join(format!("{sha}.dat"))
        );
        assert_eq!(
            cache.state_path(ListKind::Scp),
            tmp.path().join("lists").join("scp").join("state.json")
        );
        assert_eq!(cache.load_state(ListKind::Cty), Ok(Some(state.clone())));
        assert_eq!(
            cache.load_current(ListKind::Cty),
            Ok(Some((state, bytes.to_vec())))
        );
        assert_eq!(
            cache.load_version(ListKind::Cty, &sha),
            Ok(Some(bytes.to_vec()))
        );
        // Another hash, and the other list, are simply absent.
        assert_eq!(
            cache.load_version(ListKind::Cty, &sha256_hex(b"other")),
            Ok(None)
        );
        assert_eq!(cache.load_current(ListKind::Scp), Ok(None));

        // Hashes are what `sha256sum` prints.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            unix_secs(from_unix_secs(1_791_158_400).unwrap()),
            1_791_158_400
        );

        // A state.json written before `last_modified` existed still loads.
        let old = format!(
            "{{\"sha256\":\"{sha}\",\"url\":\"http://h/c\",\"downloaded_at\":1,\"checked_at\":2}}"
        );
        fs::write(cache.state_path(ListKind::Cty), old).unwrap();
        assert_eq!(
            cache
                .load_state(ListKind::Cty)
                .unwrap()
                .unwrap()
                .last_modified,
            None
        );

        // `store` refuses a state that does not name the bytes.
        let wrong = state_for(b"something else", 3);
        assert!(cache.store(ListKind::Scp, bytes, &wrong).is_err());
    }

    #[test]
    fn a_version_file_whose_bytes_do_not_match_its_name_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let cache = Cache::new(tmp.path().to_path_buf());
        let bytes = b"W1AW\nK1ABC\n";
        let sha = sha256_hex(bytes);
        cache
            .store(ListKind::Scp, bytes, &state_for(bytes, 5))
            .unwrap();
        let path = cache.version_path(ListKind::Scp, &sha);
        fs::write(&path, b"W1AW\n").unwrap();

        let actual = sha256_hex(b"W1AW\n");
        let want = format!("{} has SHA-256 {actual}, not {sha}", path.display());
        assert_eq!(cache.load_version(ListKind::Scp, &sha), Err(want.clone()));
        assert_eq!(cache.load_current(ListKind::Scp), Err(want));

        // A state naming a version that is gone is not a usable cache.
        fs::remove_file(&path).unwrap();
        let err = cache.load_current(ListKind::Scp).unwrap_err();
        assert!(err.contains("does not exist"), "{err}");
    }

    #[test]
    fn a_corrupt_state_json_is_reported_and_treated_as_no_cache() {
        let tmp = TempDir::new().unwrap();
        let cache = Cache::new(tmp.path().to_path_buf());
        let path = cache.state_path(ListKind::Cty);
        fs::create_dir_all(path.parent().unwrap()).unwrap();

        fs::write(&path, b"{\"sha256\": \"e9ff").unwrap();
        let err = cache.load_state(ListKind::Cty).unwrap_err();
        assert!(err.starts_with(&path.display().to_string()), "{err}");
        assert!(err.contains("not a valid state file"), "{err}");
        let err = cache.load_current(ListKind::Cty).unwrap_err();
        assert!(err.starts_with(&path.display().to_string()), "{err}");

        // A state naming a path rather than a hash never reaches the disk.
        fs::write(
            &path,
            b"{\"sha256\":\"../../etc/passwd\",\"url\":\"http://h/c\",\"downloaded_at\":1,\"checked_at\":2}",
        )
        .unwrap();
        let err = cache.load_current(ListKind::Cty).unwrap_err();
        assert!(err.contains("is not a SHA-256"), "{err}");

        // A time past what `SystemTime` holds is an error, not a panic.
        let sha = sha256_hex(b"cty;\n");
        for (downloaded_at, checked_at, key) in
            [(u64::MAX, 2, "downloaded_at"), (1, u64::MAX, "checked_at")]
        {
            fs::write(
                &path,
                format!(
                    "{{\"sha256\":\"{sha}\",\"url\":\"http://h/c\",\
                     \"downloaded_at\":{downloaded_at},\"checked_at\":{checked_at}}}"
                ),
            )
            .unwrap();
            let err = cache.load_state(ListKind::Cty).unwrap_err();
            assert!(err.starts_with(&path.display().to_string()), "{err}");
            assert!(err.contains(&format!("{key} {}", u64::MAX)), "{err}");
            assert!(err.contains("out of range"), "{err}");
        }

        // The next download replaces it.
        let bytes = b"cty;\n";
        let state = state_for(bytes, 7);
        cache.store(ListKind::Cty, bytes, &state).unwrap();
        assert_eq!(
            cache.load_current(ListKind::Cty),
            Ok(Some((state, bytes.to_vec())))
        );
    }

    #[test]
    fn writes_are_atomic() {
        let tmp = TempDir::new().unwrap();
        let cache = Cache::new(tmp.path().to_path_buf());
        let (a, b) = (b"version a;\n".as_slice(), b"version b;\n".as_slice());
        cache.store(ListKind::Cty, a, &state_for(a, 1)).unwrap();
        cache.store(ListKind::Cty, b, &state_for(b, 2)).unwrap();
        cache.write_state(ListKind::Cty, &state_for(b, 3)).unwrap();

        // Only the renamed results remain, complete.
        let mut want = vec![
            format!("cty/{}.dat", sha256_hex(a)),
            format!("cty/{}.dat", sha256_hex(b)),
            "cty/state.json".to_string(),
        ];
        want.sort();
        assert_eq!(files(tmp.path()), want);
        assert_eq!(
            cache.load_current(ListKind::Cty),
            Ok(Some((state_for(b, 3), b.to_vec())))
        );

        // A rename that fails (state.json is a directory) leaves the old
        // target alone and no temporary behind.
        let scp_state = cache.state_path(ListKind::Scp);
        fs::create_dir_all(&scp_state).unwrap();
        let c = b"W1AW\n".as_slice();
        assert!(cache.store(ListKind::Scp, c, &state_for(c, 4)).is_err());
        assert!(scp_state.is_dir());
        assert!(
            files(tmp.path()).iter().all(|f| !f.contains(".tmp.")),
            "{:?}",
            files(tmp.path())
        );
    }

    #[test]
    fn prune_keeps_the_30_newest_versions_and_never_touches_other_files() {
        let tmp = TempDir::new().unwrap();
        let cache = Cache::new(tmp.path().to_path_buf());
        let dir = tmp.path().join("cty");
        fs::create_dir_all(&dir).unwrap();
        let base = from_unix_secs(1_790_000_000).unwrap();
        let shas: Vec<String> = (0..35u64)
            .map(|i| {
                let bytes = format!("version {i};\n");
                let sha = sha256_hex(bytes.as_bytes());
                let path = cache.version_path(ListKind::Cty, &sha);
                fs::write(&path, &bytes).unwrap();
                fs::File::options()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_modified(base + Duration::from_secs(i * 60))
                    .unwrap();
                sha
            })
            .collect();
        // The current version is the oldest of all.
        let current = shas[0].clone();
        cache
            .write_state(
                ListKind::Cty,
                &State {
                    sha256: current.clone(),
                    url: "http://h/cty.dat".to_string(),
                    downloaded_at: 1,
                    checked_at: 2,
                    last_modified: None,
                },
            )
            .unwrap();
        let others = [
            "notes.txt".to_string(),
            format!("{}.scp", shas[1]),
            // Upper-case hex is not a version name. Not `shas[1]` upper-cased:
            // on a case-insensitive filesystem (macOS APFS, Windows NTFS) that
            // name is the same file as the version `shas[1].dat`.
            format!("{}.dat", "AB".repeat(32)),
            format!(".{}.dat.tmp.1.2", shas[2]),
        ];
        for name in &others {
            fs::write(dir.join(name), b"x").unwrap();
            fs::File::options()
                .write(true)
                .open(dir.join(name))
                .unwrap()
                .set_modified(base)
                .unwrap();
        }

        cache.prune(ListKind::Cty, &current).unwrap();

        let mut want: Vec<String> = shas[5..]
            .iter()
            .chain([&current])
            .map(|s| format!("cty/{s}.dat"))
            .chain(others.iter().map(|o| format!("cty/{o}")))
            .chain(["cty/state.json".to_string()])
            .collect();
        want.sort();
        assert_eq!(files(tmp.path()), want);
        assert_eq!(KEPT_VERSIONS, 30);

        // Pruning again, or a list with no directory, changes nothing.
        cache.prune(ListKind::Cty, &current).unwrap();
        assert_eq!(files(tmp.path()), want);
        cache.prune(ListKind::Scp, &current).unwrap();
    }

    #[test]
    fn two_writers_sharing_a_directory_leave_a_valid_state() {
        let tmp = TempDir::new().unwrap();
        let start = Arc::new(Barrier::new(2));
        let writers: Vec<_> = (0..2u64)
            .map(|w| {
                let cache = Cache::new(tmp.path().to_path_buf());
                let start = Arc::clone(&start);
                thread::spawn(move || {
                    start.wait();
                    for i in 0..20u64 {
                        let bytes = format!("writer {w} version {i}\n").repeat(500);
                        let state = state_for(bytes.as_bytes(), i);
                        cache
                            .store(ListKind::Scp, bytes.as_bytes(), &state)
                            .unwrap();
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }

        let cache = Cache::new(tmp.path().to_path_buf());
        let (state, bytes) = cache.load_current(ListKind::Scp).unwrap().unwrap();
        assert_eq!(sha256_hex(&bytes), state.sha256);
        assert_eq!(state.checked_at, 19);
        let names = files(tmp.path());
        assert_eq!(names.len(), 41, "{names:?}");
        assert!(names.iter().all(|f| !f.contains(".tmp.")), "{names:?}");
    }
}
