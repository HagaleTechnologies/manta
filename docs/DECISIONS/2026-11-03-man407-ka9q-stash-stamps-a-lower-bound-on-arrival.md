# 2026-11-03 — MAN-407: the ka9q startup stash stamps each packet no later than its arrival

**Status:** Implemented.

**Ticket:** MAN-407. Deferred from the Codex review of PR #342 (MAN-259),
thread `PRRT_kwDOTQvU8M6t2M9H` on `crates/manta-input/src/ka9q/mod.rs`,
under the round-2+ P2
[convergence policy](2026-08-07-pr-review-convergence-policy.md).
Refines decision 4 of
[MAN-259](2026-10-16-man259-acquisition-anchored-outage.md).

## Problem

After a reconnect, `Ka9qIqSource::open` reports where its replayed stream
began through `IqSource::acquisition_start_offset()`, and
`ReconnectingSource` ends the outage there (MAN-259). For ka9q-radio that
instant is the stamp of the first stashed packet the replay would accept.
The startup stash took that stamp with `Instant::now()` after `recv`
returned, which is when userspace dequeued the packet, not when it reached
the socket. Any delay between the two moved the anchor late by the same
amount. The engine counted that delay twice, once as outage and once as
replayed data, and every later spot was stamped late by it.

The delay has four windows, all between a packet's arrival and the stamp:

1. Join to first drain. After the data join, `open` creates and joins the
   status socket, sets the data socket non-blocking and enters
   `wait_for_status` before the first drain. The ticket's Gherkin puts the
   opener's descheduling here.
2. Between drain passes. Each pass is followed by up to `WAIT_POLL` (1 ms)
   on the status socket, longer if the thread stalls.
3. After `recv` returns and before `Instant::now()` runs.
4. The batch limit. A pass stops after `DRAIN_BATCH` datagrams and leaves
   the rest for the next one.

Measured at `efa79c4` with a fake radiod streaming one 50-sample `f32le`
packet per millisecond from before `open`, an SSRC set, and a stall
injected after the status-group join:

| injected stall | `acquisition_start_offset` |
|---|---|
| 0 ms | 0.10 ms |
| 400 ms | 400.2 ms |
| 1200 ms | 1.2002 s |

The data join came about 20 µs into `open` and the stream was already
running, so the true first arrival is within about 1 ms of the join in
every row. The offset tracked the stall one-for-one.
`pre_acquisition_handshake_time_still_counts_as_outage` pins gap =
backoff + offset, so a 400 ms stall moved every later spot about 400 ms
late. PR #342 called this "preemption-scale (sub-ms)". That holds only
while the opener never stalls.

## Decision

The stash stamps each packet with a lower bound on its arrival instead of
the dequeue instant. A stamp no later than the arrival cannot count the
dequeue delay as data.

1. **The floor, not `SO_TIMESTAMP`.** `stash_data_up_to` takes
   `Instant::now()` before every `recv`. When that `recv` returns
   `WouldBlock` or `TimedOut`, the floor moves to that instant: every
   packet read afterwards arrived after it. Each stashed packet carries
   the current floor. This meets the invariant by construction on every
   platform with `std` alone, adds no `unsafe` and no `libc` edge for
   `manta-input`, and runs in tests on every CI leg. A kernel receive
   stamp is wall-clock time, so mapping it onto `Instant` would need this
   same floor as a clamp. Windows would need the floor as its fallback
   anyway, and macOS was not measured. The research measured the kernel
   stamp at under 10 µs of error on Linux, but it adds code without
   changing the property this ticket asks for.
2. **The first floor is taken before the data socket exists, not at the
   join.** `open` takes it after `resolve_group` (which can wait seconds on
   mDNS) and immediately before the data `join_socket`. A socket cannot
   hold a datagram from before it existed. The join is not a bound on
   Linux. Measured on Linux 6.8 with a socket bound to a group and port
   with `SO_REUSEADDR`/`SO_REUSEPORT`, as `join_socket` binds, and never
   joined: with no member on the host it received 0 of the group's
   datagrams, and with another socket on the host already joined it
   received them. That covers two `[[input]]`s on one group (MAN-13) and
   any other local radiod client. The cost is that packets read before the
   first empty read are stamped early by socket setup time (20 to 40 µs
   measured). The join *fallback*, used when the stash retained nothing
   replayable, stays at the join as MAN-259 decision 5 set it. The
   fallback estimates a stream start nobody observed, while the floor
   bounds a packet that was observed, so the two rules do not conflict.
