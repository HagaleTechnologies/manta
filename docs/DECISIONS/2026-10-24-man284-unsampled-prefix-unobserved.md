# MAN-284: `field-node.py report` counts the time before the first sample as unobserved

When the resolved window start (a `start` note, or `--from`) precedes the
ledger's first sample, `summarize`'s interval walk credited that unsampled
prefix as up or down from the first sample's state whenever the gap was at
most 3 × interval — which happens naturally when the Stage 1 start note is
written before the ledger service's first scrape. Nothing observed the node
during the prefix, so per D-A it is unobserved time, yet it could read as
100% up and tip D-A to PASS. PR #219 (MAN-96) review round 6 thread
`PRRT_kwDOTQvU8M6qhuV8` found it; the prefix is now unobserved
unconditionally.

The code is `summarize` in `scripts/field-node.py`, the tests are in
`scripts/tests/test_field_node.py`, and the operator steps are §5, §7 and
§9 of [the field-node runbook](../RUNBOOKS/secondary-skimmer-field-node.md).
This changes the `report` behaviour that
[the MAN-96 record](2026-10-07-man96-secondary-skimmer-field-node.md)
describes; its D-A to D-N thresholds are unchanged.

## Decisions

- **D1 — The unsampled prefix is unobserved, always.** With no sample at or
  before the window start, the time from that start to the first sample is
  unobserved whatever its length and whatever the first sample read.
  Before, a prefix of at most 3 × interval took the first sample's state,
  up or down. D-F's 3 × bridge exists to tolerate timer jitter *between two
  samples*; a window start has no sample on its near side, so there is
  nothing to bridge from, and carrying a state backward to a time before
  anything was observed is what D-A forbids. `account()` still subtracts
  planned-window overlap before crediting the prefix, so D-E needs no
  special case.
- **D2 — A sample at or before the start changes nothing.** The prefix
  block runs only when the first sample is after the start; with a sample
  at or before it, the interval walk is unchanged.
- **D3 — The trailing edge is unchanged.** The `[last sample, end]` tail
  still carries the last sample's state forward, as every pair interval in
  the walk does, from a ledger that was demonstrably running; the prefix is
  the only place the walk carried a state backward, to a time before
  anything was observed. In every report the runbook runs (§5, §7 and §9,
  each with `--spots-dir`), a window end with no later sample already fails
  ledger integrity (MAN-291 D1). The ticket, its Gherkin and the review
  thread name only the prefix.
- **D4 — Small but decisive near the threshold.** The miscredited prefix is
  at most 3 × interval (about 0.2% of a 24 h window at a 60 s interval), so
  it moves D-A only near the 99.00% line — exactly where a Stage 1 or
  day-30 window sits after real outages, and where a false PASS is most
  costly.
