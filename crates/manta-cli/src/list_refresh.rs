//! MAN-316: keeps `cty.dat` and `MASTER.SCP` current while `manta run`
//! reads a live receiver. Startup picks each list's version (pin, path,
//! cache, built in: D3), and a `manta-lists` thread re-downloads the lists
//! it manages on a fixed schedule (D8), swapping a download that passes
//! `manta_spot::reference`'s acceptance checks into every consumer through
//! the shared [`ReferenceLists`]. A failed attempt changes nothing but a
//! log line and a counter.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use manta_server::metrics::{ListOrigin, ListRefreshOutcome, Metrics};
use manta_spot::reference::{accept_cty, accept_scp, ReferenceLists};
use manta_spot::{cty, scp};

use crate::list_cache::{from_unix_secs, sha256_hex, unix_secs, Cache, ListKind, State};
use crate::list_http::{self, FetchLimits, Fetched, HttpUrl};

/// After a failed or rejected download the next try is this much later.
/// Failures are not persisted, so a restart retries at once (D8).
pub(crate) const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(3600);

/// The loop's longest sleep, so a set `stop` flag is seen within a second.
const SLEEP_SLICE: Duration = Duration::from_secs(1);

/// Where a pinned list's bytes were found (Design §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PinSource {
    /// The built-in copy matched the pin.
    Bundled,
    /// `--cty`/`cty_path` (or the scp pair) matched the pin.
    File(PathBuf),
    /// `<list_cache_dir>/<name>/<sha>.<ext>`.
    Cache(PathBuf),
}

/// Where the version of a list in use came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Origin {
    Bundled,
    /// `--cty`/`cty_path` or `--scp`/`scp_path` (MAN-79).
    File(PathBuf),
    /// The cache's current version at startup.
    Cache {
        downloaded_at: SystemTime,
    },
    /// Downloaded by this process.
    Download,
    Pinned(PinSource),
}

impl Origin {
    /// The `<origin-text>` of the refresh warnings: `built in`,
    /// `from <path>`, `from the cache`, `downloaded` or `pinned`.
    fn short(&self) -> String {
        match self {
            Origin::Bundled => "built in".to_string(),
            Origin::File(path) => format!("from {}", path.display()),
            Origin::Cache { .. } => "from the cache".to_string(),
            Origin::Download => "downloaded".to_string(),
            Origin::Pinned(_) => "pinned".to_string(),
        }
    }

    /// `manta_list_info`'s `origin` label.
    pub(crate) fn metric(&self) -> ListOrigin {
        match self {
            Origin::Bundled | Origin::Pinned(PinSource::Bundled) => ListOrigin::Bundled,
            Origin::File(_) | Origin::Pinned(PinSource::File(_)) => ListOrigin::File,
            Origin::Cache { .. } | Origin::Pinned(PinSource::Cache(_)) => ListOrigin::Cache,
            Origin::Download => ListOrigin::Download,
        }
    }
}

/// The version of one list in use: what the `lists:` line, the refresh
/// warnings and `manta_list_info` name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InUse {
    pub(crate) kind: ListKind,
    pub(crate) sha256: String,
    pub(crate) origin: Origin,
    /// When this version was last confirmed current with its URL (the
    /// cache's `checked_at` at startup, else a successful attempt).
    pub(crate) confirmed_at: Option<SystemTime>,
}

impl InUse {
    pub(crate) fn new(kind: ListKind, sha256: String, origin: Origin) -> Self {
        InUse {
            kind,
            sha256,
            origin,
            confirmed_at: None,
        }
    }

    /// The built-in copy of `kind`.
    pub(crate) fn bundled(kind: ListKind) -> Self {
        InUse::new(kind, bundled_sha256(kind), Origin::Bundled)
    }

    /// The `<desc>` of the `lists:` line.
    pub(crate) fn describe(&self) -> String {
        let sha = &self.sha256;
        match &self.origin {
            Origin::Bundled => format!("built in (sha256 {sha})"),
            Origin::File(path) => format!("from {} (sha256 {sha})", path.display()),
            Origin::Cache { downloaded_at } => format!(
                "from the cache (sha256 {sha}, downloaded {})",
                utc_date(*downloaded_at)
            ),
            Origin::Download => format!("downloaded (sha256 {sha})"),
            Origin::Pinned(PinSource::Bundled) => format!("pinned (sha256 {sha}, built in)"),
            Origin::Pinned(PinSource::File(path) | PinSource::Cache(path)) => {
                format!("pinned (sha256 {sha}, {})", path.display())
            }
        }
    }

    /// True when a pin or a path chose this version, so refresh leaves it alone (D3).
    pub(crate) fn is_operator_choice(&self) -> bool {
        matches!(self.origin, Origin::File(_) | Origin::Pinned(_))
    }
}

/// `lists: cty <desc>; scp <desc>`.
pub(crate) fn lists_line(cty: &InUse, scp: &InUse) -> String {
    format!("lists: cty {}; scp {}", cty.describe(), scp.describe())
}

/// Design §4: what `run`/`soak` print for a file source with refresh on.
pub(crate) fn file_replay_note(cty: &InUse, scp: &InUse) -> String {
    format!(
        "note: list refresh is off for file replay, so the run is repeatable; using cty {} and \
         scp {}; pin a downloaded copy with --cty-sha256 or --scp-sha256",
        cty.describe(),
        scp.describe()
    )
}

