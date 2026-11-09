# MAN-474: protocol-1 stop retransmission window

Date: 2026-11-06. Ticket: MAN-474.

## Reported failure

Windows CI run 40885919491 on PR #457 at a09b5562 reported
`radio never saw streaming = false` in
`dropping_a_lone_protocol_1_receiver_stops_the_radio_for_rediscovery`.
That revision did not touch `hpsdr.rs`, and the failed-job rerun passed.
MAN-497 added two immediate stop datagrams, but the Windows loss point
remains unknown.

## Decision

Keep the immediate pair of protocol-1 stop commands and resend at 50 ms
intervals over a 250 ms window measured from retirement entry. Spacing the
retries covers a burst that loses both immediate packets. Retirement holds
the authority and pump locks throughout the window, and marks the session
retired before sending. Each packet has run = 0. Individual send errors
remain ignored in this best-effort shutdown path. Protocol 2 is unchanged.

The fake radio now allows 10 s to observe the requested streaming state.
This matches `MAX_CONSECUTIVE_TIMEOUTS` times `READ_TIMEOUT`. Its polling
still runs every 5 ms and reports the recorded commands on failure.
This extra observation time covers a descheduled receiver thread, while the
spaced sends cover packet loss. Busy discovery, stopped state, available
rediscovery and successful reopen remain end-to-end assertions.

The stale-handle test drains all retries through 300 ms of silence before
opening a replacement, checking that each drained packet is a 60-byte
stop. Concurrent retirement stays idempotent, and the existing assertion
still rejects stale stop or keepalive traffic after replacement.

## Measurements and verification

All local runs used Rust 1.98.0 in the Linux implement container.
The supplied plan recorded a baseline of 15/15 focused runs at
1.81-1.83 s and a passing stale-handle test. This session also ran the
unchanged focused test successfully at 1.81 s.

A temporary `Instant` probe around the first concurrent retirement in the
stale-handle test measured 250.099815 ms. The probe was removed afterward.
The 250 ms retry window is not a hard wall-clock guarantee: lock waits,
socket sends and OS scheduling can extend elapsed drop time.

The HPSDR library suite passed with 164 passed and 3 ignored.
At the changed tree, 15/15 consecutive focused runs passed at 2.31-2.33 s.
The extra half second covers retirement on both drop and reopened drop.
`cargo build --all-targets` passed. These plan checks run in the same
container:

- `bash scripts/check-rust-toolchain-pin.sh && cargo fmt --all --check`
- `cargo clippy -p manta-input -p manta-cli --all-targets --features hpsdr --locked -- -D warnings`
- `cargo test -p manta-input -p manta-cli --features hpsdr --locked`

The first HPSDR package run exposed a sibling CLI test that required
exactly two stops. `failed_startup_retires_another_groups_completed_radio`
now accepts the immediate pair plus scheduling-dependent retries. Its
focused rerun passed. The dedicated stale-handle test retains the
idempotence and replacement-traffic checks. This additional test-file edit
is declared in the phase deviations.

The full pre-push gate from `CLAUDE.md` runs after self-review and includes
the HPSDR package suite on the corrected tree. Its result is reported in
the phase outcome.

## Limits

Windows confirmation comes from the PR's required `test (windows-latest)`
check. Linux repetition cannot prove Windows behavior. Protocol 1 has no
stop acknowledgement, and a loss burst longer than the retry window can
still lose all stops. If Windows fails again, inspect the fake's recorded
commands before changing the retry window or observation deadline.
