---
id: spot-validation
title: How does manta decide a decoded call is trustworthy enough to spot?
kind: subsystem
status: current
maintainer: agent
sources:
  - ARCHITECTURE.md
verified:
<<<<<<< HEAD
  commit: 01d1ea1
  date: 2026-09-07
=======
  commit: a1aad7da9e8cb98de7c2c68881b81d95c2bc98e6
  date: 2026-09-09
>>>>>>> fefb469186bfa83fd546cfe8843d88e1978ad91c
links:
  - decode-chain
  - spot-output-contract
  - live-hardware-field-testing
---
Decoded CW text is noisy, so validation — not decoding — is what makes a spot trustworthy. Per track, over a rolling text window, `manta-spot` parses CQ/DE context, checks callsign plausibility against a bundled cty.dat prefix list, optionally cross-checks the SCP super-check-partial list, requires a call to repeat before first spot, and dedupes/aggregates re-spots. The full pipeline and its parameters are described in ARCHITECTURE §6 — this page is the map, not the spec.

## How it works

- CQ/DE/beacon context parse sets spot type (carried in the RBN flag): ARCHITECTURE §6.1.
- cty.dat prefix lookup rejects unallocated prefixes; SCP membership only *raises* confidence, never gates (rare/new calls must still spot, not just well-known ones): §6.2–6.3.
- Repetition requirement (a call must decode more than once within a window before first spot) is the main garble filter: §6.4.
- Dedupe key = (callsign, freq bucket) with a re-spot suppression window unless SNR improves or type changes: §6.5.
- `Validator::tracks`/`RepetitionGate::seen` are per-track_id state that must be freed on `DecoderEvent::TrackClosed` — the normative teardown contract (a real, measured leak this bug produced) lives in `docs/DECISIONS/2026-09-02-man19-track-closed-teardown-invariant.md`, not here.
- An operator-allowlisted callsign (MAN-28's Watch List) can bypass the cty.dat prefix gate entirely, so a spot can reach [[spot-output-contract]] for a call cty.dat genuinely can't resolve — see `docs/DECISIONS/2026-09-07-man136-dxcc-and-unknown-geography-sentinels.md` for what manta emits in that case.

## Why it is shaped this way

The asymmetry is deliberate: false spots (bogus callsigns) are the failure mode that discredits the whole network, so the repetition gate and cty.dat rejection are tuned to make bogus spots rare — a V8/V8w pass criterion is *0 bogus callsigns*. Validated spots flow to [[spot-output-contract]].

The repetition gate assumes a bogus decode is random noise that won't repeat identically. A deterministic front-end artifact breaks that assumption — it produces the *same* garbled decode at a fixed frequency every time, so it repeats and passes the gate. Field-confirmed against real hardware: [[live-hardware-field-testing]].

**The BEACON exemption (step 4) doesn't need that assumption to break at all — it needs zero repeats.** A candidate the CQ/DE context parse tags `BEACON` (ARCHITECTURE §6.1/MAN-37: any plausible 3-15 char word followed by a lone trailing `T`, the shape an NCDXF/IARU beacon's unmodulated power-step dashes decode to) skips the repetition gate entirely by design — real beacons only ID once per cycle. That same pattern also matches noise-decoded garble that happens to end in a solo "T" word, and once tagged BEACON, one glimpse of that garble is a "confirmed" public spot with no second decode ever required. A 2026-09-09 overnight 40m capture ([[live-hardware-field-testing]]) produced 29 confirmed spots that were entirely this: implausibly fast WPM (avg ~51.5, several pinned at the tracker's own 60 WPM ceiling) and malformed text (`4AEEEEE`, `ER1EEAE`, ...), all as one-shot BEACON-exempt spots — not evidence of a deterministic front-end artifact needing to repeat identically, as an earlier reading of that data assumed. (A fixed-bin channelizer artifact could still explain why the *same* garbled decode recurs at the *same* frequency across separate sessions — but each occurrence only needed to survive one BEACON classification, not the repetition gate, to become a spot.) PR #154 (`crates/manta-spot/src/validator.rs`, `grammar.rs` — open as of this writing, not yet merged) adds a WPM-implausibility check scoped specifically to `SpotType::Beacon` candidates to close this; a residual few (structurally plausible, not implausibly fast) aren't caught by it and remain a known gap.