/// `note: cty_path is set, so list refresh leaves cty.dat alone` (and the
/// pin/scp variants), or `None` when refresh manages the list.
pub(crate) fn left_alone_note(in_use: &InUse) -> Option<String> {
    let key = match (&in_use.origin, in_use.kind) {
        (Origin::File(_), ListKind::Cty) => "cty_path",
        (Origin::File(_), ListKind::Scp) => "scp_path",
        (Origin::Pinned(_), ListKind::Cty) => "cty_sha256",
        (Origin::Pinned(_), ListKind::Scp) => "scp_sha256",
        _ => return None,
    };
    Some(format!(
        "note: {key} is set, so list refresh leaves {} alone",
        in_use.kind.file_name()
    ))
}

/// The SHA-256 of the built-in copy's bytes.
pub(crate) fn bundled_sha256(kind: ListKind) -> String {
    sha256_hex(bundled_text(kind).as_bytes())
}

fn bundled_text(kind: ListKind) -> &'static str {
    match kind {
        ListKind::Cty => manta_spot::CTY_DAT,
        ListKind::Scp => manta_spot::MASTER_SCP,
    }
}

/// The built-in copy's retrieval date; a cached download older than it
/// loses to the built-in copy at startup (D3).
fn bundled_retrieved_at(kind: ListKind) -> Option<SystemTime> {
    match kind {
        ListKind::Cty => manta_spot::vintage::cty_dat_retrieved_at(),
        ListKind::Scp => manta_spot::vintage::master_scp_retrieved_at(),
    }
}

/// When `kind` is next due given its persisted state (D8): at once with no
/// state or a `checked_at` in the future (the clock moved back), else
/// `checked_at + interval`.
pub(crate) fn next_due(now: SystemTime, state: Option<&State>, kind: ListKind) -> SystemTime {
    match state.and_then(|state| from_unix_secs(state.checked_at)) {
        Some(checked) if checked <= now => checked + kind.interval(),
        // No state, or a `checked_at` past `now` (or past what
        // `SystemTime` holds, which `load_state` refuses anyway).
        _ => now,
    }
}

/// D8's rule: due with no state, when `now >= checked_at + interval`, or
/// when `checked_at` is in the future.
pub(crate) fn due(now: SystemTime, state: Option<&State>, kind: ListKind) -> bool {
    now >= next_due(now, state, kind)
}

/// A table that passed the acceptance checks, either kind.
enum Accepted {
    Cty(cty::Table),
    Scp(scp::Set),
}

impl Accepted {
    /// `7497 prefixes` / `50020 calls`.
    fn size(&self) -> String {
        match self {
            Accepted::Cty(table) => format!("{} prefixes", table.len()),
            Accepted::Scp(set) => format!("{} calls", set.len()),
        }
    }

    fn store(self, lists: &ReferenceLists) {
        match self {
            Accepted::Cty(table) => lists.cty.store(Arc::new(table)),
            Accepted::Scp(set) => lists.scp.store(Arc::new(set)),
        }
    }
}

/// The built-in tables, the floors every download is measured against.
struct Bundled {
    cty: cty::Table,
    scp: scp::Set,
}

impl Bundled {
    fn parse() -> Self {
        Bundled {
            cty: cty::Table::bundled(),
            scp: scp::Set::bundled(),
        }
    }

    /// UTF-8 check, then `accept_cty`/`accept_scp` (D5).
    fn accept(&self, kind: ListKind, bytes: &[u8]) -> Result<Accepted, String> {
        let text = std::str::from_utf8(bytes).map_err(|_| "it is not UTF-8 text".to_string())?;
        match kind {
            ListKind::Cty => accept_cty(text, &self.cty).map(Accepted::Cty),
            ListKind::Scp => accept_scp(text, &self.scp).map(Accepted::Scp),
        }
    }
}

/// D3 steps 3 and 4 for every list a pin or path did not choose: the
/// cache's current version when refresh is on, `cache` is set, the version
/// verifies, passes acceptance and was downloaded after the built-in copy's
/// retrieval date; else the built-in copy. A cache that is present but
/// unusable is a `warning: list cache: …` line. Returns the shared lists
/// and the version of each in use, cty first.
pub(crate) fn resolve_startup(
    cty_table: Arc<cty::Table>,
    cty_in_use: InUse,
    scp_set: Arc<scp::Set>,
    scp_in_use: InUse,
    refresh_on: bool,
    cache: Option<&Cache>,
) -> (ReferenceLists, [InUse; 2]) {
    let mut cty_pair = (cty_table, cty_in_use);
    let mut scp_pair = (scp_set, scp_in_use);
    if let (true, Some(cache)) = (refresh_on, cache) {
        let bundled = Bundled::parse();
        if !cty_pair.1.is_operator_choice() {
            if let Some((Accepted::Cty(table), in_use)) = from_cache(cache, ListKind::Cty, &bundled)
            {
                cty_pair = (Arc::new(table), in_use);
            }
        }
        if !scp_pair.1.is_operator_choice() {
            if let Some((Accepted::Scp(set), in_use)) = from_cache(cache, ListKind::Scp, &bundled) {
                scp_pair = (Arc::new(set), in_use);
            }
        }
    }
    (
        ReferenceLists::new(cty_pair.0, scp_pair.0),
        [cty_pair.1, scp_pair.1],
    )
}

