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
