---
id: spot-validation
title: How does manta decide a decoded call is trustworthy enough to spot?
kind: subsystem
status: current
maintainer: agent
sources:
  - ARCHITECTURE.md
verified:
  commit: e68b106
  date: 2026-07-07
links:
  - decode-chain
  - spot-output-contract
---
Decoded CW text is noisy, so validation — not decoding — is what makes a spot trustworthy. Per track, over a rolling text window, `manta-spot` parses CQ/DE context, checks callsign plausibility against a bundled cty.dat prefix list, optionally cross-checks the SCP super-check-partial list, requires a call to repeat before first spot, and dedupes/aggregates re-spots. The full pipeline and its parameters are described in ARCHITECTURE §6 — this page is the map, not the spec.

## How it works

- CQ/DE/beacon context parse sets spot type (carried in the RBN flag): ARCHITECTURE §6.1.
- cty.dat prefix lookup rejects unallocated prefixes; SCP membership only *raises* confidence, never gates (rare/new calls must still spot, not just well-known ones): §6.2–6.3.
- Repetition requirement (a call must decode more than once within a window before first spot) is the main garble filter: §6.4.
- Dedupe key = (callsign, freq bucket) with a re-spot suppression window unless SNR improves or type changes: §6.5.
- `Validator::tracks`/`RepetitionGate::seen` are per-track_id state that must be freed on `DecoderEvent::TrackClosed` — the normative teardown contract (a real, measured leak this bug produced) lives in `docs/DECISIONS/2026-09-02-man19-track-closed-teardown-invariant.md`, not here.

## Why it is shaped this way

The asymmetry is deliberate: false spots (bogus callsigns) are the failure mode that discredits the whole network, so the repetition gate and cty.dat rejection are tuned to make bogus spots rare — a V8/V8w pass criterion is *0 bogus callsigns*. Validated spots flow to [[spot-output-contract]].
