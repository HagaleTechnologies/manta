//! A small sliding-window rate limiter for per-connection client-initiated
//! traffic (telnet commands, WebSocket Ping frames) on publicly bound
//! endpoints. A pure lifetime cap (an earlier WS-Ping fix used one)
//! terminates well-behaved long-running consumers just for staying
//! connected a long time -- one Ping per minute hits a 60-count lifetime
//! cap after an hour regardless of how gently spaced the traffic was
//! (round-14 review finding). A rate window instead only disconnects a
//! client that's actually sending events FASTER than the allowed rate,
//! no matter how long the connection has been open.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::time::Instant;

/// Allows at most `max_per_window` events in any `window`-long span,
/// measured from whenever the window last reset (not a strict rolling
/// window) -- simple, and sufficient for bounding abuse without needing
/// per-event timestamp tracking.
pub struct RateLimiter {
    max_per_window: u32,
    window: Duration,
    window_start: Instant,
    count: u32,
}

impl RateLimiter {
    pub fn new(max_per_window: u32, window: Duration) -> Self {
        Self {
            max_per_window,
            window,
            window_start: Instant::now(),
            count: 0,
        }
    }

    /// Records one event and returns `true` if it's within budget, `false`
    /// if the caller has exceeded `max_per_window` events within the
    /// current window and should be disconnected.
    pub fn allow(&mut self) -> bool {
        let now = Instant::now();
        if now.duration_since(self.window_start) >= self.window {
            self.window_start = now;
            self.count = 0;
        }
        self.count += 1;
        self.count <= self.max_per_window
    }
}

/// Bound on `IpRateLimiterState::entries`' cardinality (MAN-68, PR #85
/// review round 6). `spawn_stale_entry_reaper` bounds how long an IDLE
/// entry survives (two full windows), not how many entries can
/// accumulate before that reaper catches up -- a source with access to
/// many distinct addresses (e.g. a routed IPv6 prefix, trivially
/// available to any real ISP-assigned block) can insert a fresh entry per
/// address at its own handshake rate, unbounded for up to those two
/// windows' worth of accumulation. Same class of gap MAN-62 already fixed
/// for `bus::SpotBus::occurrence_counts`, and the same capacity value --
/// see `bus::MAX_OCCURRENCE_ENTRIES`'s doc comment for why 20,000 is
/// generous headroom over real cardinality while still bounding a flood.
const MAX_IP_RATE_LIMITER_ENTRIES: usize = 20_000;

/// One tracked source's rate-limit window state plus an LRU touch tick,
/// mirroring `bus::OccurrenceTracker`'s entry shape.
struct IpRateLimiterEntry {
    window_start: Instant,
    count: u32,
    last_touched: u64,
}

/// Bounded, LRU-on-touch state for `IpRateLimiter` (MAN-68) -- same design
/// as `bus::OccurrenceTracker`: once at `MAX_IP_RATE_LIMITER_ENTRIES`
/// capacity, inserting a genuinely NEW source IP evicts whichever tracked
/// IP has gone longest untouched, so a real, currently-active source
/// (repeatedly touched) stays protected while addresses that appear once
/// under flood pressure and never again are what gets evicted first.
struct IpRateLimiterState {
    entries: HashMap<IpAddr, IpRateLimiterEntry>,
    next_touch: u64,
}

impl IpRateLimiterState {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            next_touch: 0,
        }
    }
}

/// MAN-57: `RateLimiter` above is instantiated fresh per connection, so a
/// source opening N connections gets N independent full budgets -- the
/// effective per-IP rate is N x the intended single-connection budget, not
/// the budget itself. `IpRateLimiter` is a SHARED, IP-keyed sibling
/// checked in addition to (never instead of) each connection's own
/// `RateLimiter`: an event must be within budget on both the connection's
/// own window AND the aggregate window for that source IP across every
/// connection it currently holds. Same `std::sync::Mutex` reasoning as
/// `tasks::IpQuota` -- the critical section is a plain hashmap read/write
/// with no `.await` inside it.
#[derive(Clone)]
pub struct IpRateLimiter {
    max_per_window: u32,
    window: Duration,
    state: Arc<StdMutex<IpRateLimiterState>>,
}

impl IpRateLimiter {
    pub fn new(max_per_window: u32, window: Duration) -> Self {
        Self {
            max_per_window,
            window,
            state: Arc::new(StdMutex::new(IpRateLimiterState::new())),
        }
    }

    /// Like `new`, but `override_val` (from `ServerConfig`'s per-listener
    /// rate-override fields) takes precedence over `default` when
    /// present -- same override shape as `tasks::IpQuota::new_with_override`
    /// and for the identical reason (PR #81 review, round 1): the
    /// documented reverse-proxy TLS-termination deployment
    /// (`docs/RUNBOOKS/network-exposure.md`) has every downstream client
    /// sharing the proxy's own IP as far as `peer.ip()` is concerned, so
    /// the built-in per-IP default would otherwise aggregate every
    /// legitimate client behind the proxy into ONE shared command/Ping
    /// budget -- an even sharper false positive here than for
    /// `IpQuota`'s connection cap, since this budget's window is much
    /// tighter. `Some(0)` means no per-IP aggregate cap at all (only each
    /// connection's own `RateLimiter` still applies); `Some(n)` for `n >
    /// 0` uses that budget directly; `None` falls back to `default`.
    pub fn new_with_override(default: u32, window: Duration, override_val: Option<u32>) -> Self {
        let max_per_window = match override_val {
            None => default,
            Some(0) => u32::MAX,
            Some(n) => n,
        };
        Self::new(max_per_window, window)
    }

