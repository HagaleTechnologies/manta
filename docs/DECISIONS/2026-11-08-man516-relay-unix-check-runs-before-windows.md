# MAN-516: relay Linux and macOS check-runs before the Windows leg ends

Date: 2026-11-08. Ticket: MAN-516.

## Context

A dispatched `ci-full.yml` run relays its results onto the explicit
`test (ubuntu-latest)`, `test (macos-latest)` and `test (windows-latest)`
check-runs that `create-ci-check` made on the reviewed head. One job,
`relay-check-runs`, did all three and needed `test`, the Windows-only matrix
job. So every PR got its two required Unix results only when Windows ended
(53-92 min). PR #530 waited 79 minutes for a Unix result that was ready at
+17 minutes.

## Decision

- The relay is two jobs. `relay-unix-check-runs` needs `create-ci-check`,
  `ubuntu-aggregate` and `macos-aggregate`, and PATCHes the two Unix
  check-runs. `relay-windows-check-run` needs `create-ci-check` and `test`,
  and PATCHes only `test (windows-latest)`. Check-run names are unchanged.
- Dropping `test` from the one relay's `needs` was rejected. `relay()` marks
  a job with no conclusion failed, so the old step run with Windows still in
  progress PATCHed `check-runs/13 ... conclusion=failure`. Each relay must look
  up only jobs in its own `needs`; `test_each_relay_reads_only_jobs_it_waits_for`
  pins that.
- Each relay mints its own hagale-agent token after its `needs` finish. Only
  the creating App can PATCH a check-run, and an installation token lasts one
  hour, shorter than the Windows leg can run.
- Both jobs carry the same 14-line `relay()` rather than a shared script, so
  neither job that holds the App key needs a checkout.
  `test_relays_share_one_fail_closed_relay_function` keeps the two copies
  byte-identical and fail-closed.
- `if`, the `codex-review` environment, `actions: read`, the pinned
  `create-github-app-token` and the Jobs API lookup by exact name are kept.

## The trade: a PR can merge before its Windows result

Windows is not a required check (MAN-371 O1 was never applied), so a PR is
merge-ready once its Unix check-runs, Supply Chain and Codex are green, while
its dispatched Windows job may still be running. A Windows-only regression
that merges this way is still caught by the merge-group run's native
`test (windows-latest)` job, by main's push run that `main-alert.yml`
watches, and by `release-publish.yml`, which refuses a commit whose push run
failed Windows. The dispatched Windows job keeps running after the merge; that
is the same runner time it used before, not new time.

`gh run rerun --failed` after a failed Windows leg re-runs the Windows job and
`relay-windows-check-run` only; the Unix relay already succeeded.

## Not verified here

This PR cannot exercise its own change. `wait-for-codex.yml` dispatches
`ci-full.yml` at the default branch, and the `codex-review` environment is
main-only (Q-0068). The stub-`gh` tests in `scripts/tests/test_ci_gates.py`
run both real step bodies. On the first trusted PR dispatched after merge,
with `RUN` its `ci-full` run id and `SHA` its head:

    gh api "repos/HagaleTechnologies/manta/actions/runs/$RUN/jobs?per_page=50" \
      --jq '.jobs[] | select(.name|test("^test \\(|^Relay")) | "\(.name)\t\(.completed_at)"'
    gh api "repos/HagaleTechnologies/manta/commits/$SHA/check-runs?per_page=100" \
      --jq '.check_runs[] | select(.name|startswith("test (")) | "\(.name)\t\(.app.slug)\t\(.conclusion)\t\(.completed_at)"'

Expect hagale-agent's two Unix check-runs within two minutes of the later
aggregate and before Windows, the Windows check-run within two minutes of its
job, and, on the first failing Windows leg, only that check-run failed.
