---
id: detector-tracks
title: How does the detector and track manager decide what to decode?
kind: subsystem
status: current
maintainer: agent
sources:
  - docs/SPEC-decode-core.md#2-noise-floor-signal-presence-detection
  - ARCHITECTURE.md
verified:
  commit: e68b106
  date: 2026-07-07
links:
  - pfb-channelizer
  - decode-chain
---
The detector estimates a per-channel noise floor by order statistics (a quantile over a sliding window — a median-like estimator, so CW keying does not inflate its own floor), gates channels active when smoothed power exceeds the floor by a threshold with hysteresis, and promotes each active channel to a **track** that leases a decoder from a bounded pool. This is what turns a spectrum into a set of things worth decoding. Exact quantile, window, on/off thresholds, hang/gc timers, and track cap are normative config keys in SPEC §2 and §9 — cite, do not restate.

## How it works

- Floor estimator + neighborhood/effective floor + gate: SPEC §2.1–2.3 (`skimmer-dsp::floor`).
- Track lifecycle state machine (OPEN → ACTIVE → hang → CLOSED): SPEC §2.4 (`skimmer-engine::track`).
- Adjacent-channel ownership so one signal yields exactly one track (no cross-channel ghost decodes): SPEC §2.5. This invariant is a V7/V8w pass criterion — see [[golden-vector-freeze]].
- Track cap with lowest-SNR eviction; evictions are counted and surfaced as metrics — **no silent coverage loss** (ARCHITECTURE §4, §8).

## Why it is shaped this way

Order statistics rather than a mean floor is the load-bearing choice: CW is bursty, and a mean would let a strong keyed signal raise the threshold above weaker neighbors. The hysteresis + hang timing exists to survive QSB and inter-word gaps without dropping a track mid-QSO. Active tracks feed the [[decode-chain]].
