---
id: pfb-channelizer
title: How does the PFB channelizer work and why is it shaped this way?
kind: subsystem
status: current
maintainer: agent
sources:
  - docs/SPEC-decode-core.md#1-channelizer
  - ARCHITECTURE.md
verified:
  commit: e68b106
  date: 2026-07-07
links:
  - overview
  - detector-tracks
  - coppa-reuse
---
The channelizer is a 4×-oversampled polyphase filterbank (PFB) that splits the wideband IQ into ~100 Hz channels, each of which fully contains one CW signal; detection runs on channel powers and decoders attach only to active channels. The PFB *is* the spectrum analyzer — its per-hop FFT output magnitude² feeds the detector directly, so there is no separate FFT path. Exact dimensions (N per input rate, hop, oversample factor, prototype taps, stopband) are normative in SPEC §1 — cite them, do not restate.

## How it works

- Polyphase FIR commutator + one N-point FFT per hop, via `coppa-dsp::fft::FftProcessor` (reused — see [[coppa-reuse]]).
- N scales with input rate to hold channel spacing near 100 Hz; the exact N table and hop are in SPEC §1.1.
- Prototype lowpass is a Kaiser-designed FIR — **new code** in `skimmer-dsp::proto`, not from coppa (SPEC §1.2, §10.1).
- Fine frequency estimate (for ±10 Hz spot accuracy) lands in `skimmer-dsp::centroid`, per SPEC §1.4.
- Module map: SPEC §8 pins each spec section to its crate::module.

## Why it is shaped this way

The whole design is viable only because it is cheap: the full-band pipeline stays well under one core, enforced by criterion benches as an M2 acceptance gate. The CPU budget table is in ARCHITECTURE §4; the 4× (not 2×) oversample choice was made to give enough envelope samples per dit for QSB'd fast CW. Stopband was tightened to 80 dB because pileup scenes need the dynamic range (SPEC §10.4). Detection and track handoff are covered in [[detector-tracks]].
