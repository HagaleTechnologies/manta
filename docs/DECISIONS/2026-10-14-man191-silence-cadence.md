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
- (d) "hot carry": the `(s, index-of-τ)` pair produced the best
  `Silence` candidate of its merge key (`round(2·u)`; a `Silence`
  successor is always `ROOT`/`AfterSpace`) at the previous productive
  anchor step, whether or not that candidate won its key or survived
  the beam. At most one pair per key;
- (e) expiry: when a key's carried pair produces no candidate at `t`
  (it passed `80·u`, or its anchor aged out of `reach_sil`), every pair
  of that key is evaluated at `t`.

The predicate is the pure function `silence_sampled`
(`crates/manta-decode/src/hsmm/mod.rs`), implementing (a)–(c); (d) is
the `hot_silence` set carried in `HsmmDecoder`, and (e) evaluates the
step's skipped pairs of an expired key after the main loop. Candidates
sort on `token_order`, then enumeration position, which is the order
the stable sort over the old loop gave, so (e)'s late evaluations break
ties the same way. No config key: `round(u)` stays hardcoded per the
spec. `decode.engine` stays `legacy`.

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

Measured on 2026-10-15 in this container against `e2c9681`, the main
commit this branch merges, which already carries MAN-189's
`commit::Frontier` and MAN-192's Silence reseed. The tools: the
unit-test `silence_trace` recorder (every `Silence` evaluation beyond
`reach_norm`), a scratch instrumented release build that counts
successor calls (never committed), and a SHA-256 byte comparison of
`manta decode --json --engine hsmm` over all 25 golden fixtures (V1–V10,
V8w, VR1–VR8 with VR6a/b, VW1–VW5).

### Evaluation counts

| input | `Silence` evaluations, every step → cadence | all successor calls |
|---|---|---|
| synthetic `N N N <1,875-hop gap> N N`, u=13, gap phase beyond `reach_norm` | 342,881 → 218,100 (−36%) | — |
| V3 | 25.76 M → 5.89 M (−77%) | 46.7 M → 26.8 M (−43%) |
| V8 | 944 M → 321 M (−66%) | 1.69 G → 1.07 G (−37%) |

"Every step" is the same build with the cadence switched off, which
decodes V3 and V8 byte-identically to `e2c9681`. Across the whole
synthetic scene, beyond `reach_norm`, evaluations go 497,022 → 310,743;
7,533 of those are hot carry and 3,939 come from the expiry rule. The
gap outlasts `80·u`, so carried origins do expire there.

### Golden-vector output

25/25 vectors decode byte-identically to `e2c9681`, V8w and V8
included. `silence_cadence_commits_what_every_step_evaluation_commits`
checks the same property on the synthetic scene: identical glyphs,
timestamps and confidences with and without the cadence.

### The first cut, and why it was not enough

The first implementation carried only beam survivors as hot (rule (d)
was "this pair's `Silence` successor survived merge + beam truncation")
and had no rule (e). At `9636fe0`, before MAN-189 and MAN-192, it
decoded 21/25 vectors byte-identically: V5 +2 characters, V8/VR8
timestamps and confidences, V8w −6 glyphs, spot lists unchanged. After
the main merge, validation found V8w spotting a bogus `U1IRL` (a misread
of the keyed `K1IRL`) that main does not; V5, V8, V8w and VR8 still
differed. The cause is a gap in the hot-carry argument below: the
`Silence` ranking among origins is fixed, but a key's best `Silence`
candidate can enter the beam at an unsampled step once the mark
candidates around it decay. Carrying only last step's survivors left it
out until its next sample point, and a worse candidate took its slot.
Carrying each key's best candidate, plus rule (e) for when that origin
expires, closes the gap.

### Bench (this container, not a Pi 4; `--sample-size 10`)

Measured 2026-10-15. Main `e2c9681` (with this branch's bench file
copied in) and this branch ran at the same time, while other decodes
and builds also ran, so the absolute times are inflated and noisy. The
branch build predates the review fixes to rule (b)'s bound and to
clearing the hot set on an empty step; neither changes the evaluation
counts by more than 0.01%.

