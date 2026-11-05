# MAN-505: protocol-2 acceptance fixtures stream at real time

Date: 2026-11-05.

## Context

The fifty-lost-datagrams acceptance test intermittently failed on
`macos-latest` with its spot clock 8,012,428 samples ahead and one reconnect.
At 2x pacing, the finite 30-second fixture ends after 15 seconds. Under
runner load, decode can miss W1AW in that first pass. After ten seconds
without IQ, manta reconnects and the MAN-459 fake starts a fresh pass.
The production clock correctly includes the first pass and the outage,
so a second-pass spot cannot match the first-pass file replay.

## Decision

Stream at `speed: 1.0` in both `live_run` and the multi-input acceptance
fixture. Both contribute load to the concurrently executed test binary.
The two `check` fixtures already stream at real time.

Keep the deliberate 50-datagram omission at 3 seconds, the loss and gap
metrics, one W1AW spot, frequency comparison and 512-sample clock tolerance.
Keep the 60-second waits, readiness handshake and finite passes. The forced
reconnect still cuts after 807 datagrams and checks the original cut plus
wall-clock outage formula. Production code, retries and concurrency are
unchanged.

## Verification

In this Linux container, the unchanged 2x loss test passed in 19.75 seconds.
The isolated 1x loss test passed in 35.21 seconds, a 15.46-second cost
compared with 2x here. At 1x the whole six-test binary passed with default
concurrency in 46.59 seconds, including the forced reconnect.

Two controlled runs of the whole binary on permitted CPUs 0 and 1, sharing
those CPUs with one busy loop, passed all six tests in 83.95 and 83.88
seconds. Each run had a 180-second bound; a shell trap stopped and reaped
the busy loop. No assertion, timeout or concurrency setting changed.

`cargo build --all-targets` passed. The full pre-push commands from
`CLAUDE.md`, including formatting and HPSDR Clippy, are the final handover
gate; their result is reported in the phase receipt.

## Limits

These are Linux results. The existing required `test (macos-latest)` check
must provide hosted macOS confirmation.

Earlier MAN-505 research found a separate -2,874-sample error with zero
reconnects at 1x under extreme starvation. Its cause remains unexplained;
stale-datagram accounting is only a hypothesis. These two passing controlled
runs do not establish correctness under arbitrary starvation. MAN-497's
separate foreign-datagram port fix and production clock investigation stay
outside this change.
