# 2026-10-05 — MAN-73: live source reconnect design

**Status:** Implemented.

## Problem

Any live-source read error (a stalled KiwiSDR socket, an HPSDR stall escalation past
`MAX_CONSECUTIVE_MALFORMED`/`MAX_CONSECUTIVE_TIMEOUTS`, a dropped audio device) propagated out of
`manta_engine::listen`'s read loop and ended the whole process — a 3am Kiwi hiccup silently killed
the node until someone restarted it by hand. `manta_source_health` was also one-sided (MAN-64
finding 1): the only production call site ever set it `true`.

## Design: wrap the source, report the gap, restart the track segment

`manta-cli::reconnect::ReconnectingSource` wraps every reconnectable `IqSource` (every kind but
file replay) at the composition root (`main.rs`'s `Command::Listen`). A read error is retried
forever — until Ctrl-C — by reopening the source with `manta-server::backoff`'s shared 1s→60s
policy (see below), rather than ever reaching `listen()`. `listen()` itself never observes an
error from a wrapped source; from its perspective `read()` just returns real samples a little
later than usual, after an outage reported once via the new `IqSource::take_discontinuity()`
trait method.

### Why not retry the whole `listen()` call

`SpotBus::unix_ts_for(sample_ts) = epoch + sample_ts / fs` with `epoch` fixed once at session
start (`manta-server/src/bus.rs`). A fresh `Channelizer`/`TrackManager` necessarily starts its hop
counter near 0, so naively tearing down and re-running `listen()` from scratch on every reconnect
would back-date every post-reconnect spot's timestamp against the session's one fixed epoch.
Instead, `listen()` (`manta-engine/src/listen.rs`) owns a `Segment` (channelizer + track manager)
that it can restart in place: on `take_discontinuity() == Some(gap)`, it closes the current
segment's tracks (emitting their `TrackClosed` events), advances its running sample-clock base by
`consumed + gap`, and builds a fresh `Segment` whose `TrackManager` resumes track-id numbering
from where the old one left off (`TrackManager::next_track_id`/`resume_track_ids_from`) — so a
restarted segment never reuses an id, and `sample_ts` stays monotonic and wall-clock-true with no
audio spliced across the gap.

### Why not zero-fill the gap

MAN-60's unmerged Kiwi-internal reconnect design (draft PR #109) zero-filled up to 60s of outage.
Measured against `manta-dsp::floor`'s 25th-percentile noise-floor estimator: zero-filling an
outage longer than ~2.5s pins the floor at -140 dBFS, and every real signal above the actual noise
floor then reads as enormously over threshold on resume, flooding false tracks/spots. Reporting
the gap in samples via `take_discontinuity()` and skipping straight to a fresh segment (no
synthetic samples at all) avoids this entirely.

If #109 is revived, its fast in-socket retry (4 attempts at 500ms→2s) can stay as a first layer
*above* `ReconnectingSource` — when it gives up, the resulting `Err` reaches `ReconnectingSource`,
which retries forever at 1s→60s. Its zero-fill must be replaced by `take_discontinuity()` instead.
`kiwi.rs`'s new `CONNECT_TIMEOUT` (this ticket) textually conflicts with #109's own 5s constant;
keep one constant on rebase.

### Shared backoff policy

`manta-server::backoff` (new module) extracts `uplink.rs`'s existing policy — reset to 1s after a
connection that reached login/produced data, double towards a 60s cap otherwise — so the outbound
RBN uplink and every live input source share one implementation and one set of tests.
`uplink::serve` keeps its own sleep-then-update ordering (can leave a stale long backoff after a
healthy connection drops — a known, accepted quirk, unchanged by this ticket).
`ReconnectingSource` instead updates-then-sleeps, so a connection that was productive and then
drops always waits exactly the 1s initial backoff, not whatever long backoff preceded it.

### Startup fail-fast, file replay excluded

A source that fails to open at all on startup (bad host, missing device) still ends the process
immediately, exactly as before — only a loss *after* the first successful open is retried.
`LiveSourceSpec::File` is never wrapped: file replay's errors and EOF must reach `listen()`
unchanged for byte-identical replay (ARCHITECTURE §9's determinism requirement), and health is set
`true` once, immediately, as before.

### MAN-55 watcher retirement

The MAN-55 watcher task (an `rt.spawn`'d loop polling `confirmed_live_handle()` to flip health
true on the first real packet) is deleted. `ReconnectingSource` subsumes it: it reads the same
`confirmed_live_handle()` signal to decide `initial_healthy` and flips health on every later
reconnect too, not just the first connection — closing MAN-64 finding 1 as a side effect.

## Coordination notes

- **MAN-60 (#109):** see zero-fill section above.
- **MAN-64 (#100, watcher-guard race):** moot — the watcher it guarded no longer exists.
- **MAN-122 (#136):** also edits `Command::Listen` in `main.rs` (startup banner/status line);
  expect a textual conflict only, not a design conflict.
- No config, schema, or wire-format migration. The metrics label values (`kiwi`/`soapy`/`hpsdr`/
  `audio`/`file`) are unchanged.
