//! MAN-301: the live comparator. Reads published spots off the [`SpotBus`],
//! RBN reports off the [`feed`](super::feed), and every [`TICK`] closes the
//! spots whose comparison window has passed: each grade is serialised once
//! and published on the JSON stream ([`SpotBus::publish_grade`]) and the
//! `manta_rbn_compare_*` metrics.
//!
//! One [`Grader`] behind a `std::sync::Mutex`, shared by three tasks (spots,
//! feed, tick). No lock is ever held across an `.await`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

use super::feed::{self, FeedConfig, FeedEvent};
use super::grade::{GradeConfig, GradeInput, Grader, SpotGrade, Submit};
use super::Clock;
use crate::bus::{BusSpot, SpotBus};
use crate::metrics::Metrics;

/// How often pending spots are checked for a closed window. A grade is
/// published at most this long after its `windowClose`.
pub const TICK: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct LiveConfig {
    pub feed: FeedConfig,
    pub grade: GradeConfig,
    pub station_callsign: String,
    /// [`TICK`] in production; tests shorten it.
    pub tick: Duration,
}

type SharedGrader = Arc<Mutex<Grader>>;

fn lock(grader: &SharedGrader) -> std::sync::MutexGuard<'_, Grader> {
    grader.lock().expect("rbn_compare grader lock poisoned")
}

/// Starts the comparator. The spot subscription is taken here, before any
/// task is spawned, so no spot published after `spawn` returns is missed
/// (the same subscribe-before-spawn rule as `json_stream::serve`'s accept
/// loop). The returned handle completes once `shutdown` fires and all
/// three tasks have stopped.
pub fn spawn(
    cfg: LiveConfig,
    bus: Arc<SpotBus>,
    metrics: Arc<Metrics>,
    clock: Clock,
    shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    let LiveConfig {
        feed: feed_cfg,
        grade: grade_cfg,
        station_callsign,
        tick,
    } = cfg;
    let spotters = if grade_cfg.spotters.is_empty() {
        "all".to_string()
    } else {
        grade_cfg
            .spotters
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(",")
    };
    tracing::info!(
        target: "manta_server::rbn_compare",
        "rbn_compare: grading spots against {}:{} (window {} s, spotters {}); read-only: manta sends only its login callsign {}",
        feed_cfg.host,
        feed_cfg.port,
        grade_cfg.window_secs,
        spotters,
        feed_cfg.login_callsign,
    );

    let grader: SharedGrader = Arc::new(Mutex::new(Grader::new(grade_cfg)));
    let spots_rx = bus.subscribe();

    let spots_task = tokio::spawn(run_spots(
        spots_rx,
        Arc::clone(&bus),
        station_callsign,
        Arc::clone(&grader),
        Arc::clone(&metrics),
        shutdown.clone(),
    ));

    let sink_grader = Arc::clone(&grader);
    let sink = move |event: FeedEvent| {
        let mut g = lock(&sink_grader);
        match event {
            FeedEvent::Up { at } => g.feed_up(at),
            FeedEvent::Report(report) => {
                g.observe(report);
            }
            FeedEvent::Down { at } => g.feed_down(at),
        }
    };
    let feed_task = tokio::spawn(feed::run(
        feed_cfg,
        Arc::clone(&clock),
        sink,
        Arc::clone(&metrics),
        shutdown.clone(),
    ));

    let tick_task = tokio::spawn(run_ticks(
        tick,
        Arc::clone(&grader),
        bus,
        Arc::clone(&metrics),
        clock,
        shutdown,
    ));

    tokio::spawn(async move {
        for (name, task) in [
            ("spots", spots_task),
            ("feed", feed_task),
            ("tick", tick_task),
        ] {
            if let Err(e) = task.await {
                tracing::error!(
                    target: "manta_server::rbn_compare",
                    "rbn_compare: {name} task ended abnormally: {e}"
                );
            }
        }
        let pending = lock(&grader).pending();
        metrics.set_rbn_pending(pending);
        tracing::info!(
            target: "manta_server::rbn_compare",
            "rbn_compare: stopped; {pending} spot(s) still inside their window were not graded"
        );
    })
}

/// True once `shutdown` has fired (or its sender is gone).
fn is_shutdown(shutdown: &watch::Receiver<bool>) -> bool {
    shutdown.has_changed().is_err() || *shutdown.borrow()
}

