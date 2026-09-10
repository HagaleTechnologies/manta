---
id: decode-chain
title: How does the per-track CW decode chain work?
kind: subsystem
status: current
maintainer: agent
sources:
  - docs/SPEC-decode-core.md#3-per-track-demodulation
  - ARCHITECTURE.md
verified:
  commit: e68b106
  date: 2026-07-07
links:
  - detector-tracks
  - spot-validation
---
Each active track runs a classical CW decode chain on its ~375 Hz complex channel stream: envelope → dual-EMA adaptive keying threshold → online 2-means speed tracking → beam search (width 4) over the Morse tree, emitting characters with per-element confidence. It is the wideband, headless port of the sibling dit repo's proven single-channel engine; classical ships first and defines the accuracy baseline an ML fusion stage (M4) must beat under fading. Thresholds, hysteresis, beam width, and gap constants are normative in SPEC §3–4 and §9 — cite, do not restate.

## How it works

- Envelope + per-track fixed reference scale (coppa's block AGC is deliberately *not* used — SPEC §3.1, §10.2): `manta-decode::envelope`.
- Dual-EMA threshold + hysteresis/debounce key decisions: SPEC §3.2–3.4.
- Online 2-means speed tracking of dit/dah durations, Farnsworth-tolerant gap classification: SPEC §4.1–4.2 (`manta-decode::timing`). Gotcha: the word-gap dits constant has two call sites with different semantics (`classify()`'s classification threshold vs. `flush_threshold_dits()`'s flush-scaling divisor) — they were split into `WORD_GAP_DITS`/`SPEC_WORD_GAP_DITS` after MAN-2 so a classification-threshold tune can't silently rescale the unrelated flush trigger; see `docs/DECISIONS/2026-09-04-word-gap-threshold-fix.md`.
- Beam search over the Morse tree keeps marginal dit/dah hypotheses alive to the character boundary: SPEC §4.3–4.5 (`manta-decode::beam`, `::tree`). Beam is **character-local** — greedy across characters; word context belongs to the validator (SPEC §10.3).
- Per-character and per-callsign confidence feed [[spot-validation]]: SPEC §4.5–4.6.

## Why it is shaped this way

Beam search rather than hard thresholding is what makes a marginal element recoverable — a small-Viterbi, not a guess. A separate tone-finder stage is dropped here because the PFB ([[pfb-channelizer]]) already did the frequency selection. ML fusion is gated on beating this baseline (ROADMAP M4), not assumed.
