//! MAN-318: the daemon's `manta_spot::CallsignLookup`, backed by QRZ.com.
//! The decode thread asks (`check`, never blocking) and collects answers
//! once per batch (`settled`); the `manta-lookup` thread does the network
//! work. Answers live 24 h; a held spot waits at most 10 s. See
//! docs/DECISIONS/2026-10-27-man318-callsign-lookup.md.

use crate::config::CallsignLookupConfig;
use crate::qrz::{Failure, QrzClient, Subscription};
use anyhow::{Context, Result};
use manta_server::backoff::{next_backoff, AttemptOutcome, INITIAL_BACKOFF};
use manta_server::metrics::{CallsignLookupOutcome, Metrics};
use manta_spot::{CallsignLookup, Existence, LookupStatus};
use std::collections::{BTreeMap, VecDeque};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

pub(crate) const LOOKUP_DEADLINE: Duration = Duration::from_secs(10);
pub(crate) const ANSWER_TTL: Duration = Duration::from_secs(24 * 60 * 60);
pub(crate) const CACHE_CAPACITY: usize = 20_000;
pub(crate) const QUEUE_CAPACITY: usize = 256;
/// QRZ blocks a user name for 10 minutes after a wrong password (observed
/// 2026-10-27); "Connection refused" means no login for 24 h (QRZ spec).
pub(crate) const REFUSED_LOGIN_PAUSE: Duration = Duration::from_secs(600);
pub(crate) const REFUSED_SERVICE_PAUSE: Duration = Duration::from_secs(24 * 60 * 60);

/// The time source; `Instant::now` outside tests.
type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;
/// Where log lines go: `warning: ...` lines as warnings, the rest as notes.
type Log = Arc<dyn Fn(&str) + Send + Sync>;

/// The worker's answer to one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Answer {
    Found,
    NotFound,
    Failed,
}

impl Answer {
    fn outcome(self) -> CallsignLookupOutcome {
        match self {
            Answer::Found => CallsignLookupOutcome::Found,
            Answer::NotFound => CallsignLookupOutcome::NotFound,
            Answer::Failed => CallsignLookupOutcome::Failed,
        }
    }
}

/// A call held for an answer.
#[derive(Debug, Clone, Copy)]
struct Pending {
    /// Request order, for `settled`'s oldest-first output.
    seq: u64,
    deadline: Instant,
    /// Spots held for this call.
    spots: u64,
}

/// The cache, the pending calls and the per-spot counts; every method
/// takes `now`, so tests never sleep.
#[derive(Default)]
struct LookupState {
    /// Answer and when it was received; `order` is the eviction queue.
    cache: BTreeMap<String, (Existence, Instant)>,
    order: VecDeque<(Instant, String)>,
    pending: BTreeMap<String, Pending>,
    next_seq: u64,
    /// Spots not yet moved into `Metrics`.
    withheld: u64,
    unverified: u64,
}