    /// Records one event for `ip` and returns `true` if the AGGREGATE
    /// count across every connection this source currently holds is still
    /// within budget for the current window, `false` otherwise. Same
    /// reset-on-elapsed-window semantics as `RateLimiter::allow`.
    pub fn allow(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap();
        state.next_touch += 1;
        let touch = state.next_touch;
        if let Some(entry) = state.entries.get_mut(&ip) {
            if now.duration_since(entry.window_start) >= self.window {
                entry.window_start = now;
                entry.count = 0;
            }
            entry.count += 1;
            entry.last_touched = touch;
            return entry.count <= self.max_per_window;
        }
        // A genuinely new source IP (MAN-68): evict the longest-untouched
        // tracked entry first if already at capacity, mirroring
        // `bus::OccurrenceTracker::touch`.
        if state.entries.len() >= MAX_IP_RATE_LIMITER_ENTRIES {
            if let Some(lru_key) = state
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_touched)
                .map(|(key, _)| *key)
            {
                state.entries.remove(&lru_key);
            }
        }
        state.entries.insert(
            ip,
            IpRateLimiterEntry {
                window_start: now,
                count: 1,
                last_touched: touch,
            },
        );
        1 <= self.max_per_window
    }
}

/// How often the background reaper evicts stale per-IP entries. Unlike
/// `IpQuota` (whose entries self-remove when a connection's guard drops to
/// zero holders), `IpRateLimiter`'s entries have no such release event --
/// a source that sent traffic once and never again would otherwise leave
/// its entry in the map for the life of the process, growing it without
/// bound across ordinary connection churn from many distinct real client
/// IPs over a long uptime (same class of gap as MAN-62's
/// `SpotBus::occurrence_counts`).
const IP_RATE_LIMITER_REAP_INTERVAL: Duration = Duration::from_secs(60);

