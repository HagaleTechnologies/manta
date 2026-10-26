# MAN-290: shadow-compare requires the RBN archive's `tx_mode` column

Date: 2026-10-26. Ticket: MAN-290, a P2 finding deferred from Codex review
round 8 of PR #219 (MAN-96), thread `PRRT_kwDOTQvU8M6qktNz`, under
`docs/DECISIONS/2026-08-07-pr-review-convergence-policy.md`.

## Context

`load_rbn` (`scripts/shadow-compare.py`) required only `callsign`, `freq`,
`dx` and `date`, and dropped non-CW rows only `if has_tx_mode`. A CSV with
no `tx_mode` column kept every row, so an FT8 or RTTY row could confirm or
corroborate a node spot and lower the "Uncorroborated" percentage the D-I
gate reads (D-H: CW rows only). One node spot (`AB1CDE`, 14050 kHz) against
a one-row RBN CSV holding a matching `W3LPL` row 30 s later, at `40d275a`:
with no `tx_mode` column, exit 0 and `Uncorroborated: 0 / 1`; with
`tx_mode` = `FT8`, `1 / 1` (the row counted non-CW); with `CW`, `0 / 1`.
The row that could not be read was treated as CW.

## Decision

`tx_mode` joins `RBN_REQUIRED_COLUMNS`. A CSV, or a zip's CSV member,
without it is an input error like any other missing required column:
`<path>: RBN CSV lacks column(s) tx_mode` on stderr, exit 2, no markdown
and no `--json` file. The `has_tx_mode` guard is gone; every row whose
`tx_mode` is not `CW` is dropped first. RBN's `mode` column is no
equivalent to filter on: it holds the spot type, `CQ` on all 26,802 rows of
`crates/manta-testkit/audio-corpus/ground-truth/B2_20251129_000000_7080kHz.rbn.csv`
and on all 10 rows of `crates/manta-cli/tests/fixtures/rbn_grade/rbn-20261011.csv`,
whose `tx_mode` holds 9 `CW` and 1 `FT8`. There is no escape-hatch flag.

## Consequences

A standard archive gives byte-identical markdown and `--json` output: on
the suite's fixtures, and on the 26,802-row excerpt above against 180
synthetic node spots drawn from it (primary `YO5LD`: 139 confirmed, 41
corroborated, 0 uncorroborated, before and after). A transformed or legacy
CSV without `tx_mode` now stops the run; the field-node runbook says to
pass the zips as downloaded. `manta grade` (`load_rbn_csv`) still treats
`tx_mode` as optional; it feeds no D-I number, so requiring it there is a
follow-up candidate, not part of this change. Regression tests:
`test_non_cw_row_neither_confirms_nor_corroborates` and
`test_rbn_csv_without_tx_mode_is_input_error_exit_2` in
`scripts/tests/test_shadow_compare.py` (33 tests OK; the second fails in
both its subtests with the fix reverted — run directly, as CI does not run
this suite).
