# MAN-189: the HSMM decoder commits each stretch of time once

## Decision

`HsmmDecoder` (`manta-decode::hsmm`, the opt-in `decode.engine = "hsmm"`)
no longer records what it has committed as a list of exact
`(sample_ts, is_char)` pairs (`sealed`). It keeps one invariant instead:
**emitted entries are one time-ordered segmentation of the signal.**

Each history entry (`token::HistEntry`) also records `char_start_ts`, the
input sample counter at the anchor where its character's first mark
began; a word boundary carries the character it follows. A
`commit::Frontier` holds the latest emitted `sample_ts` (`last_ts`) and
the latest emitted word-boundary `sample_ts` (`last_word_ts`):

- **Character entry:** decided if `char_start_ts <= last_ts`.
- **Word boundary entry:** decided if `char_start_ts <= last_word_ts`,
  meaning a boundary was already emitted at or after its character began,
  *or* if `sample_ts < last_ts`, meaning its gap began before something
  already emitted.
- **Emitting:** `last_ts = max(last_ts, sample_ts)`; for a word boundary,
  also `last_word_ts = max(last_word_ts, sample_ts)`.

`commit::commit()` strips decided entries from the front of every token
at the top of each loop iteration. `finish()` removes them from every
live token with `retain`, and emits its unclosed `AfterMark` tail glyph
only when the frontier does not call that character decided. Character
starts strictly increase along a token's `hist`, and a word boundary
shares its character's start and closing time, so decided entries always
form a prefix of a token's `hist`.

Consequences, each pinned by a unit test in `hsmm::commit::tests` or
`hsmm::tests`:

1. The ticket's case: the same glyph, the same start and a different
   close is stripped.
2. A first mark placed at a later anchor inside the committed character
   is stripped.
