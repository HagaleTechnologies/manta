//! Shared registry of spawned per-client connection tasks, so a shutdown
//! sequence can genuinely AWAIT their completion instead of guessing a
//! fixed grace period. A blind `sleep(N)` before `Runtime::shutdown_timeout`
//! (an earlier version of the shutdown sequence) gives spawned tasks SOME
//! scheduler time, but no guarantee they actually finish -- a write that
//! takes longer than the guessed sleep is aborted just the same, and the
//! sleep is paid in full even when every task finished in the first few
//! milliseconds (round-10 review finding). Tracking real `JoinHandle`s and
//! awaiting them, bounded by an overall deadline, fixes both.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
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
}
