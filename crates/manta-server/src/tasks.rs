//! Shared registry of spawned per-client connection tasks, so a shutdown
//! sequence can genuinely AWAIT their completion instead of guessing a
//! fixed grace period. A blind `sleep(N)` before `Runtime::shutdown_timeout`
//! (an earlier version of the shutdown sequence) gives spawned tasks SOME
//! scheduler time, but no guarantee they actually finish -- a write that
//! takes longer than the guessed sleep is aborted just the same, and the
//! sleep is paid in full even when every task finished in the first few
//! milliseconds (round-10 review finding). Tracking real `JoinHandle`s and
//! awaiting them, bounded by an overall deadline, fixes both.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;

/// Shared handle a server module's accept loop spawns per-client tasks
/// into (via `tasks.lock().await.spawn(future)`), and the shutdown
/// sequence later drains via `await_all`.
pub type ClientTasks = Arc<Mutex<JoinSet<()>>>;

pub fn new_client_tasks() -> ClientTasks {
    Arc::new(Mutex::new(JoinSet::new()))
}

/// Awaits every currently-tracked task to completion, bounded by
/// `deadline`. Returns once the set is empty or `deadline` elapses,
/// whichever comes first -- a caller relying on strict cleanup should
/// still follow this with a final hard deadline (e.g.
/// `Runtime::shutdown_timeout`) as a backstop for anything left running.
pub async fn await_all(tasks: &ClientTasks, deadline: Duration) {
    let mut set = tasks.lock().await;
    let _ =
        tokio::time::timeout(deadline, async { while set.join_next().await.is_some() {} }).await;
}

/// How often the background reaper removes already-completed entries from
/// the registry. Bounds how long a finished connection's `JoinHandle`
/// (and its retained task-result allocation) lingers -- without this,
/// `await_all` (called only at shutdown) is the ONLY thing that ever
/// calls `join_next`, so ordinary connect/disconnect churn on a
/// long-running daemon grows the registry without bound (round-11 review
/// finding).
const REAP_INTERVAL: Duration = Duration::from_secs(10);

/// Spawns a background task that periodically removes already-finished
/// entries from `tasks`, keeping the registry's size bounded by ongoing
/// connection churn instead of growing for the life of the process. Each
/// pass briefly locks `tasks` and drains everything currently finished via
/// the NON-blocking `try_join_next` (never `join_next`, which would hold
/// the lock until some future task happens to complete -- starving every
/// accept loop's own `tasks.lock().await.spawn(..)` for however long that
/// takes). Returns the reaper's own `JoinHandle`; letting it run
/// unawaited for the process lifetime is fine; it holds only a clone of
/// `tasks` and does no other work.
pub fn spawn_reaper(tasks: ClientTasks) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(REAP_INTERVAL).await;
            let mut set = tasks.lock().await;
            while set.try_join_next().is_some() {}
        }
    })
}

/// Bounds how many client connections an accept loop admits concurrently.
/// Each accepted connection acquires one owned permit (via
/// `limiter.acquire_owned()`) before its handler task is spawned, and holds
/// it for the connection's lifetime -- with no limit at all, an
/// unauthenticated client could open connections without bound, each one
/// costing a socket/task/broadcast-subscription and, on the JSON/WebSocket
/// and telnet listeners, its own share of every future publish's fan-out
/// cost (round-15 review finding). Bounding the ACCEPT loop itself (not
/// merely disconnecting late) means a flood beyond capacity is left
/// waiting in the OS's own connection backlog rather than ever being
/// admitted at all.
pub type ConnectionLimiter = Arc<Semaphore>;

pub fn new_connection_limiter(max_concurrent: usize) -> ConnectionLimiter {
    Arc::new(Semaphore::new(max_concurrent))
}

/// Per-source-IP connection quota (MAN-61,
/// `docs/DECISIONS/2026-09-03-man61-per-ip-connection-quota.md`).
/// `ConnectionLimiter` bounds only the TOTAL ceiling shared across every
/// client combined -- it does not prevent one source from permanently
/// holding a large share (up to all) of that ceiling by opening many
/// connections and never sending anything further after login/handshake
/// (a telnet client after login; a raw JSON/WS client, whose entire point
/// is being quiet forever). `IpQuota` caps how many of the shared
/// ceiling's permits a single source IP may hold concurrently, so one
/// misbehaving or malicious source can only ever deny a bounded slice of
/// capacity, not all of it -- other IPs still get admitted while one
/// source is parked at ITS OWN cap. `std::sync::Mutex`, not
/// `tokio::sync::Mutex`: the critical section is a plain hashmap
/// increment/decrement with no `.await` inside it, so a blocking lock is
/// both simpler and correct (holding it briefly never blocks the async
/// runtime in a way that matters).
#[derive(Clone)]
pub struct IpQuota {
    max_per_ip: usize,
    counts: Arc<StdMutex<HashMap<IpAddr, usize>>>,
}

