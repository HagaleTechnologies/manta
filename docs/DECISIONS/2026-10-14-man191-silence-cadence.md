# MAN-191: the HSMM Silence duration prior is sampled at the SPEC §4.6 cadence

## Decision

`HsmmDecoder::push`'s candidate-generation loop no longer evaluates the
`Silence` successor from every retained anchor on every anchor step.
For origin anchor `s`, token `τ`, current anchor step `t` and previous
anchor step `t_prev` (`d = t − s`, `d_prev = t_prev − s`), `Silence` is
evaluated iff `d ≥ 10·u` and one of:

- (a) `d ≤ reach_norm` — inside the normal candidate-origin set `A(t)`;
  §4.6's cadence qualifies only the extension beyond it;
- (b) `d_prev < 10·u ≤ d` — the first crossing of the prior's lower
  bound is always evaluated, so the initial "this gap just became
  Silence" decision keeps its commit latency;
- (c) a sample point `10u + m·round(u)` (`m ≥ 1`) falls in `(d_prev, d]`
  — the cadence itself;
- (d) "hot carry": the `(s, index-of-τ)` pair's `Silence` successor
  survived merge + beam truncation at the previous productive anchor
  step, and is re-derived every step while it keeps winning.

The predicate is the pure function `silence_sampled`
(`crates/manta-decode/src/hsmm/mod.rs`), implementing (a)–(c); (d) is
the `hot_silence` set carried in `HsmmDecoder`. No config key: `round(u)`
stays hardcoded per the spec. `decode.engine` stays `legacy`.

## Problem

SPEC v2 §4.6 extends the candidate-origin set to `80·u` hops for
`Silence`, "sampled every `round(u)` hops". The implementation had no
sampling: on every anchor step it evaluated `Silence` from every
retained anchor's every token once `d ≥ 10·tok.u`. With fallback anchors
every 8 hops and anchors retained up to `reach_sil = ⌈80·u_max⌉`, a
track in a long gap accumulates ~130–160 anchors and re-evaluates
`Silence` from all of them every 8 hops — the MAN-191 ticket's
"hundreds of redundant evaluations per token". Each surviving evaluation
clones the token's whole `hist` Vec.

A correction to the original finding (found while grounding the plan):
the `d ≥ 10·u` gate is the distance from the ORIGIN ANCHOR, not the
length of the gap. Any `AfterMark` token on a glyph-bearing node at an
anchor at least `10u` old gets a `Silence` successor every step,
including during continuous keying — so this cost also sat inside the
4.25× bench overage recorded for MAN-171, not only in gaps.

## Evidence

All numbers from this implementation run at `9636fe0` in this container,
via the unit-test `silence_trace` recorder (every `Silence` evaluation
beyond `reach_norm`) and the SHA-256 byte comparison of
`manta decode --json --engine hsmm` over all 25 golden fixtures
(V1–V10, V8w, VR1–VR8 with VR6a/b, VW1–VW5).

### Evaluation counts

Synthetic `N N N <1,875-hop gap> N N` scene at u=13
(`silence_cadence_samples_cold_origins_every_round_u`): gap-phase
beyond-`reach_norm` `Silence` evaluations 345,496 uncapped → 214,111
with the cadence (−38%). The plan's instrumented runs measured the same
scene at 343,075 → 223,082 (−35%) and, over whole vectors,
V3 25.76 M → 5.77 M (−78%) and V8 860 M → 318 M (−63%) `Silence`
evaluations, with total successor calls falling 24–43%.

### Golden-vector output

21/25 vectors decode byte-identically to `9636fe0`, including every
currently passing HSMM gate vector: V3, V4, V6, V9, VR1–VR4, VR6a,
VR6b, VR7 (also V1, V2, V7, V10, VR5, VW1–VW5). Exactly the four
vectors the plan predicted may differ do: V5, V8, V8w, VR8.

- V8, VR8: JSON bytes differ, assembled text identical to main's.
- V8, V8w: JSON bytes differ, spot callsign lists identical to main's;
  V8w text is also identical.
- V5: +2 characters in 363 (garble-level text on a vector whose CER
  gate is `#[ignore]`d for the HSMM engine anyway — the plan measured
  +1 char, CER 1.406 → 1.417).