/// Spot intake. Internal consumer: a `Lagged(n)` is counted as `n`
/// ungraded spots and the task carries on, rather than disconnecting the
/// way a slow network client is.
async fn run_spots(
    mut rx: broadcast::Receiver<BusSpot>,
    bus: Arc<SpotBus>,
    station_callsign: String,
    grader: SharedGrader,
    metrics: Arc<Metrics>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        if is_shutdown(&shutdown) {
            return;
        }
        tokio::select! {
            _ = shutdown.changed() => {}
            received = rx.recv() => match received {
                Ok(bus_spot) => {
                    let input = GradeInput::from_bus_spot(&bus_spot.spot, &bus, &station_callsign);
                    let (outcome, pending) = {
                        let mut g = lock(&grader);
                        let outcome = g.submit(input);
                        (outcome, g.pending())
                    };
                    if outcome == Submit::Full {
                        metrics.add_rbn_ungraded(1);
                    }
                    metrics.set_rbn_pending(pending);
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        target: "manta_server::rbn_compare",
                        "rbn_compare: fell {n} spot(s) behind the spot bus; they will not be graded"
                    );
                    metrics.add_rbn_ungraded(n);
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
        }
    }
}

/// Every `tick`: close the spots whose window has passed and publish their
/// grades, then refresh the pending gauge.
async fn run_ticks(
    tick: Duration,
    grader: SharedGrader,
    bus: Arc<SpotBus>,
    metrics: Arc<Metrics>,
    clock: Clock,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut interval = tokio::time::interval(tick);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        if is_shutdown(&shutdown) {
            return;
        }
        tokio::select! {
            _ = shutdown.changed() => {}
            _ = interval.tick() => {
                // `select!` picks a ready branch at random: a tick ready on
                // the same poll as shutdown must not publish grades the JSON
                // clients, already draining, may never send. Due spots stay
                // pending and are counted in the stop log. A shutdown that
                // lands after this check is not caught here.
                if is_shutdown(&shutdown) {
                    return;
                }
                let (grades, pending) = {
                    let mut g = lock(&grader);
                    let grades = g.close_due(clock());
                    (grades, g.pending())
                };
                emit(&grades, &bus, &metrics);
                metrics.set_rbn_pending(pending);
            }
        }
    }
}

/// Serialises each grade once and hands the same line to the JSON stream
/// and the grade to the metrics.
fn emit(grades: &[SpotGrade], bus: &SpotBus, metrics: &Metrics) {
    for grade in grades {
        let line: Arc<str> = serde_json::to_string(grade)
            .expect("SpotGrade always serializes")
            .into();
        bus.publish_grade(line);
        metrics.record_rbn_grade(grade);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::task::Poll;

    /// Polls `fut` exactly once; true when it completed.
    async fn poll_once(mut fut: std::pin::Pin<&mut impl Future<Output = ()>>) -> bool {
        std::future::poll_fn(|cx| Poll::Ready(fut.as_mut().poll(cx).is_ready())).await
    }

    /// Shutdown and a due tick both ready when `run_ticks` is next polled:
    /// whichever branch `select!` picks, no grade reaches the JSON stream.
    /// `select!` starts at a random branch, so 32 rounds all pass only if
    /// the tick branch honours shutdown too.
    #[tokio::test(start_paused = true)]
    async fn a_tick_ready_with_shutdown_publishes_no_grade() {
        for round in 0..32 {
            let bus = Arc::new(SpotBus::new(1.0, std::time::SystemTime::UNIX_EPOCH, 0));
            let mut json = bus.subscribe_json();
            let metrics = Arc::new(Metrics::new());
            let grader: SharedGrader = Arc::new(Mutex::new(Grader::new(GradeConfig::default())));
            let (tx, rx) = watch::channel(false);
            let clock: Clock = Arc::new(|| 10_000);
            let ticks = run_ticks(
                Duration::from_secs(1),
                Arc::clone(&grader),
                Arc::clone(&bus),
                metrics,
                clock,
                rx,
            );
            tokio::pin!(ticks);
            // The interval's first tick is immediate and finds nothing due.
            assert!(!poll_once(ticks.as_mut()).await);
            lock(&grader).submit(GradeInput {
                spot_id: format!("spot-{round}"),
                call: "W1AW".into(),
                time: 0,
                freq_hz: 14_025_000,
                wpm: Some(20),
            });
            // The next tick fires, and shutdown lands, before the next poll.
            tokio::time::advance(Duration::from_secs(1)).await;
            tx.send(true).unwrap();
            assert!(
                poll_once(ticks.as_mut()).await,
                "round {round}: still running"
            );
            assert!(
                json.try_recv().is_err(),
                "round {round}: a grade was published after shutdown"
            );
            assert_eq!(lock(&grader).pending(), 1, "round {round}");
        }
    }
}
