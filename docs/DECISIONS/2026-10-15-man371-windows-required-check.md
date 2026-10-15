# 2026-10-15 — MAN-371: `test (windows-latest)` becomes a required check

**Status:** Implemented (branch `MAN-371`). The ruleset change and the
live acceptance checks are owner steps O1–O4 below. They need
repository-admin rights or hosted runs, and the change does not perform
them.

**Ticket:** MAN-371.

**Supersedes** step O5 of `2026-10-13-man156-ci-regression-gates.md`:
its pre-flip 20-run streak becomes the post-flip watch in O4 below.

## Problem

`test (windows-latest)` runs on every PR, merge-group and push run, but
the `main-protection` ruleset (id 18658768) does not require it. Its
required checks are `test (macos-latest)`, `test (ubuntu-latest)`,
`Codex review clean (gate before expensive downstream jobs)` and
`Supply Chain`, read live with
`gh api repos/HagaleTechnologies/manta/rulesets/18658768` on 2026-10-15.
From #281 (2026-10-13) to #305 (2026-10-14) every `main` push failed
Windows, and PRs kept merging. #276's `list_cache` Windows failure was
visible on its own PR and merged anyway.

Two smaller defects stood in the way of requiring it:

1. **The check-run was created best-effort.** `create-ci-check` in
   `ci-full.yml` ended the Windows POST in `|| { echo "::warning::…";
   windows_id=""; }`. With the check required, a failed POST would leave
   the PR head on "Expected" behind a green job, and the PR could never
   merge.
2. **Four test files picked ports one at a time.** The sequential
   `free_port()` pattern #302 fixed in `doctor_checks.rs` (Windows hands a
   just-released port straight back out) was still in
   `node_health_acceptance.rs`, `hpsdr_metrics_acceptance.rs` and
   `ka9q_input.rs`. The ticket also named `health_command.rs`.

## Evidence

Measured on `3805f78` (`main`), 2026-10-15 00:18–00:49Z.

### Stability

Every `merge_group` run since MAN-156's own run (#286, 2026-10-13 17:01Z).
Each Windows failure's log was read with `gh run view --job … --log-failed`.

| Window | `test (windows-latest)` in merge-group runs | Cause |
|---|---|---|
| pr-286 (MAN-156's own run) | success | |
| pr-281, 273, 289, 296, 297, 298 | failure ×6 | flake: `live_state_acceptance`'s stalled client paced by wall clock; fixed by #302 |
| pr-284, 287 | failure ×2 | real Windows-only regression: an unused import at `ka9q/mod.rs:768` under clippy, from #284 (MAN-140); fixed by #289 |
| pr-299 | failure | real: `patterns.lst` checked out CRLF, from #296 (MAN-95); fixed by #302 |
| pr-276, 302, 301, 304, 285 | failure ×5 | real: `list_cache` path separators, from #276 (MAN-316); fixed by #305 |
| pr-305, 303, 248, 307 | **success ×4** | after the fixes |

- **After #305** (merged about 20:30Z on 2026-10-14):
  `test (windows-latest)` passed in **11 of 11** runs on code that
  includes the fix. Merge-group: 38627959080, 38632562829, 38650444345,
  38653994800. Push: 38632560849, 38637144787, 38653983836, 38657094982
  (the `main` tip, Windows 40.8 min). Dispatched: 38633245781,
  38637824040, 38645819103. `test-hpsdr (windows-latest)` passed in all
  of them too.
- **The one Windows failure in that window**, dispatched run 38648456157,
  tested `51864d6`, #307's head, a branch that never got #305 (checked
  with the compare API). #307's head showed `test (windows-latest)`
  `hagale-agent` `failure` and #307 merged anyway. Its merge-group run,
  rebased on `main`, passed.
- **Earlier `health_command` failures.** pr-257 (2026-10-12) and pr-282
  (2026-10-13) failed with `os error 10054`, before MAN-156's half-close
  fix. They were not port collisions.
