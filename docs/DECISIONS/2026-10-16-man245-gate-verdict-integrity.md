# 2026-10-16 — MAN-245: a green Codex gate is a complete verdict for that revision, Dependabot is classified by author, and each required check names its source

**Status:** Implemented (branch `MAN-245`). The ruleset changes (O1–O3)
and the fleet routing (O4) are owner steps below. They need
repository-admin rights, and the change does not perform them.

**Ticket:** MAN-245 (audit finding MAN-SEC-003, manta @ 187192a8).

## Problem

A required check should mean "the owning workflow evaluated this
revision". The audit found four places where that was weaker:

1. **The review-thread read was one page.** `compute-verdict` in
   `wait-for-codex.yml` read `reviewThreads(first: 100)` and
   `comments(first: 50)` with no `pageInfo`. An unresolved Codex thread
   past position 100 was never counted, so the gate reported the revision
   clean, applied `codex-clean:<sha12>` and logged nothing about the
   threads it had not seen.
2. **Dependabot handling keyed on the actor.** `github.actor ==
   'dependabot[bot]'` decided the Codex pass-through
   (`IS_EXCLUDED_DEPENDABOT`), `ci-full.yml`'s native run and all three
   `dependabot-auto-merge.yml` jobs. A Dependabot-triggered event on
   someone else's PR was treated as a Dependabot update.
3. **A `codex-clean` label looked exactly like a review.** The request job
   passes a revision that already carries `codex-clean:<sha12>`, whoever
   applied the label, and the resulting check-run had no title.
4. **Required checks are matched by name only.** None of
   `main-protection`'s four required contexts carries an `integration_id`,
   so any app or commit status that posts the name satisfies it.

## Evidence

Measured 2026-10-16 against `a7ae6c2` (`main`).

### The one-page read reproduces

The base commit's `compute-verdict` `run:` body, extracted from the YAML
and run under `bash -eo pipefail` with a stub `gh` that answers the
GraphQL query as GitHub does (the first 100 threads, `hasNextPage: true`):

| Fixture | Base commit | This change |
|---|---|---|
| 105 threads, 2 unresolved Codex threads beyond the page | exit 0, `Applied codex-clean:aaaaaaaaaaaa.` | exit 1, `::error::Review thread list incomplete: …`, no label, no resolve |
| 1 unresolved Codex thread with 51 comments (50 read) | exit 1, `Codex raised 1 new concern(s)` | exit 1, `Review thread list incomplete`, `incomplete=true` |
| 100 threads, thread 37 unresolved, `hasNextPage: false` | exit 1, `Codex raised 1 new concern(s)` | unchanged |
| 100 resolved threads, `hasNextPage: false` | exit 0, label applied | unchanged |

