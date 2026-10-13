# MAN-190: evidence anchors confirmed from actual adjacent hops, not predicted

## Decision

`Evidence::emit`'s forward-looking sign prediction (PR #161 round 13)
predicted what the *next* hop's LLR sign would be, using the hold window
`h` as it stood during `emit(center)`. Both real callers
(`TrackDecoder::push_hop_hsmm` and `finish()`'s `Engine::Hsmm` branch)
call `set_u_ref` between every pair of adjacent computations — the per-hop
speed feedback PR #161 rounds 9 and 11 built — so the prediction could go
stale whenever that feedback moved a sample into or out of the next
center's centered-max window. This was the second consecutive review
round (13, then 16) to find a genuinely different bug in the same
lookahead, and the round-16 remedy was a different *strategy*, so MAN-190
redesigned the mechanism instead of patching the prediction again.

`Evidence` now keeps one computed hop **pending** and confirms anchors
backward from two actual, immutable values:

1. Compute hop `t+1` under the currently effective `h` (base anchors
   only: periodic fallback cadence and gate transitions), advancing its
   prefix once.
2. Compare the two hops' real LLR signs (three-way rule: an exact 0 LLR
   differs from both `+1` and `-1`) and finalize hop `t`'s anchor flag.
3. Store `t+1` in the pending slot and release hop `t`.
4. The caller processes `t` and applies `best_u -> set_u_ref` feedback
   before hop `t+2` is computed.

There is no prediction left to go stale: a window update can only change
what has not yet been computed, never a pending or released hop. The
sign-crossing anchor lands on the **earlier** hop of a crossing (as
before) and also applies across gate transitions (the earlier hop gets
the sign anchor, the later hop its gate-change anchor). At EOF, remaining
centers are computed with the actual `h`, forward reach clipped to the
last real input, and when no uncomputed center remains the final pending
hop is released exactly once with only its own base anchor — no synthetic
successor, zero padding, or shortened backward window.

**Changed contract.** For a constant hold window, release latency grows
from `h` to `h+1` input hops (2.667 ms more at 375 Hz). `set_u_ref` now
promises to set the window for the next *computed* hop: one computed hop
may be pending, updating the window never recomputes it, and feedback
after returned hop `t` first affects the computation of hop `t+2`.
Input timestamps and per-hop fields do not move. `EdgeLegacy` shares the
`Evidence` implementation and inherits the same one-hop release delay
(it never calls `set_u_ref`). Rust callers that assumed one output after
exactly `h` inputs must allow `h+1` and drain at EOF.

## Reproduced standalone failure (planning measurement)

A temporary unit test against the unmodified production `Evidence`
(steady plateau amp 2.5, noise 1.0, spike 20 at input hops 66–68,
`h = 4` grown to `h = 20` by `set_u_ref` placed exactly between the two
drains) observed:

```text
center hop=60 llr=5.5555553 present=true anchor=false
next   hop=61 llr=-4.228814 present=true anchor=false
```

A real +5.56 → -4.23 crossing with `present` true at both hops and no
periodic fallback in range — hop 60 silently un-anchored. The
companion shrink direction (h=20 shrunk to 4) missed the mirrored
crossing the same way. Both regressions are now committed tests
(`man_190_actual_crossing_with_window_feedback`,
`man_190_actual_crossing_with_window_shrink_feedback` in
`crates/manta-decode/src/evidence.rs`) and were observed failing on the
original algorithm during implementation before the redesign made them
pass.

## Qualification

The forced-`set_u_ref` reproduction proves the evidence-layer defect and
the fixed contract; it does **not** prove an end-to-end missed anchor
produced by today's HSMM feedback. `HsmmDecoder::push` changes `live`
(and therefore `best_u`) on gate onset or during an anchor step; gate
onset already sets an anchor, and non-anchors return before candidate
generation, so a production speed change ordinarily follows an already
anchored hop. No integrated failure or CER change is claimed; the HSMM
engine remains non-default and stage-2-gate-failed
([2026-09-09-decode-core-v2-stage2-gate.md](2026-09-09-decode-core-v2-stage2-gate.md)).

## Alternatives rejected

| Alternative | Reason |
|---|---|
| Recompute the predicted sign when `h` changes | The earlier value has already been consumed and may have caused irreversible commits; retains the repeatedly-buggy prediction strategy. |
| Backward fallback on the *later* hop | Detects the transition but violates the earlier-hop anchor rule and reintroduces the one-hop boundary bias the lookahead existed to remove. |
| Delay release but let the successor depend on feedback from the pending hop | Cyclic: finalizing the pending anchor needs the successor's sign, while that feedback needs the finalized pending hop. |
| Replay/roll back HSMM on a changed prediction | Adds rollback for anchors, histories, commits, SNR and speed events to solve a one-hop evidence problem. |
| Buffer at the HSMM layer and leave `Evidence` predicting | Leaves the exported invariant broken and creates two owners of anchor semantics. |

## Verification (implementation measurements)

All commands run on this change:

- `cargo test -p manta-decode --lib --locked` — 138 passed, including
  the two MAN-190 regressions, the pending-immutability and
  `h+1`-delay tests, the lifecycle matrix (backlog drain with updates,
  stable-sign no-fabrication, exact-zero transitions, per-hop noise,
  gate/periodic anchors asserted separately, empty/one/two/short
  streams, final-pair crossing, `MAX_RETAIN` trimming, terminal-pending
  release), the record/drain interleave equivalence, and every
  pre-existing true-edge, gate, identity and prefix assertion.
- `cargo test -p manta-decode --locked`, `cargo test -p manta-engine
  --lib --locked` (214 passed) and `cargo test -p manta-cli --test
  determinism_hsmm --locked` (three-run JSON equality) all pass.
- Decoder integration: a clean-keying speed-change scene (35 → 25 WPM)
  with a short final word completed by `finish()` decodes the expected
  text, converges to the final speed within the existing ±2 WPM
  tolerance, keys every final-word character inside its own envelope
  range at real marks, and a second `finish()` emits no repeated
  character/word events
  (`hsmm_streams_a_speed_change_and_finishes_the_final_word`).
- A gentler speed step than 35→19 WPM was required for the integrated
  scene: the HSMM engine's speed tracking diverges on that 2x jump (a
  pre-existing engine limitation, unchanged by this ticket — the
  stage-2 gate remains failed for unrelated reasons).

`hsmm` decoder output for adaptive input can change relative to the
previous build (anchor timing and feedback apply one hop later), so
[CHANGELOG.md](../../CHANGELOG.md) carries a `### Decoder output` entry
and the next release bumps MINOR per
[MAN-83](2026-10-10-man83-build-identity-and-decoder-versioning.md).
Normative text: [SPEC-decode-core-v2.md](../SPEC-decode-core-v2.md) §1.2,
§1.7, §1.8.
