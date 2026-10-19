# MAN-291: `field-node.py report` fails ledger integrity until a sample follows the window

`report` counts the spots emitted between the last in-window sample and the
window end from the first reachable sample after the window. Run before that
sample existed (right at `END`, or right after `note --kind end`), it counted
that tail as 0. A recorder gap in the last partial interval then dropped out
of `spots_missed`, and Stage 1's D-I gate could read GO. PR #219 (MAN-96)
review thread `PRRT_kwDOTQvU8M6qmtCB` found it; the runbook asked the
operator to wait one scrape interval, and `report` now enforces that.

The code is `summarize` and `render_markdown` in `scripts/field-node.py`, the
tests are in `scripts/tests/test_field_node.py`, and the operator steps are
§5, §7 and §9 of [the field-node runbook](../RUNBOOKS/secondary-skimmer-field-node.md).
This changes the `report` behaviour that
[the MAN-96 record](2026-10-07-man96-secondary-skimmer-field-node.md)
describes; its D-A to D-N thresholds are unchanged.

## Decisions

- **D1 — A missing post-window sample fails ledger integrity.** With
  `--spots-dir`, when the last reachable sample in the window is before the
  window end and no reachable sample carrying `manta_spots_total` follows the
  window, `spots_missed` is null, `spots_missed_incomplete` and
  `integrity.missing_post_window_sample` are true, `integrity.pass` is false
  and the verdict is FAIL (exit 1). It fails rather than warns because a
  rerun one scrape later fixes it, while a warning under `## Daily` can sit
  below `**Overall: PASS**`.
- **D2 — `incomplete`, not `indeterminate`.** The `Spots missed by the
  recorder` line says `incomplete` and tells the operator to run `report`
  again after the next scrape. `indeterminate` stays for the causes a rerun
  cannot fix (a restart in the window, no sample before the window start);
  when both apply, the line says `indeterminate` and integrity still fails.
  `shadow-compare.py --missed-spots` is unchanged and rejects `incomplete`
  (exit 2), so the word cannot be passed on by mistake.
- **D3 — A sample exactly at the window end closes the window.** No
  post-window sample is needed then, and a later sample's spots and restarts
  no longer count. Before, a sample at `--to` followed by more spots raised
  `spots_missed`, and a restart after `--to` made it indeterminate. A window
  with no `--to` and no end note ends at the last sample, so a report on a
  ledger whose newest sample is reachable never needs one.
- **D4 — Only with `--spots-dir`.** Without it `report` computes no
  `spots_missed`, nothing uses the tail, and integrity is unchanged.
- **D5 — The resolved window end.** The check uses the end `report`
  resolves (`--to`, else the first end note, else the last sample), so it
  also covers the day-30 path (`note --kind end`, then `report`). Runbook §9
  now waits 90 s after the end note.
