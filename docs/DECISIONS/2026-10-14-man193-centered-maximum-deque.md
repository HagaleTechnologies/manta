# MAN-193: Evidence's centered maximum is a monotonic deque

## Decision

`manta-decode::evidence` keeps SPEC v2 §1.2's centered maximum `M` in a
private monotonic deque, `CenteredMax`, owned by each `Evidence`. It
replaces the two full-window scans `Evidence::emit` made per emitted hop:
one for the hop's own `M`, one for the sign-edge lookahead's `M` of the
next center (§1.7).

- The deque holds `(global input index, smoothed amplitude)` candidates.
  Indices increase and values strictly decrease, so the front is the
  maximum. Keys are push-order input indices, never delay-line positions
  or `sample_ts`, so trimming the delay line at `MAX_RETAIN` does not move
  them.
- Each actual computation queries the same clipped inclusive window the
  scan used, `[max(front, c − h), min(last, c + h)]`. If both bounds are
  at or past the previous query's, the deque expires candidates from the
  front and admits only the samples past the previous right bound. Before
  each admission it drops every back candidate the new sample equals or
  exceeds.
- If either bound moved backwards, the deque is cleared and rebuilt from
  the retained samples of the new window. This happens when a `set_u_ref`
  grows `h` by more than one hop between consecutive centers (the left
  bound retreats) or shrinks it by more than one hop (the right bound
  retreats), unless the delay line's ends clip that bound. A discarded
  sample can be the maximum again after a retreat, so the deque cannot
  simply carry on.
- The lookahead's next-center maximum is read without changing the deque.
  At the same `h`, the next window drops at most the current window's
  leftmost sample and gains at most one sample on the right. The peek
  reads the front candidate, or the second one if the front is that
  dropped sample, then folds in the entering sample. The deque never
  stores the predicted value: the next center can still see a new sample
  or a new `h` before it is computed.
- NaN and nonpositive smoothed amplitudes are never stored. The empty
  deque returns `0.0`, the scan's zero-seeded `f32::max` floor, so NaN
  still cannot become `M`.

The gate, LLR, three-way sign, anchor, prefix, center counter, retention,
release eligibility and the decoder's `set_u_ref` feedback order are
unchanged. So are the public `Evidence` methods, configuration, defaults
and every output. `decode.engine` still defaults to `legacy`.

## Cost

| Window schedule | Maximum-maintenance work |
|---|---|
| Fixed `h` (the ticket's 300 tracks at h = 224) | Each sample admitted and removed at most once: O(N + E) for N inputs and E emitted hops, O(1) amortized per hop. Each lookahead reads at most two candidates and one entering sample. |
| `h` changes, both bounds nondecreasing (one-hop steps, repeated setters that round to the same `h`) | Same as fixed. Rounding to the same `h` changes nothing. |
| A bound retreats | One rebuild of the new window, O(2h + 1), then incremental again. |

Overall work is O(N + E + the summed lengths of rebuilt windows). A
caller that alternates between wide windows on every center still pays
O(E·h). That is a limitation of an ordinary monotonic deque under
arbitrary resizes, not something this change claims to solve. Nothing
throttles, quantizes or delays the speed feedback to hide it.

## Evidence

Tests in `crates/manta-decode/src/evidence.rs`:

- `centered_max_tests` checks the helper against an independent zero-seeded
  `f32::max` fold. It covers shaped and seeded streams at h = 1, 60, 224
  and 1024, startup and EOF clipping, empty queues and disjoint windows. It
  also covers growth that restores a left peak, shrinkage that restores a
  dominated value, one-hop width changes, 223/225 oscillation and seeded
  jumps, retention origins past 3 × `MAX_RETAIN`, every lookahead branch,
  and NaN, infinity, negative and signed-zero values. Test-only counters
  at the operations themselves bound admissions and removals by N, back
  comparisons by 2N and lookahead reads by two candidates plus one sample,
  independent of `h`.
- A `Twin` driver makes the same calls on a cached `Evidence` and a
  test-only reference that keeps the original scan. It compares every
  `drain_one`/`flush_one` result bit for bit, including `None`, across
  the lifecycle cases. Those cases cover fixed widths, same-rounded
  setters, resizes between individual drains and flushes, short and empty
  inputs, 3 × `MAX_RETAIN` inputs with resizes near rollover, and a
  right-edge peak recorded after the lookahead ran.
- `centered_max_work_is_linear_for_300_tracks_at_h_224` is the ticket's
  scenario: 300 interleaved tracks, 3,750 hops each and a full flush. A
  setter that rounds to the same `h` runs after every drained hop. Each
  track must make at most N + E smoothed-amplitude reads for `M`. Against
  the original scan it failed with 3,262,950 reads for track 0 against a
  7,500 budget, about 870 per hop. With the deque it passes.

`benches/evidence_300_tracks.rs` runs 300 tracks × 3,750 hops of
prebuilt deterministic keyed amplitudes (a per-track phase of one 104-hop
dit/dah pattern at 40 dB depth with a slow ripple, noise 0.005) through the
public `push`/`flush`. It checks each case's emitted count outside the
timed loop. The same driver ran against the pre-change base
`525edb731d8b6321aea2f286716992d30ef462b1` in a scratch worktree and
against this change. Both ran on one AMD Ryzen 9 5950X cloud host with
rustc 1.98.0, the `bench` profile and Criterion with 10 samples. Every
case's emitted count (1,125,000) and checksum matched between the two
builds.

| Case | Base mean [95% CI] | MAN-193 mean [95% CI] | Checksum (both) |
|---|---|---|---|
| fixed h = 60 | 431.71 ms [411.70, 450.24] | 76.88 ms [74.80, 79.13] | 1052699.126897 |
| fixed h = 224 | 1461.12 ms [1389.50, 1546.25] | 72.24 ms [69.03, 75.24] | 725678.298375 |
| resize stress, h 223 ↔ 225 every third pushed hop | 1463.97 ms [1420.95, 1516.91] | 777.78 ms [733.69, 824.29] | 725738.370851 |

The fixed cases no longer depend on `h`. The stress case rebuilds about
once per three emitted hops (1,139 rebuilds and 506,782 rebuilt admissions
per 3,750-hop track, counted by
`the_benchmark_resize_stress_schedule_rebuilds_about_every_third_hop`),
which is what its remaining cost is.

`hsmm_300_tracks` (`TrackDecoder` with the HSMM engine, fed prebuilt
envelopes) measured 27.374 s [26.006, 28.687] per iteration on the same
host with `--sample-size 10 --warm-up-time 1`. It was not run against the
base. For the same 300 × 3,750 hops, the Evidence-only benchmark's base
build took 0.43 s at h = 60 and 1.46 s at h = 224, so Evidence was a small
part of that time. This is not a full-pipeline or Pi 4 result and does not
close MAN-171's CPU-budget overage.

## Limitations

- Repeated bound retreats still cost a rebuild each, as above.
- The benchmark measures `Evidence` alone; the decoder's CPU budget is
  still MAN-171.
- Timings are single-host measurements; the deterministic work-count test
  is the regression gate.
