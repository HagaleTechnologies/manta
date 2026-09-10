# MAN-122: operator liveness logging (startup banner + periodic status line)

MAN-122 (found via the 2026-09-05 broad review, item O-02): `manta listen
--server-config` logged nothing at all until a client connected — no
"listening on ...", no version, no config summary. Every `tracing` call
site in the workspace lived inside a per-connection handler (MAN-59's
audit-logging scope), which is a Repudiation/security concern, not an
operability one — an operator watching the daemon's own log output had no
way to tell it came up at all, let alone whether it was still decoding.

## Design decisions

**stderr, not stdout, via the same `tracing` machinery MAN-59
established.** `start_spot_server`'s tracing subscriber already writes to
stderr specifically so `--json`'s stdout stream stays byte-pure for
machine consumers (`main.rs`'s own comment on `with_writer(std::io::stderr)`,
MAN-59 review round 6). Both new log lines reuse that subscriber and that
convention; neither line is emitted when `--server-config` is not set (the
subscriber is only initialized inside `start_spot_server`), matching the
ticket's own "Given manta starts as a daemon" precondition.

**Banner placement: after every bind succeeds, before any listener task is
spawned.** A bind failure must never produce a banner at all. On a
multi-thread Tokio runtime a spawned accept loop can admit a client
immediately, so emitting the banner before any `tokio::spawn` call
guarantees no per-connection log line can land ahead of it.

**The banner says `listening:`; readiness is a separate, later line**
(review round 2). Binding three sockets is not evidence that anything will
ever be decoded: a replay shorter than `manta_engine`'s two-second
`CALIBRATION_SECONDS` window, or a live source that fails its initial
reads, makes `listen_with_observers` bail during calibration and the
process exits without decoding a sample — after a banner that had already
declared the daemon ready. So the banner claims exactly what it knows
(`manta <ver> listening: source=... telnet=... json=... metrics=...`,
still one line, still every field scenario 1 names), and a second line,
`manta <ver> ready: decoding source=... sample_rate_hz=...`, is emitted
once from the decode loop's first processed batch — the earliest moment
the channelizer is built, the calibration window has been read and real
hops have gone through `TrackManager`. Pinned by
`startup_banner.rs::the_daemon_logs_a_startup_banner_before_any_client_connects`
(both lines, in order) and
`::a_daemon_whose_pipeline_never_starts_never_claims_to_be_ready`
(sub-calibration fixture: banner yes, readiness no).

**Banner reports `local_addr()`, not the configured port.** Existing unit
tests and the ticket's own multi-agent-hygiene convention use `*_port = 0`
(ephemeral) — echoing the configured port would print a useless `:0`.

**Status line: one line per interval, no suppress-if-unchanged.** A status
line whose absence is meaningful ("the daemon stopped logging") is more
useful than one that goes quiet when nothing changes; suppress-if-unchanged
would make a wedged daemon and an idle band look identical.

**`pipeline=` ties the line to real decode progress** (review round 2).
The status task is scheduled independently of the synchronous decode loop,
so on its own it will happily republish the last `tracks=N` every interval
forever after that loop stops progressing — `AudioIqSource::read` blocking
because an audio device stopped delivering callbacks is the concrete case,
and it defeats the one thing the line exists to establish. `Metrics` now
carries a monotonic `pipeline_batches` counter, bumped once per processed
batch from the same `main.rs` observer that publishes the track gauge; the
status task compares it against its own previous sample and reports
`pipeline=decoding` (it moved), `pipeline=stalled` (it did not) or
`pipeline=starting` (no batch yet — the expected reading for the first
interval or two of a live source, whose calibration window is two real
seconds, and not a stall). The counter is deliberately not exported in the
Prometheus text: it is a liveness edge, not a figure worth graphing. This
is why `listen_with_observers`'s observer now fires on every batch
rather than only on a change — see below.