impl LookupState {
    /// One would-be spot of `call`. `send` queues a request and says
    /// whether the queue took it.
    fn check(
        &mut self,
        call: &str,
        now: Instant,
        send: &mut dyn FnMut(&str) -> bool,
    ) -> LookupStatus {
        if let Some(&(existence, at)) = self.cache.get(call) {
            if now.saturating_duration_since(at) < ANSWER_TTL {
                if existence == Existence::NotFound {
                    self.withheld += 1;
                }
                return LookupStatus::Known(existence);
            }
            self.cache.remove(call);
        }
        if let Some(p) = self.pending.get_mut(call) {
            p.spots += 1;
            return LookupStatus::Pending;
        }
        if !send(call) {
            self.unverified += 1;
            return LookupStatus::Known(Existence::Unknown);
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.pending.insert(
            call.to_string(),
            Pending {
                seq,
                deadline: now + LOOKUP_DEADLINE,
                spots: 1,
            },
        );
        LookupStatus::Pending
    }

    /// The worker answered `call`. Caches a real answer (even one past its
    /// deadline) and settles the call if it is still pending.
    fn answer(
        &mut self,
        call: String,
        answer: Answer,
        now: Instant,
    ) -> Option<(u64, String, Existence)> {
        let existence = match answer {
            Answer::Found => Existence::Exists,
            Answer::NotFound => Existence::NotFound,
            Answer::Failed => Existence::Unknown,
        };
        if answer != Answer::Failed {
            self.remember(&call, existence, now);
        }
        let p = self.pending.remove(&call)?;
        match existence {
            Existence::NotFound => self.withheld += p.spots,
            Existence::Unknown => self.unverified += p.spots,
            Existence::Exists => {}
        }
        Some((p.seq, call, existence))
    }

    /// Settles every pending call whose deadline has passed as `Unknown`.
    fn expire(&mut self, now: Instant) -> Vec<(u64, String, Existence)> {
        let late: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, p)| p.deadline <= now)
            .map(|(call, _)| call.clone())
            .collect();
        late.into_iter()
            .filter_map(|call| {
                let p = self.pending.remove(&call)?;
                self.unverified += p.spots;
                Some((p.seq, call, Existence::Unknown))
            })
            .collect()
    }

    fn earliest_deadline(&self) -> Option<Instant> {
        self.pending.values().map(|p| p.deadline).min()
    }

    fn remember(&mut self, call: &str, existence: Existence, now: Instant) {
        self.cache.insert(call.to_string(), (existence, now));
        self.order.push_back((now, call.to_string()));
        while self.cache.len() > CACHE_CAPACITY || self.order.len() > 2 * CACHE_CAPACITY {
            let Some((at, oldest)) = self.order.pop_front() else {
                break;
            };
            // A later answer for the same call left this entry stale.
            if self.cache.get(&oldest).is_some_and(|&(_, t)| t == at) {
                self.cache.remove(&oldest);
            }
        }
    }

    fn take_counts(&mut self) -> (u64, u64) {
        (
            std::mem::take(&mut self.withheld),
            std::mem::take(&mut self.unverified),
        )
    }
}

/// The decode side: never blocks except in `settle_all`.
pub(crate) struct QrzLookup {
    state: Mutex<LookupState>,
    requests: SyncSender<String>,
    /// `Receiver` is not `Sync`; only the decode thread locks it.
    answers: Mutex<Receiver<(String, Answer)>>,
    clock: Clock,
    metrics: Option<Arc<Metrics>>,
    log: Log,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl QrzLookup {
    /// Records `answers` and the deadlines past at `now`, oldest request
    /// first, and moves the per-spot counts into `Metrics`.
    fn absorb(&self, answers: Vec<(String, Answer)>, now: Instant) -> Vec<(String, Existence)> {
        let mut state = lock(&self.state);
        let mut out = Vec::new();
        for (call, answer) in answers {
            if let Some(m) = &self.metrics {
                m.record_callsign_lookup(answer.outcome());
            }
            if answer == Answer::NotFound {
                (self.log)(&format!(
                    "callsign lookup: {call} not spotted: QRZ.com does not list it (answer kept 24 h)"
                ));
            }
            out.extend(state.answer(call, answer, now));
        }
        out.extend(state.expire(now));
        out.sort_by_key(|(seq, _, _)| *seq);
        let counts = state.take_counts();
        drop(state);
        self.publish(counts);
        out.into_iter().map(|(_, call, e)| (call, e)).collect()
    }

    fn publish(&self, (withheld, unverified): (u64, u64)) {
        if let Some(m) = &self.metrics {
            if withheld > 0 {
                m.record_callsign_lookup_withheld(withheld);
            }
            if unverified > 0 {
                m.record_callsign_lookup_unverified(unverified);
            }
        }
    }
}

impl CallsignLookup for QrzLookup {
    fn check(&self, call: &str) -> LookupStatus {
        let now = (self.clock)();
        let mut state = lock(&self.state);
        let status = state.check(call, now, &mut |c| {
            self.requests.try_send(c.to_string()).is_ok()
        });
        let counts = state.take_counts();
        drop(state);
        self.publish(counts);
        status
    }

