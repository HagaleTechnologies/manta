---
id: source-reconnect
title: What's the gotcha with a live source disconnecting?
kind: gotcha
status: current
maintainer: agent
sources:
  - docs/DECISIONS/2026-10-05-man73-source-reconnect.md
  - crates/manta-cli/src/reconnect.rs
  - crates/manta-input/src/lib.rs (IqSource::take_discontinuity)
verified:
  commit: e398d46
  date: 2026-10-05
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
decision doc says why.

## Symptom

A reconnect test (real or simulated) shows a burst of bogus callsigns right after the source comes
back, or spot timestamps jump backwards relative to the outage.

## Where this is implemented

`manta-cli::reconnect::ReconnectingSource` wraps every reconnectable source (everything but file
replay) and retries with `manta-server::backoff`'s shared 1s-60s policy; `manta_source_health`
tracks it live (ARCHITECTURE §8).
