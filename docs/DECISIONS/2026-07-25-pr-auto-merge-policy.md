# PR auto-merge policy

## AMENDMENT (2026-09-01)

The "every PR, any author" scope below is narrowed by the codex-clean-gate
+ Mergify migration: native `auto-merge.yml` (which had no author check at
all, matching the "aware that `required_approving_review_count` is
currently 0" framing below) is retired in favor of Mergify's queue, which
adds an explicit `author = thagale` (or a vetted `dependabot[bot]`) gate in
`.mergify.yml`. This was flagged by Codex's own review of that migration
PR as a real behavior change against this doc's original "every PR"
language — correct, and intentional: an outside contributor's PR merging
unattended with zero human in the loop is a real exposure on a public repo
with 0 required reviews, independent of whether CI is green. This
restriction was NOT re-litigated per-repo — it's the same fix every other
public repo in this rollout (coppa, pancetta) already got, for the
identical reason. Flagged to Tony for confirmation rather than silently
assumed; revert the `.mergify.yml` author condition if the original
unrestricted scope was genuinely intended even for non-collaborator PRs.

## Decision

Repo-wide GitHub auto-merge is enabled (`allow_auto_merge: true`), squash
method. Every PR — Dependabot and agent-authored alike — gets
`gh pr merge --auto --squash` right after opening. GitHub merges it
unattended the moment the two required status checks
(`test (ubuntu-latest)`, `test (macos-latest)`) go green on an up-to-date
branch. This explicitly overrides the global "never merge a PR — Tony
merges" default for this repo; Tony chose the broadest of three offered
scopes (dependabot-only / all-PRs-CI-gated / all-PRs-with-required-review)
on 2026-07-25, aware that `required_approving_review_count` is currently 0
— i.e. green CI is the *only* gate, for every PR, including substantive
feature work.

## Why

Investigated why none of the repo's PRs were auto-merging. Root cause:
`allow_auto_merge` had simply never been turned on — not a missing
workflow, not a branch-protection gap. Branch protection already required
only the two CI jobs above, with 0 required reviews, `enforce_admins: true`,
`required_conversation_resolution: true`. Fixing the one setting plus
enabling auto-merge per-PR was sufficient; no new GitHub Actions workflow
was needed.

## What auto-merge does NOT do

- **Does not auto-update stale branches.** `required_status_checks.strict:
  true` means a PR must be up to date with `main` before its checks count.
  Auto-merge does not rebase/update a PR branch by itself — when several
  PRs are open together and one merges, the rest go `BEHIND` and sit
  waiting until something updates them
  (`gh api -X PUT repos/.../pulls/N/update-branch`, or Dependabot's own
  rebase). Observed directly: after PR #17 merged, PRs #14/#16/#27/#29/#30/#31
  all flipped to `mergeStateStatus: BEHIND` and needed one manual
  update-branch call each before their re-run CI could satisfy auto-merge.
  A GitHub **merge queue** would automate this serialization but was not
  configured — deferred as unnecessary at this repo's PR volume; revisit if
  branch-behind churn becomes frequent.
- **Does not skip real CI failures.** Verified live: PRs #15 and #18
  (`rand_core`/`rand_chacha` major-version Dependabot bumps) fail
  `test (ubuntu-latest)`/`test (macos-latest)` for real reasons and
  correctly sat `BLOCKED` with auto-merge armed but inert — auto-merge only
  removes the manual merge click, not the CI gate.
- **Does not gate on non-required checks.** `test-soapy`, where present, is
  not in `required_status_checks.contexts` — a red `test-soapy` run does not
  block auto-merge (observed on PR #30, `mergeStateStatus: UNSTABLE` while
  otherwise mergeable).

## Verification

Live-tested against the repo's real open PRs on 2026-07-25: enabling
`--auto --squash` on PR #17 (`chore(deps): bump anyhow`), which was already
CI-clean, merged it unattended within seconds — confirmed via
`gh pr view 17` showing `state: MERGED`. Nine remaining open PRs (#14, #15,
#16, #18, #19, #27, #29, #30, #31) all got auto-merge armed the same way;
outcomes (blocked on real failures, behind pending branch update, or queued
to merge on green) were checked individually, not assumed.
