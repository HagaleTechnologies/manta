# MAN-213: fade-tracking keying rails on top of MAN-103's unbiased edge placement

## Background

MAN-103 (`docs/DECISIONS/2026-09-07-man103-keying-edge-placement.md`) moved the
legacy `Demod` key decision from the geometric mean `T = sqrt(E_hi*E_lo)` to an
additive band about the linear midpoint, `mid ± half` (its D3,
`hyst_frac = 0.15`). That fixed the WPM bias it targeted. It also gated both
rail EMAs on the same band (its D5): `E_hi` updated only when `a > mid + half`,
`E_lo` only when `a < mid - half`. That removed every path that lets `E_hi`
follow a fading mark down. MAN-103's own D9 named the mechanism, and its
2026-09-11 status update held the PR because V8w's spot recall collapsed.

**Mechanism, measured at unit level.** `Demod` warmed up on 1.0/0.02 keying
(30 cycles of 30 hops on, 30 off), then fed a mark at `a = 0.5` for 60 hops
and 360 hops of space:

| | `E_hi` at fade hop 1 / 5 / 10 / 30 / 60 | key ever down in the fade? | runs from the last warm-up space on |
|---|---|---|---|
| MAN-103 | 1.0000 / 1.0000 / 1.0000 / 1.0000 / 1.0000 | no | `[(false,450)]` |
| MAN-103 + MAN-213 | 0.9934 / 0.5000 / 0.5000 / 0.5000 / 0.5000 | yes, from hop 5 | `[(false,34),(true,56),(false,360)]` |

Under MAN-103, `a = 0.5` lies inside `[mid - half, mid + half] ≈ [0.363, 0.657]`
when `E_lo = 0.02`. Neither rail updates, the key never goes down, and the
faded mark is deleted. (The planning phase's version of this probe, with a
shorter trailing space, ended `[(true,30),(false,420)]`: the same deletion.)
The collapse floor `E_hi ≥ 2·E_lo` only ever
raises `E_hi`, so it is not what stalls the rail. After this change it keeps
its meaning as the lowest value a fade can drive `E_hi` to.

The key decision alone cannot be loosened to fix this: key-down needs
`a > mid + half ≈ 0.65·E_hi`, the old rule needed `a > 1.25·T ≈ 0.18·E_hi`,
and MAN-103's D9 measured that `hyst_frac = 0` does not recover the gap. `E_hi`
itself has to move down fast enough that the band follows the fade.

## Decisions

