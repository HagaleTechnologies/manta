# MAN-525: constant-cost refinement bookkeeping

MAN-208's retention recorded every decoder input of every decoding track,
and each `RefineBuffers::record` summed every ring and, over the 8 MiB
global bound, scanned every ring once per evicted entry. More than 46
tracks put a node over the bound on every record, so recording cost
O(tracks) per hop. Candidate selection also folded over every track for
each candidate word and cloned strings per batch. This change makes the
bookkeeping cheap and leaves every outcome as it was.

## Reproduced cost (before)

Measured by the plan phase on main at 915db0e, release build, a shared
32-vCPU container, round-robin recording after a warm-up past the bound:

| Tracks | ns per `record` | Retained per track |
|---|---|---|
| 1 | 9.2 | 10.00 s |
| 46 | 67.7 | 10.00 s |
| 47 | 171.8 | 9.92 s |
| 100 | 375.5 | 4.66 s |
| 300 | 1107.1 | 1.55 s |

`refine_candidates` with one ambiguous word per track, 500 Hz apart: 5.13 us
per call at 25 tracks and 61.58 us at 125 tracks (quadratic in tracks).

## Design

- **Running count and front index (D1).** `RefineBuffers` keeps `total`,
  the sum of every ring's length, and `fronts`, a `BTreeSet<(u64, u32)>` of
  every non-empty ring's `(front sample_ts, track_id)`. Every mutator keeps
  both. Eviction pops `fronts.first()`, which is the same oldest
  `(sample_ts, track_id)` main picked by scanning. `record`, `len` and
  `is_empty` never iterate the rings. A record costs O(log tracks).
- **Eviction stays per record (D2).** The victim depends on the fronts at
  that moment, so deferring eviction to the end of a batch could change
  which entry goes.
- **Direct recording (D4).** `drain_pool` records straight from the tracks,
  in the same order, without an intermediate `Vec`.
- **Neighbour gaps once per track (D5).** `refine_candidates` builds one
  `(frequency, track_id)` vector of finite frequencies per call, on its
  first candidate, and computes each track's gap once by binary search. The
  value is bit-identical to the old fold: rounding is monotone, and
  non-finite frequencies of other tracks only ever contributed NaN or
  infinity, which `f64::min` from `INFINITY` ignores. A non-finite own
  frequency falls back to the fold itself.
- **Borrowed candidates (D6).** `RefineCandidate<'a>` borrows `call` and
  `rival` and is `Copy`. The scheduler's queue and active replay keep a
  private `Episode` with the six plain fields they read.
- **`required_reps` removed (D7).** No code read it, and computing it cost
  two allocations and a map lookup per candidate per batch. `reps` stays.
  If MAN-515 needs the requirement for prioritisation, it can add a lazy
  validator accessor.
- `queue.retain` binary-searches the sorted candidates (D8), and
  `refine_step` borrows the decode config instead of cloning it per batch
  (D9).

## After

Both probes (`man525_refine_cost::record_cost_by_track_count` and
`man208_late_rescue::refine_candidates_cost_by_track_count`, both
`#[ignore]`d) ran in one session on the same host, in release, one after
the other: first on main at bd701c6 (whose engine and validator code equals
915db0e's), then on the branch.

| Tracks | Main ns per `record` | Branch ns per `record` | Retained per track | Evictions in the timed window |
|---|---|---|---|---|
| 1 | 9.6 | 29.1 | 10.00 s | 0 |
| 46 | 93.3 | 45.7 | 10.00 s | 0 |
| 47 | 172.9 | 79.0 | 9.92 s | 299,954 |
| 100 | 380.4 | 91.9 | 4.66 s | 300,000 |
| 300 | 1125.8 | 165.3 | 1.55 s | 300,000 |
| 600 | 2370.8 | 131.8 | 0.78 s | 300,000 |

At 300 tracks a record falls from 1126 ns to 165 ns (6.8x). Main's cost
grows with the track count and the branch's does not. Retention and
eviction counts are identical on both. `refine_candidates` falls from
4.93 us to 0.94 us at 25 tracks and from 64.72 us to 4.36 us at 125 tracks
(14.8x). The plan phase's prototype gave 118 ns and 4.29 us on the same host
class.

## Accepted trade-off (D3)

Under the bound, every at-capacity push moves its ring's front, so the index
updates on every record. A single track's record rises from 9.6 ns to
29.1 ns. The 46-track figure is noisy on a shared host: 93 ns on main and
46 ns on the branch in this session, against 68 ns and 103 ns in the plan
phase. Even at 103 ns, 46 x 375 x 103 ns is 1.8 ms per stream second, 0.18%
of a core. A lazily built index would add a state transition to save that
much.

## Equivalence evidence

- `refine::tests::buffers_evict_exactly_as_the_reference_model` drives a
  verbatim copy of main's `RefineBuffers` and the new one through 40 seeds x
  4 configurations x 4,000 random operations (records with many equal front
  timestamps, track drops, per-input gaps and drops, whole-input gaps). After
  every operation the evicted count and every ring's contents match, and the
  running count and front index agree with the rings. It compared 62,182
  evictions.
- `validator::man525_tests::nearest_gap_equals_the_fold_bit_for_bit`
  compared 7,864 gaps, with NaN, +inf, -0.0 and duplicate frequencies, bit
  for bit against the old fold.
- `man208_late_rescue::a_crowded_node_under_the_global_bound_refines_exactly_as_before`
  pins 24 tracks under three tight global bounds: retained and evicted
  counts, every scheduler counter, refined records and the rescue order.
  `refine_candidates_report_the_nearest_other_track_gap` pins the gap
  values. Both pass on main's unmodified production code (bd701c6) and
  unchanged on the branch.
- `man208_refine_buffer::the_global_bound_keeps_the_newest_entries_across_300_tracks`
  and `equal_front_timestamps_evict_the_smallest_track_id_first` pin the
  eviction order and tie-break. They also pass on main's unmodified code.
- The A/B grid (`man398_grid`, release, 288 rows) ran on the branch and on
  its merge-base with main, bd701c6. `cmp` of the two `GRID` outputs exits
  0, and both hash to SHA-256
  `cb57e885664b7445f6b35a8f46381455a9a5cc29ae5e5eac9111cdb721dd142e`. That
  is also the hash the research and plan phases recorded for main at
  915db0e. Main's only commit between 915db0e and bd701c6 (MAN-516) changes
  CI files and docs, not decode code.

## Unchanged

- The lookback collapse: at 300 tracks the 8 MiB bound still leaves about
  1.55 s per track, pinned by the 300-track test. Where replay is spent is
  MAN-515.
- `REFINE_GLOBAL_BYTES`, `REPLAY_ENTRY_BYTES`, `RefineConfig` defaults,
  `replay_slice`, the `RefineFeed` export, and every config key, wire format,
  JSON field and metric.

## Limits

No Raspberry Pi 4 or Mac measurement. Probe numbers vary by tens of percent
on a shared host. The probes are never asserted on time.