- **What a required check would have done.** Over about 26 hours it would
  have stopped three Windows-only regressions (#284, #296, #276) from
  merging, and bounced six queue entries on the live-state flake that #302
  has since fixed.
- **The sample is small.** It is 4 merge-group runs, about four hours
  after #305, not "a few more days". Decision 3 is taken on that basis.

The tally above was refreshed at 00:49Z with O1's command. It printed the
same four `success` rows, and the newest push run, 38657094982, printed
`test (windows-latest) success`.

### Merge-group naming

`gh api repos/HagaleTechnologies/manta/commits/3805f78bcb1b166f1eba98448d62db98addb201c/check-runs`,
the commit #307's merge group tested, lists `test (windows-latest)` from
`github-actions`, completed `success`, next to `test (ubuntu-latest)` and
`test (macos-latest)`. All 40 of the most recent `merge_group` runs have a
job named exactly `test (windows-latest)`. `create-ci-check` and
`relay-check-runs` are `skipped` on `merge_group`, as designed. None of the
required contexts carries an `integration_id`, so `hagale-agent` on a PR
head and `github-actions` on a merge-group commit both satisfy it.

### Latency

Minutes from run creation to job completion, from the Jobs API. The
queue picked runners up within 0.2 min in every sample.

| Merge-group run (after #305) | ubuntu | macOS | **windows** | slowest required today → with Windows |
|---|---|---|---|---|
| 38627959080 (pr-305) | 32.1 | 32.0 | **33.5** | 32.1 → 33.5 (+1.4) |
| 38632562829 (pr-303) | 32.4 | 29.3 | **39.8** | 32.4 → 39.8 (+7.4) |
| 38650444345 (pr-248) | 33.4 | 29.8 | **39.8** | 33.4 → 39.8 (+6.4) |
| 38653994800 (pr-307) | 27.4 | 30.2 | **38.9** | 30.2 → 38.9 (+8.7) |

- **Dispatched PR runs after #305** (38633245781, 38637824040,
  38645819103): Windows took 40.3, 25.6 and 39.3 min. The slowest required
  leg moves from 32.6, 33.4 and 32.2 to 40.3, 33.4 and 39.3 min.
- **Per PR:** about +7 min at the PR stage plus about +7 min in the queue,
  so about +14 min end to end (median).
- **Queue throughput:** `max_entries_to_build` is 1, so the ceiling drops
  from about 45 to about 37 merges a day. The busiest day in the sample
  (2026-10-14) had 14 merge-group runs; the queue is not saturated.
- **Timeout margin:** the slowest successful Windows merge-group leg in the
  last 40 runs took 41.8 min, about 18 min under the ruleset's 60-min
  `check_response_timeout_minutes`.
- **Growth:** the leg grew from about 25 min (2026-10-12) to about 39 min
  (2026-10-14). Nearly all of it is `cargo test --workspace`: 37.1 of
  38.8 min in 38653994800, against 24.7 min in 38204758207. O4 watches it.

### Ports

On Linux, 0 of 2000 sequential bind-release-bind pairs returned the same
port. The collision depends on the Windows port allocator (#302 saw it in
`doctor_checks`), so no Linux test can fail first. Converting the call
sites but keeping the old `free_port()` fails
`cargo clippy -p manta-cli --all-targets --locked -- -D warnings` with
`function free_port is never used`, the MAN-230 `metrics_bind.rs` class of
failure, so each file's helper is deleted.

| File | Released-port picks in one test | Built on Windows? | Action |
|---|---|---|---|
| `crates/manta-cli/tests/node_health_acceptance.rs` | 5 (telnet, json, metrics, 2 uplinks) | yes, `test (windows-latest)` | converted |
| `crates/manta-cli/tests/hpsdr_metrics_acceptance.rs` | 3 | yes, `test-hpsdr (windows-latest)` | converted |
| `crates/manta-cli/tests/ka9q_input.rs` | 3 | no (`#![cfg(any(target_os = "linux", target_os = "macos"))]`) | converted |
| `crates/manta-cli/tests/health_command.rs` | at most 1 per test | yes | unchanged |

## Decisions

1. **The Windows check-run is created fail-closed.** The `|| { … }`
   fallback is deleted, so the Windows POST is the same shape as the
   ubuntu and macOS lines. The step sets no `shell:`, so it runs as
   `bash -e {0}`, and a failed `windows_id=$(gh …)` fails the step before
   any empty id is written. `relay()` in `relay-check-runs` is unchanged:
   it already treats all three names alike, and an empty id there means
   `create-ci-check` failed and the run is already red.
   `test_create_ci_check_fails_closed_for_every_leg` in
   `scripts/tests/test_ci_gates.py` runs the step's `run:` body under
   `bash -e` with a stub `gh` and fails if any of the three POSTs can fail
   without failing the step.
2. **The merge group needs no change.** Its commit already carries
   `test (windows-latest)` under the same name (Evidence above), so the
   queue does not wait on a context nothing posts.
3. **The owner flips right after this merges, then watches 20 merge-group
   runs with written rollback triggers** (O1, O4). This replaces MAN-156
   O5's "20 consecutive green merge-group runs before the flip". Every
   known Windows flake is fixed: live-state pacing and `doctor_checks`
   ports by #302, `os error 10054` by MAN-156, the remaining multi-port
   tests by this change. In the 26 hours before #305, three real
   Windows-only regressions merged because nothing stopped them, and each
   masked further failures. A flake in the queue costs one bounce, which
   `queue-retry-handler.yml` retries once, and rollback is one ruleset
   edit. O1's precondition keeps a red `main` from blocking every PR the
   moment the check becomes required.
4. **Latency is measured and no parameter changes.** About +7 min per
   queue entry and +7 min per PR before queueing; the throughput ceiling
   drops from about 45 to 37 merges a day; the margin to the 60-min
   timeout is about 18 min. O4 has a trigger at 50 min.
5. **Ports: three files hold every listener until all ports are read.**
   `node_health_acceptance.rs`, `hpsdr_metrics_acceptance.rs` and
   `ka9q_input.rs` each replace their `free_port()` with a verbatim copy
   of `doctor_checks.rs`'s `free_ports::<N>()` and destructure all their
   ports from one call. `ka9q_input.rs` is not built on Windows today, but
   MAN-140 leaves Windows support for ka9q open, and it was the last copy
   of the three-port shape in a file a Windows build would inherit.
   `health_command.rs` is unchanged: none of its tests picks two released
   ports, so `free_ports::<1>()` would change nothing. Each file keeps its
   own helper, as #302 did; moving it into `tests/common/mod.rs` would
   pull that module's WAV fixture code into three files that do not use
   it. Known residuals, none observed failing:
   - `health_command.rs` picks a released "closed" port that a concurrent
     test's listener could take; `free_ports` cannot fix that.
   - `FakeRadiod::start` (`crates/manta-input/src/ka9q/mod.rs`) picks two
     released UDP ports, Linux and macOS only.
   - `doctor_checks.rs`'s and `list_refresh.rs`'s single closed-port
     helpers (`list_refresh.rs` is `#![cfg(unix)]`).
6. **`test-hpsdr (windows-latest)` and every `test-soapy` leg stay
   advisory,** like their ubuntu and macOS siblings (MAN-156 decision 7).
   The ticket names only `test (windows-latest)`.
7. **A stale PR head is blocked until rebased.** Once O1 is applied, a
   PR whose head fails `test (windows-latest)`, typically because it
   predates a Windows fix on `main` (#307's `51864d6` is the example),
   cannot enter the queue. Rebasing onto `main` and pushing re-runs Codex
   review and the dispatch. This is the intended effect.
8. **Recovery from a failed `create-ci-check` is a re-run.** A failed
   POST leaves the PR head on "Expected" and the dispatched run red.
   Re-run the dispatched run's failed jobs (`gh run rerun <id> --failed`):
   `create-ci-check` creates fresh check-runs and `relay-check-runs`
   patches them from the jobs API's latest attempts. The red
   `create-ci-check` job itself does not show on the PR, because a
   dispatched run's own checks attach to `main`'s tip; GitHub notifies
   whoever triggered the dispatch, and `main-alert.yml` ignores
   `workflow_dispatch` runs.

## Owner steps

These need repository-admin rights or hosted runs; the change does not
perform them. Run them in order, after this change has merged. A trusted
PR dispatched before the merge runs `main`'s old create step, so a failed
Windows POST there would still leave a silent "Expected", and after O1
that PR would wait forever.

### O1. Precondition, then flip

**1. Precondition.** The five newest rows and the newest push run must say
`success`, and every older `failure` row must have a diagnosed cause that
is fixed on `main`. Raise `--limit` if more than 50 merge-group runs have
passed since:

```sh
gh run list --repo HagaleTechnologies/manta --workflow ci-full.yml --event merge_group --limit 50 \
  --json databaseId,createdAt,headBranch \
  --jq '.[] | select(.createdAt >= "2026-10-14T19:57:00Z") | "\(.databaseId) \(.createdAt) \(.headBranch | capture("pr-(?<n>[0-9]+)").n)"' |
while read -r id at pr; do
  printf '%s %s pr-%s %s\n' "$id" "$at" "$pr" \
    "$(gh api "repos/HagaleTechnologies/manta/actions/runs/$id/jobs?per_page=50" --jq '.jobs[] | select(.name == "test (windows-latest)") | .conclusion // .status')"
done
id=$(gh run list --repo HagaleTechnologies/manta --workflow ci-full.yml --event push --branch main --limit 1 \
  --json databaseId --jq '.[0].databaseId')
gh api "repos/HagaleTechnologies/manta/actions/runs/$id/jobs?per_page=50" \
  --jq '.jobs[] | select(.name == "test (windows-latest)") | "\(.name) \(.conclusion // .status)"'
```

Run at 00:49Z on 2026-10-15, the tally printed four `success` rows
(pr-307, 248, 303, 305), and the newest-push command printed
`test (windows-latest) success` for run 38657094982. For a `failure` row
that is not yet fixed:

- **A real Windows regression on `main`:** fix it first. Flipping now
  would block every PR.
- **A flake:** fix it first, then flip.

**2. Flip:**

```sh
RS=repos/HagaleTechnologies/manta/rulesets/18658768
gh api "$RS" --jq '{rules: [.rules[]
    | if .type == "required_status_checks"
      then .parameters.required_status_checks += [{"context": "test (windows-latest)"}] else . end
    | if .parameters == null then del(.parameters) else . end]}' > /tmp/man371-rules.json
gh api -X PUT "$RS" --input /tmp/man371-rules.json \
  --jq '.rules[] | select(.type == "required_status_checks") | .parameters.required_status_checks[].context'
```

**Expected output, five lines:** `test (macos-latest)`,
`test (ubuntu-latest)`,
`Codex review clean (gate before expensive downstream jobs)`,
`Supply Chain`, `test (windows-latest)`. The input transform was dry-run
on 2026-10-15 and produced exactly that list; the `PUT` was not run.

### O2. A Windows-only failure cannot merge

- `gh api repos/HagaleTechnologies/manta/rules/branches/main --jq '.[] | select(.type=="required_status_checks") | .parameters.required_status_checks[].context'`
  includes `test (windows-latest)`.
- For any open PR, `gh pr checks <N> --required` lists
  `test (windows-latest)`.
- During the watch, record the first PR whose head shows
  `test (windows-latest)` failing.
  `gh pr view <N> --json mergeStateStatus --jq .mergeStateStatus` should
  print `BLOCKED`, and the PR should never get a merge-group run. Stale
  heads such as #307's are the expected case. Do not construct a failing
  PR: auto-merge is armed on every trusted, non-draft PR when it opens
  (`auto-merge-trigger.yml`), so if O1 had silently not applied, a probe
  PR with a Windows-only failing test would merge into `main`.

### O3. The queue still lands normal PRs

The first three PRs enqueued after O1 each:

- merge;
- have a merge-group run with `test (windows-latest)` = `success`;
- record their queue time: merge-group run creation to merge.

### O4. Watch the 20 merge-group runs after O1, with rollback triggers

Tally with O1's command, changing the start time to O1's.

- **Two or more `test (windows-latest)` failures in those 20 that are
  flakes** (pass on re-run with no code change): roll back the
  requirement, file a ticket per flaky test, and re-flip after the fix
  plus 10 consecutive green merge-group runs. Rollback:

  ```sh
  RS=repos/HagaleTechnologies/manta/rulesets/18658768
  gh api "$RS" --jq '{rules: [.rules[]
      | if .type == "required_status_checks"
        then .parameters.required_status_checks |= map(select(.context != "test (windows-latest)")) else . end
      | if .parameters == null then del(.parameters) else . end]}' > /tmp/man371-rollback.json
  gh api -X PUT "$RS" --input /tmp/man371-rollback.json \
    --jq '.rules[] | select(.type == "required_status_checks") | .parameters.required_status_checks[].context'
  ```

  Expected: the original four contexts. The transform was dry-run and
  produced them.
- **One flake:** file a ticket and keep the requirement.
  `queue-retry-handler.yml` re-queues once.
- **Any merge-group `test (windows-latest)` taking more than 50 min** from
  run creation: file a ticket to shorten the leg. Headroom to the 60-min
  `check_response_timeout_minutes` is then under 10 min.
- **An entry timing out at 60 min:** raise
  `check_response_timeout_minutes` to 90 in the same ruleset as a
  stopgap, and keep the requirement.
- **A `create-ci-check` failure leaving a PR on "Expected":** re-run the
  dispatched run's failed jobs (`gh run rerun <id> --failed`).

## Not done

- Changing the ruleset (O1 above); it needs admin rights.
- Requiring `test-hpsdr (windows-latest)` or any `test-soapy` leg.
- Retries on the three check-run POSTs. That would be new behaviour for
  all three; a transient API failure is re-run the same way for all
  three.
- A `timeout-minutes` on the `test` job, or a changed
  `check_response_timeout_minutes`. The margin is watched (O4).
- Making the Windows leg faster. That is a follow-up if O4's latency
  trigger fires.
- `FakeRadiod::start`, `health_command.rs` and the single closed-port
  helpers (decision 5's residuals).
- A shared `free_ports` in `crates/manta-cli/tests/common/mod.rs`.
- Constructing a deliberately failing PR to prove the block (O2).
- `release.yml`'s header comment, `queue-retry-handler.yml`,
  `main-alert.yml` and `CHANGELOG.md`.

## Rollback

The two halves are independent.

- **Ruleset:** run O4's rollback command. Rolling back the ruleset alone
  is safe: fail-closed creation is harmless while the check is advisory.
- **Code:** revert the squash commit, for the workflow, test and port
  changes. The port change and the workflow change revert independently.
