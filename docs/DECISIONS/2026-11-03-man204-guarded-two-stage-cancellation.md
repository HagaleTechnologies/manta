# MAN-204: guarded two-stage cancellation recovers a weak CW station beside a strong one (offline experiment)

## Problem

When a weak CW station sits within one 93.75 Hz channel of a much stronger
one, manta builds one track, decodes the strong station and never decodes
the weak one. MAN-168's narrowband refiner and MAN-188's guard-band noise
reference only centre or discount a track's own channel; neither fits or
removes a specific interferer. Source: investigation 14 of
[the decoder-recall research](2026-09-09-decoder-recall-research.md), the
lowest-confidence, highest-effort item of the ranked set.

This record covers the ticket's first stage: a bounded, offline two-stage
experiment. The full joint two-emitter decoder is a follow-up whose go/no-go
reads this record.

## Decision

The experiment is `manta_engine::cancel::decode_with_cancellation`, with its
signal processing in `manta_dsp::cancel`. It runs on an in-memory buffer and
nothing in `decode`, `run`, `listen`, telnet, JSON, `/metrics` or the RBN
uplink calls it.

- **D1. Stage 1 only, offline, Rust-only, off by default.** No config key,
  CLI flag or spot field.
- **D2. Residual identities never reach public output.**
  `CancellationReport::original` is `decode_samples`' result, unchanged
  (`original_branch_is_identical_to_decode_samples`). The residual is decoded
  by a separate `decode_samples` call, so it gets its own validator. No public
  identity can rest on an artifact of the subtraction.
- **D3. Carrier: spot frequency, then one refinement.** The hint is the
  strong spot's `freq_hz / calibration_factor - centre`. The fits' weighted
  mean phase slope (weight = interior length × |gain|) moves the carrier once;
  edges and fits are then redone. A step over `MAX_CARRIER_STEP_HZ` (5 Hz)
  skips the candidate (`UnstableCarrier`).
- **D4. Edges come from the raw IQ.** Derotate to the carrier (f64 phase),
  take a centred `EDGE_WINDOW_S` (2.667 ms) moving average every window/8
  samples, and take half-amplitude crossings against 0.5 × the 98th
  percentile. No delay constant, no channel-alignment dependence, no
  alternation loop.
- **D5. Evidence gate in the original buffer.** In the strong station's key-up
  gap blocks: `OneSignal` if the hot fraction `h < 0.15`; `NonCw` if
  `h > 0.75`; `TwoSignalCw { offset_hz }` if `0.15 ≤ h ≤ 0.75`, coherence
  ≥ 0.8 and 10 ≤ |Δf| ≤ 150 Hz; `Unresolved` otherwise. Only `TwoSignalCw`
  is subtracted while `CancelOptions::evidence_gate` is on (the default).
  The lag-1 phase is ambiguous modulo the block rate (375 Hz at 96 kS/s), so
  the offset is the one of `Δf` and `Δf ± 375 Hz` whose derotated hot blocks
  hold the most power: a neighbour 260 Hz away reads +257.8 Hz, not -115 Hz.
  Every element the edge finder found, fitted or not, is kept out of the gap
  blocks, so a strong element too short to fit cannot read as a neighbour.
- **D6. Residual check before subtraction, relative to the neighbour.**
  Computed blockwise from the fits, without building the residual. The
  neighbour level is the 90th percentile of gap-block residual power. A
  candidate is `Clean` when the median edge block (centre within
  `EDGE_ZONE_S` = 2.5 ms of an edge) is ≤ -7 dB and the median interior block
  (more than 10 ms inside an element) is ≤ -10 dB under that level. A `Dirty`
  candidate is not subtracted.
- **D7. Admission.** A residual spot is `recovered` only within 15 Hz of a
  subtracted candidate's evidence frequency (`carrier + Δf`), with a call that
  is not the candidate's and not among the original spots. Otherwise it is
  `rejected` as `OutsideNeighbourhood`, `DuplicateOfStrong` or
  `DuplicateOfOriginal`. A call is recovered once, from its first admitted
  spot.
- **D8. A typed `NoSignal` error.** `decode_source`'s three "no signal found"
  bails return `manta_engine::NoSignal`, whose `Display` text is the old
  message byte for byte. The experiment downcasts it, so an empty residual is
  an empty branch and every other error propagates.
- **D9. Bounded work.** Candidates are the original CW spots grouped by track,
  ordered by best SNR descending then track id, capped at 8. A candidate
  whose refined carrier lies within 10 Hz of one already subtracted is
  skipped (`DuplicateCarrier`): the same station spotted on two tracks is
  subtracted once. `CancelOptions::carrier_hint_override_hz` applies to the
  first candidate only. All `Clean` subtractions go into one residual copy,
  decoded once.
