//! Reconnect backoff policy shared by every reconnect loop in manta: the
//! outbound RBN uplink (`uplink.rs`, MAN-32) and live input sources
//! (`manta-cli`'s `ReconnectingSource`, MAN-73). One policy, one place --
//! extracted from `uplink.rs`, which was the only implementation until now.

use std::time::Duration;

pub const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Whether a connection attempt got far enough to matter for backoff:
/// a connection that completed login (the uplink) or returned real data
/// (a source) before dropping was healthy, so the *next* attempt should
/// retry quickly rather than inherit backoff state from an unrelated
/// earlier outage. A connection that never got that far (target down,
/// refusing, wrong port) hasn't demonstrated that, so backoff keeps
/// growing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptOutcome {
    NeverConnected,
    Disconnected,
}

/// Pure backoff-transition function, split out so this policy is
/// unit-testable without any real sleeping/timing.
pub fn next_backoff(current: Duration, outcome: &AttemptOutcome) -> Duration {
    match outcome {
        AttemptOutcome::Disconnected => INITIAL_BACKOFF,
        AttemptOutcome::NeverConnected => (current * 2).min(MAX_BACKOFF),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_resets_after_a_connection_that_reached_login() {
        let grown = Duration::from_secs(16);
        assert_eq!(
            next_backoff(grown, &AttemptOutcome::Disconnected),
            INITIAL_BACKOFF
        );
    }

    #[test]
    fn backoff_doubles_and_caps_when_never_connected() {
        assert_eq!(
            next_backoff(Duration::from_secs(1), &AttemptOutcome::NeverConnected),
            Duration::from_secs(2)
        );
        assert_eq!(
            next_backoff(Duration::from_secs(45), &AttemptOutcome::NeverConnected),
            MAX_BACKOFF
        );
    }

    #[test]
    fn never_connected_ladder_from_initial_is_2_4_8_16_32_60_60() {
        let mut b = INITIAL_BACKOFF;
        let mut seen = vec![];
        for _ in 0..7 {
            b = next_backoff(b, &AttemptOutcome::NeverConnected);
            seen.push(b.as_secs());
        }
        assert_eq!(seen, [2, 4, 8, 16, 32, 60, 60]);
    }

    #[test]
    fn policy_constants_are_uplinks_1s_and_60s() {
        assert_eq!(INITIAL_BACKOFF, Duration::from_secs(1));
        assert_eq!(MAX_BACKOFF, Duration::from_secs(60));
    }
}