**`starting` is bounded by a grace period** (review round 3). "No batch
yet" was originally classified as `starting` unconditionally, which made a
daemon wedged *before* its first batch — a first `IqSource::read` that
blocks, a source that never accumulates the two-second calibration window —
report `starting` on every status line for the rest of its life: the one
wedge the field was added to catch was the one wedge it could not name.
Zero progress now reads as `starting` only within `status::STARTUP_GRACE`
(30 s, ~15x the calibration window: ample for a KiwiSDR TCP connect or a
SoapySDR device init, and shorter than the 60 s default interval, so the
very first status line of a wedged startup already says `stalled`), and as
`stalled` after it. Nonzero progress is never reclassified by elapsed time.

**Shutdown beats the interval tick** (review round 2). The status task's
`select!` is `biased` with the shutdown arm first, and re-checks
`shutdown.borrow()` after the sleep arm wins. Unbiased, `select!` picks at
random among ready branches, so a sleep and a shutdown notification that
come ready in the same poll could emit one more status line into the
middle of the shutdown drain — the exact thing the shutdown arm was added
to prevent.

**Status interval: default 60 s, configurable via `status_interval_secs`,
`0` disables.** 60 s is a quiet default for a process expected to run for
months on a Raspberry Pi; operators who want the previously-proposed 30 s
cadence (broad review lens 1 #7) or who ship logs by the byte have a
one-line config knob.

**`uplink=none` is distinct from `uplink=disconnected`.** `Metrics::
uplink_connected()` is a boolean with no way to distinguish "no
`[[rbn_uplink]]` configured" from "configured but down" — the single most
misreadable field on the line if collapsed. The status task counts
`enabled` uplink configs at spawn time and reports `none` when that count
is zero, `connected`/`disconnected` from the live gauge otherwise.

**`manta_active_tracks` is populated as part of this ticket, not deferred.**
The ticket's own title is "tell manta is alive **and decoding**" — shipping
the status line with a permanently-frozen `tracks=0` (ARCHITECTURE.md §8's
pre-existing, documented gap) would read as a fault on a healthy,
actively-decoding node, which is worse than not shipping the field at all.
**The count is `TrackManager`'s lifecycle state, not the decode event
stream** (review round 1). The first cut derived it in `main.rs`'s
`on_event` closure -- a `HashSet<u32>` of open `track_id`s, inserted on any
track-scoped event and freed on `TrackClosed`. That is wrong in the one
direction that matters for a liveness line: `TrackDecoder` emits no
`TrackMeta` until its demodulator latches (`snr_2500_db()` returns `Some`),
and emits nothing else at all until it decodes a character, while
`TrackManager` will legitimately hold such a track ACTIVE with a leased
decoder until the ~30 s `gc_hops` silent GC. A weak, drifting, or
unmodulated signal that real decoders are grinding on would therefore have
reported `tracks=0` and `manta_active_tracks 0` -- exactly the "healthy
node reads as a fault" failure this field exists to avoid, just with a
narrower trigger. `crates/manta-engine/src/track.rs`'s
`decoding_track_count()` counts tracks holding a leased decoder (`Active`
or `Hang`) and `listen_with_observers()` reports it to `main.rs` after
every processed batch. Repeats were suppressed in the engine at first; as
of review round 2 they are not, because that call is also the daemon's
only per-batch decode-progress signal (see `pipeline=` above) and a
change-only notification cannot distinguish "quiet band, count steady at
2" from "read wedged, count frozen at 2 since an hour ago". The cost is
two relaxed atomic ops per ~43 ms chunk, immediately after that chunk's
channelizer and `TrackManager` work — unmeasurable against it, including
against the Pi 4 single-core budget. End of stream reports `0`, so the
gauge doesn't stay stuck at the last live value after EOF or an SDR
disconnect.

Deliberately *not* `TrackManager::active_track_count()`, which counts every
entry in `tracks` including unconfirmed CANDIDATEs -- rise crossings that
mostly close `Unconfirmed` within `confirm_hops` (~50 ms) without ever
leasing a decoder. Counting those would inflate an operator-facing "is it
decoding?" line with signals nothing is decoding; `active_track_count`
keeps its existing meaning for `soak_metrics`' peak/eviction accounting.

This costs no new `manta-engine` entry point. MAN-45 landed
`listen_with_observers` on main in parallel with this ticket, publishing
`active_track_count()` into a shared `AtomicU64` (`ListenObservers`) that
the daemon polls into `Metrics` every 250 ms; merging main into this branch
folded MAN-122's per-batch observer into that same function as an
additional `on_tracks` callback rather than adding a second entry point,
and corrected the published number from `active_track_count()` to
`decoding_track_count()` for the reasons just above. Both observers carry
the same count and both are wanted: the atomic is what a consumer on
another thread can poll on its own schedule, and the callback is the
per-batch *edge* -- a polled gauge value cannot distinguish a steady count
from a wedged decode loop. `listen()` is a no-op-observer wrapper over the
merged function, so every existing caller and test is untouched. The regression is pinned by
`track::tests::decoding_track_count_sees_a_promoted_track_that_has_emitted_nothing`,
which drives a steady unmodulated carrier through `process_hops`, asserts
the event stream is *empty*, and asserts the count is nevertheless 1, plus
`listen::tests::listen_reports_the_managers_track_count_and_clears_it_at_end_of_stream`
for the observer's rise/clear/per-batch behaviour.

**Formatting lives in `manta-server::status`, not `manta-cli`.**
`manta-server` is a lib crate, so the two line-format functions and the
spots-per-minute derivation get ordinary unit tests, and the periodic task
sits next to the `Metrics` it reads. `manta-cli` keeps only the call
sites (banner emission point, status-task spawn point).

## Explicitly out of scope

- **No `manta status` subcommand / `GET /status` JSON route** — MAN-44,
  which also owns per-target uplink health and a `connected`/`flapping`/
  `down` verdict.
- **No `/healthz`, no per-band spot metrics** — MAN-128.
- **No CPU-percent field** — no CPU accounting exists anywhere in this
  workspace today; adding process-CPU sampling is a new dependency for a
  field the ticket doesn't ask for.
- **No change to stdout in text mode** — the decoded-character flood in
  server mode is a separate default-output-verbosity concern.
- **No git SHA / build info in `--version`** — broad review R-13.
- **No `manta_engine::listen()` signature change** to expose `TrackManager`
  directly.

## Scenario 2's timing is pinned in `manta-server`, not through the binary

Review round 2 removed `startup_banner.rs`'s "a status line appears during
a 240-second replay" test. It was only ever true while the unpaced replay
took longer in real time than the status interval — a property of the
runner, not of the daemon. Nothing in the CLI's surface can pace a file
replay (`AudioIqSource` eager-loads the whole WAV and then reads from
memory), so on a fast enough machine the fixture decodes inside one
interval, shutdown cancels the status task before its first tick, and the
test goes red on correct production behaviour — on a repo that requires
both platform test jobs green. The timing half of scenario 2 is pinned
where time is controllable instead:
`manta-server/tests/status_line_acceptance.rs` (the real
`spawn_status_line` task, a 50 ms interval, emission + rate limit) and
`manta-server::status`'s unit tests (zero-interval disable, shutdown
return, `pipeline=` classification). What stays end-to-end through the
real binary is the part that cannot race: the banner, its field content,
its being line 1, and readiness never being claimed by a daemon whose
pipeline never started.

## Review round 4 (P2): shutdown and status emission are ordered by a join

`spawn_status_line`'s two in-task guards -- the `biased` `select!` and the
post-sleep `shutdown.borrow()` -- narrow the window in which a status line
can land inside the shutdown drain, but they cannot close it: a shutdown
signalled after that borrow reads `false` and before the loop's
`tracing::info!` still produces one line, and no in-task check can be made
atomic against an external signal.

The ordering is therefore established from the outside. `spawn_status_line`
already returned a `JoinHandle`; the daemon now RETAINS it
(`SpotServer::status_line`) and awaits it immediately after
`shutdown_tx.send(true)` and before the client drain starts. Once that join
returns the task is gone, so no further line is possible. The wait is
bounded by one scheduler poll rather than by `status_interval_secs`,
because the task's `shutdown.changed()` arm is `biased`-first -- pinned by
`main.rs`'s `the_status_line_task_is_retained_and_joins_promptly_on_shutdown`,
which configures a one-hour interval and joins inside a one-second budget.