- **D10. Acceptance floor.** At least 7 of B's 9 calls on seeds 204, 2 and 3.
  The baseline is 0, so the research doc's relative-recall metric is
  undefined in the reference cell.
- **D11. Determinism.** See below.

### Constants

| constant | value | where |
|---|---|---|
| `EDGE_WINDOW_S` | 256 / 96000 s (2.667 ms) | `crates/manta-dsp/src/cancel.rs` |
| `RISE_S` | 5 ms | `crates/manta-dsp/src/cancel.rs` |
| `EDGE_ZONE_S` | 2.5 ms (`RISE_S / 2`) | `crates/manta-dsp/src/cancel.rs` |
| `MIN_ELEMENTS` | 20 | `crates/manta-dsp/src/cancel.rs` |
| `MAX_CARRIER_STEP_HZ` | 5 Hz | `crates/manta-dsp/src/cancel.rs` |
| `MIN_INTERIOR_S` | 10 ms | `crates/manta-dsp/src/cancel.rs` |
| `SLOPE_BLOCK_S` | 0.5 ms | `crates/manta-dsp/src/cancel.rs` |
| `RIM_S` | 0.2 s | `crates/manta-dsp/src/cancel.rs` |
| `EDGE_PERCENTILE` | 0.98 | `crates/manta-dsp/src/cancel.rs` |
| `CLEAN_EDGE_OVER_NEIGHBOUR_DB` | -7 dB | `crates/manta-dsp/src/cancel.rs` |
| `CLEAN_INTERIOR_OVER_NEIGHBOUR_DB` | -10 dB | `crates/manta-dsp/src/cancel.rs` |
| `NEIGHBOUR_PERCENTILE` | 0.90 | `crates/manta-dsp/src/cancel.rs` |
| `ONE_SIGNAL_MAX_HOT` | 0.15 | `crates/manta-dsp/src/cancel.rs` |
| `NON_CW_MIN_HOT` | 0.75 | `crates/manta-dsp/src/cancel.rs` |
| `MIN_COHERENCE` | 0.8 | `crates/manta-dsp/src/cancel.rs` |
| `MIN_OFFSET_HZ` / `MAX_OFFSET_HZ` | 10 / 150 Hz | `crates/manta-dsp/src/cancel.rs` |
| `HOT_FACTOR` / `PAIR_FACTOR` | 4 (6 dB) / 10 (10 dB) | `crates/manta-dsp/src/cancel.rs` |
| `REFERENCE_OFFSET_HZ` | 500 Hz | `crates/manta-dsp/src/cancel.rs` |
| `ADMIT_RADIUS_HZ` | 15 Hz | `crates/manta-engine/src/cancel.rs` |
| `CancelOptions::max_candidates` | 8 | `crates/manta-engine/src/cancel.rs` |

### Calibration against the plan

The plan proposed a ±5 ms edge zone with a -5 dB edge threshold and a -4 dB
interior threshold. Two measurements moved them:

- **Edge zone ±2.5 ms, threshold -7 dB.** With a ±5 ms zone, the median edge
  block sits outside the 5 ms ramp, where an edge shift leaves nothing: a
  0.67 ms shift read -6.6 dB on the module's synthetic pair and passed. With
  the ramp-only zone, the synthetic pair (seeds 204, 2, 3) reads -14.7 to
  -15.3 dB clean, -5.8 to -6.3 dB at 0.4 ms jitter (a cell that lost B in the
  plan's table 7) and +3.5 dB or more at a 0.67 ms shift. Rule A puts 0.4 ms
  jitter on the dirty side, so the threshold is -7 dB.
- **Interior threshold -10 dB.** The sweep's degraded table (below) showed a
  3 % gain error reading Clean at -8.3 to -8.4 dB while the forced residual
  decoded 2, 0 and 0 of B's 9 calls. Rule A (every row that loses B is
  `Dirty`) needs a threshold under -8.4 dB. Every row that kept 8 or 9 calls
  read -13.4 dB or lower. -10 dB sits between them with about 1.6 dB of
  margin to the bad side and 3.4 dB to the good side. A second sweep run at
  the final thresholds printed the 3 % gain rows `Dirty` and changed no other
  verdict.

Rule B (rows with 8 or more calls and no stray spot should be `Clean`) loses
two kinds of row, accepted because a false accept costs more: a 0.33 ms
constant edge shift (9 of 9, edge -1.7 dB) and 0.2 ms per-edge jitter (7 or
8 of 9, edge -5.0 to -5.2 dB).

