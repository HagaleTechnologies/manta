//! `/healthz` and `manta_healthy`/`manta_listener_up` evaluation (MAN-128).
//! Kept out of `metrics.rs` to avoid a third claimant on the `status.rs`
//! name (PRs #95/#136 both already claim it) and to keep `metrics.rs` from
//! growing a second concern -- the state this evaluates lives on `Metrics`,
//! but the evaluation logic itself is self-contained and independently
//! testable with synthetic `Instant`s.

use std::time::{Duration, Instant};

/// How long the decode watchdog tolerates no observed progress before
/// `/healthz` reports unhealthy (MAN-128 D9). Comfortably exceeds the 2s
/// startup calibration block plus realistic real-time-rate startup, while
/// staying well under a typical orchestrator's `failureThreshold *
/// periodSeconds`.
pub const DECODE_STALL_THRESHOLD: Duration = Duration::from_secs(10);

/// One named health check and its outcome, e.g. `source file: healthy` or
/// `listener json: down`. `detail` is the fully rendered line body (without
/// a trailing newline) -- `HealthReport::render_text` joins these directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthCheck {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthReport {
    pub healthy: bool,
    pub checks: Vec<HealthCheck>,
}

impl HealthReport {
    /// First line `ok`/`unhealthy`, then one line per check -- an operator
    /// running `curl` sees not just the verdict but *why*.
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        out.push_str(if self.healthy { "ok\n" } else { "unhealthy\n" });
        for check in &self.checks {
            out.push_str(&check.detail);
            out.push('\n');
        }
        out
    }
}

/// The decode loop's own liveness state, as last observed by the daemon
/// wiring layer. `NotMonitored` is the library-use default (`Metrics::new()`
/// with no `arm_decode_watchdog` call) -- the daemon always arms it, so this
/// state is only ever reachable in tests or non-daemon library use.
#[derive(Debug, Clone, Default)]
pub(crate) enum DecodeWatch {
    #[default]
    NotMonitored,
    Running {
        last_progress: Instant,
        last_count: u64,
    },
    Stopped,
}

impl DecodeWatch {
    pub(crate) fn check(&self, now: Instant) -> HealthCheck {
        match self {
            DecodeWatch::NotMonitored => HealthCheck {
                name: "decode".to_string(),
                ok: true,
                detail: "decode: not monitored".to_string(),
            },
            DecodeWatch::Stopped => HealthCheck {
                name: "decode".to_string(),
                ok: false,
                detail: "decode: stopped".to_string(),
            },
            DecodeWatch::Running { last_progress, .. } => {
                let stalled_for = now.saturating_duration_since(*last_progress);
                if stalled_for > DECODE_STALL_THRESHOLD {
                    HealthCheck {
                        name: "decode".to_string(),
                        ok: false,
                        detail: format!("decode: stalled {}s", stalled_for.as_secs()),
                    }
                } else {
                    HealthCheck {
                        name: "decode".to_string(),
                        ok: true,
                        detail: "decode: ok".to_string(),
                    }
                }
            }
        }
    }

    /// Updates `last_progress`/`last_count` when `total_count` has grown
    /// since the last observation -- a repeated snapshot with the same
    /// count (e.g. two scrapes between chunks) must not refresh the
    /// progress clock, or a genuinely stalled decode loop would never be
    /// detected. No-op outside `Running` (the daemon always arms before
    /// decoding starts; `NotMonitored`/`Stopped` are terminal/unset states
    /// a progress observation shouldn't resurrect).
    pub(crate) fn observe_progress(&mut self, now: Instant, total_count: u64) {
        if let DecodeWatch::Running {
            last_progress,
            last_count,
        } = self
        {
            if total_count > *last_count {
                *last_progress = now;
                *last_count = total_count;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_monitored_is_healthy_with_a_not_monitored_detail() {
        let check = DecodeWatch::NotMonitored.check(Instant::now());
        assert!(check.ok);
        assert_eq!(check.detail, "decode: not monitored");
    }

    #[test]
    fn stopped_is_unhealthy() {
        let check = DecodeWatch::Stopped.check(Instant::now());
        assert!(!check.ok);
        assert_eq!(check.detail, "decode: stopped");
    }

    #[test]
    fn running_within_threshold_is_healthy() {
        let t0 = Instant::now();
        let watch = DecodeWatch::Running {
            last_progress: t0,
            last_count: 5,
        };
        let check = watch.check(t0 + Duration::from_secs(1));
        assert!(check.ok);
        assert_eq!(check.detail, "decode: ok");
    }

    #[test]
    fn running_past_threshold_is_unhealthy_with_stalled_detail() {
        let t0 = Instant::now();
        let watch = DecodeWatch::Running {
            last_progress: t0,
            last_count: 5,
        };
        let check = watch.check(t0 + DECODE_STALL_THRESHOLD + Duration::from_millis(1));
        assert!(!check.ok);
        assert_eq!(check.detail, "decode: stalled 10s");
    }

    #[test]
    fn progress_requires_count_to_increase_not_just_a_snapshot() {
        let t0 = Instant::now();
        let mut watch = DecodeWatch::Running {
            last_progress: t0,
            last_count: 5,
        };
        // Same count again, later: must not refresh last_progress.
        watch.observe_progress(t0 + Duration::from_secs(5), 5);
        let check = watch.check(t0 + DECODE_STALL_THRESHOLD + Duration::from_millis(1));
        assert!(!check.ok, "an unchanged count must not count as progress");
    }

    #[test]
    fn render_text_lists_the_verdict_then_every_check() {
        let report = HealthReport {
            healthy: false,
            checks: vec![
                HealthCheck {
                    name: "source:file".to_string(),
                    ok: true,
                    detail: "source file: healthy".to_string(),
                },
                HealthCheck {
                    name: "listener:json".to_string(),
                    ok: false,
                    detail: "listener json: down".to_string(),
                },
            ],
        };
        let text = report.render_text();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "unhealthy");
        assert_eq!(lines[1], "source file: healthy");
        assert_eq!(lines[2], "listener json: down");
    }
}
