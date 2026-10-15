---
id: coppa-reuse
title: What does manta reuse from coppa, and what is deliberately new code?
kind: interface
status: current
maintainer: agent
sources:
  - ARCHITECTURE.md
  - docs/SPEC-decode-core.md#10-deviations-from-architecturemd
  - CLAUDE.md
verified:
  commit: d286147
  date: 2026-10-13
links:
  - pfb-channelizer
  - decode-chain
  - watterson-dependency
---
manta consumes DSP building blocks from the sibling coppa repo rather than reimplementing them — but the reuse boundary is narrower than it first looks, and getting it wrong is a documented footgun. manta **reuses** `coppa-dsp::fft` (FFT), `coppa-channel` (Watterson fading for tests; the golden vectors' reference-bandwidth AWGN is manta-testkit's own stand-in, `crates/manta-testkit/src/noise.rs`), and `coppa-audio` (cpal device input). It writes **new code** for the PFB channelizer, the Kaiser prototype designer, the order-statistic noise floor, envelope normalization, and the whole decode/validation/server stack. The authoritative reuse-vs-new table is ARCHITECTURE §2; the corrections are SPEC §10.

## Pointers

- Reuse/new table: ARCHITECTURE §2 ("Reused from coppa vs. new").
- Deviations from the original table, which ARCHITECTURE §2 now reflects: SPEC §10 — most importantly, **FIR design is NOT reused**. coppa-dsp ships only `RrcFilter`, so the PFB Kaiser prototype is new code (`manta-dsp::proto`), and coppa's block AGC (`AdaptiveAgc`) is dropped from the decode path in favor of a per-track fixed reference scale.
- Watterson reuse has its own caveats — see [[watterson-dependency]] and [[golden-vector-freeze]].
- coppa is consumed as a path/git dependency during co-development (a workspace-of-workspaces); switch to versioned deps if coppa publishes to crates.io first (ARCHITECTURE §2).

## Gotcha

The single reuse mistake to avoid: assuming "FIR design → coppa-dsp::filter". The PFB prototype is new code (SPEC §10.1, and ARCHITECTURE §2's table now says so); only `FftProcessor` reuse stands. cross-repo references (coppa module names) are plain text here by design — never `[[...]]`.