## Evidence

All scenes use the research's reference geometry: 96 kS/s, centre
14.030 MHz, 60 s; station A sends "CQ TEST DE W1AW W1AW K" at 25 WPM, B sends
"CQ TEST DE K5ARH K5ARH K" at 20 WPM (9 calls in 60 s). Each station and the
noise are rendered separately by `manta_testkit::scene::render_scene` and
summed in a fixed order. Recall counts B's calls in the decoded text of every
track. `PipelineConfig::default()` (the `legacy` engine). Reproduce with
`cargo test -p manta-engine --test man204_cancellation_sweep --locked -- --ignored --nocapture`
(3 to 4 minutes on 32 cores in the dev profile). The tables below are from
the run at the final constants.

### Acceptance (`crates/manta-engine/tests/man204_cancellation.rs`, CI-gated)

| test | result |
|---|---|
| reference pair, seeds 204 / 2 / 3 (A +30 dB on channel 107's centre, B +10 dB 40 Hz above) | original 0 of 9 B calls; residual 9 / 8 / 8; `recovered` = `K5ARH` |
| single-signal control (A alone) | `OneSignal`, no residual, nothing recovered |
| control, forced subtraction with both gates bypassed | residual decode spots nothing |
| B 40 Hz below A | `K5ARH` recovered |
| A mid-channel (+46.875 Hz), B 40 Hz above | `K5ARH` recovered |
| steady carrier 40 Hz above A (non-CW filter off) | `NonCw`, nothing subtracted |
| carrier hint forced 3 Hz off | refined within 0.1 Hz; `K5ARH` recovered |
| original branch vs `decode_samples` | identical JSON and spots |

### Reference cell on six seeds (gate on)

| seed | 204 | 2 | 3 | 5 | 6 | 7 |
|---|---|---|---|---|---|---|
| B calls, original → residual | 0 → 9 | 0 → 8 | 0 → 8 | 0 → 8 | 0 → 9 | 0 → 9 |
| evidence offset (Hz) | +39.9 | +39.9 | +39.9 | +39.8 | +39.9 | +39.9 |
| refined carrier error (Hz) | -0.004 | -0.004 | -0.004 | -0.003 | -0.005 | -0.004 |
| edge / interior over neighbour (dB) | -8.6 / -15.2 | -8.5 / -15.4 | -8.4 / -15.4 | -8.4 / -15.6 | -8.4 / -15.4 | -8.5 / -15.4 |

### Single-signal controls

A alone at +20, +30 and +40 dB, on-bin and mid-channel, seeds 204, 2 and 3,
gate on and off (36 runs): every one classifies `OneSignal` (hot fraction
0.056 to 0.066, coherence 0.00), recovers nothing and spots nothing in a
residual. With the gate off the check reads -4.6 to -5.1 dB (edge) and -5.1
to -5.4 dB (interior), so it refuses the subtraction on its own as well.

### Strata (seed 204, B +10 dB)

B's calls in the residual (the original branch decoded 0 in every cell).
"no cand." means the original branch spotted no strong station, so there was
nothing to cancel.

| A | difference | -80 Hz | -40 Hz | +20 Hz | +30 Hz | +40 Hz | +60 Hz | +80 Hz | +120 Hz |
|---|---|---|---|---|---|---|---|---|---|
| on-bin | 10 dB | no cand. | 9 | no cand. | no cand. | 9 | 9 | no cand. | no cand. |
| on-bin | 20 dB | 9 | 9 | 2 | 8 | 9 | 9 | 9 | 9 |
| on-bin | 30 dB | 9 | 9 | 2 | 9 | 9 | 9 | 9 | 9 |
| mid-channel | 10 dB | no cand. | no cand. | no cand. | no cand. | no cand. | no cand. | no cand. | no cand. |
| mid-channel | 20 dB | 9 | 9 | 6 | 9 | 9 | 9 | 9 | 9 |
| mid-channel | 30 dB | 9 | 9 | 6 | 9 | 9 | 9 | 9 | 9 |

Every cell with a candidate classified `TwoSignalCw` with the offset within
0.5 Hz of the truth and coherence 0.99, and every one was `Clean` (edge -7.5
to -8.8 dB, interior -14.2 to -15.8 dB). Every recovered identity was
`K5ARH`, within 3.6 Hz of B's true frequency except one cell (on-bin, 10 dB,
+40 Hz: 10.3 Hz, inside the 15 Hz admission radius). The on-bin 20 dB +30 Hz
cell's residual spotted `K5ARH` twice; `recovered` lists it once. No cell recovered or
rejected any other call. The refined carrier error was at most 0.06 Hz.

