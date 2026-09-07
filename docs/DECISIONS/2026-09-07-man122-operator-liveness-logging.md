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
`main.rs`'s `on_event` closure already observes every `DecoderEvent`
in-process; it now maintains a `HashSet<u32>` of currently-open `track_id`s
(insert on any track-scoped event -- `CharDecoded`/`WordBoundary`/
`SpeedUpdate`/`TrackMeta` -- remove on `TrackClosed`) and republishes
`set_active_tracks` on every change. This mirrors `manta-spot::Validator`'s
existing per-track bookkeeping (MAN-19), which keys the same way rather
than on `TrackMeta` alone: `TrackMeta` only arrives at a 375-hop/~1 s
cadence and only once the envelope demod is `running()`, so inserting on
`TrackMeta` alone would leave short-lived or weak-demod tracks undercounted
even while they decode and eventually close. Needs no `manta-engine` API
change — `TrackManager::active_track_count()` exists but stays
unreachable from outside `listen()`; deriving the count from the event
stream `main.rs` already receives was the smaller change. Measured against
a 60 s/22 wpm/48 kHz replay (2522 events): 179 `TrackMeta`/`TrackClosed`
pairs, zero unpaired ids in either direction, zero left open at EOF. A long
live soak (unavailable in this environment) is the real bound on whether
this invariant holds indefinitely; see ARCHITECTURE.md's Performance
Considerations note in the implementation plan.

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