| scene | main `e2c9681` | with cadence |
|---|---|---|
| `hsmm/300_tracks_10s` | 41.753 s [34.163, 49.996] | 42.678 s [35.180, 50.779] |
| `hsmm/300_tracks_10s_gappy` | 18.010 s [17.302, 18.762] | 22.671 s [21.947, 23.440] |

The continuous scene is within noise. The gappy scene came out ~26 %
**slower** with the cadence. Its intervals do not overlap, so this is
probably real and not noise. The likely cost is hot carry: one
successful, `hist`-cloning evaluation per `Silence` merge key per step,
where the first cut carried at most `beam`. Rule (e) also runs when the
5 s gap outlasts a key's `80·u`. The bench drives 300 tracks of
identical keying through the same gap.

On the golden vectors, single decodes on an otherwise idle container
(wall time, `--engine hsmm`), main → first cut → this change:

| vector | main | first cut | this change |
|---|---|---|---|
| V3 | 15.3 s | 12.3 s | 12.4 s |
| VW3 | 5.6 s | 4.6 s | 4.8 s |
| VR1 | 8.4 s | 8.6 s | 8.8 s |

The previous record's numbers (`9636fe0`, first cut) were 23.952 s →
24.407 s continuous and 15.050 s → 13.819 s gappy. The new
`hsmm/300_tracks_10s_gappy` scene (3 s keying, 5 s gate-low, 2 s
keying) is the first bench coverage of the long-silence path; the
existing continuous scene's longest quiet stretch (39 hops) never
reaches `10·u`. For context, the stage-2 record measured
`300_tracks_10s` at 10.625 s mean on an M-series core. The 4.25×
MAN-171 overage is unchanged by this ticket, and the gappy-scene cost
is a MAN-171 input.

## Why hot carry (d) and expiry (e) exist

A `Silence` candidate's score is `τ.score + ev + 0 + lp_type` with
`ev = −(C[t] − C[s])` (the duration prior is flat on `[10u, 80u]`).
Between two `Silence` candidates the `C[t]` term cancels, so their
`token_order` ranking does not depend on `t`. A pair can only start
producing by crossing `10·u` (rule (b)) or inside `reach_norm` (rule
(a)), and both are always evaluated. So if each key's best `Silence`
candidate is evaluated at one step, it is still the best at the next
step unless it expired (rule (e) then evaluates the whole key) or a
newly crossing pair beats it (rule (b) evaluated that pair). Merge keeps
only the best candidate per key, so the merged beam is the one
evaluating every origin at every step would give. That holds in exact
arithmetic. Scores are `f32` and the evidence term `C[t] − C[s]` is
rounded at each step, so two same-key candidates within about one
rounding step can swap order between steps; the cadence then keeps the
one it carried. The 25-vector comparison above found no such case.

Rule (b)'s bound is `ceil(10·u)` in whole hops, the first `d` the
`Silence` prior accepts (`d as f32 >= 10·u`). The first cut truncated
it, which spends (b) on a step the prior still rejects when `10·u` is
fractional and skips the pair's first productive step. A step that
produces no candidate at all clears the hot set: every carried pair
expired there and rule (e) found no other producible pair of its key.

Without any carry, unsampled steps fill the beam with mark candidates
that ran through negative evidence; the plan's mode-3 prototype shifted
commits and regressed V10's CER 0.493 → 0.579. Hot carry costs one
evaluation per `Silence` merge key per step. Rule (e) costs a full
pass over one key, and only on a step where that key's carried origin
expires.

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
  unchanged by the cadence (the synthetic scene's commit stream is
  identical with and without it): each retained origin's `Silence`
  successor commits its node glyph at its own origin `sample_ts`, and
  MAN-189's `commit::Frontier` only drops a glyph whose character
  started at or before the last emitted entry, so later origins still
  emit. A candidate for its own ticket, not MAN-191 scope.
- MAN-192 (reseed after a committed `Silence`, SPEC §4.7) landed before
  this merged; the byte comparison above was rerun against it. Reseed
  score shifts are uniform across anchors, so they leave the `Silence`
  ranking that hot carry relies on unchanged.
