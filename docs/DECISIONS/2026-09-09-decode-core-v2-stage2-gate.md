# decode-core-v2 stage-2 gate (Hsmm engine) -- measured, FAIL

SPEC v2 (`docs/SPEC-decode-core-v2.md`) §8.4 Task 12
(`docs/superpowers/plans/2026-09-09-decode-core-v2.md`). This is the
measurement task for the redesign's stage-2 gate: does the fully-
implemented, reviewed `Engine::Hsmm` (Tasks 1-8, CLI-reachable since Task
11) actually clear the bars SPEC v2 §8.4 sets for it. It is not a tuning
task -- Step 3 below records the bounded, honest exception to that.

**Measured against commit `70da36836ad949b58da499458abe87b7131d3364`**
(Task 11's bench/determinism commit, `HEAD` at the start of this task), on
2026-09-09, real/synthetic corpora as specified below.

**Verdict: the stage-2 gate FAILS.** Hsmm is a large, real improvement
over `legacy` on the oracle metric and on RBN recall, but does not clear
either of SPEC v2 §8.4's two oracle thresholds, and the majority of
VR1-VR8 and V1-V10 fail outright -- several catastrophically (three VR
vectors decode to nothing at all; V8w decodes 0/34 strong pileup signals).

---

## 1. Oracle (SPEC v2 §8.3), B2/K5TR real recording, 221 windows

**Corpus**: `B2_20251129_000000_7080kHz.wav` (`crates/manta-testkit/audio-corpus/`,
git-ignored, 690 MB, ~15 minutes, 192 kS/s stereo IQ, 2025-11-29 CQ WW CW
contest capture centered near 7.080 MHz) with its `.json` sidecar, scored
against `ground-truth/B2_20251129_000000_7080kHz.rbn.csv` (26,803 lines
including a header row -- RBN's raw daily-dump format for this
recording's window).

`cargo run --release -p manta-cli -- oracle <B2 wav> <B2 rbn csv> --engine
<engine> --jsonl <out>`. `n` = 221 both engines (unchanged fixture set).

| metric | gate (§8.4 stage 2) | legacy | hsmm | hsmm meets gate? |
|---|---|---|---|---|
| as_word | >= 60 % | 59/221 = **26.7 %** | 124/221 = **56.1 %** | **NO** (3.9 pts short) |
| framed | >= 40 % | 29/221 = **13.1 %** | 70/221 = **31.7 %** | **NO** (8.3 pts short) |
| substring | (not gated) | 106/221 = 48.0 % | 130/221 = 58.8 % | -- |
| wpm_ratio (median) | (not gated) | 0.8231 | 1.1797 | -- |

Legacy's `as_word 26.7 %, framed 13.1 %, substring 48.0 %, wpm_ratio
0.823` matches SPEC v2 §8.3's recorded 2026-09-09 baseline (`as_word 27
%, framed 13 %, substring 48 %, wpm_ratio 0.82`) almost exactly -- the
oracle harness is reproducing its own documented reference point, a
sanity check that this measurement is comparing like for like.

By-SNR breakdown (`n`, `as_word`, `framed`):

| SNR bucket | legacy n/as_word/framed | hsmm n/as_word/framed |
|---|---|---|
| < 15 dB | 29 / 4 / 2 | 29 / 7 / 5 |
| 15-25 dB | 99 / 25 / 16 | 99 / 57 / 35 |
| >= 25 dB | 93 / 30 / 11 | 93 / 60 / 30 |

Hsmm roughly doubles `as_word` in every SNR bucket, and more than doubles
`framed` in the two higher buckets -- a real, substantial gain, just not
enough to clear the raised stage-2 bar (60 %/40 %, vs stage-1's 45 %/no
`framed` gate).

**Oracle sub-gate: FAIL** (both `as_word` and `framed` miss).

## 2. RBN spot recall/precision, B2 recording, full 15-minute decode

`cargo run --release -p manta-cli -- decode --json --engine <engine> <B2
wav>` -- legacy: 743.8 s real / 634.8 s user / 88.5 s sys; hsmm: 889.9 s
real / 2501.6 s user / 87.5 s sys. Hsmm uses ~1.2x the wall time
(889.9/743.8) and ~3.9x the CPU time (2501.6/634.8) of legacy on this
~15-minute recording. Hsmm's own user-time/real-time ratio here
(2501.6/889.9 = ~2.8x) shows it parallelizing across multiple cores; this
is not a comparison to legacy. Both the elevated CPU time and the
parallelism are consistent with the CPU-budget overage measured in §6
below. Scored with `scripts/score-against-rbn.py --capture-start
2025-11-29T00:00:00Z --sample-rate-hz 192000`.

**This branch's own `scripts/score-against-rbn.py` is currently a stub**
(commit `2357a80`, "score RBN benchmark against a single spotter") that
only loads and filters the truth CSV -- it never reads a decode report or
computes recall/precision at all. The real 230-line scorer (callsign +
frequency + time-window matching, callsign-grammar-based truth
exclusion) landed on `main` in PR #144 (`02863cd`,
"feat(testkit): add RBN ground-truth benchmark ... (#144)") *before* this
branch's stub commit, but this branch forked before that and never
picked it up -- the two commits independently created the same file path
with unrelated content. This is a real branch-hygiene gap worth a
fast-follow (rebase/merge `main`'s scorer into this branch so
`scripts/score-against-rbn.py` stops being two different tools depending
on which branch you're on); it is out of this task's file scope to fix
(not in Task 12's Files list). To get the numbers this task needs, `main`
HEAD's `scripts/score-against-rbn.py` (`02863cd`) was run locally,
uncommitted, against both report JSONs -- it does not need `--spotter`;
K5TR-only is obtained by pre-filtering the truth CSV to `callsign ==
K5TR` rows before scoring (274 rows, 2 excluded by grammar -> 209 unique
call+kHz bins, vs 1,441 all-RBN bins after 126 grammar-excluded).

| metric | legacy | hsmm |
|---|---|---|
| manta spots emitted | 197 | 501 |
| **K5TR-only recall** | 28/209 = **13.40 %** | 52/209 = **24.88 %** |
| **K5TR-only precision (lower bound)** | 28/197 = **14.21 %** | 52/501 = **10.38 %** |
| **all-RBN recall** | 59/1441 = **4.09 %** | 125/1441 = **8.68 %** |
| **all-RBN precision** | 59/197 = **29.95 %** | 125/501 = **24.95 %** |

"K5TR-only precision" is a **lower bound, not true precision**: it
divides K5TR-attributed true positives by the *total* spot count (197 /
501), not by K5TR's own true+false positives -- so a correctly-decoded
spot that K5TR simply never heard (e.g. a station too weak for K5TR's
own antenna/receiver, or on a band K5TR wasn't skimming that moment)
counts as a false positive here even though it isn't one against the
full RBN set. All-RBN precision above is the better-founded figure, though
still bounded below by RBN's own truth incompleteness -- a correct decode
no spotter happened to hear also counts as a false positive there.

Legacy's all-RBN 29.9 % precision / 4.1 % recall matches this project's
prior historical measurement (`docs/DECISIONS/2026-09-09-decoder-recall-research.md`
line 11, "Baseline: 29.9% precision, 4.1% recall" on `fix/man-166-detector-timing-v2`)
almost exactly -- another cross-check that this measurement is sound.

**Honest read**: hsmm roughly doubles recall against K5TR and against
all-RBN (13.4->24.9 %, 4.1->8.7 %), consistent with its much stronger
oracle numbers -- but it does so by emitting 2.5x as many spots (501 vs
197), and precision drops accordingly (K5TR: 14.2->10.4 %; all-RBN:
30.0->25.0 %). This is not gated by SPEC v2 §8.4 (M3's parity benchmark,
ROADMAP.md, is the formal recall/precision gate; §8.4 only gates the
oracle/V/VR/determinism/bench numbers above and below), but it's the
real side-by-side comparison Task 12 asked for: hsmm finds more real
signals at the cost of more false spots, at a CPU cost §6 already shows
is 4.25x over budget.

## 3. V1-V10 (SPEC v1 §7) with `--engine hsmm`

SPEC v2 §8.1 requires hsmm to clear V1-V10 including V2, V5, V6, V8w --
the three vectors (V2, V5, V6) and V8w that stay permanently `#[ignore]`d
under `legacy` for unrelated, already-filed bugs are explicitly IN SCOPE
here, not exempted. New `_with_hsmm_engine` test functions added to
`crates/manta-cli/tests/golden_v1.rs`, `golden_v2_v3.rs`,
`golden_v7_v9_v10.rs`, `golden_v8_v8w.rs` at the same SPEC §7 bars as
each vector's existing legacy copy, with one exception: V1's hsmm copy
checks only CER and freq error (not the legacy copy's additional
single-track invariant, nor its non-gated "free bonus" WPM check) --
see that test's doc comment.

| vector | gate | measured (hsmm) | pass? |
|---|---|---|---|
| V1 | CER < 0.02 | CER 0.0412 | **FAIL** |
| V2 | CER <= 0.01, wpm 35+/-2 | CER 0.0244 | **FAIL** |
| V3 | CER <= 0.05 | CER 0.0270 | pass |
| V4 | CER <= 0.05 | CER 0.0210 | pass |
| V5 | CER <= 0.20, ZL2XYZ validated <= 90 s | CER 1.375 (over-generated/garbled decode) | **FAIL** |
| V6 | CER <= 0.10, survives past midpoint | CER 0.0794 | pass |
| V7 | 2 tracks, each CER <= 0.05, freq err <= 15 Hz | signal 0 passes; signal 1 CER 0.0540 (need <= 0.05) | **FAIL** (0.004 over) |
| V8 | >= 45/50 validated, 0 bogus | 40/50 validated, 0 bogus | **FAIL** |
| V8w | >= 90 % of >= +6 dB signals at CER < 0.10 (>= 31/34) | **0/34 (0.0 %)** | **FAIL** |
| V9 | 1 track, CER <= 0.10, freq err <= 15 Hz | CER 0.0250, 1 track | pass |
| V10 | CER <= 0.05, word count in [expected, expected+4] | CER 0.50 (see below) | **FAIL** |

**4/11 pass (V3, V4, V6, V9); 7/11 fail (V1, V2, V5, V7, V8, V8w, V10).**

V10's CER 0.50 is not a character-accuracy failure in the usual sense:
the decoded text is every expected letter, correctly ordered, with a
space inserted between every single character (`"E Q D E G 4 X X X G 4
X X X K ..."` against expected `"CQ CQ DE G4XXX G4XXX K ..."`) -- a pure
word/char-gap misclassification (every inter-character gap read as an
inter-word gap) on this Farnsworth-spaced vector, not garbled content.
This is the clearest single "word boundary" failure kind in the whole
run (§4 below).

V7's failure is a 0.004 near-miss (CER 0.0540 vs 0.05 gate) on one of two
co-existing signals -- the closest thing to a genuine pass in the fail
column.

## 4. VR1-VR8 "real-conditions" vectors (SPEC v2 §8.2) with `--engine hsmm`

`crates/manta-cli/tests/golden_vr.rs`, `cargo test -p manta-cli --release
--test golden_vr -- --include-ignored` (the brief's literal `cargo test
--workspace --release -- --include-ignored golden_vr` matches 0 tests:
the test function names, e.g. `vr1_contest_40_passes_end_to_end`, don't
contain the substring `golden_vr`, so the filter silently matches
nothing and the whole workspace suite runs with every test filtered out.
Scoping to `-p manta-cli --test golden_vr` and dropping the dead name
filter is the actual command that runs these 9 tests).

| vector | gate | measured CER | measured WPM | measured word-boundary frac | pass? |
|---|---|---|---|---|---|
| VR1 (contest-40) | CER <= 0.05, WPM 40+/-2, word boundary >= 0.95 | **1.0000 (empty decode)** | 12.16 (vs true 40) | 0.0091 | **FAIL** |
| VR2 (contest-45) | CER <= 0.10, WPM 45+/-3 | **1.0000 (empty decode)** | 47.12 (would pass alone) | 0.0079 | **FAIL** |
| VR3 (deep-keying) | CER <= 0.02, dit 40ms+/-10% | **1.0000 (empty decode)** | 33.54 (dit 35.78ms, itself 4.22ms outside the +/-4ms/40ms bound -- fails the dit check too, not just CER) | 0.0128 | **FAIL** |
| VR4 (hard-edges) | CER <= 0.10 | 0.9972 (1 char decoded of 360) | 32.70 | 0.0274 | **FAIL** |
| VR5 (co-channel) | signal A CER <= 0.10, 0 bogus calls | 0.1299 | -- | -- | **FAIL** (0.030 over) |
| VR6a (weighting 2.6) | CER <= 0.05 | 0.0324 | 29.54 | 1.0000 | **pass** |
| VR6b (weighting 3.4) | CER <= 0.05 | 0.0248 | 26.58 | 1.0000 | **pass** |
| VR7 (tight-spacing) | CER <= 0.05, word boundary >= 0.90 | 0.0241 | 34.28 | 0.9857 | **pass** |
| VR8 (qsb-fast) | CER <= 0.15 | 0.6873 | 29.48 | 1.0000 | **FAIL** |

**3/9 pass (VR6a, VR6b, VR7); 6/9 fail (VR1, VR2, VR3, VR4, VR5, VR8).**

### Failure-kind grouping (brief Step 3)

- **Speed / total decode collapse** -- VR1 (40 WPM), VR2 (45 WPM), VR3 (30
  WPM but +45 dB "deep keying," effectively near-noiseless). All three
  decode to a completely empty top-level `text`, despite real
  `CharDecoded` events firing internally (VR1: 100 `CharDecoded` events,
  258 `TrackClosed` events, 4,036 `SpeedUpdate` events over one
  continuous, constant-SNR, single-frequency 120 s signal -- the track is
  being closed and reopened dozens of times a second instead of
  persisting as one track, and the aggregated `wpm` (12.16, next to
  `HsmmConfig::seed_units_hops`'s slowest seed of ~11.8 WPM) suggests the
  speed-tracking hypothesis is converging to the wrong (slowest-seeded)
  speed rather than following the signal's true fast speed).
  **Implicated knob**: `HsmmConfig::speed_alpha` / the token-passing
  speed-hypothesis convergence, not `beam` (see the bounded experiment
  below, which ruled `beam` out).
- **Element split/merge** -- VR4 (hard transmitter edges / click
  sidebands: near-total failure, 1 char of 360 decoded) and V10 (every
  character isolated by a spurious extra word gap, CER 0.50 despite
  perfect character content). **Implicated knob**: `HsmmConfig::
  mark_insert_penalty` (too permissive, allowing extra element
  insertions/gap misclassifications) or `dur_sigma` (duration-tolerance
  width feeding the mark/gap segmentation).
- **Garble under QRM/fading** -- VR5 (co-channel, close miss at 0.030
  over gate), VR8 (fast QSB, CER 0.687), V8w (pileup + fading, 0/34
  strong signals clear CER < 0.10). The V8w number in particular is a
  regression from V8's clean AWGN pass (40/50, no fading) to a complete
  wipeout under Watterson fading -- the same fading-robustness gap
  already tracked for `legacy` (issues #25/#28) appears to still be
  present, or worse, in `hsmm`. **Implicated knob**: none identified with
  confidence within this task's budget; this looks like a genuine
  algorithmic gap (classical fading resilience), not a single scalar to
  retune, consistent with this project's stated M4 ML-fusion boundary.

### One bounded tuning iteration (brief Step 3, "at most one knob per iteration")

**Iteration 1**: `HsmmConfig::beam` 12 -> 24 (doubled token-passing beam
width), rebuilt into an isolated `--target-dir` (not the shared
`target/release` other measurements in this doc were using), re-measured
VR1 only (cheapest, fastest-to-check catastrophic failure).

Result: **no improvement** -- identical `wpm` (12.16), identical
`TrackClosed` (258) and `SpeedUpdate` (4,036) counts, decoded text still
empty (`CharDecoded` dropped 100 -> 62, if anything slightly worse).
Beam width is not the bottleneck for VR1's failure mode. Change reverted
(not committed); `HsmmConfig::beam` stays at its Task-8 default of 12.

No second iteration was attempted (bounded per the brief's guidance;
`speed_alpha`/the anchor-seeding interaction is the next thing to try,
but that's real algorithm work, not a cheap one-line knob flip, and
belongs in a dedicated MAN-166 follow-up ticket, not an open-ended
tuning session inside a measurement task).

## 5. Determinism

`crates/manta-cli/tests/determinism_hsmm.rs` (Task 11): asserts three
runs of `manta decode --json --engine hsmm` over the same input produce
byte-identical output. Already in the workspace's normal (non-ignored)
test suite and green as of `70da368` -- re-verified as part of this
task's pre-commit `cargo test --workspace` run. **Determinism sub-gate:
PASS.**

## 6. CPU budget

`cargo bench -p manta-decode --bench hsmm_300_tracks` (already run in
Task 11, cited here per the brief -- not re-run): **10.625 s mean
[10.620 s, 10.630 s] for 300 tracks x 10 s**, against the SPEC v2 §8.4
budget of <= 2.5 s wall (<= 25 % of real time) -- a **4.25x overage**.
Filed as MAN-171 (not this task's job to fix). The B2 full-decode times
in §2 above (legacy 743.8 s / hsmm 889.9 s wall, ~1.2x; 634.8 s / 2501.6 s
user/CPU, ~3.9x, for the same ~15-minute recording) are consistent with
this being a real, present cost on real audio, not a synthetic-benchmark
artifact. **CPU-budget sub-gate: FAIL** (already established, cited not
re-measured).

## 7. Gate verdict and rationale

SPEC v2 §8.4 stage 2 requires ALL of: oracle `as_word >= 60 %`, `framed
>= 40 %`; V1-V10 + VR1-VR8 pass; determinism; bench within budget.

| sub-gate | result |
|---|---|
| oracle as_word >= 60 % | FAIL (56.1 %) |
| oracle framed >= 40 % | FAIL (31.7 %) |
| V1-V10 all pass | FAIL (4/11) |
| VR1-VR8 all pass | FAIL (3/9, VR6 counted as 2 tests) |
| determinism | pass |
| CPU budget | FAIL (4.25x over, MAN-171) |

**Overall: FAIL.** Five of six sub-gates fail (only determinism passes).
Oracle `as_word`/`framed`, V1-V10, and VR1-VR8 all fail by a wide margin,
not a near-miss; CPU budget also fails, at 4.25x over. Hsmm is a
substantial, real improvement over `legacy` on every headline metric
measured (oracle as_word/framed roughly doubled, RBN recall roughly
doubled), but is not yet ready to be the default engine, and does not
yet clear the specific bars SPEC v2 §8.4 set for promotion.

## 8. Actions taken (brief Step 3, fail path)

- `engine` stays default `legacy` (SPEC v2's `decode.engine` runtime
  switch permanently retains `Legacy`/`EdgeLegacy`/`Hsmm`, per Step 0 --
  not touched by this task, and no case for flipping the default exists
  given the verdict above).
- **Individual tests are un-ignored where individually measured
  passing**, independent of the overall gate verdict (the plan's own
  pre-commit checklist explicitly allows either choice per test, "just
  confirm the whole suite is internally consistent"): `golden_vr.rs`'s
  `vr6a_weighting_2_6_passes_end_to_end`,
  `vr6b_weighting_3_4_passes_end_to_end`,
  `vr7_tight_spacing_passes_end_to_end`; the new hsmm copies
  `v3_passes_end_to_end_with_hsmm_engine`,
  `v4_passes_end_to_end_with_hsmm_engine`,
  `v6_passes_end_to_end_with_hsmm_engine` (`golden_v2_v3.rs`),
  `v9_passes_end_to_end_with_hsmm_engine` (`golden_v7_v9_v10.rs`). All
  other hsmm-engine copies (V1, V2, V5, V7, V8, V8w, V10; VR1-VR5, VR8)
  stay `#[ignore]`d, each citing this doc.
- `README.md`/`AGENTS.md`/`CLAUDE.md` Status section updated with the
  measured facts (default engine unchanged; hsmm implemented, measured,
  not yet promoted) -- see those files' diffs, not restated here.
- `ROADMAP.md` M2's VR1-VR8 entry gets a measured-status line added
  (3/9 pass, gate not yet cleared, citing this doc); its **normative
  status is unchanged** by this task -- VR1-VR8 were already gates per
  the docs PR that shipped this plan, still are, and this task measures
  against them rather than deciding whether they count, per Step 0.
- No MAN-171-style new ticket filed for the correctness gaps in this
  doc: SPEC v2 §8.4/this plan's Task 12 is itself the tracking record
  for "hsmm doesn't clear stage 2 yet"; a follow-up implementation task
  (existing MAN-166 sub-scope, not a new ticket) should target, in
  priority order: (1) the VR1-VR3/V1 "empty decode at high
  speed/high-SNR" collapse (highest severity: total failure on clean
  signals), (2) V10/VR4's element/gap-boundary misclassification, (3)
  the V8w/VR8/VR5 fading-robustness gap. `scripts/score-against-rbn.py`'s
  branch-diverged-stub state (§2) is a separate, small hygiene fix worth
  its own quick follow-up.
