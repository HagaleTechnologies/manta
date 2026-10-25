# MAN-277: `field-node.py sample` rejects a non-finite or out-of-range `--interval-s`

Date: 2026-10-23. Ticket: MAN-277, deferred from PR #219 (MAN-96) review
thread `PRRT_kwDOTQvU8M6qedLU` under
`docs/DECISIONS/2026-08-07-pr-review-convergence-policy.md`.

## Context

`cmd_sample` in `scripts/field-node.py` checked `--interval-s` with
`<= 0`. NaN compares false against everything, so `nan` passed, and so did
`inf` and huge finite values. At `f578cb5`, `sample --loop` with `nan`,
`inf` or `1e308` appended one ledger record and then died in `sleep()` with
a traceback (exit 1). With `--recover-cmd`, `nan` died earlier, in the
`Watchdog` constructor, before any sample. The shipped
`manta-field-ledger.service` runs under `Restart=always`, so a bad value
became a restart loop that wrote one stray record per cycle until systemd's
start limit stopped the unit. `report` already rejected all of these.

## Decision

`sample` and `report` share one `validate_interval_s`: the value must be
finite, `> 0` and `<= 86400` (`MAX_INTERVAL_S`, which replaces `report`'s
`MAX_REPORT_INTERVAL_S`). A bad value is a usage error (exit 2, one stderr
line naming `--interval-s`), raised with or without `--loop`, before any
sample, ledger write or watchdog. The upper bound is `report`'s, because
`report --interval-s` must equal the interval the ledger was sampled at
(the MAN-96 record's D-F); a ledger sampled more slowly than once a day
could not be scored at its own interval.

## Consequences

- `sample --interval-s` above 86400, which used to sleep that long between
  samples, is now a usage error. The shipped unit uses 60.
- One-shot `sample --interval-s nan` (no `--loop`), which used to exit 0
  after one record, now exits 2.
- The `0` or negative message changes from `--interval-s must be > 0` to
  `report`'s `--interval-s must be > 0 and <= 86400`. `report` is unchanged.
- Other `sample` floats keep their plain checks: `--recover-after-s nan`
  with `--recover-cmd` still crashes the `Watchdog` constructor.
- The tests are in `scripts/tests/test_field_node.py` (`CliTests`). The
  MAN-96 record is not rewritten.