/// The cache's current version of `kind`, when it may be used at startup.
fn from_cache(cache: &Cache, kind: ListKind, bundled: &Bundled) -> Option<(Accepted, InUse)> {
    let unusable = |path: PathBuf, reason: &str| {
        crate::logging::warning(&format!(
            "warning: list cache: {} unusable ({reason}); using the built-in {}",
            path.display(),
            kind.name()
        ));
    };
    let (state, bytes) = match cache.load_current(kind) {
        Ok(Some(current)) => current,
        Ok(None) => return None,
        Err(reason) => {
            unusable(cache.state_path(kind), &reason);
            return None;
        }
    };
    // `load_state` refuses a state whose times `SystemTime` cannot hold.
    let (Some(downloaded_at), Some(checked_at)) = (
        from_unix_secs(state.downloaded_at),
        from_unix_secs(state.checked_at),
    ) else {
        return None;
    };
    if bundled_retrieved_at(kind).is_some_and(|retrieved| downloaded_at <= retrieved) {
        return None;
    }
    match bundled.accept(kind, &bytes) {
        Ok(accepted) => {
            let mut in_use =
                InUse::new(kind, state.sha256.clone(), Origin::Cache { downloaded_at });
            in_use.confirmed_at = Some(checked_at);
            Some((accepted, in_use))
        }
        Err(reason) => {
            unusable(cache.version_path(kind, &state.sha256), &reason);
            None
        }
    }
}

/// One list the refresher manages: its URL and the version in use.
#[derive(Debug, Clone)]
pub(crate) struct ListSetting {
    pub(crate) url: HttpUrl,
    pub(crate) in_use: InUse,
}

/// What `spawn` needs: the lists to refresh (those no pin or path chose),
/// the cache, and the fetch limits.
#[derive(Debug, Clone)]
pub(crate) struct RefreshSetup {
    pub(crate) lists: Vec<ListSetting>,
    pub(crate) cache: Option<Cache>,
    pub(crate) limits: FetchLimits,
}

impl RefreshSetup {
    /// `list refresh: on; cty every 7 days from <url>, scp every day from
    /// <url>; cache <dir>`.
    pub(crate) fn startup_line(&self) -> String {
        let lists: Vec<String> = self
            .lists
            .iter()
            .map(|list| {
                let every = match list.in_use.kind {
                    ListKind::Cty => "every 7 days",
                    ListKind::Scp => "every day",
                };
                format!("{} {every} from {}", list.in_use.kind.name(), list.url)
            })
            .collect();
        let cache = match &self.cache {
            Some(cache) => cache.dir().display().to_string(),
            None => "none".to_string(),
        };
        format!("list refresh: on; {}; cache {cache}", lists.join(", "))
    }
}

/// One managed list's schedule and the state its conditional GET uses.
struct ListRun {
    url: HttpUrl,
    in_use: InUse,
    /// The cache's `state.json`, or the in-memory equivalent without one.
    state: Option<State>,
    next: SystemTime,
    /// When `next` was set (startup or the last attempt). A `now` before
    /// it means the clock moved back, so the list is due at once (D8).
    scheduled_at: SystemTime,
}

/// The refresher's whole state; [`Refresher::refresh_once`] is the
/// testable unit, [`spawn`] the loop around it.
struct Refresher {
    runs: Vec<ListRun>,
    cache: Option<Cache>,
    limits: FetchLimits,
    lists: Arc<ReferenceLists>,
    metrics: Option<Arc<Metrics>>,
    bundled: Bundled,
}

impl Refresher {
    fn new(
        setup: RefreshSetup,
        lists: Arc<ReferenceLists>,
        metrics: Option<Arc<Metrics>>,
        now: SystemTime,
    ) -> Self {
        let runs = setup
            .lists
            .into_iter()
            .map(|list| {
                let kind = list.in_use.kind;
                // A corrupt state.json was reported at startup; it is
                // treated as no state, so the list is due at once.
                let state = setup
                    .cache
                    .as_ref()
                    .and_then(|cache| cache.load_state(kind).ok().flatten());
                // So is a state naming a version other than the one in use:
                // its file was missing, altered or rejected at startup and
                // the built-in copy won, so its `checked_at` says nothing
                // about what manta uses now.
                let schedule = state
                    .as_ref()
                    .filter(|state| state.sha256 == list.in_use.sha256);
                // A list due now (no state, a week/day old, or a clock
                // that moved back) is fetched on the loop's first pass.
                let next = if due(now, schedule, kind) {
                    now
                } else {
                    next_due(now, schedule, kind)
                };
                // A list not yet due counts from its `checked_at`, so the
                // clock must move back past that, as D8 says, to make it due.
                let scheduled_at = if next > now {
                    next - kind.interval()
                } else {
                    now
                };
                ListRun {
                    url: list.url,
                    in_use: list.in_use,
                    state,
                    next,
                    scheduled_at,
                }
            })
            .collect();
        Refresher {
            runs,
            cache: setup.cache,
            limits: setup.limits,
            lists,
            metrics,
            bundled: Bundled::parse(),
        }
    }

    /// The earliest time any managed list is next due.
    fn next_wake(&self) -> Option<SystemTime> {
        self.runs.iter().map(|run| run.next).min()
    }

