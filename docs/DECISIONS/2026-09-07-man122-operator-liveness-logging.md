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
spawned.** A bind failure must never produce a "ready" line. On a
multi-thread Tokio runtime a spawned accept loop can admit a client
immediately, so emitting the banner before any `tokio::spawn` call
guarantees no per-connection log line can land ahead of it.

**Banner reports `local_addr()`, not the configured port.** Existing unit
tests and the ticket's own multi-agent-hygiene convention use `*_port = 0`
(ephemeral) — echoing the configured port would print a useless `:0`.

**Status line: one line per interval, no suppress-if-unchanged.** A status
line whose absence is meaningful ("the daemon stopped logging") is more
useful than one that goes quiet when nothing changes; suppress-if-unchanged
would make a wedged daemon and an idle band look identical.

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
or `Hang`) and `listen_with_track_count()` reports it to `main.rs` after
every processed batch, suppressing repeats so a steady band is one relaxed
atomic store per real change rather than ~24 a second. End of stream
reports `0`, so the gauge doesn't stay stuck at the last live value after
EOF or an SDR disconnect.

Deliberately *not* `TrackManager::active_track_count()`, which counts every
entry in `tracks` including unconfirmed CANDIDATEs -- rise crossings that
mostly close `Unconfirmed` within `confirm_hops` (~50 ms) without ever
leasing a decoder. Counting those would inflate an operator-facing "is it
decoding?" line with signals nothing is decoding; `active_track_count`
keeps its existing meaning for `soak_metrics`' peak/eviction accounting.

This does cost a new `manta-engine` entry point, which the plan had hoped
to avoid. `listen_with_track_count` is additive: `listen()` is now a
no-op-observer wrapper over it, so every existing caller and test is
untouched. The regression is pinned by
`track::tests::decoding_track_count_sees_a_promoted_track_that_has_emitted_nothing`,
which drives a steady unmodulated carrier through `process_hops`, asserts
the event stream is *empty*, and asserts the count is nevertheless 1, plus
`listen::tests::listen_reports_the_managers_track_count_and_clears_it_at_end_of_stream`
for the observer's rise/clear/dedup behaviour.

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
