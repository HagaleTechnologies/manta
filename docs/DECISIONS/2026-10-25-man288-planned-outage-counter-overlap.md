# MAN-288: planned windows absorb counter-attributed downtime

Date: 2026-10-25. Ticket: MAN-288, deferred from PR #219 review round 8
under the review convergence policy. This refines MAN-96 D-A availability
accounting in `scripts/field-node.py`.

## Problem

The report removed planned-window overlap from an interval's eligible time,
but still charged the full source-outage counter delta against that time.
A 60-second interval with 20 planned seconds and a 20-second counter delta
therefore reported 20 seconds up and 20 down. Its 50% availability could
falsely fail D-A even when the outage fit within the planned window.

## Decision

After clipping each segment to the evaluation window, let `P` be its overlap
with the merged, registered planned windows, `L` its duration minus `P`, and
`E` its accumulated counter-attributed downtime. An otherwise-up segment
contributes `min(max(E - P, 0), L)` to both `down_s` and `counter_down_s`.
The rest of `L` contributes to `up_s`. A fully excluded segment contributes
nothing. Availability divides `up_s` by the evaluation span minus excluded
planned time, retaining the existing zero-denominator behavior.

Counters record duration without exact outage timestamps. The report credits
planned overlap against the segment's counter duration first. This is an
allocation assumption; it does not establish when the outage happened.

The subtraction happens once, in `account()`, after the interval walk has
assigned all counter duration. It covers both direct deltas and the remainder
moved back from an unhealthy run into the preceding healthy segment. Sampled
downtime and unobserved time retain their existing planned-window exclusion.

| Counter path | Planned seconds | Effective down seconds | Counter down seconds | Up seconds |
|---|---:|---:|---:|---:|
| Direct, 20-second delta in a 60-second interval | 20 | 0 | 0 | 40 |
| Direct, same interval | 10 | 10 | 10 | 40 |
| Direct, no planned note | 0 | 20 | 20 | 40 |
| Reassigned, 80-second delta over 120 seconds with 60 sampled down | 20 | 60 | 0 | 40 |
| Reassigned, same intervals | 10 | 70 | 10 | 40 |
| Reassigned, no planned note | 0 | 80 | 20 | 40 |

Raw `source_outages.down_s` still reports the full counter duration: 20 seconds
for the direct cases and 80 for the reassigned cases. JSON and Markdown use
the corrected effective downtime and availability for D-A. In the full-overlap
direct example, availability becomes 100%, and the completed synthetic report
passes D-A at the existing 99% threshold.

## Compatibility and verification

MAN-279 still requires a changed process start time before a planned window
registers. Its capped-window evidence search, window bounds and restart
classification are unchanged. A cancelled note excludes nothing, so its
counter duration still counts in full. Caps, long-gap handling, report fields,
command syntax and production thresholds are unchanged.

`scripts/tests/test_field_node.py` exercises all six rows through `summarize`,
using later restart evidence to register the capped windows. It also checks
a 5-second delta under 20 planned seconds, a cancelled note, raw totals,
eligible-time conservation and the real CLI's JSON and Markdown output and
exit status. The field-node Python suite must run explicitly; it is not part
of the CI workflow's enumerated Python suites. The workspace docs-consistency
test checks this record's index entry. Synthetic ledgers cover this report
correction without live SDR access.