    /// One attempt for `kind` at `now`: a (conditional) GET, the checks,
    /// and on success the swap, the cache write and the metrics.
    fn refresh_once(&mut self, kind: ListKind, now: SystemTime) -> ListRefreshOutcome {
        let Some(index) = self.runs.iter().position(|run| run.in_use.kind == kind) else {
            return ListRefreshOutcome::Unchanged;
        };
        let run = &self.runs[index];
        // If-Modified-Since only when the state describes the version in
        // use at this URL, so a 304 really means "what we have is current".
        let if_modified_since = run
            .state
            .as_ref()
            .filter(|state| state.url == run.url.as_str() && state.sha256 == run.in_use.sha256)
            .and_then(|state| state.last_modified.clone());
        let fetched = list_http::get(&run.url, if_modified_since.as_deref(), &self.limits);
        let outcome = match fetched {
            Err(error) => self.failed(index, now, &format!("failed: {error}")),
            Ok(Fetched::NotModified) => self.unchanged(index, now, "304 Not Modified", None),
            Ok(Fetched::Ok {
                body,
                last_modified,
            }) => {
                let sha256 = sha256_hex(&body);
                if sha256 == self.runs[index].in_use.sha256 {
                    self.unchanged(index, now, "same bytes", Some((&body, last_modified)))
                } else {
                    match self.bundled.accept(kind, &body) {
                        Err(reason) => {
                            self.failed(index, now, &format!("download rejected: {reason}"))
                        }
                        Ok(accepted) => {
                            self.updated(index, now, accepted, &body, sha256, last_modified)
                        }
                    }
                }
            }
        };
        if let Some(metrics) = &self.metrics {
            metrics.record_list_refresh(kind.name(), outcome, now);
        }
        outcome
    }

    fn failed(&mut self, index: usize, now: SystemTime, what: &str) -> ListRefreshOutcome {
        let run = &mut self.runs[index];
        run.next = now + RETRY_AFTER_FAILURE;
        run.scheduled_at = now;
        crate::logging::warning(&format!(
            "warning: list refresh: {} {what}; still using sha256 {} ({}); next try in 1 hour",
            run.in_use.kind.name(),
            run.in_use.sha256,
            run.in_use.origin.short()
        ));
        ListRefreshOutcome::Failed
    }

    /// A 304, or a 200 whose bytes are the version in use: the state's
    /// `checked_at` moves to `now` (a new state is written when the
    /// cache had none for these bytes).
    fn unchanged(
        &mut self,
        index: usize,
        now: SystemTime,
        why: &str,
        body: Option<(&[u8], Option<String>)>,
    ) -> ListRefreshOutcome {
        let run = &mut self.runs[index];
        let kind = run.in_use.kind;
        let fresh_last_modified = body.as_ref().and_then(|(_, lm)| lm.clone());
        let state = match run.state.take() {
            Some(mut state)
                if state.sha256 == run.in_use.sha256 && state.url == run.url.as_str() =>
            {
                state.checked_at = unix_secs(now);
                if fresh_last_modified.is_some() {
                    state.last_modified = fresh_last_modified;
                }
                state
            }
            _ => State {
                sha256: run.in_use.sha256.clone(),
                url: run.url.as_str().to_string(),
                downloaded_at: unix_secs(now),
                checked_at: unix_secs(now),
                last_modified: fresh_last_modified,
            },
        };
        if let Some(cache) = &self.cache {
            let written = match body {
                // The cache may lack these bytes (the built-in copy is in
                // use); store them so the state points at a real file.
                Some((bytes, _)) => cache.store(kind, bytes, &state),
                None => cache.write_state(kind, &state),
            };
            if let Err(error) = written {
                crate::logging::warning(&format!(
                    "warning: list cache: writing {}: {error}",
                    cache.state_path(kind).display()
                ));
            }
        }
        run.state = Some(state);
        run.in_use.confirmed_at = Some(now);
        run.next = now + kind.interval();
        run.scheduled_at = now;
        crate::logging::note(&format!(
            "list refresh: {} unchanged ({why}); next check {}",
            kind.name(),
            utc_minute(run.next)
        ));
        ListRefreshOutcome::Unchanged
    }

    fn updated(
        &mut self,
        index: usize,
        now: SystemTime,
        accepted: Accepted,
        body: &[u8],
        sha256: String,
        last_modified: Option<String>,
    ) -> ListRefreshOutcome {
        let size = accepted.size();
        accepted.store(&self.lists);
        let run = &mut self.runs[index];
        let kind = run.in_use.kind;
        let state = State {
            sha256: sha256.clone(),
            url: run.url.as_str().to_string(),
            downloaded_at: unix_secs(now),
            checked_at: unix_secs(now),
            last_modified: last_modified.clone(),
        };
        if let Some(cache) = &self.cache {
            let saved = cache
                .store(kind, body, &state)
                .and_then(|()| cache.prune(kind, &sha256));
            if let Err(error) = saved {
                crate::logging::warning(&format!(
                    "warning: list cache: saving {}: {error}; the download is in use, but a \
                     restart starts from the previous copy",
                    cache.version_path(kind, &sha256).display()
                ));
            }
        }
        run.state = Some(state);
        run.in_use = InUse {
            kind,
            sha256: sha256.clone(),
            origin: Origin::Download,
            confirmed_at: Some(now),
        };
        run.next = now + kind.interval();
        run.scheduled_at = now;
        if let Some(metrics) = &self.metrics {
            metrics.set_list_in_use(kind.name(), &sha256, ListOrigin::Download);
        }
        let detail = match &last_modified {
            Some(lm) => format!("{size}, Last-Modified {lm}"),
            None => size,
        };
        crate::logging::note(&format!(
            "list refresh: {} updated to sha256 {sha256} ({detail}); new spots use it now",
            kind.name()
        ));
        ListRefreshOutcome::Updated
    }

    /// Every list due at `now`, in `ListKind` order, until `stop` is set:
    /// its next check has come, or the clock has moved back past the time
    /// that check was set.
    fn run_due(&mut self, now: SystemTime, stop: &AtomicBool) {
        for kind in ListKind::ALL {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            if self
                .runs
                .iter()
                .any(|run| run.in_use.kind == kind && (now >= run.next || now < run.scheduled_at))
            {
                self.refresh_once(kind, now);
            }
        }
    }
}

