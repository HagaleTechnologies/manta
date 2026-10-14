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
- Online 2-means speed tracking of dit/dah durations, Farnsworth-tolerant gap classification: SPEC §4.1–4.2 (`manta-decode::timing`).
- Per-track sender timing (dot, dash, element/char/word gap, mean and variance in dits; `SenderTiming`, `TimingUpdate`): SPEC §4.1b, MAN-315 decision `docs/DECISIONS/2026-10-11-man315-sender-timing.md`.
- Beam search over the Morse tree keeps marginal dit/dah hypotheses alive to the character boundary: SPEC §4.3–4.5 (`manta-decode::beam`, `::tree`). Beam is **character-local** — greedy across characters; word context belongs to the validator (SPEC §10.3).
- Per-character and per-callsign confidence feed [[spot-validation]]: SPEC §4.5–4.6.

## Gotchas

- The keying-decision band must not gate the rail updates: a mark that fades into the band would then update neither rail, freezing `E_hi` at its pre-fade level and deleting the faded marks (MAN-103 D5 did this; V8w recall fell to 0/50 until MAN-213). See `docs/DECISIONS/2026-10-05-man213-fade-tracking-keying-rails.md` and SPEC §3.2.

- HSMM evidence's centered maximum is an incremental deque that stays O(1) amortized only while its window bounds move forward; a `set_u_ref` that moves a bound backwards rebuilds it from the retained samples (SPEC v2 §1.2 implementation note, MAN-193 decision `docs/DECISIONS/2026-10-14-man193-centered-maximum-deque.md`).

- The gap unit is `dit_estimate_ms` (the symmetric mark/gap dit), not `mu_dit_ms`: the transmitter's rise time shortens marks and lengthens gaps by the same amount, so gaps in mark dits read long and 4.5-dit letter gaps split words (MAN-315 D3, SPEC §4.2). Beam likelihoods and the keying threshold still use `mu_dit_ms`.

- In the HSMM engine, a history entry's closing-segment provenance (`HistEntry::closing_segment`) is what makes a committed long gap reseedable: a word boundary closed by `Silence` merges the configured seed set even when the keying gate never re-opens, and the uniform score normalization that accompanies reseeding means a zero score with empty history is no longer proof a token is a raw seed — the scored-prefix count (`scored_live_len`) is. See `docs/DECISIONS/2026-10-14-man192-silence-reseed.md` and SPEC v2 §4.7.

## Why it is shaped this way

Beam search rather than hard thresholding is what makes a marginal element recoverable — a small-Viterbi, not a guess. A separate tone-finder stage is dropped here because the PFB ([[pfb-channelizer]]) already did the frequency selection. ML fusion is gated on beating this baseline (ROADMAP M4), not assumed.
