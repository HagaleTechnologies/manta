# MAN-208: bounded delayed refinement

The causal decoder remains the default first pass. On every decode path --
single-input and multi-input `listen`, and offline `manta decode` -- a scheduler
replays preserved decoder inputs for ambiguous words under an
alternate engine, then a seeded timing hypothesis when new context arrives.
`[decode.refine] enabled = false` opts out. Defaults are ten seconds of retained
hops, two attempts, 1500 hops per batch and sixteen waiting episodes. Retention
has an additional global ceiling of 8 MiB of estimated entries.

An attempt revises the same observation id and span. Reassigning the call moves
its repetition support. A spot uses the newest real supporting RF timestamp;
the observation separately records the sample position at replay completion.
Repeated hypotheses cannot manufacture a repetition.

## Measurement boundary

`crates/manta-engine/tests/man208_late_rescue.rs` freezes an event/hop fixture.
The causal first reading is K3ABC with a D alternative. Preserved rectangular
25 WPM hops encode K3ABD; a separate K3ABD event repetition arrives eight seconds
later at 96 kS/s. The base engine is HSMM, so the first replay is legacy.
This is a scheduler/validator experiment, not a measured RF recall improvement.
It does not exercise the default legacy-to-HSMM direction through a channelizer.
No Watterson or AWGN result, CPU benchmark, or live radio measurement is claimed.

The rescue test compares an empty causal spot list with exactly one refined
K3ABD spot. It checks that the spot time equals the second repetition's boundary.
The decision is requested 32000 samples later, an additional 333 ms at 96 kS/s.
The disabled control emits no rescue. The unambiguous control admits zero
attempts. The budget test uses seventeen hops per batch and asserts both
progress and the per-batch ceiling without replacing the scheduler. Overflow and
per-episode cap have separate deterministic tests. The fixed rescue consumes 1423 replayed hops, admits one episode, pauses zero
batches and drops zero backlog entries. It moves from zero to one spot, or one
fixture rescue per 1423 extra hops. RF time is 1768500 and decision time is
1800500. These counts describe the boundary fixture only. The fixture prints the actual
replayed-hop count and spot/decision timestamps with `--nocapture`.

These tests establish evidence accounting and bounded execution only. They do
not establish true rescue rate per replayed hop on noisy RF, delayed false rescue
rate, or production CPU cost. Those measurements need a frozen RF corpus with
coppa pinned to f8a4d16d and an enabled/disabled comparison that discloses latency.

## Wiring (validate round 2)

The offline loop now validates each batch as it is produced and runs
`TrackManager::refine_step` after it, so `manta decode --observations` writes
`cause: "refined"` revisions (`offline_decode_runs_refinement_and_records_the_revision`).
Multi-input front ends export their retained decoder inputs on the common clock
with each batch; the shared back end keeps them beside the validator and runs
the same scheduler (`SharedRefine`). One input gives identical scheduler counters
through either live path (`both_live_paths_run_the_refinement_scheduler`).

The pass's clock is its owner's stream position. An earlier version took the
maximum event timestamp of the batch, and `TrackClosed` sorts at `u64::MAX`, so
any unrelated closure expired every retained episode and its cap bookkeeping.
A call-changing refinement now revises the rival ledger occurrence as well as
the repetition gate's, so the rival check no longer weighs the superseded
reading. The speed trigger requires a stable estimate: two words decoded since
the track's last `SpeedUpdate` (which fires only on a 1 WPM change).

## Current limits

On the rendered rival scene (`man208_rival_scene`: the V1 signal at 20 dB SNR, seed 1)
the engine-flip hypothesis read no word over the occurrence's span in either
direction; only the seeded-timing replay of an HSMM base produced a reading. The
alternate-engine hypothesis therefore needs RF tuning (preroll length, the
fields each engine reads) before any recall claim. The candidate prefix in
`apply_refinement` still mirrors the causal checks rather than sharing one
helper.