/// Publishes the version of each list in use to `manta_list_info`, and for
/// a list the refresher manages starts its outcome counters at 0 and its
/// last-success gauge at the cache's `checked_at`.
pub(crate) fn publish(metrics: &Metrics, in_use: &[InUse], refreshed: &[ListKind]) {
    for list in in_use {
        let name = list.kind.name();
        metrics.set_list_in_use(name, &list.sha256, list.origin.metric());
        if refreshed.contains(&list.kind) {
            metrics.enable_list_refresh(name);
            if let Some(at) = list.confirmed_at {
                metrics.set_list_confirmed_at(name, at);
            }
        }
    }
}

/// Starts the `manta-lists` thread. It is never joined: a fetch in flight
/// is bounded by its deadline and every cache write is an atomic rename, so
/// the process may exit under it (D7).
pub(crate) fn spawn(
    setup: RefreshSetup,
    lists: Arc<ReferenceLists>,
    metrics: Option<Arc<Metrics>>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("manta-lists".into())
        .spawn(move || {
            let mut refresher = Refresher::new(setup, lists, metrics, SystemTime::now());
            run_loop(&mut refresher, &stop);
        })
        .map(|_| ())
}

fn run_loop(refresher: &mut Refresher, stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        let now = SystemTime::now();
        refresher.run_due(now, stop);
        let wait = refresher
            .next_wake()
            .and_then(|wake| wake.duration_since(SystemTime::now()).ok())
            .unwrap_or(SLEEP_SLICE);
        std::thread::sleep(wait.min(SLEEP_SLICE));
    }
}

/// `YYYY-MM-DD` in UTC.
fn utc_date(t: SystemTime) -> String {
    let (y, m, d, _, _) = utc_parts(t);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `YYYY-MM-DD HH:MMZ`.
fn utc_minute(t: SystemTime) -> String {
    let (y, m, d, hh, mm) = utc_parts(t);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}Z")
}

