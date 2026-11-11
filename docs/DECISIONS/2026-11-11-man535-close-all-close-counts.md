# MAN-535: count source-end and reconnect track closes

The 2026-11-07 bug review identified that `close_all()` cleared every
track without recording closes. MAN-179 had noted this undercount. This
record supersedes its statement that `close_all()` records nothing;
the MAN-179 figures remain historical measurements.

## Reproduction

Run `cargo test -p manta-engine --locked --test close_all_counts`.
The fixed-seed scene contains 40 keyed signals. The plan-phase reproduction
at 86194a4 reported 47 spawns and 7 closes at EOF, or 94 spawns and 14
closes with a reconnect followed by EOF. The regression requires 47 and
94 closes respectively, including 40 source-end closes in both cases and
40 reconnect closes in the latter. Counts include unconfirmed candidates.

## Decisions

1. Add counter fields, without changing `ClosureKind` or `CloseReason`.
   Existing signal-ended and bookkeeping events already describe the teardown.
2. Count the track map length before clearing it. Candidates without events
   count too. An empty second teardown adds zero.
3. Name the fields and labels `source_end` and `reconnect`. The latter
   includes stalls, read errors and stops during outages.
4. Keep the eleven labels in manta-server. The CLI maps engine counts with
   an exhaustive destructure, excluding `false_lease_hops`, which counts hops.
5. Attach a lifecycle observer to single-input server runs. The existing
   250 ms poller copies counts, including during stalls, and shutdown takes
   a final snapshot.
6. Give every multi-input front an observer and sum them in the daemon's
   per-batch callback. Publish teardown counts before sending the batch.
7. Multi-input observers publish closes and spawns only. The shared
   validator owns gate counts, and the existing gauge owns active tracks.

## Metrics

`manta_track_closes_total{reason}` is a counter with eleven series, zeros
included from startup. Its labels are `restricted`, `unconfirmed`,
`hang_expired`, `silent`, `merged`, `evicted`, `rtty`, `not_cw`,
`unkeyed`, `source_end` and `reconnect`. Counts accumulate across source
segments and inputs. Each rendered series uses a monotonic maximum.
After source end, the sum equals all tracks spawned, including candidates.
This population differs from `manta_active_tracks`, which counts decoders.
No counters feed back into decoding or change spot output.
