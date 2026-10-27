# MAN-292: reconcile completed outages across long sample gaps

Date: 2026-10-26. Ticket: MAN-292, deferred from PR #219 review round 13.
This refines MAN-96's D-A availability accounting and preserves D-F's
classification of gaps longer than three sampling intervals as unobserved.

`scripts/field-node.py` now reconciles a positive completed `source_down_s`
delta across a contiguous same-process run of down and unobserved intervals.
The run already counts against availability. Only the delta exceeding its
duration moves from up to down in the immediately preceding healthy interval.
The gap itself stays unobserved. A delta covered by the run moves no time,
and the existing accounting cap prevents moving more healthy time than remains.
The excess is taken per source: each source's outage is one wall-clock
interval ending in the run, so what moves is the longest single-source excess
over the run, never the summed multi-input delta minus the run (overlapping
source-seconds are not elapsed time; PR review). Two inputs each down 200 s
inside a 340 s gap move nothing.

For samples at 0 s healthy, 60 s unhealthy, and 400 s healthy with a 390 s
completed-outage delta, a 60 s sampling interval and a report ending at 460 s:

| Bucket | Before | After |
|---|---:|---:|
| Up | 120 s | 70 s |
| Down | 0 s | 50 s |
| Unobserved | 340 s | 340 s |
| Counter-derived down | 0 s | 50 s |

The correction moves 50 s out of `(0, 60]`. Availability becomes `70/460`;
the separate source-outage total remains 390 s. A diagnostic 20% threshold
now returns FAIL with exit 1 in both JSON and Markdown. The production
threshold remains 99%.

Counter reconciliation requires known, equal `start_time` values on every
pair it crosses. A changed or missing process identity stops the backward
walk. Missing or decreasing counters contribute no positive delta. A later
short pair can reconcile through an earlier long gap under the same rules.
Ordinary healthy-pair accounting, short-pair restart splitting, planned
exclusions, report fields and output formats retain their existing behavior.

`scripts/tests/test_field_node.py` covers the corrected CLI output, deltas
covered by the unavailable run, mixed down/gap runs, process boundaries,
missing/decreasing counters, the healthy-time cap and conservation of time.
Existing sub-interval, unhealthy-run and restart tests cover short gaps.