/// Civil date and time of day from Unix seconds (Howard Hinnant's
/// `civil_from_days`), so no date crate is needed.
fn utc_parts(t: SystemTime) -> (i64, u32, u32, u32, u32) {
    let secs = unix_secs(t) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d, (rem / 3600) as u32, ((rem % 3600) / 60) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    const DAY: Duration = Duration::from_secs(86_400);
    const HOUR: Duration = Duration::from_secs(3600);

    /// The test fixture already used in main.rs: a prefix the bundled
    /// cty.dat does not allocate.
    const QQ9_ENTITY: &str = "Test DXpedition: 14: 27: EU: 50.0: -5.0: 0.0: QQ9:\n QQ9;\n";

    fn qq9_cty() -> String {
        format!("{}{QQ9_ENTITY}", manta_spot::CTY_DAT)
    }

    fn state(sha256: &str, checked_at: SystemTime) -> State {
        State {
            sha256: sha256.to_string(),
            url: "http://127.0.0.1/cty.dat".to_string(),
            downloaded_at: unix_secs(checked_at),
            checked_at: unix_secs(checked_at),
            last_modified: None,
        }
    }

    fn at(secs: u64) -> SystemTime {
        from_unix_secs(secs).unwrap()
    }

    #[test]
    fn due_rules() {
        let now = at(1_800_000_000);
        assert!(due(now, None, ListKind::Cty));
        let week = state("a", now - 7 * DAY);
        assert!(due(now, Some(&week), ListKind::Cty));
        let almost = state("a", now - 7 * DAY + HOUR);
        assert!(!due(now, Some(&almost), ListKind::Cty));
        assert_eq!(next_due(now, Some(&almost), ListKind::Cty), now + HOUR);
        let day = state("a", now - DAY);
        assert!(due(now, Some(&day), ListKind::Scp));
        assert!(!due(now, Some(&day), ListKind::Cty));
        let future = state("a", now + DAY);
        assert!(due(now, Some(&future), ListKind::Cty));

        // After a failure the next attempt is an hour later, not at once.
        let lists = Arc::new(bundled_lists());
        let mut refresher = refresher_for(ListKind::Scp, closed_url(), None, lists, now);
        assert_eq!(
            refresher.refresh_once(ListKind::Scp, now),
            ListRefreshOutcome::Failed
        );
        assert_eq!(refresher.next_wake(), Some(now + RETRY_AFTER_FAILURE));
    }

    #[test]
    fn a_state_naming_a_version_not_in_use_is_due_at_once() {
        // The cache's version was unusable at startup, so the built-in copy
        // is in use; its fresh `checked_at` must not delay the first fetch.
        let now = at(1_800_000_000);
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path().to_path_buf());
        cache
            .write_state(ListKind::Cty, &state(&"0".repeat(64), now - HOUR))
            .unwrap();
        let lists = Arc::new(bundled_lists());
        let refresher = refresher_for(ListKind::Cty, closed_url(), Some(cache), lists, now);
        assert_eq!(refresher.next_wake(), Some(now));

        // The same state naming the version in use waits out the week.
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path().to_path_buf());
        cache
            .write_state(
                ListKind::Cty,
                &state(&bundled_sha256(ListKind::Cty), now - HOUR),
            )
            .unwrap();
        let lists = Arc::new(bundled_lists());
        let refresher = refresher_for(ListKind::Cty, closed_url(), Some(cache), lists, now);
        assert_eq!(refresher.next_wake(), Some(now - HOUR + 7 * DAY));
    }

    #[test]
    fn a_cache_time_out_of_range_falls_back_instead_of_panicking() {
        let now = at(1_800_000_000);
        let mut far = state("a", now);
        far.checked_at = u64::MAX;
        assert!(due(now, Some(&far), ListKind::Cty));

        // A valid version whose state.json says it was downloaded at
        // u64::MAX seconds: a cache warning and the built-in copy.
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path().to_path_buf());
        let bytes = qq9_cty();
        write_cache(&cache, ListKind::Cty, bytes.as_bytes(), SystemTime::now());
        let mut st = cache.load_state(ListKind::Cty).unwrap().unwrap();
        st.downloaded_at = u64::MAX;
        cache.write_state(ListKind::Cty, &st).unwrap();
        let (lists, in_use) = resolve_startup(
            Arc::new(cty::Table::bundled()),
            InUse::bundled(ListKind::Cty),
            Arc::new(scp::Set::bundled()),
            InUse::bundled(ListKind::Scp),
            true,
            Some(&cache),
        );
        assert_eq!(in_use[0], InUse::bundled(ListKind::Cty));
        assert!(!lists.cty.load().is_allocated("QQ9ZZZ"));
        let refresher = refresher_for(ListKind::Cty, closed_url(), Some(cache), lists.into(), now);
        assert_eq!(refresher.next_wake(), Some(now));
    }

    #[test]
    fn a_clock_that_moves_back_while_running_makes_the_list_due_at_once() {
        // A node booted with its clock a year fast checks at `now`, then
        // NTP steps the clock back: the next check must not wait a year.
        let now = at(1_800_000_000);
        let (base, server) = serve(vec![ok(manta_spot::MASTER_SCP.as_bytes(), None)]);
        let url = HttpUrl::parse(&format!("{base}/MASTER.SCP")).unwrap();
        let lists = Arc::new(bundled_lists());
        let mut refresher = refresher_for(ListKind::Scp, url, None, lists, now);
        let stop = AtomicBool::new(false);
        refresher.run_due(now, &stop);
        assert_eq!(refresher.next_wake(), Some(now + DAY));
        // An hour on, nothing is due.
        refresher.run_due(now + HOUR, &stop);
        assert_eq!(server.requests.lock().unwrap().len(), 1);

        let corrected = now - 365 * DAY;
        refresher.run_due(corrected, &stop);
        assert_eq!(server.requests.lock().unwrap().len(), 2);
        assert_eq!(refresher.next_wake(), Some(corrected + DAY));
        // That check reset the schedule: no second fetch at once.
        refresher.run_due(corrected + HOUR, &stop);
        assert_eq!(server.requests.lock().unwrap().len(), 2);

        // At startup a persisted schedule counts from `checked_at`: a step
        // back that stays after it changes nothing, one before it is due.
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path().to_path_buf());
        cache
            .write_state(
                ListKind::Cty,
                &state(&bundled_sha256(ListKind::Cty), now - HOUR),
            )
            .unwrap();
        let lists = Arc::new(bundled_lists());
        let mut refresher = refresher_for(ListKind::Cty, closed_url(), Some(cache), lists, now);
        refresher.run_due(now - Duration::from_secs(2), &stop);
        assert_eq!(refresher.next_wake(), Some(now - HOUR + 7 * DAY));
        refresher.run_due(now - 2 * HOUR, &stop);
        assert_eq!(
            refresher.next_wake(),
            Some(now - 2 * HOUR + RETRY_AFTER_FAILURE)
        );
    }

    #[test]
    fn utc_formatting() {
        // 2026-09-15 20:52:28 UTC.
        assert_eq!(utc_date(at(1_789_505_548)), "2026-09-15");
        assert_eq!(utc_minute(at(1_789_505_548)), "2026-09-15 20:52Z");
        assert_eq!(utc_date(at(0)), "1970-01-01");
    }

    fn bundled_lists() -> ReferenceLists {
        ReferenceLists::new(
            Arc::new(cty::Table::bundled()),
            Arc::new(scp::Set::bundled()),
        )
    }

    fn write_cache(
        cache: &Cache,
        kind: ListKind,
        bytes: &[u8],
        downloaded_at: SystemTime,
    ) -> String {
        let sha = sha256_hex(bytes);
        let mut st = state(&sha, downloaded_at);
        st.downloaded_at = unix_secs(downloaded_at);
        cache.store(kind, bytes, &st).unwrap();
        sha
    }

    #[test]
    fn startup_resolution_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path().to_path_buf());
        let recent = SystemTime::now();
        let cached_sha = write_cache(&cache, ListKind::Cty, qq9_cty().as_bytes(), recent);
        let bundled_cty = Arc::new(cty::Table::bundled());
        let bundled_scp = Arc::new(scp::Set::bundled());

        // The cache beats bundled when refresh is on.
        let (lists, in_use) = resolve_startup(
            bundled_cty.clone(),
            InUse::bundled(ListKind::Cty),
            bundled_scp.clone(),
            InUse::bundled(ListKind::Scp),
            true,
            Some(&cache),
        );
        assert_eq!(in_use[0].sha256, cached_sha);
        assert!(matches!(in_use[0].origin, Origin::Cache { .. }));
        assert!(in_use[0].confirmed_at.is_some());
        assert!(lists.cty.load().is_allocated("QQ9ZZZ"));
        assert_eq!(in_use[1], InUse::bundled(ListKind::Scp));

        // Refresh off: the cache is never read.
        let (_, in_use) = resolve_startup(
            bundled_cty.clone(),
            InUse::bundled(ListKind::Cty),
            bundled_scp.clone(),
            InUse::bundled(ListKind::Scp),
            false,
            Some(&cache),
        );
        assert_eq!(in_use[0], InUse::bundled(ListKind::Cty));

        // A path (and a pin) beats the cache.
        let file = InUse::new(ListKind::Cty, "f".repeat(64), Origin::File("x.dat".into()));
        let pinned = InUse::new(
            ListKind::Cty,
            "e".repeat(64),
            Origin::Pinned(PinSource::Bundled),
        );
        for chosen in [file, pinned] {
            let (lists, in_use) = resolve_startup(
                bundled_cty.clone(),
                chosen.clone(),
                bundled_scp.clone(),
                InUse::bundled(ListKind::Scp),
                true,
                Some(&cache),
            );
            assert_eq!(in_use[0], chosen);
            assert!(Arc::ptr_eq(&lists.cty.load(), &bundled_cty));
        }

        // A cache older than the built-in copy's retrieval date loses.
        let old_dir = tempfile::tempdir().unwrap();
        let old = Cache::new(old_dir.path().to_path_buf());
        write_cache(&old, ListKind::Cty, qq9_cty().as_bytes(), at(1_700_000_000));
        let (_, in_use) = resolve_startup(
            bundled_cty.clone(),
            InUse::bundled(ListKind::Cty),
            bundled_scp.clone(),
            InUse::bundled(ListKind::Scp),
            true,
            Some(&old),
        );
        assert_eq!(in_use[0], InUse::bundled(ListKind::Cty));

        // A cache that fails acceptance loses (and is warned about).
        let bad_dir = tempfile::tempdir().unwrap();
        let bad = Cache::new(bad_dir.path().to_path_buf());
        write_cache(&bad, ListKind::Scp, b"<html>404</html>\n", recent);
        let (_, in_use) = resolve_startup(
            bundled_cty,
            InUse::bundled(ListKind::Cty),
            bundled_scp,
            InUse::bundled(ListKind::Scp),
            true,
            Some(&bad),
        );
        assert_eq!(in_use[1], InUse::bundled(ListKind::Scp));
    }

    #[test]
    fn describe_and_notes_match_the_contract() {
        let sha = "a".repeat(64);
        let bundled = InUse::new(ListKind::Cty, sha.clone(), Origin::Bundled);
        let cached = InUse::new(
            ListKind::Scp,
            sha.clone(),
            Origin::Cache {
                downloaded_at: at(1_789_505_548),
            },
        );
        assert_eq!(
            lists_line(&bundled, &cached),
            format!(
                "lists: cty built in (sha256 {sha}); scp from the cache (sha256 {sha}, \
                 downloaded 2026-09-15)"
            )
        );
        let pinned = InUse::new(
            ListKind::Cty,
            sha.clone(),
            Origin::Pinned(PinSource::Cache("/c/cty/x.dat".into())),
        );
        assert_eq!(
            pinned.describe(),
            format!("pinned (sha256 {sha}, /c/cty/x.dat)")
        );
        assert_eq!(
            left_alone_note(&pinned).unwrap(),
            "note: cty_sha256 is set, so list refresh leaves cty.dat alone"
        );
        let path = InUse::new(ListKind::Scp, sha, Origin::File("m.scp".into()));
        assert_eq!(
            left_alone_note(&path).unwrap(),
            "note: scp_path is set, so list refresh leaves MASTER.SCP alone"
        );
        assert_eq!(left_alone_note(&bundled), None);
    }

    /// A one-request-per-connection HTTP server answering every request
    /// with the next canned response (the last one repeats). Returns the
    /// base URL and the handle; requests seen are counted in the handle.
    struct Server {
        requests: Arc<std::sync::Mutex<Vec<String>>>,
    }

    fn serve(responses: Vec<Vec<u8>>) -> (String, Server) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = requests.clone();
        std::thread::spawn(move || {
            let mut i = 0;
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match stream.read(&mut byte) {
                        Ok(1) => head.push(byte[0]),
                        _ => break,
                    }
                }
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&head).into_owned());
                if responses.is_empty() {
                    continue;
                }
                let response = &responses[i.min(responses.len() - 1)];
                i += 1;
                let _ = stream.write_all(response);
            }
        });
        (url, Server { requests })
    }

    fn ok(body: &[u8], last_modified: Option<&str>) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n", body.len());
        if let Some(lm) = last_modified {
            out.push_str(&format!("Last-Modified: {lm}\r\n"));
        }
        out.push_str("Connection: close\r\n\r\n");
        let mut out = out.into_bytes();
        out.extend_from_slice(body);
        out
    }

    fn closed_url() -> HttpUrl {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        HttpUrl::parse(&format!("http://{addr}/MASTER.SCP")).unwrap()
    }

    fn fast_limits() -> FetchLimits {
        FetchLimits {
            connect_timeout: Duration::from_secs(2),
            deadline: Duration::from_secs(5),
            ..FetchLimits::default()
        }
    }

    fn refresher_for(
        kind: ListKind,
        url: HttpUrl,
        cache: Option<Cache>,
        lists: Arc<ReferenceLists>,
        now: SystemTime,
    ) -> Refresher {
        let setup = RefreshSetup {
            lists: vec![ListSetting {
                url,
                in_use: InUse::bundled(kind),
            }],
            cache,
            limits: fast_limits(),
        };
        Refresher::new(setup, lists, None, now)
    }

    #[test]
    fn refresh_once_updates_on_a_new_valid_body() {
        let body = qq9_cty();
        let (base, _server) = serve(vec![ok(
            body.as_bytes(),
            Some("Tue, 15 Sep 2026 20:52:28 GMT"),
        )]);
        let url = HttpUrl::parse(&format!("{base}/cty.dat")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path().to_path_buf());
        let lists = Arc::new(bundled_lists());
        let now = at(1_800_000_000);
        let mut refresher =
            refresher_for(ListKind::Cty, url, Some(cache.clone()), lists.clone(), now);
        assert_eq!(
            refresher.refresh_once(ListKind::Cty, now),
            ListRefreshOutcome::Updated
        );
        assert_eq!(lists.cty.generation(), 1);
        assert_eq!(lists.scp.generation(), 0);
        assert!(lists.cty.load().is_allocated("QQ9ZZZ"));
        let sha = sha256_hex(body.as_bytes());
        assert!(cache.version_path(ListKind::Cty, &sha).is_file());
        let st = cache.load_state(ListKind::Cty).unwrap().unwrap();
        assert_eq!(st.sha256, sha);
        assert_eq!(st.checked_at, unix_secs(now));
        assert_eq!(
            st.last_modified.as_deref(),
            Some("Tue, 15 Sep 2026 20:52:28 GMT")
        );
        assert_eq!(refresher.runs[0].in_use.origin, Origin::Download);
        assert_eq!(refresher.next_wake(), Some(now + ListKind::Cty.interval()));
    }

    #[test]
    fn refresh_once_keeps_the_table_on_failure() {
        let now = at(1_800_000_000);
        // A closed port.
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path().to_path_buf());
        let lists = Arc::new(bundled_lists());
        let mut refresher = refresher_for(
            ListKind::Scp,
            closed_url(),
            Some(cache.clone()),
            lists.clone(),
            now,
        );
        assert_eq!(
            refresher.refresh_once(ListKind::Scp, now),
            ListRefreshOutcome::Failed
        );
        assert_eq!(lists.scp.generation(), 0);
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());

        // A web page: rejected by the acceptance checks.
        let (base, _server) = serve(vec![ok(b"<html><body>404 Not Found</body></html>\n", None)]);
        let url = HttpUrl::parse(&format!("{base}/MASTER.SCP")).unwrap();
        let mut refresher =
            refresher_for(ListKind::Scp, url, Some(cache.clone()), lists.clone(), now);
        assert_eq!(
            refresher.refresh_once(ListKind::Scp, now),
            ListRefreshOutcome::Failed
        );
        assert_eq!(lists.scp.generation(), 0);
        assert_eq!(cache.load_state(ListKind::Scp).unwrap(), None);
        assert_eq!(refresher.runs[0].in_use, InUse::bundled(ListKind::Scp));
    }

    #[test]
    fn refresh_once_on_304_or_identical_bytes_is_unchanged() {
        let now = at(1_800_000_000);
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path().to_path_buf());
        let bytes = manta_spot::MASTER_SCP.as_bytes();
        let (base, server) = serve(vec![
            ok(bytes, Some("Fri, 09 Oct 2026 00:00:00 GMT")),
            b"HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n".to_vec(),
        ]);
        let url = HttpUrl::parse(&format!("{base}/MASTER.SCP")).unwrap();
        let lists = Arc::new(bundled_lists());
        let mut refresher =
            refresher_for(ListKind::Scp, url, Some(cache.clone()), lists.clone(), now);
        // The same bytes as the built-in copy: unchanged, state written.
        assert_eq!(
            refresher.refresh_once(ListKind::Scp, now),
            ListRefreshOutcome::Unchanged
        );
        assert_eq!(lists.scp.generation(), 0);
        let st = cache.load_state(ListKind::Scp).unwrap().unwrap();
        assert_eq!(st.checked_at, unix_secs(now));
        // A day later: conditional GET, 304, checked_at advances.
        let later = now + DAY;
        assert_eq!(
            refresher.refresh_once(ListKind::Scp, later),
            ListRefreshOutcome::Unchanged
        );
        assert_eq!(lists.scp.generation(), 0);
        let st = cache.load_state(ListKind::Scp).unwrap().unwrap();
        assert_eq!(st.checked_at, unix_secs(later));
        let requests = server.requests.lock().unwrap();
        assert!(requests[1].contains("If-Modified-Since: Fri, 09 Oct 2026 00:00:00 GMT"));
    }

    #[test]
    fn the_loop_exits_promptly_when_stop_is_set() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path().to_path_buf());
        let now = SystemTime::now();
        for kind in ListKind::ALL {
            let mut st = state(&bundled_sha256(kind), now);
            st.url = "http://127.0.0.1:9/x".to_string();
            cache.write_state(kind, &st).unwrap();
        }
        let setup = RefreshSetup {
            lists: ListKind::ALL
                .iter()
                .map(|&kind| ListSetting {
                    url: HttpUrl::parse("http://127.0.0.1:9/x").unwrap(),
                    in_use: InUse::bundled(kind),
                })
                .collect(),
            cache: Some(cache),
            limits: fast_limits(),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let lists = Arc::new(bundled_lists());
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let thread_stop = stop.clone();
        std::thread::spawn(move || {
            let mut refresher = Refresher::new(setup, lists, None, SystemTime::now());
            run_loop(&mut refresher, &thread_stop);
            done_tx.send(()).unwrap();
        });
        std::thread::sleep(Duration::from_millis(200));
        stop.store(true, Ordering::Relaxed);
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the loop exits within 2 s of stop");
    }
}
