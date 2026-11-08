use super::*;

#[test]
fn parse_replay_epoch_accepts_a_unix_seconds_value() {
    assert_eq!(parse_replay_epoch("1751635200").unwrap(), 1_751_635_200);
}

#[test]
fn parse_replay_epoch_rejects_negative_and_non_numeric_values() {
    assert!(parse_replay_epoch("-1").is_err());
    assert!(parse_replay_epoch("not-a-number").is_err());
    assert!(parse_replay_epoch("2026-07-04T12:00:00Z").is_err());
}

#[test]
fn parse_replay_epoch_rejects_unrealistic_far_future_values() {
    // Regression (round-9 review): an unbounded upper end let
    // i64::MAX (or anything close to it) through, which later
    // overflows SystemTime arithmetic in SpotBus::unix_ts_for
    // (`epoch + elapsed`) and panics on the very first spot delivered
    // to any client -- reject it here, at CLI-parse time, with a
    // clear error instead.
    assert!(parse_replay_epoch(&i64::MAX.to_string()).is_err());
    // A plausible near-future value must still be accepted.
    assert!(parse_replay_epoch("2000000000").is_ok());
}
