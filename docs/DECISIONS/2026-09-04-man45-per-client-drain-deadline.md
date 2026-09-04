# MAN-45: per-client shutdown-drain deadline (telnet.rs, json_stream.rs, tasks.rs, main.rs)

PR #63's shutdown-drain region (`manta-cli`'s `SHUTDOWN_DRAIN_DEADLINE`,
`manta-server`'s `tasks::await_all` and the three per-client drain loops in
`telnet.rs`/`json_stream.rs`) has been touched by four separate review
rounds: 10, 15, and twice at 16 (a P1 and this record's own finding). This
document is the fourth-round check-in the fleet's round-based escalation
policy calls for when the same region keeps recurring with a genuinely new
bug shape, rather than another same-region point patch.

## History on this region

- **Round 10**: an earlier version of `shutdown_runtime_after_drain` used a
  blind fixed `sleep` before `Runtime::shutdown_timeout` — real scheduler
  time, but no guarantee spawned client tasks had actually finished
  draining before the sleep elapsed. Fixed by tracking every spawned
  per-client task in a `ClientTasks` registry (`tasks.rs`) and genuinely
  AWAITING their completion via `tasks::await_all`, bounded by one outer
  deadline.
- **Round 15**: that outer deadline (`SHUTDOWN_DRAIN_DEADLINE`) was 2
  seconds — shorter than even a SINGLE write's own `WRITE_TIMEOUT` (10s),
  so a genuinely slow-but-completing client was routinely cut off mid-drain
  for no reason. Fixed by widening it to 25s, enough to cover one
  worst-case telnet spot (two separately-timed 10s writes: the RBN line,
  then `\r\n`).
- **Round 16 (P1, this ticket's own finding)**: the widened 25s deadline
  still only budgets for ONE spot, but a client can have up to the
  broadcast channel's full retention queued (documented as up to 1,024 in
  the finding) and drains it SEQUENTIALLY. Two slow-but-individually-
  compliant spots (15s each, each write within its own 10s `WRITE_TIMEOUT`)
  already exceed the 25s ceiling; `await_all` times out, `shutdown_timeout`
  aborts the second write mid-flight, and everything still queued goes
  uncounted.

## Why this is architectural, not another numeric tuning

Round 16's point is that **no constant value of one flat outer deadline can
correctly bound an unbounded-depth per-client backlog.** Rounds 10 and 15
both adjusted the same single number
(`manta-cli::SHUTDOWN_DRAIN_DEADLINE`), applied registry-wide by
`tasks::await_all` around the ENTIRE tracked-task set at once. That shape
has a ceiling no constant escapes: for any deadline `D`, a backlog whose
sequential drain time exceeds `D` breaks it, and the broadcast channel's
retention capacity means that backlog depth is not bounded by anything the
outer deadline's owner controls. Widening `D` again (a fifth round on this
region) would only raise the backlog depth needed to reproduce the same
failure, not eliminate it.

There is a second blast-radius problem with the outer-only shape,
independent of sizing: `await_all`'s one `tokio::time::timeout` wraps the
WHOLE registry, not per-task, so one slow client with a deep backlog
consumes the entire budget and causes every OTHER client's still-draining
task to be aborted too, not just the slow one.

Per this fleet's round-based escalation policy, 3+ consecutive rounds on
the same region with a genuinely new bug shape (not the same bug found
again) is an explicit check-in trigger. This is that check-in: the user
was consulted (2026-09-02) and explicitly decided to defer the fix to this
ticket as an architectural follow-up rather than implement a fifth
same-region change under time pressure at round 16 itself.

## Decision: an inner deadline that lives with the loop, not above it

Each of the three per-client drain loops (`telnet::handle_client`'s one,
`json_stream::handle_tcp_client`'s and `handle_ws_client`'s) now enforces
its OWN deadline, `tasks::CLIENT_DRAIN_DEADLINE` (20s), checked as a
monotonic remaining-budget computation before each queued spot's write —
the same idiom `json_stream::looks_like_websocket_handshake` already uses
in this same file, not a new pattern. When the remaining budget hits zero
(checked explicitly, not just left to `tokio::time::timeout`'s own
handling — see below) or a write times out against it, the loop stops,
counts the just-attempted spot plus everything still retained in `rx` via
`metrics::abandoned_spot_count`, and disconnects.