The deltas are timestamps/confidences and gap-artifact glyphs — the same
class the plan's mode-6 measurement accepted. The direct-drive
characterization test
(`silence_cadence_preserves_commit_stream_across_a_long_gap`) pins the
post-cadence commit stream; main's pre-cadence stream on the same scene
differed only in the gap's junk-`E` artifact entries (origin
timestamps; the winning junk-Dit origin changes under sampling) and two
post-gap confidences. Every keying glyph and keying commit timestamp is
identical.

### Bench (this container, not a Pi 4; `--sample-size 10`)

| scene | main `9636fe0` | with cadence |
|---|---|---|
| `hsmm/300_tracks_10s` | 23.952 s [23.093, 24.922] | 24.407 s [23.342, 25.547] |
| `hsmm/300_tracks_10s_gappy` | 15.050 s [13.596, 16.597] | 13.819 s [13.611, 14.037] |

The new `hsmm/300_tracks_10s_gappy` scene (3 s keying, 5 s gate-low,
2 s keying) is the first bench coverage of the long-silence path — the
existing continuous scene's longest quiet stretch (39 hops) never
reaches `10·u`. Both scenes' confidence intervals overlap heavily on
this container: the gappy mean improves ~8 % while the continuous mean
is within timer noise, so the wall-time claim is weak here and the
evaluation-count reductions above are the measurable effect. For
context, the stage-2 record measured `300_tracks_10s` at 10.625 s mean
on an M-series core; this container runs the same scene ~2.3× slower.
The 4.25× MAN-171 overage is unchanged by this ticket.

## Why hot carry (d) exists

A `Silence` candidate's score is `τ.score + ev + 0 + lp_type` with
`ev = −(C[t] − C[s])`. Between two `Silence` candidates the `C[t]` term
cancels, so their ranking does not depend on `t`: per-step re-derivation
adds only one thing — keeping the winning "still silent" hypothesis in
the beam. Without (d), unsampled steps fill the beam with mark
candidates that ran through negative evidence; the plan's mode-3
prototype (cadence without hot carry) shifted commits and regressed
V10's CER 0.493 → 0.579. Hot carry is bounded by `beam` (≤ 12 pairs per
step) against the ~1,300 evaluations per step the cadence removes.

Rejected alternatives (measured in the plan): a strict
`d % round(u) == 0` grid (aliases against the 8-hop fallback grid,
fires once per `8·r` hops at u=13 and changes output); origin thinning
on an absolute grid (drops the true falling-edge origin); lazily
materialising only the best `Silence` successor per merge key
(output-identical and cuts clones, but does not implement the mandated
cadence — noted for MAN-171).

The plan's full variant comparison, retained verbatim (25 vectors,
`--engine hsmm`; measured 2026-10-13 at `9636fe0`):

| mode | semantics | byte-identical to main | text changes |
|---|---|---|---|
| 0 | main | 25/25 | — |
| 1 | `d % round(u) == 0` | not run on vectors | synthetic: extra `N`/`E` glyphs |
| 2 | mode 1, only beyond `reach_norm` | not run on vectors | synthetic: same as mode 1 |
| 3 | first crossing of `10u`, then one sample per `round(u)` of `d`, beyond `reach_norm` | 16/25 | V10 CER 0.493→0.579 (worse); V7/V8/V8w/V5/VR8/VW5 shifted; V8w spot set changed |
| 4 | origin thinning on an absolute `round(u)` grid | not run on vectors | synthetic: extra `N N` |
| **6** | **mode 3 plus hot carry** | **21/25** (V5, V8, V8w, VR8 differ) | V8/VR8 text-identical; V5 +1 char; V8w −6 of 12,694 chars; spot lists identical everywhere |

## Follow-ups

- MAN-171: the overall HSMM CPU budget (4.25× bench overage) remains
  open; lazy `Silence` materialisation is still the candidate lever.
- The junk-`E` flood in gaps longer than `80·u` exists on main and is
  unchanged by the cadence (same glyph count in the synthetic scene,
  shifted origin timestamps): each retained origin's `Silence`
  successor commits its node glyph at its own origin `sample_ts`, and
  the `sealed` dedupe keys on `sample_ts`, so distinct origins all
  emit. A candidate for its own ticket, not MAN-191 scope.
- MAN-192 (reseed after a committed `Silence`, SPEC §4.7) will change
  the beam composition during gaps; rerun the byte comparison when it
  lands. Hot carry still applies — hot entries are (anchor hop, token
  index) pairs and anchors stay frozen apart from reseed score shifts.