3. **A pass cut off by `DRAIN_BATCH` leaves the floor where it was.** That
   pass never saw the socket empty, so no new bound exists.
4. **The anchor rule moved, unchanged, into `acquisition_anchor`.** It is
   still the stamp of the first stashed packet `packet_yields_samples`
   accepts, or the data join when none is retained. `open` and the
   end-to-end test call the same function, so the test drives the
   production rule after a real stall without a test hook in `open`.

Nothing outside `ka9q/mod.rs` changes behaviour. `ReconnectingSource`,
the `IqSource` trait signature and every wrapper already carry the
offset. KiwiSDR, HPSDR, SoapySDR and audio take their instant before their
start trigger, so a stall there reads early, not late.

## Residual

The floor reads early, never late. Two cases read early by more than a
drain pass:

- A stream that *starts* inside a stall. No empty read happens after the
  stall begins, so its first packet keeps the pre-stall floor and reads
  early by up to the stall. The research measured about 300 ms early for
  an onset 300 ms into a 400 ms stall, where the dequeue stamp read about
  100 ms late.
- A post-stall backlog shares the pre-stall floor until a pass finds the
  socket empty, because passes cut off by `DRAIN_BATCH` never raise it.
  Only the first replayable packet sets the anchor. For a stream already
  running before the stall, that packet arrived near the floor, because a
  full UDP receive buffer keeps its oldest datagrams. But when the stash
  cap evicts inside the backlog (a stash already near its cap before the
  stall, or a kernel buffer raised to hold more than the cap), the first
  retained packet arrived later than the floor it carries. The anchor then
  reads early by up to the evicted span: the stall plus the passes it takes
  to drain the backlog (about 1 ms per 256 datagrams).

Early is the direction MAN-259's residual already accepts. Late is the
direction this ticket forbids. A kernel receive stamp (`SO_TIMESTAMP`)
would close the early cases, and is left for a follow-up only if field
data shows they matter. A NIC or stack timestamp can precede the socket
enqueue by softirq latency. The bound here is on when the datagram became
readable on the socket, which is the delay this ticket is about.

## Verification

`cargo test -p manta-input --locked --lib ka9q`. With the stash stamp
switched back to `Instant::now()` at dequeue, all four new tests failed
(65 passed, 4 failed, 1 ignored):

- `a_stashed_packet_is_stamped_no_later_than_its_arrival`: one packet
  sent, a 300 ms sleep, then one drain. The dequeue stamp landed
  300.13 ms after the send returned.
- `the_stash_floor_advances_when_the_socket_is_seen_empty`: an empty read
  raises the floor, so a packet sent after it is stamped between that
  read and the send.
- `a_pass_cut_off_by_the_batch_limit_keeps_the_floor`: a pass stopped by
  the limit leaves the floor alone, and the whole backlog keeps the
  starting floor.
- `sockets::a_stall_after_the_data_join_does_not_move_the_acquisition_anchor`
  (Linux and macOS, the ticket's Gherkin): real joins of the data and
  status groups, one packet, a 300 ms stall, then `wait_for_status` and
  `acquisition_anchor`. The dequeue stamp put the anchor 300.20 ms after
  the send.

With the floor, all 69 pass (1 ignored, the live-radiod test), including
the three MAN-259 anchor tests unchanged:
`acquisition_start_offset_is_measured_before_the_settle_wait`,
`acquisition_start_offset_lands_at_the_first_packet_retained_past_the_stash_cap`
and `acquisition_start_offset_skips_packets_that_cannot_yield_samples`.
The `reconnect.rs` anchor tests in `manta-cli` are unchanged and still
pass.