The research doc's metric over its predeclared stratum (A on-bin or
mid-channel, B 30 to 120 Hz either side, 10 to 30 dB weaker, AWGN, constant
keying; here the -80, -40, +30, +40, +60, +80 and +120 Hz columns): at 20 and
30 dB all 28 cells hold a candidate, and B's recall rises from 0 to 9 of 9 in
27 of them and to 8 of 9 in one. At 10 dB the 3 cells with a candidate reach
9 of 9 and the other 11 have none and stay at 0. Outside the stratum, 20 Hz
separation reaches 2 of 9 on-bin and 6 of 9 mid-channel.

### Weak B (+40 Hz, seed 204)

| B | hypothesis | h | coherence | edge / interior (dB) | B calls |
|---|---|---|---|---|---|
| 0 dB | `TwoSignalCw` +40.2 Hz | 0.417 | 0.94 | -6.8 / -7.8 | 0 (`Dirty`, not subtracted) |
| -5 dB | `TwoSignalCw` +31.6 Hz | 0.238 | 0.86 | -5.7 / -6.1 | 0 (`Dirty`, not subtracted) |

### Degraded reconstructions (forced subtraction, seeds 204 / 2 / 3)

Each row perturbs `estimate_strong`'s output for the reference pair or its
single-signal control, then subtracts and decodes regardless of the verdict.
Verdicts are at the final thresholds.

| degradation | pair edge / interior (dB) | pair B calls | control residual | verdict |
|---|---|---|---|---|
| none | -8.4 to -8.6 / -15.2 to -15.4 | 9 / 8 / 8 | nothing | pair `Clean`, control `Dirty` |
| gain × 1.01 | -9.1 to -9.4 / -13.4 to -13.5 | 9 / 9 / 9 | nothing | pair `Clean` |
| gain × 1.03 | -10.0 to -10.2 / -8.3 to -8.4 | 2 / 0 / 0 | a `W1AW` spot | `Dirty` |
| gain × 1.10 | -6.0 / -0.1 to -0.2 | 0 | a `W1AW` spot | `Dirty` |
| phase 3° | -5.9 to -6.2 / -4.9 to -5.1 | 0 | a `W1AW` spot | `Dirty` |
| phase 10° | -0.6 to -0.9 / +4.5 | 0 | a `W1AW` spot | `Dirty` |
| shift 0.33 ms | -1.7 / -15.2 to -15.4 | 9 / 9 / 9 | nothing | `Dirty` (rule B loss) |
| shift 0.67 ms | +2.0 / -15.2 to -15.4 | 1 / 0 / 1 | at most one track, no spot | `Dirty` |
| shift 1.33 ms | +9.3 / -15.2 to -15.4 | 0 | one track, no spot | `Dirty` |
| jitter 0.2 ms | -5.0 to -5.2 / -15.2 to -15.4 | 7 / 8 / 8 | nothing | `Dirty` (rule B loss) |
| jitter 0.4 ms | -2.6 to -2.7 / -15.2 to -15.4 | 3 / 5 / 5 | nothing | `Dirty` |
| jitter 0.8 ms | 0.0 to +0.3 / -15.2 to -15.5 | 1 | one track, no spot | `Dirty` |
| carrier 0.2 Hz, no refinement | -3.7 to -3.9 / -0.5 to -0.6 | 0 | one track, no spot | `Dirty` |
| carrier 1 Hz, no refinement | +1.9 to +2.0 / +6.2 | 0 | one track, no spot | `Dirty` |
| carrier 3 Hz, no refinement | +9.7 / +13.9 to +14.0 | 0 | one track, no spot | `Dirty` |

Rule A holds: every row that left a stray spot or fewer than 7 of B's calls is
`Dirty`, and so is every control row. The control rows with a `W1AW` spot are
the strong station's own remainder, which D7 would reject as
`DuplicateOfStrong` or `DuplicateOfOriginal` even if a subtraction got that
far.

### Plan-phase prototype measurements (at `212fa51`, before this code)

The plan measured a scratch prototype on the same geometry. Its tables 1 to
7, condensed:

| table | measurement | result |
|---|---|---|
| 1 | baseline, carrier hint from the original `W1AW` spot | original 0 of 9 B calls on seeds 204, 2, 3; hint error +0.33 Hz; residual 9 of 9; A alone: nothing |
| 2 | one slope-refinement pass | a +3 Hz hint lands within 0.01 Hz; B 9 of 9; A alone with +3 Hz: no garbage track |
| 3 | edges from raw IQ vs the channelizer | edge sd 0.224 vs 0.377 ms on-bin, 0.224 vs 4.174 ms mid-channel; mid-channel B 9 of 9 vs 0; an alternation pass gains 0.01 ms and no recall |
| 4 | strata with refinement | -40, +60, +80 Hz and 10 or 30 dB apart: 9 of 9 |
| 5 | a noise-relative ±10 ms residual check | catches a 3 Hz carrier error (+14 to +19 dB) and mid-channel channel edges, but misses a 1.33 ms edge shift (+3.1 dB) that grows a garbage track; superseded |
| 6 | gap occupancy in the original buffer | one signal 5.9 to 6.6 % hot; keyed neighbour 23.8 to 45.7 %, offset within 0.4 Hz, coherence 0.86 to 0.99; steady carrier 86.7 to 100 % |
| 7 | neighbour-relative check, forced subtractions | cells that kept B read -6.5 dB or lower (edge) and -7.8 dB or lower (interior); cells that lost B -3.8 dB or higher (edge) or -1.0 dB (interior) |

### Unit evidence (`crates/manta-dsp/src/cancel.rs`)

Twenty-one unit tests on synthetic tones pin the estimator: edges within
0.02 ms of the truth noiseless at 48, 96 and 192 kS/s and at any channel
alignment, edge rms at most 0.30 ms beside a 20 dB weaker neighbour, a 3 Hz
hint refined to within 0.1 Hz, a clean tone subtracted to -40 dB or lower
(also over the last second of 60 s at +45 kHz, the f32-phase regression), the
check's verdicts on carrier, shift, jitter and rim cases, the classifier's
boundaries, a 260 Hz neighbour that must not alias into range, and found but
unfitted elements that must not read as gaps.

## Determinism

`manta_dsp::cancel` has no RNG and no clock outside its tests. Every sum over
samples accumulates in `f64` or `Complex64`, sequentially. Every phase
argument is reduced with `rem_euclid` in `f64` before narrowing; the research
prototype's f32 phase put `W1AW` ghosts 14 to 34 kHz away. Candidates run in
the fixed D9 order on one thread, and every list is a sorted `Vec`. The sweep
runs its cells on scoped threads but sorts its rows by job order before
printing.

## Known limits

- **No candidate without a strong spot.** Candidates come only from the
  original branch's spots. With A only 10 dB above B, the original branch
  often spots nothing (13 of 16 strata cells at 10 dB), so there is nothing to
  cancel.
- **20 Hz separation:** 2 of 9 on-bin, 6 of 9 mid-channel.
- **Weak stations near 0 dB and below:** the check refuses them (0 dB reads
  -6.8 / -7.8 dB); the plan's mode-1 prototype recovered 9 of 9 at 0 dB with
  a looser check.
- **Watterson fading, chirp and real rise times** are not modelled; the rise
  is fixed at 5 ms.
- **Asymmetric keying:** a constant edge bias of 0.33 ms is rejected by the
  check although B would decode (rule B loss).
- **Two weak callers or three emitters** are not modelled.
- **The `hsmm` engine** is not tried on residuals.
- **The live path:** no raw-IQ ring in `listen`, no CPU work on a Pi 4.
- **The ±500 Hz evidence reference** assumes an empty neighbourhood there; a
  crowded band raises it and lowers the hot fraction.
- **Memory** peaks at twice the input (one residual copy).
- **No real recording** was at hand. `man204_recording_probe` (ignored; set
  `MANTA_MAN204_RECORDING` to a MAN-309 `cf32` recording) prints the report
  for one.

## Follow-ups (for the owner to file)

1. **MAN-313's `non_cw_filter` discards a strong keyed station** when a steady
   carrier 20 to 30 dB weaker sits 40 to 60 Hz away: in the reference
   geometry with an unkeyed carrier at +40 Hz and +10 dB, the default pipeline
   decodes nothing, and with `detector.non_cw_filter = false` it spots `W1AW`.
   `steady_carrier_beside_a_strong_station_is_not_cancelled` runs with the
   filter off for this reason.
2. **Promote recovered identities to public output**, with a corroboration
   rule and the research doc's ≥ 20 % relative-recall metric in a
   predeclared stratum on held-out seeds plus at least one real recording.
3. **The joint two-emitter model (stage 2)**: per-emitter carrier, drift,
   gain, pulse shape and key-state histories, gated on this record's limits.
4. **A weak-station assertion in VR5.**
5. **A live path**: a raw-IQ ring and the CPU budget.