Exposure is low today: the most threads on any merged PR is 76 (#63),
and no PR has had `hasNextPage: true`. `manta-review-gate` (ruleset
22918070) still sets `required_review_thread_resolution: true`, so a
false green could not merge on its own.

### The label shortcut is a live override path

The issue-events API lists 413 `codex-clean:*` labels applied by
`github-actions[bot]` (this workflow's verdict step) and 18 applied by
`thagale`, the latest on 2026-10-15. On PR #285 head `fedb14d54deb` the
gate's check-run went `in_progress` (16:04:02Z), then `failure`
(16:08:52Z) after a Codex findings review at 16:08:33Z. `thagale` applied
`codex-clean:fedb14d54deb` at 16:09:38Z, and a rerun of the request job
created a `completed/success` check-run at 16:09:51Z with `started_at ==
completed_at`: a pass-through with no Codex round. PR #283 head
`98e1978baa91` shows the same sequence (in progress 04:51:09Z, findings
04:55:14Z, label 04:56:42Z, success 04:56:54Z). The heads that merged
were reviewed later. The labels come from the fleet's convergence
tooling (credenza `resolve-review-feedback` Step 7.5 and `land-pr`),
which "self-heals" `codex-clean` so a round whose findings were ticketed
under the convergence policy can pass.

### Producers of each required check

Read from check-runs (`.app.slug`, `.app.id`) on three merged trusted PR
heads (`c0361519`, `af7233b0`, `5c5bd846`), two Dependabot heads (#269
`a625b107`, #270 `c5699783`) and `main` HEAD `a7ae6c2` (a merge-group
commit). Main carries no commit statuses.

| Required check | Trusted PR head | Dependabot PR head | Fork / untrusted PR head | Merge-group commit |
|---|---|---|---|---|
| `test (ubuntu-latest)`, `test (macos-latest)`, `test (windows-latest)` | hagale-agent 4696353 (`create-ci-check`, `relay-check-runs`) | GitHub Actions 15368 | GitHub Actions 15368 (native) | GitHub Actions 15368 |
| `Codex review clean (gate before expensive downstream jobs)` | hagale-agent 4696353 | GitHub Actions 15368 (the mint step is skipped, `github.token` fallback) | hagale-agent 4696353 (`pull_request_target` runs in the base context) | GitHub Actions 15368 (merge-group pass-through) |
| `Supply Chain` | GitHub Actions 15368 | GitHub Actions 15368 | GitHub Actions 15368 | GitHub Actions 15368 |
| `PR title` (not yet required) | GitHub Actions 15368 | GitHub Actions 15368 | GitHub Actions 15368 | GitHub Actions 15368 |

`name: Supply Chain` is a job only in `supply-chain.yml`, and `name: PR
title` only in `pr-title.yml`. The only Checks API POSTs in the workflows
create `test (*)` (`ci-full.yml`) and the Codex check
(`wait-for-codex.yml`). `PinnedCheckProducerTests` in
`scripts/tests/test_ci_gates.py` keeps it that way.

### The ruleset can pin a source

GitHub's REST reference for repository rules documents
`required_status_checks[].integration_id` (integer, optional): "The
optional integration ID that this status check must originate from."
`gh api repos/HagaleTechnologies/manta/rulesets/18658768` shows the four
contexts with no `integration_id`, `strict_required_status_checks_policy:
true` and a `merge_queue` rule with `check_response_timeout_minutes: 60`.

### Dependabot's inner check

`dependabot/fetch-metadata` at the pinned `25dd0e34`
(`src/dependabot/verified_commits.ts`) refuses a PR whose
`pr.user.login` is not `dependabot[bot]` ("It looks like this PR was not
created by Dependabot, refusing to proceed.") and checks the commit
signature. A Dependabot-triggered event on a human's PR therefore already
failed at metadata in `dependabot-auto-merge.yml` rather than approving.

## Decisions

1. **An incomplete thread read fails the check, not pending (D1).** Both
   thread queries now request `pageInfo { hasNextPage }` and
   `comments(first: 50) { totalCount … }`. One jq `def`,
   `thread_list_incomplete`, identical in the baseline and verdict steps,
   is true when `hasNextPage` is anything but `false` (missing counts as
   incomplete) or when an unresolved thread has more comments than were
   read. `compute-verdict` checks it, and the baseline step's
   `incomplete` output, before it counts, labels or resolves anything. It
   exits 1 with `::error::Review thread list incomplete: …`, and "Update
   the check-run" titles the failure `Review thread list incomplete`. The
   baseline step logs a `::warning::` on the same condition. Pending was
   rejected: it waits for credenza's timeout scan and says less. Full
   pagination was rejected: restructuring the gate is a non-goal, and the
   all-time maximum is 76 of 100. A PR that passes 100 threads moves its
   remaining work to a new PR, and the message says so. A resolved thread
   with more than 50 comments does not count, because every verdict
   predicate filters on `isResolved == false` first.
2. **A Dependabot update needs author, same-repository head and actor
   (D2).** One expression, `github.actor == 'dependabot[bot]' &&
   github.event.pull_request.user.login == 'dependabot[bot]' &&
   github.event.pull_request.head.repo.full_name == github.repository`,
   is `IS_EXCLUDED_DEPENDABOT` in `wait-for-codex.yml` and the `if:` of
   `verify`, `check-needs-human` and `auto-merge` in
   `dependabot-auto-merge.yml`. A PR not authored by Dependabot takes the
   normal path whoever triggered the event. The actor stays as a third
   condition so a maintainer's push to a Dependabot branch keeps getting
   a real Codex round instead of a review-free pass-through for
   human-written commits. In `ci-full.yml` the actor term is deleted, not
   replaced: `!contains(trusted, author)` already routes Dependabot-authored
   PRs to the native run. The app-token mint keeps `github.actor !=
   'dependabot[bot]'`, because it is about which runs GitHub gives
   secrets to, not about the PR.

   | Case | Actor | Author | Head | Dependabot update | `ci-full` native |
   |---|---|---|---|---|---|
   | Genuine update | dependabot[bot] | dependabot[bot] | this repo | yes, as before | runs |
   | Human pushes to a bot PR | thagale | dependabot[bot] | this repo | no, as before | runs |
   | Owner or agent PR | thagale / catalyst-cloud-connector[bot] | same | this repo | no | dispatched |
   | Bot actor on a human PR | dependabot[bot] | thagale | this repo | **no** (was yes) | dispatched (was: also native) |
   | Bot login from a fork, or a deleted head | dependabot[bot] | dependabot[bot] | fork / none | no | runs |

3. **The label shortcut stays, and an override says so (D3, the owner's
   label-shortcut decision).** Removing the label read, or accepting only
   a label applied by hagale-agent, would break the fleet convergence
   tooling, which runs with a human's `gh` identity and holds no
   hagale-agent key. Changing that tooling is a fleet decision outside
   this repository. So `ALREADY_CLEAN` still passes. The label lookup now
   reads every page of labels and the latest `labeled` event's actor, and
   every completed check-run carries a title:
   - `Codex review clean`: a real clean verdict;
   - `Codex review clean (earlier verdict for this revision)`: the label
     was applied by `github-actions[bot]` **and** a `Codex review clean`
     check-run that hagale-agent (4696353) completed as success is on the
     head (`filter=all`). Every workflow that uses `GITHUB_TOKEN` labels
     as `github-actions[bot]`, including one defined on a branch nobody
     has reviewed and run on `push`, so the actor alone proves nothing.
     hagale-agent's key is in the `main`-only codex-review environment,
     and only the verdict step's PATCH titles its check-run `Codex review
     clean`;
   - `Codex review overridden by <login> (codex-clean label)`: anyone
     else, or `github-actions[bot]` with no such check-run, with a
     `::warning::` in the log;
   - `Codex review overridden (codex-clean label; applier unknown)`: the
     timeline could not be read, with the same warning;
   - `Draft: not reviewed` and `Dependabot update: not sent to Codex`
     for the other two pass-throughs;
   - `Review thread list incomplete` and `Codex raised new concerns` for
     the two failures.

   Pass/fail behaviour is unchanged. Thread resolution
   (`manta-review-gate`) still blocks a merge past unresolved Codex
   threads. Removing the read later is owner step O5.
4. **The label lookup paginates (D4).** `labels?per_page=100` without
   `--paginate` missed a label at position 101. That miss only caused a
   redundant review, but the step was rewritten anyway.
5. **Pin only the checks with one producer on every path (D5).**
   `scripts/required-check-sources.py` holds this table as `DECISIONS`
   and compares a ruleset against it.

   | Required check | Decision | Reason |
   |---|---|---|
   | `Supply Chain` | **pin to GitHub Actions (15368)** (O1–O3) | The only producer on every path. The pin stops a commit status, or another app's check-run, from satisfying it: agents running with a human's PAT and any app with `checks:write` or `statuses:write` could post the name. |
   | `PR title` | pin to GitHub Actions (15368) when MAN-156 O4 requires it | Same shape as `Supply Chain`: one `pull_request` job, also run on `merge_group`. |
   | `test (ubuntu-latest)`, `test (macos-latest)`, `test (windows-latest)` | not pinned | hagale-agent (4696353) on trusted PR heads, GitHub Actions (15368) on Dependabot/fork/untrusted PR heads and merge groups. A dispatched run's native check-runs land on `main`, so the trusted path must post them through the Checks API (MAN-243); one producer would undo that routing. |
   | `Codex review clean (gate before expensive downstream jobs)` | not pinned | hagale-agent (4696353) on non-Dependabot PR heads, GitHub Actions (15368) on Dependabot PR heads (secrets withheld from Dependabot-actor runs) and merge groups. The codex-review environment admits `main` only (dispensa Q-0068); admitting `gh-readonly-queue/main/*` would expose the app key to workflow definitions from the PR under test. Moving every path to GitHub Actions brings back HAG-106/108 (check-runs in a superseded suite). |

6. **The pin is trialled on a scratch branch first (D6).** "Trial any pin
   on a duplicate non-required check" is O1: a ruleset on a scratch base
   branch requiring `PR title` pinned to 15368, with a negative control
   pinned to 4696353. `main` is not touched until the mechanism is shown
   to work.
7. **Tests run the real workflow bodies (D7).** Every behaviour test
   extracts a step's `run:` body from the YAML and runs it against a stub
   `gh` that records every call, so the tests can show that no label was
   POSTed and no thread was resolved on the fail-closed path.
   `compute-verdict` now takes its three baseline inputs through `env:`,
   so its body contains no `${{ }}`, like the trust steps.

## Tests

- `scripts/tests/test_ci_gates.py`:
  - `CodexThreadListCompletenessTests`: the query text, the shared `def`,
    the `env:` inputs, the verdict table above (including missing
    `pageInfo`, a baseline marked incomplete and a resolved 51-comment
    thread), the baseline's `incomplete` output and the failure title;
  - `CodexCheckRunProvenanceTests`: the label lookup (no label, the
    gate's label with and without hagale-agent's verdict check-run, an
    unreadable check-run list, a later human label, a label on page 2, an
    unreadable timeline) and every pass-through and clean title;
  - `RequiredCheckSourcesTests`: the script against the live ruleset as
    read, with `Supply Chain` pinned right and wrong, with `test
    (windows-latest)` and `PR title` added, an unknown context, a pinned
    Codex check and unusable input;
  - `PinnedCheckProducerTests`: each pinned check has one native job and
    no Checks API POST, and the POST sites create exactly the unpinned
    checks.
- `scripts/tests/test_ci_trust_boundary.py`: `DependabotClassificationTests`
  (the expression at its four sites, no actor-only Dependabot test left,
  the case table) and the updated `NATIVE_IF`.

## Residual risks

- A `github-actions[bot]` label can also come from a workflow defined on
  a branch, which runs on `push` or `pull_request` before anyone reviews
  it (CODEOWNERS `/.github/` only blocks the merge). It still passes the
  gate, as any `codex-clean` label does (decision 3), but without
  hagale-agent's `Codex review clean` check-run on the head it is titled
  an override.
- The Codex and `test (*)` checks stay unpinned, so a commit status or
  another app's check-run with those names is not rejected by the
  ruleset.
- The merge-group pass-through posts the Codex check unconditionally
  (MAN-246's territory).
- Auto-resolve's live re-checks compare only the bare login
  `chatgpt-codex-connector`, so a thread opened by
  `chatgpt-codex-connector[bot]` is never auto-resolved. That fails
  closed and is outside this ticket.
- A PR with more than 100 review threads must be split.

## Owner steps

### O1. Trial the pin on a duplicate non-required check

1. `git push origin main:refs/heads/man245-pin-trial`.
2. Create ruleset `man245-pin-trial` (target branch, include
   `refs/heads/man245-pin-trial`, enforcement active) with one
   `required_status_checks` rule:
   `[{"context": "PR title", "integration_id": 15368}]`.
3. Open a draft PR with a Conventional Commits title from a scratch
   branch into `man245-pin-trial`.
4. Expect `PR title` from GitHub Actions, and
   `gh pr view <n> --json statusCheckRollup` showing it `SUCCESS`. The
   GraphQL `isRequired(pullRequestNumber: <n>)` on that check-run must be
   true, and `mergeStateStatus` must not be `BLOCKED` by checks.
5. Negative control: change the trial's `integration_id` to `4696353`.
   The PR must now show `PR title` as expected and not satisfied.
6. Delete the trial ruleset, the PR and both scratch branches.

### O2. Pin `Supply Chain` on `main-protection`

```bash
gh api repos/HagaleTechnologies/manta/rulesets/18658768 > ruleset-before.json
jq '{name, target, enforcement, conditions, bypass_actors, rules: (.rules | map(if .type == "required_status_checks" then .parameters.required_status_checks |= map(if .context == "Supply Chain" then . + {integration_id: 15368} else . end) else . end))}' ruleset-before.json > ruleset-after.json
gh api -X PUT repos/HagaleTechnologies/manta/rulesets/18658768 --input ruleset-after.json
gh api repos/HagaleTechnologies/manta/rulesets/18658768 | python3 scripts/required-check-sources.py   # expect exit 0
```

Diff `ruleset-before.json` against a fresh read: only `integration_id`
on the `Supply Chain` entry may change. Before O2 the script prints, and
exits 1:

```text
test (macos-latest)                                          any source   not pinned: hagale-agent (4696353) on trusted PR heads, GitHub Actions (15368) on Dependabot/fork/untrusted PR heads and merge groups
test (ubuntu-latest)                                         any source   not pinned: hagale-agent (4696353) on trusted PR heads, GitHub Actions (15368) on Dependabot/fork/untrusted PR heads and merge groups
Codex review clean (gate before expensive downstream jobs)   any source   not pinned: hagale-agent (4696353) on non-Dependabot PR heads, GitHub Actions (15368) on Dependabot PR heads and merge groups
Supply Chain                                                 any source   PIN NOT APPLIED: record says GitHub Actions (15368) -- owner step O2
decisions: docs/DECISIONS/2026-10-16-man245-gate-verdict-integrity.md
```

### O3. Watch the queue

Watch the next 5 PRs and the merge-group entries that carry them. Each
PR head shows `Supply Chain` required and green, and each entry merges
within `check_response_timeout_minutes` (60).

- **Rollback trigger:** any PR or entry showing `Supply Chain` as
  "Expected — Waiting for status to be reported" while a GitHub Actions
  `Supply Chain` check-run exists on that SHA.
- **Rollback:**
  `gh api -X PUT repos/HagaleTechnologies/manta/rulesets/18658768 --input ruleset-before.json`,
  then requeue.

### O4. Route the thread-limit fix to the fleet

The `reviewThreads(first: 100)` and `comments(first: 50)` reads come from
credenza's template at `03dd34fb`, which cqdx, widdershins and vanity
pin, and which `wait-for-codex.yml`'s header says this file ports by
hand. File the port in each repository; the jq `def` and the stub-`gh`
fixture tests port as they are.

### O5. The label shortcut

Kept, with attribution (decision 3). Removing it is one deletion: the
`ALREADY_CLEAN` branch in `create-check`. Do it only after the fleet
convergence tooling (credenza `resolve-review-feedback` Step 7.5 and
`land-pr`) has another way to pass a round whose findings were ticketed.

MAN-371 O1 and MAN-156 O4 do not change. When they add `test
(windows-latest)` (unpinned) and `PR title` (pinned to 15368), rerun the
script.

## Not done

- Full pagination of review threads or comments.
- Any change to the merge-group pass-through or `queue-retry-handler.yml`
  (MAN-243 and MAN-246 items).
- Removing the label shortcut, or changing credenza's tooling.
- Any ruleset change; O1–O3 need admin rights.
- The auto-resolve login comparison (residual risks).
- Ports to cqdx, widdershins, vanity and credenza (O4).
- Pruning the 431 accumulated `codex-clean:*` labels.

## Rollback

- **Ruleset:** O3's rollback command.
- **Code:** revert the squash commit. `wait-for-codex.yml` and
  `dependabot-auto-merge.yml` run from `main` (`pull_request_target`,
  `issue_comment`) and `ci-full.yml`'s native run from the PR, so the
  next event uses the reverted version.
