---
id: spot-validation
title: How does manta decide a decoded call is trustworthy enough to spot?
kind: subsystem
status: current
maintainer: agent
sources:
  - ARCHITECTURE.md
  - docs/SPEC-decode-core.md#46-per-callsign-confidence-consumed-by-manta-spot
verified:
  commit: 5b9e747
  date: 2026-09-04
links:
  - decode-chain
  - spot-output-contract
---
Decoded CW text is noisy, so validation — not decoding — is what makes a spot trustworthy. Per track, over a rolling text window, `manta-spot` parses CQ/DE context, checks callsign plausibility against a bundled cty.dat prefix list, optionally cross-checks the SCP super-check-partial list, requires a call to repeat before first spot, and dedupes/aggregates re-spots. Two MAN-28 exemptions cut across that and are easy to miss: a BEACON-tagged message is exempt from the repetition requirement, and an operator-allowlisted callsign is exempt from the context parse, the grammar/cty.dat check *and* the repetition requirement. The full pipeline and its parameters are described in ARCHITECTURE §6 — this page is the map, not the spec.

## How it works

- CQ/DE/beacon context parse sets spot type (carried in the RBN flag): ARCHITECTURE §6.1.
- cty.dat prefix lookup rejects unallocated prefixes — unless the callsign is operator-allowlisted, which is the *only* exemption from this check (a BEACON tag is **not** one: beacons still have to pass grammar/cty.dat); SCP membership only *raises* confidence, never gates (rare/new calls must still spot, not just well-known ones): §6.2–6.3.
- Repetition requirement (a call must decode more than once within a window before first spot) is the main garble filter, and has two exemptions: BEACON-tagged messages (NCDXF-style beacons ID once per power-step cycle and legitimately won't repeat in the window) and operator-allowlisted callsigns both spot on their first decode — the gate is lifted, the `r=1` confidence penalty is not: §6.4, SPEC §4.6.
- Operator allowlist (Watch List, config key `[spot] allowlist`) is the broader of the two exemptions: a listed call bypasses steps 1, 2 and 4 — it spots with no CQ/DE/UP/beacon framing at all, tagged `Unknown`, and is reclassified promotion-only if framing arrives later. It does **not** bypass the operator blocklist/notch overrides (evaluated first) or dedupe: ARCHITECTURE §6's "Operator allowlist (Watch List)" paragraph.
- Dedupe key = (callsign, freq bucket) with a re-spot suppression window unless SNR improves or type changes: §6.5.
- `Validator::tracks`/`RepetitionGate::seen` are per-track_id state that must be freed on `DecoderEvent::TrackClosed` — the normative teardown contract (a real, measured leak this bug produced) lives in `docs/DECISIONS/2026-09-02-man19-track-closed-teardown-invariant.md`, not here.

## Why it is shaped this way

The asymmetry is deliberate: false spots (bogus callsigns) are the failure mode that discredits the whole network, so the repetition gate and cty.dat rejection are tuned to make bogus spots rare — a V8/V8w pass criterion is *0 bogus callsigns*. The two exemptions are kept narrow for the same reason: BEACON lifts only the repetition gate (cty.dat rejection still applies), and the allowlist is opt-in per callsign by the operator who owns the consequences. Validated spots flow to [[spot-output-contract]].
