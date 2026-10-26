# MAN-280: shadow-compare renders the comparison window in true UTC

Date: 2026-10-23. Ticket: MAN-280, a P2 finding deferred from Codex review
round 4 of PR #219 (MAN-96), comment `4223328415`, under
`docs/DECISIONS/2026-08-07-pr-review-convergence-policy.md`.

## Context

`_iso` (`scripts/shadow-compare.py`) rendered whatever wall-clock fields a
datetime carried and appended a literal `Z`:
`dt.strftime("%Y-%m-%dT%H:%M:%SZ")`. An offset `--start` like
`2026-09-09T00:00:00-05:00` filtered on the correct aware instant
(`parse_iso` always returns aware datetimes) but was recorded as
`2026-09-09T00:00:00Z`, so the markdown `- Window (UTC):` line and the JSON
`inputs.start`/`inputs.end` named a different window than the one actually
evaluated. For offsets past midnight like `+05:30` the recorded UTC **date**
was wrong too, disagreeing with the script's own By-UTC-day table.

## Decision

`_iso` converts to UTC before formatting: a naive datetime is treated as
UTC at render time (matching `parse_iso`'s documented assumption, keeping
direct callers host-TZ-independent), then
`dt.astimezone(timezone.utc).strftime(...)`. The `%Y-%m-%dT%H:%M:%SZ`
spelling stays, and every Z or naive input renders byte-identically to
before — the fix is purely additive on the offset path.

## Consequences

Evidence now names the window actually evaluated: a rerun over the recorded
window reproduces the recorded numbers (the D-N "raw evidence recorded
verbatim" property). Filtering behaviour, the runbook (which already
instructs Z/UTC inputs), `--help` and `scripts/score-against-rbn.py` are
unchanged; previously recorded field reports are historical human evidence
and are not rewritten. Regression tests:
`test_offset_window_recorded_as_its_utc_instant`,
`test_z_window_recorded_unchanged`,
`test_offset_window_summary_equals_z_window_summary` and
`test_iso_unit_converts_before_labelling` in
`scripts/tests/test_shadow_compare.py` (31 tests OK; the offset end-to-end
and unit tests fail with the fix reverted — run directly, as CI does not
run this suite).