    fn settled(&self) -> Vec<(String, Existence)> {
        let answers: Vec<(String, Answer)> = lock(&self.answers).try_iter().collect();
        self.absorb(answers, (self.clock)())
    }

    fn settle_all(&self) -> Vec<(String, Existence)> {
        let mut out = self.settled();
        loop {
            let Some(deadline) = lock(&self.state).earliest_deadline() else {
                break;
            };
            let wait = deadline.saturating_duration_since((self.clock)());
            // The state lock is not held while waiting.
            let got = lock(&self.answers).recv_timeout(wait);
            match got {
                Ok(answer) => out.extend(self.absorb(vec![answer], (self.clock)())),
                Err(RecvTimeoutError::Timeout) => {
                    out.extend(self.absorb(Vec::new(), deadline.max((self.clock)())))
                }
                Err(RecvTimeoutError::Disconnected) => {
                    out.extend(self.absorb(Vec::new(), Instant::now() + 2 * ANSWER_TTL));
                    break;
                }
            }
        }
        out
    }
}

/// The `manta-lookup` thread's state: the client, and the pauses after a
/// refused login (D8) or a failed request (backoff).
struct Worker {
    client: QrzClient,
    username: String,
    clock: Clock,
    log: Log,
    /// No login before this (a refused login).
    login_paused_until: Option<Instant>,
    /// No request before this (a timeout, network or HTTP error).
    backoff_until: Option<Instant>,
    backoff: Duration,
    /// Failed lookups since the last answer: the current episode.
    failures: u64,
}

impl Worker {
    fn new(client: QrzClient, username: &str, clock: Clock, log: Log) -> Self {
        Self {
            client,
            username: username.to_string(),
            clock,
            log,
            login_paused_until: None,
            backoff_until: None,
            backoff: INITIAL_BACKOFF,
            failures: 0,
        }
    }

    /// The eager login at start, so a bad password shows at once.
    fn login(&mut self) {
        match self.client.login() {
            Ok(logged_in) => {
                let line = match logged_in.subscription {
                    Subscription::Until(date) => format!(
                        "callsign lookup: logged in to QRZ.com as {} (subscription expires {date})",
                        self.username
                    ),
                    Subscription::None => format!(
                        "warning: callsign lookup: logged in to QRZ.com as {} without a Logbook \
                         Data subscription; QRZ limits non-subscriber access to testing",
                        self.username
                    ),
                    Subscription::Unknown => {
                        format!("callsign lookup: logged in to QRZ.com as {}", self.username)
                    }
                };
                (self.log)(&line);
            }
            Err(failure) => {
                self.failed(failure);
            }
        }
    }

    fn paused(&self, now: Instant) -> bool {
        self.backoff_until.is_some_and(|t| now < t)
            || (!self.client.has_key() && self.login_paused_until.is_some_and(|t| now < t))
    }

    /// One request off the queue.
    fn serve(&mut self, call: &str) -> Answer {
        if self.paused((self.clock)()) {
            self.failures += 1;
            return Answer::Failed;
        }
        match self.client.lookup(call) {
            Ok(existence) => {
                self.answered();
                if existence == Existence::NotFound {
                    Answer::NotFound
                } else {
                    Answer::Found
                }
            }
            Err(failure) => self.failed(failure),
        }
    }

    fn answered(&mut self) {
        if self.failures > 0 {
            (self.log)(&format!(
                "callsign lookup: QRZ.com answering again after {} failed lookups",
                self.failures
            ));
        }
        self.failures = 0;
        self.backoff_until = None;
        self.login_paused_until = None;
        self.backoff = next_backoff(self.backoff, &AttemptOutcome::Disconnected);
    }