impl IpQuota {
    pub fn new(max_per_ip: usize) -> Self {
        Self {
            max_per_ip,
            counts: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    /// Like `new`, but `override_val` (from
    /// `ServerConfig::max_connections_per_ip`) takes precedence over
    /// `default` when present (PR #81 review, round 1): a reverse-proxy
    /// TLS-termination deployment (`docs/RUNBOOKS/network-exposure.md`)
    /// has every downstream client sharing the proxy's own IP as far as
    /// `peer.ip()` is concerned, so a listener's built-in per-IP default
    /// would otherwise cap TOTAL concurrent clients at the quota instead
    /// of the listener's real capacity. `Some(0)` means no per-IP cap at
    /// all (only the listener's total `ConnectionLimiter` ceiling
    /// applies); `Some(n)` for `n > 0` uses that cap directly; `None`
    /// falls back to `default`.
    pub fn new_with_override(default: usize, override_val: Option<usize>) -> Self {
        let max_per_ip = match override_val {
            None => default,
            Some(0) => usize::MAX,
            Some(n) => n,
        };
        Self::new(max_per_ip)
    }

    /// Reserves one slot for `ip`, returning `None` if `ip` is already at
    /// `max_per_ip` -- the caller must then decline the connection (drop
    /// the socket without spawning a handler or consuming a
    /// `ConnectionLimiter` permit) rather than admit it.
    pub fn try_acquire(&self, ip: IpAddr) -> Option<IpQuotaGuard> {
        let mut counts = self.counts.lock().unwrap();
        let entry = counts.entry(ip).or_insert(0);
        if *entry >= self.max_per_ip {
            return None;
        }
        *entry += 1;
        Some(IpQuotaGuard {
            ip,
            counts: self.counts.clone(),
        })
    }
}

/// Held for the life of one admitted connection; releases its IP's slot
/// on drop (connection handler task completing, panicking, or being
/// aborted all release it the same way, matching `ConnectionLimiter`'s
/// own `OwnedSemaphorePermit` drop-releases-on-completion behavior).
pub struct IpQuotaGuard {
    ip: IpAddr,
    counts: Arc<StdMutex<HashMap<IpAddr, usize>>>,
}

impl Drop for IpQuotaGuard {
    fn drop(&mut self) {
        let mut counts = self.counts.lock().unwrap();
        if let Some(entry) = counts.get_mut(&self.ip) {
            *entry -= 1;
            if *entry == 0 {
                counts.remove(&self.ip);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[tokio::test(start_paused = true)]
    async fn await_all_waits_for_a_task_that_finishes_within_the_deadline() {
        let tasks = new_client_tasks();
        let done = Arc::new(AtomicBool::new(false));
        let done_task = done.clone();
        tasks.lock().await.spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            done_task.store(true, Ordering::SeqCst);
        });

        await_all(&tasks, Duration::from_secs(2)).await;

        assert!(
            done.load(Ordering::SeqCst),
            "a task that finishes well within the deadline must actually be awaited to completion"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn await_all_does_not_wait_the_full_deadline_once_all_tasks_are_done() {
        // The specific improvement over a blind fixed sleep: once the last
        // tracked task finishes, await_all returns immediately rather than
        // consuming the rest of its deadline budget regardless.
        let tasks = new_client_tasks();
        tasks.lock().await.spawn(async {
            tokio::time::sleep(Duration::from_millis(10)).await;
        });

        let start = tokio::time::Instant::now();
        await_all(&tasks, Duration::from_secs(30)).await;
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_secs(1),
            "must return once the task finishes, not consume the full 30s deadline; elapsed={elapsed:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn reaper_removes_completed_tasks_without_waiting_for_shutdown() {
        // Regression (round-11 review): `await_all` is the ONLY thing that
        // ever called `join_next` on this registry, and it's only ever
        // called at shutdown. During normal operation, every connection
        // that completes (a client connects then disconnects) leaves its
        // finished JoinHandle sitting in the JoinSet forever -- on a
        // long-running daemon with ordinary reconnect churn, this grows
        // without bound. `spawn_reaper` must periodically drain finished
        // entries on its own, independent of shutdown ever happening.
        let tasks = new_client_tasks();
        tasks.lock().await.spawn(async {});
        // Give the trivial task a chance to actually complete before the
        // reaper's first tick.
        tokio::task::yield_now().await;
        assert_eq!(
            tasks.lock().await.len(),
            1,
            "task must still be registered pre-reap"
        );

        let _reaper = spawn_reaper(tasks.clone());
        // The reaper needs at least one poll to reach its first
        // `sleep(REAP_INTERVAL).await` and register that timer BEFORE
        // advancing the clock -- otherwise `advance` below runs while the
        // freshly-spawned task hasn't started at all yet, and there's no
        // timer registered for it to fire.
        tokio::task::yield_now().await;
        tokio::time::advance(REAP_INTERVAL + Duration::from_millis(1)).await;
        // Let the reaper task actually run its post-sleep steps (wake,
        // acquire the lock, drain) -- each is a separate poll point, so
        // one yield isn't reliably enough to observe them all settle.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        assert_eq!(
            tasks.lock().await.len(),
            0,
            "the reaper must have removed the completed task without any call to await_all"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn await_all_bounds_a_task_that_never_finishes() {
        let tasks = new_client_tasks();
        let started = Arc::new(AtomicUsize::new(0));
        let started_task = started.clone();
        tasks.lock().await.spawn(async move {
            started_task.fetch_add(1, Ordering::SeqCst);
            std::future::pending::<()>().await;
        });

        let start = tokio::time::Instant::now();
        await_all(&tasks, Duration::from_millis(200)).await;
        let elapsed = start.elapsed();

        assert_eq!(
            started.load(Ordering::SeqCst),
            1,
            "the task must have actually run"
        );
        assert!(
            elapsed >= Duration::from_millis(200) && elapsed < Duration::from_secs(2),
            "a never-finishing task must not block past the deadline; elapsed={elapsed:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn connection_limiter_blocks_admission_past_capacity_until_a_permit_is_released() {
        let limiter = new_connection_limiter(2);
        let first = limiter.clone().acquire_owned().await.unwrap();
        let _second = limiter.clone().acquire_owned().await.unwrap();

        // A 3rd acquire beyond the capacity of 2 must not resolve while
        // both existing permits are still held.
        let blocked =
            tokio::time::timeout(Duration::from_millis(100), limiter.clone().acquire_owned()).await;
        assert!(
            blocked.is_err(),
            "a 3rd connection must be left waiting while capacity is exhausted"
        );

        // Releasing one existing permit (simulating that connection's
        // handler task completing) must free capacity for the waiter.
        drop(first);
        let third =
            tokio::time::timeout(Duration::from_millis(100), limiter.clone().acquire_owned()).await;
        assert!(
            third.is_ok(),
            "releasing a permit must admit the next waiting connection"
        );
    }

    fn ip(octet: u8) -> IpAddr {
        IpAddr::from([127, 0, 0, octet])
    }

    #[test]
    fn ip_quota_rejects_a_source_past_its_own_cap_but_admits_other_sources() {
        let quota = IpQuota::new(2);
        let addr = ip(1);
        let other = ip(2);

        let _g1 = quota.try_acquire(addr).expect("1st connection from addr");
        let _g2 = quota.try_acquire(addr).expect("2nd connection from addr");
        assert!(
            quota.try_acquire(addr).is_none(),
            "a 3rd connection from the SAME source must be rejected at its per-IP cap"
        );

        assert!(
            quota.try_acquire(other).is_some(),
            "a DIFFERENT source must still be admitted while addr is at its own cap -- \
             this is the whole point of a per-IP quota over a global one"
        );
    }

    #[test]
    fn ip_quota_frees_a_slot_when_its_guard_drops() {
        let quota = IpQuota::new(1);
        let addr = ip(1);

        let g1 = quota.try_acquire(addr).unwrap();
        assert!(
            quota.try_acquire(addr).is_none(),
            "at cap while g1 is still held"
        );

        drop(g1);
        assert!(
            quota.try_acquire(addr).is_some(),
            "dropping the guard (connection handler completing) must free the slot"
        );
    }

    /// PR #81 review, round 1: a reverse-proxy deployment needs to raise
    /// or disable the per-IP cap since every client shares the proxy's IP.
    #[test]
    fn ip_quota_override_disables_the_cap_at_zero_and_uses_default_when_unset() {
        let addr = ip(1);

        let disabled = IpQuota::new_with_override(2, Some(0));
        // Guards held (not dropped per-iteration), so the count genuinely
        // accumulates -- otherwise every acquire trivially succeeds
        // regardless of any cap, testing nothing.
        let mut disabled_guards = Vec::new();
        for _ in 0..100 {
            disabled_guards.push(
                disabled
                    .try_acquire(addr)
                    .expect("Some(0) must mean no per-IP cap at all"),
            );
        }

        let overridden = IpQuota::new_with_override(2, Some(5));
        let mut overridden_guards = Vec::new();
        for _ in 0..5 {
            overridden_guards.push(overridden.try_acquire(addr).unwrap());
        }
        assert!(
            overridden.try_acquire(addr).is_none(),
            "Some(n) must use n as the cap, not the default"
        );

        let defaulted = IpQuota::new_with_override(2, None);
        let _g1 = defaulted.try_acquire(addr).unwrap();
        let _g2 = defaulted.try_acquire(addr).unwrap();
        assert!(
            defaulted.try_acquire(addr).is_none(),
            "None must fall back to the listener's own default cap"
        );
    }
}
