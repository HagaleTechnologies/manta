---
id: overview
title: skimmer — what is this and where do things live?
kind: overview
status: current
maintainer: agent
sources:
  - README.md
  - ARCHITECTURE.md
  - ROADMAP.md
  - CLAUDE.md
verified:
  commit: e68b106
  date: 2026-07-07
links:
  - pfb-channelizer
  - decode-chain
  - coppa-reuse
---
skimmer is an open-source, cross-platform, wideband multi-signal CW skimmer (Rust) that consumes wideband IQ from commodity SDRs, decodes every CW signal in the passband concurrently, validates callsigns, and emits RBN-compatible spots — an open replacement for the single closed-source Windows program the Reverse Beacon Network depends on. It is **design-phase**: ARCHITECTURE, ROADMAP, and `docs/SPEC-decode-core.md` are frozen; no implementation has started. Read the specs first — the design decisions are already made.

## Where things live (planned 8-crate workspace)

The workspace layout and dependency graph are normative in ARCHITECTURE §2 — do not restate the crate table here; the pointers below map crates to wiki pages.

- `crates/skimmer-dsp/` — PFB channelizer, noise floor, envelope. See [[pfb-channelizer]] and [[detector-tracks]].
- `crates/skimmer-decode/` — keying state machine, timing, Morse decode. See [[decode-chain]].
- `crates/skimmer-spot/` — callsign validation, dedupe, scoring. See [[spot-validation]].
- `crates/skimmer-server/` — telnet cluster + JSON/WebSocket. See [[spot-output-contract]].
- `crates/skimmer-engine/` — track lifecycle, decoder pool orchestration.
- `crates/skimmer-input/`, `skimmer-testkit/`, `skimmer-cli/` — IQ sources, synthetic/golden harness, binary.
- `docs/SPEC-decode-core.md` — normative constants, equations, config keys, golden vectors. The wiki points here, never restates.

## Start here

- [[coppa-reuse]] — what is reused from the sibling coppa repo vs. new code here (the boundary that trips people up).
- [[watterson-dependency]] — the cross-repo dependency saga that blocked the golden-vector freeze until 2026-07-07.
- [[golden-vector-freeze]] — why V4/V5/V8w vectors must pin an exact coppa commit.
