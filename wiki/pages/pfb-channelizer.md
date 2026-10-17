---
id: pfb-channelizer
title: How does the PFB channelizer work and why is it shaped this way?
kind: subsystem
status: current
maintainer: agent
sources:
  - docs/SPEC-decode-core.md#1-channelizer-4-oversampled-polyphase-filterbank
  - ARCHITECTURE.md
verified:
  commit: d286147
  date: 2026-10-13
links:
  - overview
  - detector-tracks
  - coppa-reuse
---
The channelizer is a 4×-oversampled polyphase filterbank (PFB) that splits the wideband IQ into ~100 Hz channels, each of which fully contains one CW signal; detection runs on channel powers and decoders attach only to active channels. The PFB *is* the spectrum analyzer — its per-hop FFT output magnitude² feeds the detector directly, so there is no separate FFT path. Exact dimensions (N per input rate, hop, oversample factor, prototype taps, stopband) are normative in SPEC §1 — cite them, do not restate.

## How it works

- Polyphase FIR commutator + one N-point FFT per hop, via `coppa-dsp::fft::FftProcessor` (reused — see [[coppa-reuse]]).
- N scales with input rate to hold channel spacing near 100 Hz; the exact N table and hop are in SPEC §1.1.
- Prototype lowpass is a Kaiser-designed FIR — **new code** in `manta-dsp::proto`, not from coppa (SPEC §1.2, §10.1).
- Fine frequency estimate (for ±10 Hz spot accuracy, SPEC §1.4): two estimators. The dB quadratic in `manta-dsp::channelizer::interpolate_offset` feeds the ownership centroid (`Track::update_centroid`) and the drift fitter. The spot frequency (`Track::freq_hz`) comes from a power-weighted EMA of lag-1 phase products (`phase_lag_product`/`phase_position`), because AWGN on the quadratic's weak neighbour bin biased it by up to 38 Hz off channel centre on every table (`docs/DECISIONS/2026-10-17-man114-phase-difference-spot-frequency.md`). Both are live EMAs, deliberately not §1.4's old lifetime mean (`docs/DECISIONS/2026-07-19-m2-detector-track-pool-pins.md` item 5).
- Module map: SPEC §8 pins each spec section to its crate::module (its `manta-dsp::pfb`/`::centroid` rows predate the build; the channelizer is `manta-dsp::channelizer`).

## Why it is shaped this way

The whole design is viable only because it is cheap: the full-band pipeline is budgeted to stay well under one core, measured by the criterion bench `crates/manta-engine/benches/cpu_budget.rs`; the Pi 4 leg of that M2 acceptance gate is still open. The CPU budget table is in ARCHITECTURE §4; the 4× (not 2×) oversample choice was made to give enough envelope samples per dit for QSB'd fast CW. Stopband was tightened to 80 dB because pileup scenes need the dynamic range (SPEC §10.4). Detection and track handoff are covered in [[detector-tracks]].
