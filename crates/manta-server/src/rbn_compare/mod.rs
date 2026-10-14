//! MAN-301: live comparison of manta's spots with what the Reverse Beacon
//! Network heard. With `[rbn_compare]` enabled, the daemon reads RBN's
//! public CW telnet feed ([`feed`], parsed by [`ak1a`]), holds each
//! published spot until its comparison window closes, then grades it
//! ([`grade`]) and publishes a `spotGrade` record on the JSON stream and the
//! `manta_rbn_compare_*` metrics ([`live`]). `manta grade` replays a node's
//! spot archive through the same [`grade`] engine offline.
//!
//! Read-only toward RBN: the feed client writes exactly one line, its login
//! callsign, and nothing else. See
//! `docs/DECISIONS/2026-10-11-man301-live-rbn-comparison.md`.

use std::sync::Arc;

pub mod ak1a;
pub mod feed;
pub mod grade;
pub mod live;

/// Wall clock in Unix seconds. Production uses `SystemTime::now()`; tests
/// inject a controllable one.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// The production clock: `SystemTime::now()` in whole Unix seconds.
pub fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    })
}