/// Spawns a background task that periodically evicts entries whose window
/// has been idle for more than one full `window` past its own reset --
/// i.e. genuinely stale, not just mid-window. Letting it run unawaited for
/// the process lifetime is fine; it holds only a clone of the shared
/// state and does no other work.
pub fn spawn_stale_entry_reaper(limiter: IpRateLimiter) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(IP_RATE_LIMITER_REAP_INTERVAL).await;
            let now = Instant::now();
            let mut state = limiter.state.lock().unwrap();
            state
                .entries
                .retain(|_, entry| now.duration_since(entry.window_start) < limiter.window * 2);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn allows_events_up_to_the_budget_within_one_window() {
        let mut limiter = RateLimiter::new(3, Duration::from_secs(10));
        assert!(limiter.allow());
        assert!(limiter.allow());
        assert!(limiter.allow());
    }

    #[tokio::test(start_paused = true)]
    async fn rejects_events_past_the_budget_within_one_window() {
        let mut limiter = RateLimiter::new(3, Duration::from_secs(10));
        assert!(limiter.allow());
        assert!(limiter.allow());
        assert!(limiter.allow());
        assert!(
            !limiter.allow(),
            "a 4th event within the same window must be rejected"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn resets_the_budget_once_the_window_elapses() {
        // The specific behavior round-14's finding required: a
        // well-behaved long-running client sending events slower than
        // the budget must never be disconnected, no matter how long the
        // connection has been open.
        let mut limiter = RateLimiter::new(1, Duration::from_secs(60));
        assert!(limiter.allow());
        assert!(
            !limiter.allow(),
            "2nd event within the same window must be rejected"
        );

        tokio::time::advance(Duration::from_secs(61)).await;

        assert!(
            limiter.allow(),
            "an event after the window elapsed must be allowed again"
        );
    }

    fn addr(n: u8) -> IpAddr {
        IpAddr::from([127, 0, 0, n])
    }

    #[tokio::test(start_paused = true)]
    async fn ip_rate_limiter_caps_the_aggregate_across_many_connections_from_one_source() {
        // MAN-57's core scenario: N connections from the same IP must NOT
        // each get a full independent budget.
        let limiter = IpRateLimiter::new(3, Duration::from_secs(10));
        let ip = addr(1);
        assert!(limiter.allow(ip));
        assert!(limiter.allow(ip)); // as if from a 2nd connection
        assert!(limiter.allow(ip)); // as if from a 3rd connection
        assert!(
            !limiter.allow(ip),
            "a 4th event from the SAME source IP (regardless of which connection) must be rejected"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn ip_rate_limiter_override_disables_the_cap_at_zero_and_uses_default_when_unset() {
        let ip = addr(1);

        let defaulted = IpRateLimiter::new_with_override(2, Duration::from_secs(10), None);
        assert!(defaulted.allow(ip));
        assert!(defaulted.allow(ip));
        assert!(
            !defaulted.allow(ip),
            "None must fall back to the default cap"
        );

        let overridden = IpRateLimiter::new_with_override(2, Duration::from_secs(10), Some(5));
        for _ in 0..5 {
            assert!(overridden.allow(ip));
        }
        assert!(!overridden.allow(ip), "Some(n) must use n, not the default");

        let disabled = IpRateLimiter::new_with_override(2, Duration::from_secs(10), Some(0));
        for _ in 0..1000 {
            assert!(disabled.allow(ip), "Some(0) must mean no per-IP cap at all");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn ip_rate_limiter_tracks_sources_independently() {
        let limiter = IpRateLimiter::new(1, Duration::from_secs(10));
        assert!(limiter.allow(addr(1)));
        assert!(
            limiter.allow(addr(2)),
            "a different source IP must have its own independent budget"
        );
        assert!(
            !limiter.allow(addr(1)),
            "the first source is still over its own budget"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn ip_rate_limiter_resets_the_budget_once_the_window_elapses() {
        let limiter = IpRateLimiter::new(1, Duration::from_secs(60));
        let ip = addr(1);
        assert!(limiter.allow(ip));
        assert!(!limiter.allow(ip));

        tokio::time::advance(Duration::from_secs(61)).await;

        assert!(
            limiter.allow(ip),
            "an event after the window elapsed must be allowed again"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stale_entry_reaper_evicts_sources_idle_past_two_windows() {
        // window (100s) deliberately larger than IP_RATE_LIMITER_REAP_INTERVAL
        // (60s) so the first reap pass lands well inside 2 windows (200s) and
        // a later pass lands past it -- exercising "survives, then evicted"
        // across two real reaper ticks rather than one.
        let limiter = IpRateLimiter::new(1, Duration::from_secs(100));
        let ip = addr(1);
        assert!(limiter.allow(ip));
        assert_eq!(limiter.state.lock().unwrap().entries.len(), 1);

        let _reaper = spawn_stale_entry_reaper(limiter.clone());
        tokio::task::yield_now().await;
        tokio::time::advance(IP_RATE_LIMITER_REAP_INTERVAL + Duration::from_secs(1)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            limiter.state.lock().unwrap().entries.len(),
            1,
            "an entry less than 2 windows (200s) idle must survive a reap pass"
        );

        // Land just past the reap tick at t=240s (elapsed=240s > 2*window's
        // 200s), the first tick after which eviction is actually due.
        tokio::time::advance(IP_RATE_LIMITER_REAP_INTERVAL * 3 + Duration::from_secs(1)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            limiter.state.lock().unwrap().entries.len(),
            0,
            "an entry idle past 2 full windows must be reaped"
        );
    }

    /// MAN-68, PR #85 review round 6: a source with access to many
    /// distinct addresses (a routed prefix) must not grow
    /// `IpRateLimiter`'s tracked-entry count without bound between reaper
    /// passes -- one more distinct source IP past capacity must evict the
    /// LONGEST-UNTOUCHED entry, not simply be refused tracking or grow the
    /// map past the cap.
    #[tokio::test(start_paused = true)]
    async fn ip_rate_limiter_caps_entry_cardinality_via_lru_eviction() {
        let limiter = IpRateLimiter::new(1, Duration::from_secs(3600));
        let ip_for = |i: usize| IpAddr::from((i as u32).to_be_bytes());

        for i in 0..MAX_IP_RATE_LIMITER_ENTRIES {
            assert!(limiter.allow(ip_for(i)));
        }
        assert_eq!(
            limiter.state.lock().unwrap().entries.len(),
            MAX_IP_RATE_LIMITER_ENTRIES
        );

        // LRU-on-touch (not FIFO-by-insertion) is what makes it hold:
        // re-touch every existing entry EXCEPT the very first one, so it
        // alone is now the longest-untouched.
        for i in 1..MAX_IP_RATE_LIMITER_ENTRIES {
            limiter.allow(ip_for(i));
        }

        // One more distinct source IP past capacity must evict the
        // longest-untouched entry (index 0), not grow the map past the
        // cap or refuse to track the new source.
        let overflow_ip = ip_for(MAX_IP_RATE_LIMITER_ENTRIES);
        limiter.allow(overflow_ip);

        let state = limiter.state.lock().unwrap();
        assert_eq!(state.entries.len(), MAX_IP_RATE_LIMITER_ENTRIES);
        assert!(
            !state.entries.contains_key(&ip_for(0)),
            "the longest-untouched entry must be the one evicted"
        );
        assert!(
            state.entries.contains_key(&ip_for(1)),
            "a recently re-touched entry must survive"
        );
        assert!(
            state.entries.contains_key(&overflow_ip),
            "the newly-inserted entry must be present"
        );
    }
}