    fn failed(&mut self, failure: Failure) -> Answer {
        let now = (self.clock)();
        let first = self.failures == 0;
        self.failures += 1;
        match failure {
            Failure::Refused { text, day_long } => {
                let pause = if day_long {
                    REFUSED_SERVICE_PAUSE
                } else {
                    REFUSED_LOGIN_PAUSE
                };
                self.login_paused_until = Some(now + pause);
                // At most one refused login per pause, so always logged.
                (self.log)(&format!(
                    "warning: callsign lookup: QRZ.com refused the login for {}: \"{text}\"; spots \
                     go out unverified; next login attempt in {} s",
                    self.username,
                    pause.as_secs()
                ));
            }
            other => {
                let wait = self.backoff;
                self.backoff_until = Some(now + wait);
                self.backoff = next_backoff(wait, &AttemptOutcome::NeverConnected);
                if first {
                    let what = match other {
                        Failure::Timeout => "did not answer within 10 s".to_string(),
                        Failure::Transport(t) | Failure::Protocol(t) => format!("failed: {t}"),
                        Failure::Refused { .. } => unreachable!("handled above"),
                    };
                    (self.log)(&format!(
                        "warning: callsign lookup: QRZ.com {what}; spots go out unverified \
                         (manta_callsign_lookup_unverified_total) until it does; next try in {} s",
                        wait.as_secs()
                    ));
                }
            }
        }
        Answer::Failed
    }

    fn run(mut self, requests: Receiver<String>, answers: Sender<(String, Answer)>) {
        self.login();
        while let Ok(call) = requests.recv() {
            let answer = self.serve(&call);
            if answers.send((call, answer)).is_err() {
                return;
            }
        }
    }
}

/// Spawns `manta-lookup` (logs in at once, then serves the queue) and
/// returns the decode side; `metrics` gets `enable_callsign_lookup` here.
pub(crate) fn start(
    cfg: &CallsignLookupConfig,
    metrics: Option<Arc<Metrics>>,
) -> Result<Arc<QrzLookup>> {
    let client = QrzClient::new(
        &cfg.qrz_url,
        &cfg.qrz_username,
        &cfg.qrz_password,
        crate::qrz::REQUEST_TIMEOUT,
    );
    let log: Log = Arc::new(|line: &str| {
        if line.starts_with("warning:") {
            crate::logging::warning(line);
        } else {
            crate::logging::note(line);
        }
    });
    spawn(
        client,
        &cfg.qrz_username,
        Arc::new(Instant::now),
        log,
        metrics,
    )
}

fn spawn(
    client: QrzClient,
    username: &str,
    clock: Clock,
    log: Log,
    metrics: Option<Arc<Metrics>>,
) -> Result<Arc<QrzLookup>> {
    let (requests, request_rx) = mpsc::sync_channel(QUEUE_CAPACITY);
    let (answer_tx, answers) = mpsc::channel();
    let worker = Worker::new(client, username, clock.clone(), log.clone());
    std::thread::Builder::new()
        .name("manta-lookup".into())
        .spawn(move || worker.run(request_rx, answer_tx))
        .context("starting the manta-lookup thread")?;
    if let Some(m) = &metrics {
        m.enable_callsign_lookup();
    }
    Ok(Arc::new(QrzLookup {
        state: Mutex::new(LookupState::default()),
        requests,
        answers: Mutex::new(answers),
        clock,
        metrics,
        log,
    }))
}

/// The startup line `run` prints with the lookup on.
pub(crate) fn startup_line(cfg: &CallsignLookupConfig) -> String {
    format!(
        "callsign lookup: on; QRZ.com as {}; a call QRZ does not list is not spotted, a lookup \
         unanswered after 10 s spots anyway; answers kept 24 h",
        cfg.qrz_username
    )
}

/// What `run` prints for a file source with the lookup on.
pub(crate) const FILE_REPLAY_NOTE: &str =
    "note: callsign lookup is off for file replay, so the run is repeatable";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qrz::tests::{Canned, Mock, FOUND, LOGIN_OK, NOT_FOUND};

    const H: Duration = Duration::from_secs(60 * 60);

    /// A queue of `capacity` that records what it took.
    struct Queue {
        capacity: usize,
        sent: Vec<String>,
    }

