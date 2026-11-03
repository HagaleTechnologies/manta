# MAN-459: the protocol-2 acceptance fake follows manta across a reconnect, and every live-run failure explains itself

Date: 2026-11-02. Ticket: MAN-459, filed from two failures of
`crates/manta-cli/tests/hpsdr_protocol2_acceptance.rs` on `macos-latest` in
manta#425's merge group (run 40511031593, 2026-10-27).

## Context

`fifty_lost_datagrams_are_counted_and_keep_the_spot_clock_true` failed with
`no spot within 60 s`: manta stalled, reconnected 3 times, and got no IQ.
`protocol2_radio_at_192k_spots_w1aw_like_the_file_replay` failed with its
spot 645,318 samples late and no stderr in the message. The same code
passed PR CI on every OS and 16 local repeat runs, and neither test has
failed in any manta CI run since 2026-10-19.

The fake radio copied manta's address from the first run = 1 command and
streamed one finite pass there. Manta opens a new socket on every
reconnect, so after any reconnect the fake sent nothing manta could read.
Delaying the fake's first datagram by 12 s reproduced the first failure in
the implement container: `no spot within 60 s` after four `stalled ...
reconnecting in 2s/4s/8s/16s` lines.

The CI log's first backoff read `reconnecting in 1s`. Backoff starts at
1 s only after a productive connection (`crates/manta-cli/src/reconnect.rs`),
so in CI manta's first connection did receive IQ before it stalled. The
fake then never reached the reconnected socket.

Two readings the research weighed do not hold:

- Foreign IQ from another test's fake cannot explain either failure.
  `fill_queue` (`crates/manta-input/src/hpsdr/proto2/mod.rs`) accepts IQ
  only from the radio's `base+11` port and counts anything else as
  malformed.
- `manta check` sends run = 0 before it exits: the source's `Drop` sends
  it twice. Stopping the `check_*` fakes on run = 0 is hygiene, not the fix.

## Decision

- **Follow the latest run = 1 sender.** The fake keeps a `Peer` with the
  current run = 1 sender and a connection count that goes up each time a
  new sender, or a sender back after its own run = 0, starts streaming.
  Run = 0 clears `current` only from the same sender, so a late run = 0
  from a dead socket cannot stop the new stream. Each new connection gets
  a fresh pass from block 0, as a radio restarts its stream on run = 1,
  even if manta's new socket reuses the dead one's port. A finished or cut
  pass is not resent on the same connection.
- **Test the reconnect over the wire.**
  `the_fake_radio_follows_manta_across_a_reconnect` cuts the first
  connection after 807 datagrams (1 s of content). Manta stalls after 10 s,
  reconnects on a new port, and must spot W1AW once. The cut does not race
  manta's stall timer and gives a productive first connection, so the
  backoff reads `reconnecting in 1s` as in the CI run. The spot's
  `sampleTs` must equal the file replay's plus 807 x 238 plus the reported
  outage x 192,000, within 0.05 s (the note's `{:.1}` rounding) plus the
  existing 512-sample tolerance. A fake that keeps sending to the first
  address fails it with `no spot within 60 s` and `reconnected 0 time(s)`.
- **Every live-run assertion explains itself.** `explain` prints how many
  times manta lost and regained the source, its stderr, and a `/metrics`
  body. `assert_same_spot` checks the spot count itself and appends it to
  each message, the `sampleTs` one included. The 192k, 50-lost and
  multi-input tests use it on every assertion, and `live_run`'s ready,
  `no spot within 60 s` and end-of-stream waits scrape `/metrics` once
  before printing it.
- **No retry and no wider tolerance** on the existing tests.

Only test code changed. No production file changed.

## Not done

Follow-up candidates (research D8, `CLAUDE.md` convergence policy):

- The 2x pacing and finite stream that make the suite sensitive to a
  starved runner. With 2 cores and 6 busy loops the fake streamed a second
  full pass after each reconnect and manta still decoded nothing.
- The cause of the +645,318-sample spot. The new message will show the
  reconnect count and stderr the next time it happens.
- The separate protocol-1 fake in `crates/manta-cli/tests/multi_input_hpsdr.rs`,
  which this change did not examine for the same reconnect question.

## Residual risk

A starved runner still fails the live tests. The failure now says how
many times manta lost and regained the source and shows its stderr and
metrics. The macOS failures were not reproduced on macOS, so this record
does not claim the macOS flake is fixed. It claims that a reconnect is now
followed and that every live-run failure prints manta's stderr.
