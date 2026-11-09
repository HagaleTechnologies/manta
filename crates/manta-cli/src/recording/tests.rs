use super::*;
use crate::SHUTDOWN_DRAIN_DEADLINE;

/// MAN-453: every recording waits out the rest of one 5 s deadline, then
/// the rest of one 2 s publication grace after it, not a grace each.
#[test]
fn recording_finish_budget_shares_one_grace_after_the_deadline() {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let s = std::time::Duration::from_secs;
    assert_eq!(
        recording_finish_budget(deadline, deadline - s(3)),
        (s(3), RECORDING_PUBLICATION_GRACE)
    );
    assert_eq!(
        recording_finish_budget(deadline, deadline + s(1)),
        (s(0), RECORDING_PUBLICATION_GRACE - s(1))
    );
    assert_eq!(
        recording_finish_budget(deadline, deadline + s(9)),
        (s(0), s(0))
    );
}

/// MAN-453: recordings finish inside the client drain, so the grace adds
/// nothing to the stop budget `TimeoutStopSec=60` covers.
#[test]
fn recording_finalization_ends_inside_the_client_drain() {
    assert!(RECORDING_FINISH_TIMEOUT + RECORDING_PUBLICATION_GRACE <= SHUTDOWN_DRAIN_DEADLINE);
}
