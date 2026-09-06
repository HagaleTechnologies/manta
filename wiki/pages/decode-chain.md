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
  commit: 826bdd8
  date: 2026-09-06
links:
  - detector-tracks
  - spot-validation
---
Each active track runs a classical CW decode chain on its ~375 Hz complex channel stream: envelope → dual-EMA adaptive keying threshold → online 2-means speed tracking → beam search (width 4) over the Morse tree, emitting characters with per-element confidence. It is the wideband, headless port of the sibling dit repo's proven single-channel engine; classical ships first and defines the accuracy baseline an ML fusion stage (M4) must beat under fading. Thresholds, hysteresis, beam width, and gap constants are normative in SPEC §3–4 and §9 — cite, do not restate.

## How it works

- Envelope + per-track fixed reference scale (coppa's block AGC is deliberately *not* used — SPEC §3.1, §10.2): `manta-decode::envelope`.
- Dual-EMA threshold + hysteresis/debounce key decisions: SPEC §3.2–3.4.
- Online 2-means speed tracking of dit/dah durations, Farnsworth-tolerant gap classification: SPEC §4.1–4.2 (`manta-decode::timing`).
- Beam search over the Morse tree keeps marginal dit/dah hypotheses alive to the character boundary: SPEC §4.3–4.5 (`manta-decode::beam`, `::tree`). Beam is **character-local** — greedy across characters; word context belongs to the validator (SPEC §10.3).
- Per-character and per-callsign confidence feed [[spot-validation]]: SPEC §4.5–4.6.

## Why it is shaped this way

Beam search rather than hard thresholding is what makes a marginal element recoverable — a small-Viterbi, not a guess. A separate tone-finder stage is dropped here because the PFB ([[pfb-channelizer]]) already did the frequency selection. ML fusion is gated on beating this baseline (ROADMAP M4), not assumed.

## MAN-9: the V8w fading-robustness ceiling, and two inert-lever findings

Under Watterson-Poor fading, coherence time (~0.32 s) is shorter than a single Morse element at slow WPM, so multiple independent fades land inside one dit/dah. Every adaptive stage in this chain (debounce, key rails, speed centroids) is tuned to a slower timescale, and a fade-truncated mark's distortion outlives the fade itself via the speed tracker's EMA — the mechanism behind V8w's 1/34 strong-signal pass rate (median CER ≈ 0.276, gate wants ≥ 90 % at < 0.10). A bounded three-rung classical mitigation ladder (proportional debounce, quality-gated beam width, speed-tracker outlier admission) shipped behind inert-by-default config fields but did not close the gap — see `docs/DECISIONS/2026-09-04-man9-v8w-fading-baseline.md` for the full measurement record and `v8w_classical_baseline_does_not_regress` for the pinned floor M4 must beat.

Two things that look like the fix but are not, confirmed from the code rather than assumed:

- **`A_ref` re-estimation** (one-shot rescale in `manta-decode::envelope`) is decision-neutral for keying: it scales `a_ref`, `e_hi`, `e_lo`, and `t` by the same factor, and every downstream decision is a ratio of those.
- **`BeamConfig::sigma`** cannot change which glyph is emitted: every hypothesis surviving to a given beam truncation step has consumed the same number of marks, so `sigma` enters the log-likelihood as a common positive factor across all of them and at the final argmax. It only affects `confidence` (and does so more than expected — widening beam *width* under low channel quality also lowers reported confidence, by enlarging the softmax denominator over more post-truncation survivors).
