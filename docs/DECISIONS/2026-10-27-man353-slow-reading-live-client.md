# MAN-353: a reading live client is dropped only past the lag allowance; the stall test runs a slow reader on every leg

Date: 2026-10-27. Ticket: MAN-353, filed from the `test (windows-latest)`
failures of PR #281 (MAN-324): PR run 38434850417 and merge-queue run
38440097481.

## Context

`a_live_client_that_stops_reading_is_dropped_and_publishing_never_blocks`
(`crates/manta-server/tests/live_state_acceptance.rs`) connects two clients
to the live-state listener. One never reads; the other reads every line.
The test then publishes `LIVE_CHANNEL_CAPACITY + 20_000` records of about
1 KiB each, and expects only the stalled client to be dropped.

Between 2026-10-13T16:58Z and 2026-10-14T17:36Z it failed 19 times on
`windows-latest` with `the reading client was disconnected after N lines`,
N from 3,040 to 10,457. The test paced its publisher by the clock: 256
records, then a 1 ms sleep. Windows debug-build loopback carried about
15 MB/s of these lines, so the reading client fell more than 4096 records
behind the live channel and the server dropped it, as `docs/LIVE-STATE.md`
specifies. The server was not at fault.

PR #302 (`d9b2218`, 2026-10-14) paced the publisher on what the reading
client has read instead, keeping it at most `LIVE_CHANNEL_CAPACITY / 2`
records ahead. From then through 2026-10-27, 324 of 324 passing
`test (windows-latest)` jobs ran the test `ok`, and it failed in none.
Nobody linked #302 to this ticket.

Windows is not a required check, and on Linux the clock-paced test passes
even pinned to two cores beside four busy loops. CPU load does not expose
the regression; only a reader slower than a free-running publisher does.
So the required legs could not see the clock pacing come back.

## Decision

- **The drop rule stays as documented.** A live client is disconnected
  when a single write takes longer than 10 s, or when it falls more than
  4096 records behind the live channel (`docs/LIVE-STATE.md`,
  `docs/SPOT-SCHEMA.md`). That applies whether the client has stopped
  reading or only reads slower than the node publishes; the server does
  not tell the two apart beyond that allowance. Waiting for slow readers
  would spend memory or back-pressure the decode loop, which
  `ARCHITECTURE.md` ("slow clients are disconnected, never back-pressure
  the pipeline") and MAN-324 D9's bounded cap rule out. The same
  `json_stream::serve` also serves `:7301` (MAN-324 D3), so a rule change
  would move the spot stream too.
- **The test keeps #302's reader-paced publisher.** The ticket's other
  suggestion, a deadline-based drain in the reading client, cannot bound
  the server-side lag: a reader slower than a free-running publisher falls
  behind whatever its read timeout is. Pacing the publisher on what the
  reader has consumed bounds that client's lag by construction.
- **The scenario runs twice, through one helper.**
  `only_the_stalled_live_client_is_dropped(reader_lines_per_sec)` holds the
  body. `a_live_client_that_stops_reading_is_dropped_and_publishing_never_blocks`
  keeps its name and runs it with an unthrottled reader.
  `a_live_client_that_reads_slowly_stays_connected_while_a_stalled_one_is_dropped`
  runs it with the reader capped at `SLOW_READER_LINES_PER_SEC` = 10,000
  lines/s, about 10 MB/s, below what the failing Windows runner carried.
  The cap sleeps every 64 lines until an absolute deadline measured from
  the reader's start, so a coarse timer (about 15.6 ms on Windows) late on
  one batch is caught up on the next.
- **Both runs check the reader is still connected after the drop.** Once
  the stalled socket reaches EOF, the helper publishes `"{total} end"` and
  requires the reading client to receive it. That states "only the stalled
  client is dropped" as an assertion.

## Consequences

- No server code, wire format, metric, config or documented limit changes.
- With the pre-#302 clock pacing put back in the helper, the slow-reader
  test fails on Linux with the Windows message, and the unthrottled test
  still passes. Three runs of
  `cargo test -p manta-server --test live_state_acceptance --locked` on a
  Linux container:
  `a_live_client_that_reads_slowly_stays_connected_while_a_stalled_one_is_dropped ... FAILED`
  with `the reading client was disconnected after 2733 lines`, then 2896,
  then 2785, while
  `a_live_client_that_stops_reading_is_dropped_and_publishing_never_blocks ... ok`
  in each. The required `ubuntu-latest` and `macos-latest` legs now guard
  the Windows fix.
- The test binary took 2.94 s on that container with both runs; the
  capped run reads its 24,096 lines in about 2.4 s.
- `the_live_listener_admits_at_most_32_clients` failed twice on Windows
  with a different symptom (`left: 0, right: 32`). It is a different test
  and is not covered here.
