# MAN-524: ownership, spawn admission and merge read only nearby tracks

## Decision

`TrackManager` answers its per-hop ownership, spawn-admission and merge
checks from a per-pass neighbour index, `CenterIndex`
(`crates/manta-engine/src/track/neighbours.rs`), instead of scanning every
track for every track or rising channel. Decoded output does not change.

- The index holds one moment's track centres twice: in ascending id (the
  old `centers` slice and `BTreeMap` order) and sorted by circular channel
  position `c.rem_euclid(n)`, ties by id. `any_near(x, r, f)` hands `f`
  every entry whose circular distance from `x` may be at most `r`, using
  two `partition_point` lookups plus a second range across the 0/n seam.
- Each check queries a radius at least as large as the farthest track that
  can change its answer, plus `NEAR_SLACK_CH = 1e-6`: `own` for
  `channel_exclusive`, 2.5 for `decoder_channel`'s neighbour test, Promoted
  isolation and `birth_isolated`, 2.0 for the straddle neighbour and the
  `spawn_admits` owner scan (an owner's centre is within 1.5 channels), 1.0
  for merge. Each check then runs its original float comparison on every
  candidate.
- An `all(p)` scan became `!any_near(.., |..| !(p))` with the original
  expression inside the negation. `!(d >= r)` is not `d < r` when `d` is
  NaN, so this keeps NaN behaviour exactly. Clippy denies a negated
  partial-order comparison, so the isolation checks spell it
  `short_of(d, r)`, a `partial_cmp` match with the same truth table.
- If any indexed centre, the query point or the radius is NaN or
  infinite, or the window covers the whole circle, `any_near` visits every
  entry in id order: the old loop. No code path produces a non-finite
  centre today.
- The straddle loop was the one order-sensitive scan ("first nearest by
  id wins"). Its body now also keeps the lower id on an exact distance
  tie, so position order and id order pick the same neighbour.
- `merge_converged` visits only `merge_candidates`: every pair `(a, b)`,
  `a < b`, with `channel_distance < 1.0`, sorted. Every other pair hit
  `continue` with no side effect, so the greedy `to_close` list and the
  `TrackClosed` flush order (SPEC §6 rule 6, MAN-172 incumbent ties) are
  unchanged.

Where each index is built:

| Pass | Built | Kept in step with |
|---|---|---|
| Channel selection, Promoted isolation, lead-in | once per hop before the per-track loop, replacing the `centers` `Vec` | nothing: it is the same pre-loop snapshot |
| Spawn pass, including the funnel's `admission_veto` snapshot | after the closes' `recompute_ownership` | `spawn_positioned` inserts each new id at its final centre (sorted insert: ids can wrap, MAN-13 D3) |
| Merge | inside `merge_converged` | nothing; tests that set centres and call it directly keep working |

The index is never cached on `TrackManager`: centres move every hop and
tests set them directly. The spawn scan also evaluates each rising
channel's `spawn_phase_position` once and passes it to `cw_spawn_admits`,
`spawn_admits` and `spawn_positioned`, which used to recompute it (the
function is pure).

`step_hop` no longer holds the per-track channel selection or the spawn
scan. `select_channels` (reading the hop's inputs through a
`HopSelection`) and `spawn_pass` hold them, moved without changing what
they compute. `step_hop` is 705 lines, from `fn step_hop(` to its
closing brace, against 837 at 915db0e.

The 1e-6 slack: the plan's standalone check found 1 mismatch in 3,888,308
randomized checks with no slack (float rounding at a window edge after
`rem_euclid`) and none with 1e-6. Centres stay within a few spans of
`[0, n)`, where `channel_distance`'s rounding is around 1e-12.

## Cost

Reads of a track record per check, counted by a test-only thread-local
counter (`note_near_read`) placed in each check's per-track closure. One
hop of `linear_work_scene` (2,048 channels, 192 kS/s, evenly spaced +22 dB
tones, all tracks ACTIVE):

| Pass | 150 tracks | 300 tracks |
|---|---|---|
| Ownership, integer centres | 600 | 1,200 |
| Ownership, centres moved +0.45 (straddle path runs) | 1,200 | 2,400 |
| Spawn admission inside the hop | 150 | 300 |
| Spawn admission, direct `spawn_admits(k, Some(k))` per tone | 300 | 600 |
| Merge | 150 | 300 |

Before the change, the plan's probe at 915db0e counted 450,000
`channel_distance` calls in one such hop at 300 tracks (810,000 straddled),
growing 4.0 times per doubling of tracks: 3 × T² for `select_exclusive`,
T² for `decoder_channel`, T²/2 for `birth_isolated` and T(T−1)/2 merge
pairs. After it, every pass grows 2.0 times per doubling. The tests
`ownership_reads_per_hop_are_linear_in_tracks`,
`spawn_admission_reads_per_hop_are_linear_in_tracks` and
`merge_reads_per_hop_are_linear_in_tracks` hold the budgets (16, 8 and 4
reads per track at 300, at most 2.5 times growth from 150).

Complexity is one O(T log T) sort per pass (three per hop, a few hundred
tuples each) plus constant reads per query. The constant holds because
tracks closer than 0.5 channel always merge and the spawn owner rule
refuses most births on owned channels, so few tracks fit in a 2.5-channel
window.

## Equivalence

- `track::neighbours::tests::matches_the_full_scans` compares verbatim
  copies of the five 915db0e scans with the index-backed forms on
  randomized scenes (n in {64, 512, 1024, 2048}; centres anywhere, on a
  quarter-channel grid, at the 0/n seam, unwrapped by ±n, in dense
  clusters, and 1 in 40 non-finite): 1,009,528 checks, 0 mismatches,
  82,495 exact exclusivity ties, 7,185 non-finite fallbacks. The owner-set
  comparison runs on finite scenes only: `Track::owned` overflows its
  `i64` cast for an infinite centre at 915db0e as well.
- `straddle_tie_goes_to_the_lower_id_in_any_order`,
  `spawn_positioned_keeps_the_spawn_index_in_step`,
  `merge_candidates_are_the_close_pairs_in_id_order` and
  `a_three_way_merge_closes_the_same_losers_in_the_same_order` pin the
  order rules. Every existing unit, integration and golden test passes
  unchanged in what it asserts.
- End to end against base 915db0e2383c8ca517deed570469fab8e8c6a1d5, both
  trees built with `MANTA_GIT_SHA=man524cmp` and separate target
  directories: `decode_fingerprints`
  (`crates/manta-cli/tests/decode_fingerprints.rs`, the SHA-256 of
  `manta decode --json` stdout for V1–V10, V8w, V31, V34, V51, VR1–VR5,
  VR6a, VR6b, VR7 and VR8 under both engines, 46 lines), the
  `man398_grid` A/B grid (288 rows) and `close_pair_sweep` (181 result
  lines). All three are byte-identical: `diff` of the 46 `FINGERPRINT`
  lines (every exit code 0), of the 288 `GRID` rows and of the 181 sorted
  sweep result lines prints nothing. The branch binaries were built from
  the working tree at 11:28 UTC, before three later edits: the
  clippy-driven `short_of` spelling (same truth table), `CenterIndex::insert`
  replacing an id still present, which only an id wrap onto a live track
  reaches, and the move of the selection chain and the spawn scan out of
  `step_hop` (see Decision). After all three, `decode_fingerprints` was
  re-run on 915db0e and on the final tree, one after the other (release
  builds, `MANTA_GIT_SHA=man524cmp`, separate target directories; run in
  parallel the two exceed a 9 GB memory limit): 46 identical lines, every
  exit code 0. The grid and the sweep were not re-run.
- The close-pair sweep reports `merged: 0` on all 181 lines at 915db0e,
  so it does not exercise the merge path end to end. Merge equivalence
  rests on the unit tests above and the differential test's merge-pair
  check.

## Bench

`cargo bench -p manta-engine --bench cpu_budget --locked --
'cpu_budget/192khz_300tracks$'`, Criterion's default settings for this
bench (10 samples, 150 s measurement, 1 s warm-up), base saved as baseline
`main` and the branch compared against it, back to back on one host: AMD
Ryzen 9 5950X (32 threads), rustc 1.98.0 (88d9e12ae 2026-08-18). The host
was shared and loaded throughout (load averages 15–23, against 6–8 in
the research run). Each figure is slower than its counterpart from the
research run on the same host model: 18.888 s against 15.161 s at base,
and 6.404 s against 6.370 s for the research's uncommitted spike of the
same change.

| Build | Criterion time [low mean high] | Per hop (mean / 5,625 hops) | Load average at start |
|---|---|---|---|
| 915db0e (baseline `main`) | [17.988 s 18.888 s 19.798 s] | 3.358 ms | 17.98, 20.74, 22.84 |
| MAN-524 | [6.0882 s 6.4039 s 6.7189 s] | 1.138 ms | 16.83, 17.52, 19.33 |

Criterion reports the change as [−68.348% −66.096% −63.823%] (p = 0.00 <
0.05), "Performance has improved". The base ran first, then the branch,
four minutes apart. Both runs predate the move of the selection chain
and the spawn scan out of `step_hop`, which was not re-benched.

The bench scene holds 286 tracks, not 300: MAN-311's FT8/JS8/FT4 windows
veto the 14 signals at +74.3 to +82.8 kHz. This record supersedes, as the
current figures, the MAN-18 record's 0.53–0.58× Mac reading and the
MAN-112 record's +50.9%; both predate MAN-112's quadratic checks or this
change, and neither record is edited.

## Limits and follow-up

- No Pi 4 or quiet-Mac measurement. Both M2 CPU-budget legs stay open.
- The `cpu_budget_mac_under_half_core` gate test fails at
  `assert_eq!(sustained, 300)` (286 tracks since MAN-311) before it prints
  a ratio. Fixing the scene changes the bench workload and needs its own
  decision.
- The rest of `step_hop` (per-channel dB conversion, floor and gate
  updates, the co-modulation pass, per-track bodies) is unprofiled; the
  research measured about 467 µs per hop of it at 297 tracks.
- The envelope-correlation cost for dense pairs 0.5 to 1.0 channel apart
  is unmeasured.