    impl Queue {
        fn new(capacity: usize) -> Self {
            Self {
                capacity,
                sent: Vec::new(),
            }
        }
        fn send(&mut self) -> impl FnMut(&str) -> bool + '_ {
            |call| {
                if self.sent.len() >= self.capacity {
                    return false;
                }
                self.sent.push(call.to_string());
                true
            }
        }
    }

    fn settled(
        state: &mut LookupState,
        answers: Vec<(&str, Answer)>,
        now: Instant,
    ) -> Vec<(String, Existence)> {
        let mut out: Vec<(u64, String, Existence)> = answers
            .into_iter()
            .filter_map(|(c, a)| state.answer(c.to_string(), a, now))
            .collect();
        out.extend(state.expire(now));
        out.sort_by_key(|(seq, _, _)| *seq);
        out.into_iter().map(|(_, c, e)| (c, e)).collect()
    }

    #[test]
    fn scenario_3_a_not_found_answer_is_reused_for_24_hours() {
        let t0 = Instant::now();
        let mut state = LookupState::default();
        let mut q = Queue::new(QUEUE_CAPACITY);
        assert_eq!(
            state.check("KQ9ZZZ", t0, &mut q.send()),
            LookupStatus::Pending
        );
        assert_eq!(q.sent, ["KQ9ZZZ"]);
        assert_eq!(
            settled(&mut state, vec![("KQ9ZZZ", Answer::NotFound)], t0),
            [("KQ9ZZZ".to_string(), Existence::NotFound)]
        );
        assert_eq!(
            state.check("KQ9ZZZ", t0 + H, &mut q.send()),
            LookupStatus::Known(Existence::NotFound)
        );
        assert_eq!(q.sent.len(), 1, "no second lookup within 24 h");
        assert_eq!(
            state.check(
                "KQ9ZZZ",
                t0 + 24 * H - Duration::from_secs(1),
                &mut q.send()
            ),
            LookupStatus::Known(Existence::NotFound)
        );
        assert_eq!(q.sent.len(), 1);
        assert_eq!(
            state.check(
                "KQ9ZZZ",
                t0 + 24 * H + Duration::from_secs(1),
                &mut q.send()
            ),
            LookupStatus::Pending
        );
        assert_eq!(q.sent.len(), 2, "one new lookup after 24 h");
    }

    #[test]
    fn a_found_answer_is_reused_for_24_hours() {
        let t0 = Instant::now();
        let mut state = LookupState::default();
        let mut q = Queue::new(QUEUE_CAPACITY);
        state.check("W1AW", t0, &mut q.send());
        settled(&mut state, vec![("W1AW", Answer::Found)], t0);
        assert_eq!(
            state.check("W1AW", t0 + H, &mut q.send()),
            LookupStatus::Known(Existence::Exists)
        );
        assert_eq!(q.sent.len(), 1);
        assert_eq!(
            state.check("W1AW", t0 + 24 * H + Duration::from_secs(1), &mut q.send()),
            LookupStatus::Pending
        );
        assert_eq!(q.sent.len(), 2);
    }

    #[test]
    fn scenario_2_an_unanswered_call_settles_unknown_at_10_seconds() {
        let t0 = Instant::now();
        let mut state = LookupState::default();
        let mut q = Queue::new(QUEUE_CAPACITY);
        state.check("W1AW", t0, &mut q.send());
        state.check("W1AW", t0, &mut q.send());
        assert!(settled(&mut state, vec![], t0 + Duration::from_millis(9_999)).is_empty());
        assert_eq!(
            settled(&mut state, vec![], t0 + LOOKUP_DEADLINE),
            [("W1AW".to_string(), Existence::Unknown)]
        );
        assert_eq!(state.take_counts(), (0, 2), "each held spot is unverified");
    }

    #[test]
    fn an_answer_after_the_deadline_still_fills_the_cache() {
        let t0 = Instant::now();
        let mut state = LookupState::default();
        let mut q = Queue::new(QUEUE_CAPACITY);
        state.check("W1AW", t0, &mut q.send());
        settled(&mut state, vec![], t0 + LOOKUP_DEADLINE);
        let late = t0 + Duration::from_secs(11);
        assert!(settled(&mut state, vec![("W1AW", Answer::NotFound)], late).is_empty());
        assert_eq!(
            state.check("W1AW", late, &mut q.send()),
            LookupStatus::Known(Existence::NotFound)
        );
        assert_eq!(q.sent.len(), 1);
    }

    #[test]
    fn a_failure_is_not_cached() {
        let t0 = Instant::now();
        let mut state = LookupState::default();
        let mut q = Queue::new(QUEUE_CAPACITY);
        state.check("W1AW", t0, &mut q.send());
        assert_eq!(
            settled(&mut state, vec![("W1AW", Answer::Failed)], t0),
            [("W1AW".to_string(), Existence::Unknown)]
        );
        assert_eq!(
            state.check("W1AW", t0, &mut q.send()),
            LookupStatus::Pending
        );
        assert_eq!(q.sent.len(), 2);
    }

    #[test]
    fn spots_of_one_pending_call_share_one_request() {
        let t0 = Instant::now();
        let mut q = Queue::new(QUEUE_CAPACITY);
        let mut state = LookupState::default();
        for _ in 0..3 {
            assert_eq!(
                state.check("W1AW", t0, &mut q.send()),
                LookupStatus::Pending
            );
        }
        assert_eq!(q.sent.len(), 1);
        settled(&mut state, vec![("W1AW", Answer::NotFound)], t0);
        assert_eq!(state.take_counts(), (3, 0));

        let mut state = LookupState::default();
        for _ in 0..3 {
            state.check("K1ABC", t0, &mut q.send());
        }
        settled(&mut state, vec![("K1ABC", Answer::Failed)], t0);
        assert_eq!(state.take_counts(), (0, 3));
    }

    #[test]
    fn a_full_queue_spots_unverified() {
        let t0 = Instant::now();
        let mut state = LookupState::default();
        let mut q = Queue::new(2);
        state.check("W1AW", t0, &mut q.send());
        state.check("K1ABC", t0, &mut q.send());
        assert_eq!(
            state.check("N0CALL", t0, &mut q.send()),
            LookupStatus::Known(Existence::Unknown)
        );
        assert_eq!(state.take_counts(), (0, 1));
    }

    #[test]
    fn the_cache_keeps_the_newest_20000_answers() {
        let t0 = Instant::now();
        let mut state = LookupState::default();
        for i in 0..=CACHE_CAPACITY {
            state.remember(
                &format!("K{i}"),
                Existence::Exists,
                t0 + Duration::from_millis(i as u64),
            );
        }
        assert_eq!(state.cache.len(), CACHE_CAPACITY);
        assert!(!state.cache.contains_key("K0"), "the oldest is evicted");
        assert!(state.cache.contains_key("K1"));
        assert!(state.cache.contains_key(&format!("K{CACHE_CAPACITY}")));
    }

    #[test]
    fn settled_reports_oldest_request_first() {
        let t0 = Instant::now();
        let mut state = LookupState::default();
        let mut q = Queue::new(QUEUE_CAPACITY);
        for call in ["W1AW", "K1ABC", "AA1A"] {
            state.check(call, t0, &mut q.send());
        }
        assert_eq!(
            settled(
                &mut state,
                vec![
                    ("AA1A", Answer::Found),
                    ("K1ABC", Answer::Found),
                    ("W1AW", Answer::Found)
                ],
                t0
            ),
            [
                ("W1AW".to_string(), Existence::Exists),
                ("K1ABC".to_string(), Existence::Exists),
                ("AA1A".to_string(), Existence::Exists),
            ]
        );
    }

    // ---- the worker and the service against a loopback QRZ mock ----

    /// A clock the test moves.
    fn manual_clock() -> (Clock, Arc<Mutex<Instant>>) {
        let now = Arc::new(Mutex::new(Instant::now()));
        let read = now.clone();
        (Arc::new(move || *read.lock().unwrap()), now)
    }

    fn capture() -> (Log, Arc<Mutex<Vec<String>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = lines.clone();
        (
            Arc::new(move |line: &str| sink.lock().unwrap().push(line.to_string())),
            lines,
        )
    }

    fn worker(mock: &Mock, password: &str, timeout: Duration, clock: Clock, log: Log) -> Worker {
        Worker::new(
            QrzClient::new(&mock.url(), "N1TEST", password, timeout),
            "N1TEST",
            clock,
            log,
        )
    }

    const REFUSED: &str = r#"<?xml version="1.0" ?>
