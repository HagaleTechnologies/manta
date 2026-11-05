# MAN-208: bounded delayed refinement

The causal decoder remains the default first pass. On the single-input listen
path, a scheduler replays preserved decoder inputs for ambiguous words under an
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

## Current limits

Offline `manta decode` does not yet run this scheduler. Its separate
`decode_source_observed` loop validates events after the input and track
managers have finished. The plan's CLI observations example therefore needs
additional offline-loop wiring; the live boundary fixture does not prove it.

Multi-input batch decoding does not yet run this scheduler. Its shared validator
uses a common clock while each input owns a local-clock track manager; directly
passing the manager into that shared boundary would mismatch retained spans.
The candidate prefix in `apply_refinement` still mirrors the causal checks
rather than sharing one helper. Speed triggers use the current tracker estimate;
there is no separate stability confidence contract. These limits are declared
in the implementation run's deviations for validation.
