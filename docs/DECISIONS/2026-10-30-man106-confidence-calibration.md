# MAN-106: spot confidence is a committed empirical calibration; zero-evidence spots abstain

Date: 2026-10-30. Ticket: MAN-106. Implements the SPEC-decode-core §4.6
amendment and the §4.5 reversal recorded in
[the q-input record](2026-10-30-man106-q-snr-input.md). Scope widened by
Tony Hagale's 2026-09-10 comment folding in investigation 11 of the
[decoder-recall research](2026-09-09-decoder-recall-research.md)
(separate the four quantities; held-out calibration discipline; explicit
emission states; the zero-evidence regression).

## What changed

1. `Spot.confidence` (`decodeConfidence`) is a calibrated probability:
   `geo_mean · (1 − 0.5^r)^rep_scale · tier_factor[tier]` mapped through
   the committed monotone piecewise-linear calibration
   (`manta-spot/src/confidence/calibration.rs`), SCP boost ×1.15 capped
   at 1.0 on top. Constants, measurement CSVs and the fit script are
   committed; `python3 -m unittest scripts.tests.test_fit_calibration`
   pins that the script reproduces the committed constants byte-for-byte
   from the committed CSVs.
2. `q` reads the engine-supplied floor-based SNR (see the q-input
   record) — the measured saturation root cause.
3. A candidate whose characters all carry zero decoder confidence is
   never emitted: an abstention, recorded in the decision stream with
   reason `NoEvidence`. The gate is on evidence, not a score threshold
   (investigation 11: no arbitrary high cutoff). Reproduced pre-fix at
   exactly 1.028e-7 confidence (EPSILON geo-mean × 0.75 × 1.15 SCP).
4. Every published spot carries `emissionState`: `confirmed` when the
   calibrated probability ≥ `PUBLISH_CONFIRMED_THRESHOLD` = 0.5, else
   `tentative` (both publish; reviewer question 2's default). CLI JSON
   only; the :7301 spot stream does not carry it (question 1's default).
5. `manta bench sensitivity` reports, per point, a `confidence` block
   (true/bogus medians, separation, 10-bin reliability, ECE, Brier, log
   loss, risk-vs-coverage, and the labeled fit rows) — report
   `format_version` 2, additive. `--reliability` prints the human table
   to stderr. Bogus population = bench truth: any emitted spot or
   withheld validator candidate whose call no station keyed (the
   hard negatives investigation 11 asked for).
6. `Decision` carries `confidence` (the raw `c_call` evidence) on every
   stage, and offline decodes record decisions (`DecodeReport.decisions`,
   serde-skipped: the CLI JSON surface is unchanged).

## The fit

- Instrument: bench `--json` decision rows, labeled by the scene's
  stations. Recipe (deterministic seeds): `manta bench sensitivity
  --conditions awgn,good,poor --wpm 25 --snr-db 0,6,12,18 --duration-s
  30 --trials 2 --json`; 24 decodes, 130 labeled rows (84 true, 46
  bogus). Split rule: alternating rows → 65 fit / 65 held-out
  (`fit-confidence-calibration.py extract`).
- Model: joint, not stacked — repetition exponent and tier factors
  re-fitted in log space (grids 0.0–2.0 × 0.70–1.30 × 0.50–1.20) under a
  PAVA isotonic map over the joint raw score, reduced to 8 knots
  anchored at (0, 0), with isotonic shrinkage toward the identity
  (α on 0.50–1.00) so the published map keeps graded probabilities
  instead of a hard step.
- Committed constants: `rep_scale` 2.0, tier factors `[1.0, 0.7, 0.5]`,
  8 knots with the soft step at joint ≈ 0.169, shrink α 0.95.
- Quality: fit Brier 0.0269; **held-out ECE 0.0331 (bar: ≤ 0.20)**,
  held-out Brier 0.0305 (asserted in `manta-spot`'s
  `held_out_ece_is_at_most_one_fifth` over the committed held-out CSV).
- Before/after on the same 24-point sweep (after the `q` fix, before vs
  after the calibration): min/mean true-vs-bogus separation
  **−0.074 / 0.045 → 0.955 / 0.959**; mean per-point ECE
  **0.426 → 0.040**.

The soft-step shape is real, not overfitting noise: emission is
conditioned on the repetition gate, so candidates that clear their
tier's rep count with a clean geometric mean are correct with ~0.96
probability in the labeled population, and everything below joint
≈ 0.17 is garble. The map says character quality dominates; repetition
and rarity sharpen the boundary.

## The SPEC-v2 §8.4 stage-3 gate, V8w leg

Automated in `manta-cli`'s `man106_separation_gate` test: render the
canonical V8w pileup, decode through the full pipeline with decisions
recorded, and require `median(emitted spot confidence) −
median(withheld-candidate confidence where no station keyed the call)`
≥ 0.3. The ticket's original CURRENTLY line (V8w emitting bogus spots)
no longer reproduces since MAN-213 — V8w emits zero bogus spots — so
the bogus population is the withheld-candidate hard negatives, exactly
what investigation 11 wanted labeled.

**Conflict recorded:** SPEC-v2 §8.4 words the gate as "on the V8w scene
and on B2/K5TR". The B2/K5TR leg needs the real capture corpus behind
`MANTA_CORPUS_BASE_URL`, a maintainer secret
(`crates/manta-testkit/audio-corpus/README.md`); it stays with MAN-116's
real-RBN calibration work, which MAN-102 D102.3 already designated the
real-data calibration venue. The spec table carries a note saying so.

## Threshold choice

`PUBLISH_CONFIRMED_THRESHOLD` = 0.5, chosen from the instrument's
risk-vs-coverage tables: at 0.5 the observed risk among
confidence-above-threshold spots is ≤ 5% on every point of the fit
sweep (0.00 at 17 of 24 points, ≤ 0.03 overall), while a publish
*suppression* threshold remains deliberately unbuilt (the What We're
Not Doing section of the plan): only the zero-evidence abstention can
suppress a spot.

## What deliberately did not change

- Variant arbitration / the MAN-100 support ledger still sums the raw
  pre-`c_call` `geo_mean` — recalibration cannot change which spots win
  arbitration.
- No golden vector pins an absolute confidence (verified), so no golden
  expectation moved; golden V8/V8w recall and arbitration gates are
  byte-identical in spot sets (the only new suppression is zero-evidence
  abstention, which no committed fixture contains — the repro needed a
  hand-built event stream).
- Telnet :7300 and the RBN uplink never carried confidence; still don't.
  The :7301 spot stream does not gain `emissionState` (reviewer
  question 1's default: wait for a consumer).
- Mixed-`?` words (≈0.03 raw confidence) are handled by calibration —
  they land near zero, `tentative` — not by a new gate that would
  re-litigate the MAN-272/MAN-100 exemption structure.