This makes `SHUTDOWN_DRAIN_DEADLINE`/`await_all` a genuine **backstop**
rather than the actual bound: every healthy handler now provably returns
from `await_all` well within its own `CLIENT_DRAIN_DEADLINE`, so the outer
deadline only needs to stay comfortably above that one per-client number
(asserted directly by
`the_outer_shutdown_deadline_outlives_every_handlers_own_drain_deadline` in
`manta-cli`), never sized against any particular backlog depth or spot
count again.

### Why 20s inner / 25s outer

20s still covers one worst-case telnet spot (its two separately-timed 10s
`WRITE_TIMEOUT` writes — the property round 15 established and this change
preserves). The 5s margin above it covers task scheduling overhead, not
another spot's write, so the outer deadline provably never fires first for
a handler that's honoring its own inner deadline. Total worst-case
shutdown latency for a healthy daemon is unchanged from round 15's value —
no regression for an operator stopping a normally-behaving process. A
healthy client drains a full backlog in milliseconds regardless of depth;
the inner deadline only ever binds for a genuinely stalled peer, and now
counts what it abandons instead of relying on `Runtime::shutdown_timeout`
to truncate it silently.

### Why a per-spot remaining-budget check, not `timeout()` around the whole loop

Wrapping the entire drain loop in one `tokio::time::timeout` is simpler
code but reintroduces exactly the silent-loss shape this fix exists to
end: a spot already removed from `rx` by `try_recv()` but not yet written
when the wrapping timeout fires is neither delivered NOR counted — it's
just gone, dropped by the timeout's own cancellation with no chance for
the loop to record it. Checking the remaining budget explicitly BEFORE
each spot's write (and passing `WRITE_TIMEOUT.min(remaining)` as the
per-write timeout on the JSON/WS side, so an individual write can't outrun
what's left of the loop's own budget either) means the spot already pulled
off `rx` is always accounted for — passed as the in-flight `true` argument
to `abandoned_spot_count` — whether its own write succeeds, fails, or the
budget runs out first.

The `remaining.is_zero()` short-circuit checked explicitly (rather than
relying on `tokio::time::timeout(Duration::ZERO, fut)` alone) is
load-bearing, not defensive: `timeout` with a zero duration still polls
the wrapped future once before checking its timer, so without the
explicit check a fast socket could keep draining past an already-expired
budget and a zero-deadline test of the expiry path would be meaningless.

### Why the three drain loops stay duplicated

The three loops (telnet's `write_spot_line`, json_stream TCP's inline
`async` block over a raw `TcpStream`, json_stream WS's `ws.send`) share
the same shape but write via three different types with no common trait
or boxed-future abstraction that would meaningfully collapse them. In a
region already at review round 16, introducing a new shared abstraction
trades one kind of complexity (duplication, but each site individually
simple and locally readable) for another (an abstraction boundary that
itself becomes a new thing to get right and re-review) — the wrong trade
under this region's own review history. The duplication is noted directly
in a comment on the telnet site (the canonical one) so a future reader
sees it was a decision, not an oversight.

## What this does not change

- **Not** back-pressuring slow clients, at shutdown or otherwise —
  ARCHITECTURE §7's "slow clients are disconnected, never back-pressure
  the pipeline" is preserved verbatim; this only changes how the drain's
  own bound is applied and accounted, not the disconnect-don't-backpressure
  policy itself.
- **Not** reworking `tasks::await_all` or `Runtime::shutdown_timeout` —
  they remain the outer/final backstops, now provably non-binding for a
  well-behaved handler rather than removed.
- **Not** a promised zero-loss shutdown guarantee. ARCHITECTURE §7 now
  states explicitly: shutdown-time drain is a best-effort courtesy bounded
  by a per-client deadline, and anything that deadline abandons is
  COUNTED, never silently truncated (§8) — that is the actual guarantee
  this fix establishes, not "every queued spot always gets delivered no
  matter how slow the peer."