3. A repeated glyph that starts after the commit (VR3's `N5ZZ`) is still
   emitted. Sealing by glyph value would be unsound.
4. A conflicting segmentation that ends before the committed close is
   stripped (the out-of-order `T@1611 after B@1795` class).
5. A word boundary at the same gap as an emitted character, from the WGap
   lineage after the CGap lineage won, is still emitted once.
6. A second word boundary for the same character is stripped.
7. A late word boundary behind a later character is stripped.
8. Output `sample_ts` never decreases within a track.

## Mechanism and evidence

A history entry's `sample_ts` is the anchor where its closing gap began.
The decoder keeps old anchors alive for up to `80·u_max` hops, and
fallback anchors fire every 8 hops, so a lineage can read the same
character but close it at a neighbouring anchor. Under `sealed` that
variant had the same glyph and a different `sample_ts`, never matched,
and was committed a second time. A lineage that split already-committed
marks differently was committed out of time order for the same reason.

The measurements below were taken during planning on `30e6f41` (main
before this change) with a prototype that is byte-identical in output to
the committed code on all 15 generated vectors.

VR6a, `manta decode --json --engine hsmm`:

```
… E@39819 E@39818 S@39946 T@40036 …      (CharDecoded glyph@sample_ts/256)
text: "… F6XYZ F6XYZ TEEST CQ TEST …"
```

Per-track `CharDecoded`/`WordBoundary` whose `sample_ts` is lower than the
previous one on that track:

| vector | backwards commits on main | example | with frontier |
|---|---|---|---|
| VR6a | 1 | `E@39818 after E@39819` | 0 |
| V7 | 3 | `B@2120 after B@2123` (decodes `N2BBB`) | 0 |
| VR8 | 19 | `T@1611 after B@1795` | 0 |
| V5 | 26 | `K@844 after C@896` | 0 |

Commits whose character began at or before the latest emitted entry, on
main:

| vector | DUP (same glyph, overlapping time) | CONFLICT (different glyph) | ORDER | total |
|---|---|---|---|---|
| V5 | 27 | 19 | 13 | 59 |
| VR8 | 15 | 17 | 8 | 40 |
| VR5 | 2 | 7 | 0 | 9 |
| VR6a | 1 | 0 | 0 | 1 |
| V1–V4, V6, VR1–VR4, VR6b, VR7 | 0 | 0 | 0 | 0 |

CER per signal (same normalised Levenshtein as `manta_testkit::cer`, on
the track nearest each expected frequency):

| vector | CER main | CER frontier | chars main→frontier |
|---|---|---|---|
| V1, V2, V3, V4, V6, VR1, VR2, VR3, VR4, VR6b, VR7 | unchanged | unchanged (identical text) | unchanged |
| V5 | 1.4062 | 1.1771 | 237→205 |
| VR5 (sig 0 / sig 1) | 0.1582 / 0.7228 | 0.1497 / 0.6966 | 308→300 |
| VR6a | 0.0065 | 0.0032 | 248→247 |
| VR8 | 0.6687 | 0.5604 | 316→289 |

No vector got worse. `v7_passes_end_to_end_with_hsmm_engine` (was CER
0.0540, `N2BBB`; the stage-2 record's "0.004 near-miss") and
`v8_pileup_validates_at_least_45_of_50_with_hsmm_engine` (was failing on
the bogus spot `K2ST`, real call `K2STA`) now pass and are no longer
ignored. `crates/manta-cli/tests/hsmm_commit_order.rs` checks the
observable invariant on VR6a and V7.

HSMM JSON bytes change even where the text is identical, because
stripping a decided entry from a non-best token can let consensus fire
on an earlier hop, which reorders `CharDecoded`/`WordBoundary` relative
to `SpeedUpdate`/`TrackMeta`. Nothing pins HSMM bytes across builds;
`determinism_hsmm.rs` still compares runs of one build.

The planning criterion run of `hsmm_300_tracks` measured no change
(−2.4 %, p = 0.50): each strip check is O(1), where `sealed.contains` was
O(|sealed|), and the per-anchor-step `min_hist_ts` scan is gone.

## What it replaces

- **`sealed`** (PR #161, Task 8 and review rounds 2 and 6): the
  `(sample_ts, is_char)` list. It stripped a re-derived entry only when
  its exact closing timestamp matched, which is the gap MAN-189 reports.
  Frontier plus `sealed` was measured byte-identical to the frontier
  alone on 15 vectors, so `sealed` is removed rather than kept alongside.
- **`Anchor::min_hist_ts` and the `sealed` prune block** (rounds 3 and
  4): they existed only to bound `sealed`'s growth. The frontier is two
  integers and needs no pruning.
- **The round-7 test's second assertion.**
  `finish_does_not_reemit_an_entry_sealed_by_a_different_lineage`
  injected `sealed = [(200, true)]` and asserted that a live `A@100` was
  *still emitted* after the committed `B@200`. Under the frontier an
  entry that ends before an already-emitted entry is out of order:
  emitting it would produce `…B A`, the same text corruption as the
  CONFLICT/ORDER rows above. The test is now
  `finish_does_not_reemit_an_entry_decided_by_a_different_lineage`: a
  straggler holding the committed `B` at a later close plus a genuinely
  new `C` emits only `C`. The round-15 re-sort test keeps its assertion
  with an injected frontier in place of `sealed`.

## Not done

- **Eager anchor or token retirement** (dropping mid-character tokens
  whose `char_start_ts` is at or before the frontier). Lazy stripping
  matches how `sealed` worked and already gives zero violations on every
  vector; eager pruning would change beam contents and scores beyond this
  bug.
- **The stale V2/hsmm and VR1 ignores.** Both pass under
  `--include-ignored` before and after this change; their `#[ignore]`
  reasons are out of date but unrelated to MAN-189. V5, V8w, V10, VR2,
  VR5 and VR8 still fail and stay ignored.
- The stage-2 oracle numbers were not re-measured. The stage-2 verdict
  stays FAIL and `decode.engine` stays `legacy`.
