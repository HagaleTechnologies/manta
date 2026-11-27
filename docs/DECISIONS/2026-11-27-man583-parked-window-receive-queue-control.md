# MAN-583: a committed, OS-measured failing control for the parked-window protocol-2 test

Date: 2026-11-27
Ticket: MAN-583 (test infrastructure)
Status: decided and built; the named test passes on Linux with both
schedules under one measured queue. The macOS and Windows results come
from the pull request's own `test (macos-latest)` and
`test (windows-latest)` legs and are pending until it runs them.
Extends: [MAN-551](2026-11-19-man551-windows-o1-readout.md) row 15, whose
hand-run 96 KiB control this record replaces with a committed one.

## Context

MAN-551 fixed the Windows failure of
`hpsdr::proto2::tests::a_second_pause_while_a_park_drains_keeps_its_anchor`
(run 40923755629, the 10 s no-IQ stall after loopback lost the tail of an
un-drained 100-datagram burst) by splitting the burst into two
50-datagram batches. Its failing control -- the un-split burst starving
under a 96 KiB receive-queue pin -- was run by hand and left out of the
branch, because a fixed byte pin is not portable: macOS accounts the
~1.4 KiB datagram at roughly an mbuf cluster, so 50 datagrams may not fit
96 KiB there. Nothing committed kept the split load-bearing; the test
would pass with the burst un-split again and only a Windows run would
notice.

## Decisions

### D1. The queue comes from a measurement, not a byte pin

The test calibrates on the running OS with the fixture's own datagram:
fresh IPv4 loopback UDP sockets, the receiver's `SO_RCVBUF` set through
`socket2::SockRef` after bind and read back, `wire::IQ_DATAGRAM_LEN`
datagrams sent with no reader draining, a 20 ms loopback settle, then a
bounded drain that ends on the read timeout (`WouldBlock` on Unix,
`TimedOut` on Windows) and checks sender, length and
in-order sequence on every receive. The search starts at 64 datagrams'
worth, steers up on zero deliveries and down on a hundred, and estimates
the request for 75 queued datagrams as `request * 75 / delivered` (the
observed requested/reported ratio cancels, so a capped or doubled grant
needs no special case). At most 12 distinct requests are probed, search
probes and confirmations together; a request is accepted only after two
separate 50- and 100-datagram probe rounds at one effective size
deliver the whole batch and part of the burst, and rounds that disagree
fail calibration with the attempted measurements rather than silently
re-trying.

### D2. Both schedules run under the one measured queue in one test

Each schedule opens a fresh `FakeP2Radio` and source, pins the source
socket to the selected request before sequence 0 is sent, and asserts
the socket's readback equals the probe's. The positive case is the
committed split schedule with exact per-read sequence and 238-sample
assertions and the original anchor checks (11,000 dropped packets, one
gap, a 2,618,000-sample discontinuity, no missing samples). The failing
control replays the Windows incident's shape -- one un-drained
100-datagram burst, the same settle as the probe, the 200 ms pause kept
after the 50th successful read, the jump withheld -- and must return the
typed hard `NoUsableSamples` error with `stalled: no IQ datagram for 10 s`
after the one soft `take_stalled` report, with a delivered count of 50 to
99 datagrams so the failure sits in the last half of the backlog. Both
drains read through a local fallible helper that handles at most one
`Ok(0)`, requires the pending report, and lets the hard error through:
`read_one`'s unwrap panicked on that error, which is what the Windows run
showed. Production deadlines (4.5 s soft, 10 s hard) are untouched.

### D3. The helpers stay inside the one test function

The calibration, verify, pin and read helpers are nested in the test.
`scripts/socket-test-repeat.py` treats changed code outside a test
function as a test-only shared change and would select every test of the
module for repetition; local helpers keep the change scoped to this test,
the failure-registry entry
(`scripts/ci-flake-classes.json` line for the test name) stays valid, and
libtest discovery is unchanged.

### D4. The default buffer proves nothing, and the control says so

On this Linux host the source's own 4 MiB request caps at 425,984
effective bytes and a 100-datagram burst delivers in full (measured
2026-11-27, the plan-run's number reproduced), so an un-split schedule
under the default would not starve on Linux; on Windows the default
already lost the tail (the incident). A committed assertion about the
default would be wrong on one of the two, so the default is recorded
here, not asserted in the test.

## Verification (Linux x86_64, Rust 1.98.0, this container)

- The named test passes three consecutive times in isolation
  (`cargo test -p manta-input --locked --lib
  hpsdr::proto2::tests::a_second_pause_while_a_park_drains_keeps_its_anchor
  -- --exact --nocapture`), 19.3 s each, printing the same control every
  time: request 85,570 bytes, effective 171,140 bytes, batch probes
  delivering 50 and 50 of 50, burst probes 75 and 75 of 100. The search
  started at 92,416 bytes (64 datagrams), measured 81 of 100 delivered,
  and its estimate `92,416 * 75 / 81` is the accepted 85,570.
- Mutation (run by hand, not committed): moving the second batch's send
  ahead of the first drain -- the un-split schedule with both pauses kept
  -- fails at the second batch's drain with
  `HPSDR protocol 2 radio 127.0.0.1 stalled: no IQ datagram for 10 s`,
  the incident's error, 14.9 s in. Restoring the split and rerunning the
  same command passes.
- The un-split branch's assertions fired as designed before the mutation
  was restored: the drain delivered 75 of 100 in a contiguous prefix, one
  soft report, then the typed hard error.
- The full `manta-input` lib suite, `manta-cli`'s
  `decision_records_have_indexable_names_and_headings` and
  `scripts/decisions-index.py` (lag-tolerant default mode) pass with the
  record in place; `docs/DECISIONS/README.md` is left to the
  MAN-440 workflow, per its design.

macOS and Windows: not run here (no such host in this container). The
pull request's required native test legs run the workspace suite that
contains this test; their results close the three-platform proof before
merge. If calibration cannot establish the window on one of them, the
test fails with the OS, datagram length and every attempted request,
readback and delivery count, per D1.
