//! A small sliding-window rate limiter for per-connection client-initiated
//! traffic (telnet commands, WebSocket Ping frames) on publicly bound
//! endpoints. A pure lifetime cap (an earlier WS-Ping fix used one)
//! terminates well-behaved long-running consumers just for staying
//! connected a long time -- one Ping per minute hits a 60-count lifetime
//! cap after an hour regardless of how gently spaced the traffic was
//! (round-14 review finding). A rate window instead only disconnects a
//! client that's actually sending events FASTER than the allowed rate,
//! no matter how long the connection has been open.

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
}