<QRZDatabase version="1.36" xmlns="http://xmldata.qrz.com">
<Session><Error>Username/password incorrect</Error></Session>
</QRZDatabase>"#;

    #[test]
    fn the_worker_logs_in_at_start_and_reuses_the_key() {
        let mock = Mock::start(vec![
            Canned::ok(LOGIN_OK),
            Canned::ok(FOUND),
            Canned::ok(NOT_FOUND),
        ]);
        let (log, lines) = capture();
        let metrics = Arc::new(Metrics::new());
        let lookup = spawn(
            QrzClient::new(&mock.url(), "N1TEST", "pw", Duration::from_secs(5)),
            "N1TEST",
            Arc::new(Instant::now),
            log,
            Some(metrics.clone()),
        )
        .unwrap();
        assert_eq!(lookup.check("W1AW"), LookupStatus::Pending);
        assert_eq!(lookup.check("KQ9ZZZ"), LookupStatus::Pending);
        let mut got = lookup.settle_all();
        got.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            got,
            [
                ("KQ9ZZZ".to_string(), Existence::NotFound),
                ("W1AW".to_string(), Existence::Exists),
            ]
        );
        let seen = mock.seen();
        assert_eq!(seen.len(), 3, "1 login + 2 lookups: {seen:?}");
        assert!(seen[0]
            .1
            .starts_with("username=N1TEST&password=pw&agent=manta%2F"));
        assert!(seen[1].1.starts_with("s=k3y0001&callsign="));
        assert!(seen[2].1.starts_with("s=k3y0001&callsign="));
        let lines = lines.lock().unwrap();
        assert!(lines.iter().any(|l| l
            == "callsign lookup: logged in to QRZ.com as N1TEST (subscription expires Wed Jan 1 12:34:03 2027)"));
        assert!(lines.iter().any(|l| l
            == "callsign lookup: KQ9ZZZ not spotted: QRZ.com does not list it (answer kept 24 h)"));
        let text = metrics.render_prometheus_text();
        assert!(text.contains("manta_callsign_lookups_total{outcome=\"found\"} 1\n"));
        assert!(text.contains("manta_callsign_lookups_total{outcome=\"not_found\"} 1\n"));
        assert!(text.contains("manta_callsign_lookup_withheld_total 1\n"));
        assert_eq!(
            lookup.check("KQ9ZZZ"),
            LookupStatus::Known(Existence::NotFound),
            "cached"
        );
        assert!(metrics
            .render_prometheus_text()
            .contains("manta_callsign_lookup_withheld_total 2\n"));
    }

    #[test]
    fn a_refused_login_pauses_logins_for_600_seconds() {
        let mock = Mock::start(vec![Canned::ok(REFUSED)]);
        let (clock, now) = manual_clock();
        let (log, lines) = capture();
        let mut w = worker(&mock, "pw", Duration::from_secs(5), clock, log);
        w.login();
        assert_eq!(mock.seen().len(), 1);
        assert_eq!(w.serve("W1AW"), Answer::Failed);
        *now.lock().unwrap() += REFUSED_LOGIN_PAUSE - Duration::from_secs(1);
        assert_eq!(w.serve("W1AW"), Answer::Failed);
        assert_eq!(mock.seen().len(), 1, "no second login inside the pause");
        *now.lock().unwrap() += Duration::from_secs(1);
        assert_eq!(w.serve("W1AW"), Answer::Failed);
        assert_eq!(mock.seen().len(), 2, "one login once the pause ends");
        let lines = lines.lock().unwrap();
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.contains("refused the login"))
                .count(),
            2,
            "{lines:?}"
        );
        assert!(
            lines[0].contains("next login attempt in 600 s"),
            "{lines:?}"
        );
    }

    #[test]
    fn a_timeout_backs_off_1_then_2_seconds_and_an_answer_resets_it() {
        let mock = Mock::start(vec![
            Canned::ok(LOGIN_OK),
            Canned::Silent,
            Canned::Silent,
            Canned::ok(FOUND),
            Canned::Silent,
        ]);
        let (clock, now) = manual_clock();
        let (log, lines) = capture();
        let mut w = worker(&mock, "pw", Duration::from_millis(300), clock, log);
        let t0 = *now.lock().unwrap();
        let at = |d: Duration| *now.lock().unwrap() = t0 + d;
        w.login();
        assert_eq!(w.serve("W1AW"), Answer::Failed, "timeout");
        assert_eq!(mock.seen().len(), 2);
        at(Duration::from_millis(999));
        assert_eq!(w.serve("W1AW"), Answer::Failed);
        assert_eq!(mock.seen().len(), 2, "inside the 1 s pause");
        at(Duration::from_secs(1));
        assert_eq!(w.serve("W1AW"), Answer::Failed, "timeout again");
        assert_eq!(mock.seen().len(), 3);
        at(Duration::from_millis(2_999));
        assert_eq!(w.serve("W1AW"), Answer::Failed);
        assert_eq!(mock.seen().len(), 3, "inside the 2 s pause");
        at(Duration::from_secs(3));
        assert_eq!(w.serve("W1AW"), Answer::Found);
        assert_eq!(mock.seen().len(), 4);
        assert_eq!(w.serve("W1AW"), Answer::Failed, "timeout after the reset");
        assert_eq!(
            w.backoff_until,
            Some(t0 + Duration::from_secs(4)),
            "1 s again"
        );
        let lines = lines.lock().unwrap();
        assert!(lines.iter().any(|l| {
            l == "warning: callsign lookup: QRZ.com did not answer within 10 s; spots go out \
                unverified (manta_callsign_lookup_unverified_total) until it does; next try in 1 s"
        }));
        assert!(lines
            .iter()
            .any(|l| l == "callsign lookup: QRZ.com answering again after 4 failed lookups"));
    }

    #[test]
    fn settle_all_waits_for_in_flight_answers() {
        let mock = Mock::start(vec![
            Canned::ok(LOGIN_OK),
            Canned::Late(Duration::from_millis(200), FOUND.into()),
        ]);
        let (log, _) = capture();
        let lookup = spawn(
            QrzClient::new(&mock.url(), "N1TEST", "pw", Duration::from_secs(5)),
            "N1TEST",
            Arc::new(Instant::now),
            log,
            None,
        )
        .unwrap();
        let started = Instant::now();
        assert_eq!(lookup.check("W1AW"), LookupStatus::Pending);
        assert!(lookup.settled().is_empty());
        assert_eq!(
            lookup.settle_all(),
            [("W1AW".to_string(), Existence::Exists)]
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn the_password_never_appears_in_a_log_line() {
        let password = "s3cret-p&ss";
        let (log, lines) = capture();
        let mock = Mock::start(vec![Canned::ok(REFUSED)]);
        let (clock, _) = manual_clock();
        let mut w = worker(
            &mock,
            password,
            Duration::from_secs(5),
            clock.clone(),
            log.clone(),
        );
        w.login();
        w.serve("W1AW");
        let mock = Mock::start(vec![Canned::ok(LOGIN_OK), Canned::Silent]);
        let mut w = worker(&mock, password, Duration::from_millis(300), clock, log);
        w.login();
        w.serve("W1AW");
        let lines = lines.lock().unwrap();
        assert!(lines.len() >= 3, "{lines:?}");
        for line in lines.iter() {
            assert!(!line.contains(password), "{line}");
            assert!(!line.contains("s3cret"), "{line}");
            assert!(!line.contains("k3y0001"), "{line}");
        }
    }
}
