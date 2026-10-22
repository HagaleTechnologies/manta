---
id: source-reconnect
title: What's the gotcha with a live source disconnecting?
kind: gotcha
status: current
maintainer: agent
sources:
  - docs/DECISIONS/2026-10-05-man73-source-reconnect.md
  - docs/DECISIONS/2026-10-16-man259-acquisition-anchored-outage.md
  - crates/manta-cli/src/reconnect.rs
  - crates/manta-cli/src/reconnect.rs (InputHealthTotals)
  - crates/manta-input/src/lib.rs (IqSource::take_discontinuity)
  - crates/manta-input/src/lib.rs (IqSource::acquisition_start_offset)
  - docs/DECISIONS/2026-10-15-man13-multi-input-daemon.md
  - docs/DECISIONS/2026-10-16-man252-cancellable-source-reopen.md
  - docs/DECISIONS/2026-10-19-man307-hpsdr-multiple-receivers.md
  - crates/manta-cli/src/hpsdr_group.rs
verified:
<<<<<<< HEAD
  commit: cdf8487
  date: 2026-10-19
=======
  commit: b8b9367
  date: 2026-10-16
>>>>>>> 90e7406b565c582487848e6fb21638838fb3baf1
links:
  - determinism
---
Never zero-fill an input outage to paper over a reconnect. Measured: zero-filling past ~2.5s pins
`manta-dsp::floor`'s 25th-percentile noise-floor estimator at -140 dBFS, so every real signal reads
as wildly over threshold on resume and floods false tracks. Report the outage in samples via
`IqSource::take_discontinuity()` instead and let `manta_engine::listen` restart the track segment
with no synthetic samples at all — see the decision doc for the full design and why retrying the
whole `listen()` call (instead of restarting just the segment) would back-date timestamps against
`SpotBus`'s fixed session epoch. Gotcha: the restart closes the old tracks with
`TrackManager::finish_for_discontinuity` (`Bookkeeping`), never `finish()` (`SignalEnded`) — the
decision doc says why; a stop during an outage takes the same teardown (MAN-258): the wrapper ends
the stream with the outage still pending, and `listen` checks for a pending gap after its loop.
Gotcha: a per-connection handle taken before wrapping (`health_counters()`,
`confirmed_live_handle()`-style) goes stale at the first reconnect, because every reopen builds a
new device with fresh counters, so poll `ReconnectingSource::input_health()` (MAN-228) instead; and
syncing counts after `read()` returns is not enough, because HPSDR counts inside a `read()` that
blocks until a valid packet arrives. Gotcha (MAN-252): every reopen runs on its own `reopen-<kind>`
thread, which a stop request abandons, so anything `LiveSourceSpec::open` builds must be `Send`; a
source or wrapper holding `Rc` or raw pointers will not compile into the opener. Gotcha: the
outage ends where the reopened device started streaming, not where its opener returned (MAN-259).
Every driver starts acquiring inside its open call and returns what it buffered meanwhile, so each
reports `IqSource::acquisition_start_offset()` and every wrapper between it and
`ReconnectingSource` (`DecimatingSource`, `FixedCenterFreqSource`) must forward it; anchoring at
the opener's return counts those samples twice and stamps every later spot late.

## Symptom

A reconnect test (real or simulated) shows a burst of bogus callsigns right after the source comes
back, or spot timestamps jump backwards relative to the outage, or run late after it by about as
long as the receiver took to reopen.

## Where this is implemented

`manta-cli::reconnect::ReconnectingSource` wraps independently reconnectable sources and retries with `manta-server::backoff`'s shared 1s-60s policy; `manta_source_health`
tracks it live (ARCHITECTURE §8).
`manta_source_outages_total`/`manta_source_down_seconds_total` (MAN-96) count each drop and its
completed down time, so a reconnect between two scrapes still shows; the 30-day field-node
procedure reads them (`docs/RUNBOOKS/secondary-skimmer-field-node.md`).

## Several inputs

With several independent `[[input]]` entries (MAN-13), each input reconnects on its own
thread: one input dropping changes only its own `manta_source_health{source}` series and logs
`input NAME: lost; retrying, k of n inputs decoding` — the others keep decoding, `/healthz` stays
`ok` while at least one input is healthy, and the `MantaInputDown` alert (not `MantaNodeUnhealthy`)
reports the dead one. Track ids are interleaved per input, so an input's per-track validator state
never collides with another's. See
[the decision record](../../docs/DECISIONS/2026-10-15-man13-multi-input-daemon.md).


HPSDR receivers sharing a radio instead share one control/reconnect owner
(MAN-307). Do not wrap each DDC in an independent opener. See the
[four-band guide](../../docs/GUIDES/multiple-receivers.md#several-bands-on-one-hpsdr-radio)
and [generation, queue-loss and timing decisions](../../docs/DECISIONS/2026-10-19-man307-hpsdr-multiple-receivers.md).