**B1 — Ship with MAN-103.** The defect exists only in MAN-103's code; `main`
is fade-tolerant. The `MAN-213` branch carries MAN-103's three commits
(PR #137) rebased onto `main@0792f22` (clean cherry-pick), plus this ticket's
commits. The PR supersedes #137.

**B2 — Rail rule: geometric-mean split plus a sustained-fade re-anchor.**
`Demod::step` step (1), with `mid`, `half` and `T_cls` all taken from the
rails before this hop's update:

```
T_cls = sqrt(E_hi * max(E_lo, 1e-6))
if a > T_cls: E_hi ← E_hi + α_hi·(a − E_hi)      # pre-MAN-103 classification
else:         E_lo ← E_lo + α_lo·(a − E_lo)
if 1.25·T_cls < a ≤ mid + half: fade_run += 1; fade_sum += a
else:                           fade_run = 0;  fade_sum = 0
if fade_run ≥ max(debounce_hops, 1): E_hi ← fade_sum / fade_run
```

The re-anchor fires on every hop while the run is at or above `debounce_hops`
(it is not reset when it fires). The `max(…, 1)` only matters for a configured
`debounce_ms` that rounds to 0 hops, where an empty run would otherwise give
`E_hi = 0/0 = NaN` (found in self-review; pinned by
`zero_debounce_does_not_poison_e_hi`). The collapse floor, the one-shot `A_ref`
re-estimation (which now also rescales `fade_sum`) and MAN-103's D3 key
decision all run afterwards, unchanged. `K = FADE_FLOOR_RATIO = 1.25` is
exactly the pre-MAN-103 key-down threshold, so the fade margin restored is the
old threshold's margin. `N = debounce_hops = 5` (`ms_to_hops(12.0)`) needs no
new constant: evidence must persist as long as a run needs to be confirmed.
Neither is a SPEC §9 config key.

Alternatives measured in planning (MAN-103 rebased, release build; "N" is the
re-anchor run length, "K" the fade-floor ratio):

| # | rail rule | re-anchor | V6 CER | V8w validated / bogus | edge test | VR5 legacy CER |
|---|---|---|---|---|---|---|
| 0 | MAN-103 D5 band-gated | — | 0.5397 | 0/50 / 3 | pass | 0.0169 |
| 1 | gate on `key_down` (research rec.) | — | 0.5132 | 0/50 / 2 | — | — |
| 2 | `a > sqrt(E_hi·E_lo)` split | — | 0.3016 | 1/50 / 5 | fail (+2..+3 @ ramp 12) | 0.3588 |
| 3 | D5 band-gated | snap, N=8 | 0.0529 | 14/50 / 1 | pass | 0.8729 (N=5) |
| 6 | E_hi D5, E_lo `a < min(T, mid−half)` | snap, N=5 | 0.0159 | 27/50 / 1 | pass | 0.8898 |
| 7 | geometric split, edge-excluded E_hi update | snap, N=4..6 | 0.0159 | 25–30/50 / 0–2 | pass | — |
| 9 | D5 normally, geometric split for 1 s after a re-anchor | snap, N=4..6 | 0.0159 | 21–30/50 / 0–2 | pass, bit-identical to 0 | 0.8898 |
| 10 | geometric split, in-band E_hi weight 0.25/0.5 | snap, N=4..6 | 0.0159 | 24–29/50 / 0–2 | fail | — |
| **5** | **`a > sqrt(E_hi·E_lo)` split** | **snap, N=5, K=1.25** | **0.0159** | **28/50 / 0** | **fail at ramp 12 only (+2..+3)** | **0.8898** |

Robustness of #5 at K = 1.25 (planning):

| N | 2 | 3 | 4 | 5 | 6 | 8 | 12 |
|---|---|---|---|---|---|---|---|
| V8w validated / bogus | 32/0 | 33/1 | 30/0 | 28/0 | 26/0 | 21/0 | 13/1 |

At N = 5: K = 1.0 gives 35/50 with 1 bogus, K = 1.5 gives 15/50 with 3 bogus,
K = 0.8 gives 38/50 with 0 bogus.

#5 is the only variant with a contiguous 0-bogus plateau around its chosen
constants; "0 bogus" is SPEC §7's hard V8w criterion, and the bogus count flips
by ±1 on small perturbations in every other variant. Each half fails alone:
the geometric split alone gives 1/50 with 5 bogus; the re-anchor alone (D5
rails kept) gives 14/50 with 1 bogus at N = 8 and 20/50 with 2 bogus at N = 5.

Gating the rails on `key_down` (the research document's recommendation, row 1)
fails because a faded mark that starts after a space never keys down, so
key-state gating sends it into `E_lo`.

Also rejected: a continuity guard on the key-up re-anchor (the candidate level
must be ≥ R × the `E_hi` at the last key release). R = 0.3–0.4 changes nothing
on VR5 (0.887–0.890); R = 0.5 already drops V8w to 15/50 with 1 bogus;
re-anchoring only while the key is down (R = ∞) collapses V8w to 3/50 with
2 bogus. The key-up re-anchor is essential.

**B3 — Accept the slow-ramp edge-bias residual and bound it in the test.** The
geometric-mean split lets rise/fall transition samples pull `E_hi` slightly
low, the residual MAN-103's D5 removed. Steady-state mark bias in hops on
MAN-103's synthetic raised-cosine test (24-hop ON, 24-hop OFF, 50 cycles, first
15 marks skipped), re-measured in this implement phase, depth 12/20/30/40 dB:

| ramp hops | 2 | 4 | 6 | 8 | 10 | 12 |
|---|---|---|---|---|---|---|
| MAN-103 (D5) | 0/0/0/0 | 0/0/0/0 | 0/0/0/0 | 0/0/0/0 | +1/+1/+1/+1 | +1/+1/+1/+1 |
| **MAN-213** | 0/0/0/0 | 0/0/0/0 | 0/0/0/0 | 0/+1/+1/+2 | +1/+1/+2/+2 | +2/+3/+3/+3 |

Ramps ≤ 6 hops (≤ 16 ms) are unchanged. `keying_edge_placement_is_unbiased_across_depth_and_ramp`
keeps ±1 hop for ramps 2 and 6; only the 12-hop degenerate case (a triangular
pulse with no flat top) gets the measured `0..=3` bound. This is an explicit
trade, not a silently loosened gate: the test exists only in MAN-103's
unmerged PR, and the combined change is what lands. The alternative that keeps
the test bit-identical (#9) cannot hold 0 bogus on V8w. The real pipeline
shows the residual is small; see the WPM grid below.

**B4 — Accept VR5 under `--engine legacy` reverting to `main`'s level.**

| | VR5 (co-channel) legacy CER | VR8 (fast QSB) legacy CER |
|---|---|---|
| `main` (planning, quoted) | 0.8983 | 0.3096 |
| MAN-103 | **0.0169** | 0.6037 |
| MAN-103 + MAN-213 | 0.8898 | **0.2724** |

VR5's second signal is 10 dB weaker and 30 Hz away, inside the same channel,
keying in the first signal's spaces. To a scalar envelope demod this looks
identical to the first signal faded by 10 dB. MAN-103's VR5 win came from the
same high midpoint threshold that destroyed V8w: fade margin and co-channel
rejection are the same margin. **VR1–VR8 (`crates/manta-cli/tests/golden_vr.rs`)
all run `--engine hsmm` and never exercise `Demod`**, which runs only on
`Engine::Legacy` (`decoder.rs`, `push_envelope_legacy`), so the ROADMAP M2 VR
gate cannot change. See Follow-ups.

**B5 — Raise `MIN_V8W_VALIDATED` from 15 to 20.** It encodes the ticket's "at
least the pre-existing ~20 of 50": `main`'s pre-MAN-103 baseline is 21/50, and
this change measures 28/50, which leaves 8 calls of margin.

**B6 — Un-ignore V6.** It passes with a 6× margin (CER 0.0159 against
≤ 0.10), including the "survives past half" check. It is the end-to-end form
of the ticket's "the character stream stays readable through the fade".

## Measurements

All numbers in this section were re-measured in the implement phase on this
branch (32-core container, release build, `manta decode --json` per vector
through a throwaway harness that used the golden tests' own CER and
validated/bogus definitions), unless marked as quoted. Every re-measured
number matches the planning phase's number to the digit, except the one WPM
grid claim noted below.

### Golden vectors

| vector | `main` (pre-MAN-103, quoted) | MAN-103 rebased | **MAN-103 + MAN-213** | gate |
|---|---|---|---|---|
| V1 CER / WPM | 0.0155 / 17.96 | 0.0155 / 20.02 | **0.0155 / 19.93** | enforced |
| V2 CER / WPM | 0.0325 / 29.04 | 0.0244 / 35.04 | **0.0244 / 34.96** | WPM 35±2 enforced; CER ≤ 0.01 `#[ignore]`d (pre-existing) |
| V3 CER | 0.0180 | 0.0180 | **0.0180** | enforced |
| V4 CER | 0.0168 | 0.0336 | **0.0168** | enforced |
| V5 CER | 1.4167 | 1.1250 | **0.7500** | ≤ 0.20 `#[ignore]`d, still fails |
| V6 CER | 0.1429 | 0.5397 | **0.0159** | ≤ 0.10, **un-ignored** |
| V10 CER | 0.0395 | 0.0263 | **0.0263** | enforced |
| V8 validated / bogus | 49/50 / 0 | 50/50 / 0 | **50/50 / 0** | enforced |
| V8w validated / bogus | 21/50 / 0 | 0/50 / `N4TI, W5NB, W8A` | **28/50 / 0** | 0 bogus + floor 20 |

`cargo test -p manta-cli --release --no-fail-fast --test golden_v1 --test golden_v2_v3 --test golden_v7_v9_v10 --test golden_v8_v8w -- --include-ignored --skip hsmm`
fails exactly the four pre-existing `#[ignore]`d tests:
`v2_char_accuracy_meets_spec`, `v2_passes_with_edge_legacy_engine`,
`v5_passes_end_to_end_from_wav` and
`v8w_pileup_fading_decodes_90pct_of_strong_signals_no_ghosts`. Every other
test passes, including the un-ignored V6 and the V8w spots test at floor 20.

### Offset × WPM grid (`crates/manta-engine/tests/wpm_across_channel.rs`, `--nocapture`)

```
wpm  residual   MAN-103   MAN-213   move
 20  0          20.00     20.00     0.00
 20  0.15       20.00     20.00     0.00
 20  0.3        20.00     19.98    -0.02
 20  0.4667     20.00     20.02    +0.02
 25  0          25.00     25.00     0.00
 25  0.15       25.00     25.00     0.00
 25  0.3        24.99     24.95    -0.04
 25  0.4667     24.97     24.94    -0.03
 35  0          34.72     34.87    +0.15
 35  0.15       34.71     35.02    +0.31
 35  0.3        35.03     35.37    +0.34
 35  0.4667     35.07     35.40    +0.33
 40  0          40.31     39.51    -0.80
 40  0.15       39.99     39.65    -0.34
 40  0.3        39.70     39.94    +0.24
 40  0.4667     39.61     40.42    +0.81
```

CER is identical in every cell before and after (0.0309 / 0.0328 / 0.0294 /
0.0361 by speed). Worst error: −0.49 WPM (40 WPM, on-centre) against
MAN-103's −0.39 (40 WPM, near-edge), well inside the test's ±2 WPM gate.
**Correction to the plan:** the planning document said the grid "moves by at
most 0.33 WPM in any cell". That holds for 20–35 WPM, but the two outer
40 WPM cells move by −0.80 and +0.81. They move toward and past the true
value rather than away from it (`|err|` goes 0.31 → 0.49 and 0.39 → 0.42), so
the planning conclusion — the slow-ramp residual is negligible on the real
pipeline — stands, with a larger per-cell spread at 40 WPM than stated.

### Unit tests

- `envelope::tests::faded_mark_rail_tracks_down_and_stays_keyed` (Scenario 1,
  demod level): fails on MAN-103 at the `key_down` assertion; passes with this
  change (`E_hi` = 0.5000 from hop 5, marks `[56, 30, 30, 30, 30, 30]`).
- `decoder::tests::legacy_decodes_through_a_mid_text_fade` (Scenario 1,
  character stream): fails on MAN-103 on `step -6 dB`
  (`"CQ CQ DE W1AW W1AW K T"`); passes on all four profiles (step −6 dB,
  step −9 dB, ramp −6 dB, 10 dB / 2 s QSB) with this change.
- `decoder::tests::edge_legacy_decodes_contest_speed_deep_keying`: its
  `assert_ne!` pinned the Legacy chain's old word-merge on a deep-keyed
  40 WPM scene; MAN-103 fixed that merge and this change keeps it, so the
  assertion is flipped to `assert_eq!` (Legacy returns
  `"CQ TEST W5AU W5AU TEST"`).
- `envelope::tests::zero_debounce_does_not_poison_e_hi`: a `debounce_ms`
  that rounds to 0 hops leaves `E_hi` finite (fails with `NaN` without the
  `max(…, 1)` guard).
- `cargo test --release -p manta-decode --lib`: 108 passed, 0 failed (the
  plan's 107 plus the review-driven test above).

### First mark after an abrupt fade

The first mark after an abrupt in-gap fade starts `debounce_hops` late
(56 hops for a 60-hop mark, about 13 ms). It affects only the first element
after an abrupt fade, and the beam still classifies it as a dah. Backdating it
would reach into the open/held run bookkeeping and is out of proportion here.

### Determinism and cost

The new state is two scalar fields updated in sample order, with no clocks,
hashing or threads, so file input still gives byte-identical spot logs.
Per hop the change adds one `sqrt`, two compares and one add (MAN-103 had
removed exactly this `sqrt`), plus one division on re-anchor hops. The
in-container `cpu_budget` test runs in the workspace suite; the Pi4 leg is not
reachable from containers (the standing gap in `CLAUDE.md`).

## Follow-ups

- Legacy `Demod` cannot separate a weaker co-channel signal from the same
  signal faded by the same amount (VR5 under `--engine legacy`: 0.0169 with
  MAN-103 alone, 0.8898 with MAN-213). Co-channel rejection needs a non-scalar
  observation (evidence front end / hsmm, or MAN-107-class matched
  filtering).

Still open and out of scope here: V5 (CER 0.75 against ≤ 0.20; needs
MAN-107-class work such as matched filtering or a fading observation model,
`docs/DECISIONS/2026-09-09-decoder-recall-research.md` rank 6), V8w's raw
per-signal CER gate, and V2's CER half (SPEC §2.1 warmup-floor dilution).

## References

- Ticket: MAN-213. Unblocks MAN-103 / PR #137.
- `docs/DECISIONS/2026-09-07-man103-keying-edge-placement.md` — D3 (kept), D5
  (superseded here), D9 (resolved here), status updates.
- `docs/DECISIONS/2026-09-06-broad-review-decisions.md` D8 — MAN-107–113 scope.
- `docs/superpowers/specs/2026-09-09-decode-core-real-hf-design.md` §0 item 2 —
  independent support for D3's midpoint band.
- `docs/SPEC-decode-core.md` §3.2, §3.3.
- `crates/manta-decode/src/envelope.rs` — `decision_band`, `Demod::step`,
  `FADE_FLOOR_RATIO`.
- `crates/manta-cli/tests/golden_v8_v8w.rs` (`MIN_V8W_VALIDATED`),
  `crates/manta-cli/tests/golden_v2_v3.rs` (`v6_passes_end_to_end_from_wav`).
