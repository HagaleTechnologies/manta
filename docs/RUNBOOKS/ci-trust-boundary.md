# CI trust boundary: who runs where, and the owner's one-time steps

This runbook is for the repository owner. It covers how each kind of pull
request is tested since MAN-243, how to get an outside contribution tested
and merged, and the one-time steps that need admin rights, a second
account or hosted GitHub. The rationale is in
`docs/DECISIONS/2026-10-10-man243-ci-trust-boundary.md`. How the Codex
gate classifies Dependabot, titles an override and fails closed on an
incomplete thread read, and which source may report each required check,
are in `docs/DECISIONS/2026-10-16-man245-gate-verdict-integrity.md`.
Why a dispatched run tests the PR head merged with `main`, and what
`PR head conflicts with main` means, is in
`docs/DECISIONS/2026-10-20-man340-dispatched-ci-tests-the-merge-with-main.md`.

## Routing

"Trusted" means the PR opener is `thagale` or
`catalyst-cloud-connector[bot]`. "In-repository" means the head branch is
in `HagaleTechnologies/manta`, not a fork.

| PR | Codex gate check | CI path | Cache writes | Auto-merge |
|---|---|---|---|---|
| Trusted, in-repository, ready | Codex request → verdict, as before; a `codex-clean:<sha12>` label the gate did not apply passes as `Codex review overridden by <login>` (MAN-245) | `ci-full.yml` dispatched on `main` after a clean verdict (or the existing `codex-clean:<sha12>` label); each `test`/`test-soapy` leg tests the PR head merged with the `main` commit the run was dispatched from (MAN-340) | none (restore-only) | arms |
| Trusted, in-repository, draft | completed as success at once, as before | none until marked ready for review | none | does not arm (draft) |
| Untrusted author, in-repository | Codex request → verdict, as before | `ci-full.yml` native `pull_request` run (`refs/pull/N/merge` scope) | none (restore-only) | never arms; owner merges |
| Fork (any author) | Codex request → verdict, as before | native `pull_request` run; waits for approval under the fork-approval setting below | none (restore-only) | never arms; owner merges |
| Dependabot update (opened by dependabot[bot] from this repository, event from dependabot[bot]) | completed as success at once, as before | native `pull_request` run, as before | none (restore-only) | `dependabot-auto-merge.yml`, as before |
| Human event on a Dependabot PR | Codex request → verdict, as before | native `pull_request` run, as before | none (restore-only) | no auto-approve, as before |

Only a push to `main` (a merged commit) saves a build cache. Release
builds (`release-publish.yml`) restore none.

## Getting an outside pull request tested and merged

1. Read the diff first.
2. Approve its native `pull_request` run: the PR's Checks tab, or
   **Actions → the waiting run → Approve and run**.
3. Review it as usual. Auto-merge never arms for an untrusted author, so
   merge it yourself once the required checks are green:
   `gh pr merge <n> -R HagaleTechnologies/manta --squash --auto`.
4. Alternatively, adopt it: push the reviewed commits to an in-repository
   branch and open the PR as a trusted author. It then takes the normal
   dispatched path.

Never run `gh workflow run ci-full.yml -f head_sha=<outside sha>` before
reading the diff. That runs the outside code in `main`'s context, which is
exactly what MAN-243 closed.

## When a trusted PR's dispatched test check fails

A dispatched run reads `ci-full.yml` from `main`, so its steps are
`main`'s. Before any other step, each `test` and `test-soapy` leg merges
`github.sha` (the `main` commit the run was dispatched from) into the PR
head, in the runner's own checkout. Nothing is pushed. A step added to
`main` after the branch was cut therefore finds its files, and the PR is
judged as it would land.

- If a leg's first error is `PR head conflicts with main`, the head does
  not merge cleanly with that `main` commit. The annotation names the
  conflicted files. Merge `main` into the branch, resolve those files and
  push. The new head gets a fresh Codex round and dispatch.
- `Unexpected checkout`, `Shallow checkout` or
  `Default-branch commit missing` means the run's own setup broke, not the
  PR. Re-run the failed jobs; if it repeats, the checkout step changed.
- Any other failure is the PR's code as it would land on `main`.

Find the run and what it tested:

```bash
gh run list -R HagaleTechnologies/manta --workflow ci-full.yml --event workflow_dispatch --limit 20 --json databaseId,createdAt,conclusion
gh run view <id> -R HagaleTechnologies/manta --log | grep -m3 -E 'HEAD is now at|Tested with main merged in|already contains|PR head conflicts with main'
```

## One-time owner steps

### O1. Confirm the pre-fix behaviour (before approving the MAN-243 PR)

While the old gate is still live, from a second account with no access to
this repository: fork it, make a harmless one-line change (for example a
README comment) and open a **draft** PR. Then:

```bash
gh run list -R HagaleTechnologies/manta --workflow ci-full.yml --event workflow_dispatch --limit 5 --json databaseId,createdAt
gh run view <id> -R HagaleTechnologies/manta --log | grep 'HEAD is now at'   # for a run created after the PR opened
```

If that names the fork's head SHA, the audit's inference is confirmed.
Close the PR afterwards; O4 cleans up the cache.

### O2. Review and approve the MAN-243 PR

Code-owner review is required for `/.github/` and `/scripts/`.

### O3. Fork-approval setting

Settings → Actions → General → "Approval for running fork pull request
workflows from contributors" → **Require approval for all external
contributors**. This makes native fork runs wait for the owner too.

API alternative (check the accepted values with a `GET` first):

```bash
gh api repos/HagaleTechnologies/manta/actions/permissions/fork-pr-contributor-approval
gh api -X PUT repos/HagaleTechnologies/manta/actions/permissions/fork-pr-contributor-approval -f approval_policy=all_external_contributors
```

### O4. One-time cache purge (after merge)

Wait until the merge commit's push-to-`main` CI run has **finished**.
Purging before merge would let the old gate re-poison the cache, and
purging during that run could let it save an entry derived from a
poisoned restore. Do this before MAN-84's first release tag.

```bash
gh run list -R HagaleTechnologies/manta --workflow ci-full.yml --event push --limit 1 --json databaseId,status,headSha
gh cache list -R HagaleTechnologies/manta --limit 200 --json id,key,ref,createdAt > caches-pre-purge.json   # keep as the record
gh cache delete --all -R HagaleTechnologies/manta --succeed-on-no-caches
gh cache list -R HagaleTechnologies/manta                       # expect: no caches
gh run rerun <that push run id> -R HagaleTechnologies/manta     # repopulate from the trusted merge commit
gh cache list -R HagaleTechnologies/manta --json key,ref,createdAt   # expect: only refs/heads/main entries created after the purge
```

## Verifying after merge

### O5. An outside contribution waits for the owner

Repeat O1 as both a draft and a ready PR. Expect no new
`workflow_dispatch` run building the fork head. The PR shows a
`pull_request`-event CI run, possibly awaiting approval under O3, and the
gate's Codex check behaves as before. The gate's log names the reason:
`Not dispatching ci-full.yml: head repository '…' is not
HagaleTechnologies/manta …`.

### O6. A pre-merge test run leaves shared build state untouched

Around the next trusted PR's dispatched run:

```bash
gh cache list -R HagaleTechnologies/manta --ref refs/heads/main --limit 200 --json id,createdAt > before.json
# ... dispatched run <id> completes ...
gh cache list -R HagaleTechnologies/manta --ref refs/heads/main --limit 200 --json id,createdAt > after.json
gh run view <id> -R HagaleTechnologies/manta --log | grep -c '\.\.\. Saving cache \.\.\.'   # expect 0
gh run view <id> -R HagaleTechnologies/manta --log | grep -m3 'save-if: false'           # expect hits
```

New ids in `after.json` may only come from a push-to-`main` run that
overlapped; check that run's log if any appear.

### O7. A release build starts cold

```bash
gh workflow run release-publish.yml -R HagaleTechnologies/manta --ref main   # publish defaults to false; build-only, never creates a Release
gh run view <id> -R HagaleTechnologies/manta --log | grep -cE 'Restoring cache|Cache restored|Cache hit for|Restored from cache key'   # expect 0
```

### O8. The trusted path still runs unattended

The next trusted, ready PR goes Codex clean → dispatched CI → auto-merge
armed → merge queue → merged, with no manual merge:
`gh pr view <n> -R HagaleTechnologies/manta --json mergedAt,mergedBy`.

### O9. A dispatched run tests the merge with main (MAN-340)

The MAN-340 PR's own dispatched run still reads the old `ci-full.yml` from
`main`, so these checks start with the first dispatch after it merges.

- **O9a, first dispatch.** On the next trusted PR's dispatched run, every
  `test (...)` and `test-soapy (...)` leg shows the step
  `Merge the main commit this workflow came from into the PR head`, and its
  log shows `already contains` or `Tested with main merged in`. Check the
  Windows leg too: it is the only run of the step under Git Bash. The PR
  head's three `test (...)` check-runs still complete with the legs'
  conclusions.
- **O9b, a stale branch (optional).** From a trusted account, push a branch
  cut from a `main` commit before MAN-301 (so
  `scripts/tests/test_score_against_rbn_replay.py` is absent) with a
  one-line docs change, and open it ready for review. Expect every leg to
  pass `python3 -m unittest scripts.tests.test_score_against_rbn_replay -v`,
  or to fail only with `PR head conflicts with main`. Close the PR and
  delete the branch afterwards.
- **O9c, the native path is unchanged.** On the next native `pull_request`
  run (a Dependabot PR is the usual one), the merge step shows as skipped
  and the checkout fetched one commit:
  `gh run view <id> -R HagaleTechnologies/manta --log | grep -m1 -- '--depth=1'`.
  The Codex gate's log for an untrusted PR still reads
  `Not dispatching ci-full.yml: …`.

## Changing the trusted list

The list lives in four workflow places. Change all of them, and the
`TRUSTED` set in `scripts/tests/test_ci_trust_boundary.py`, together; the
test fails unless they all match:

- `.github/workflows/auto-merge-trigger.yml` (the job `if:`)
- `.github/workflows/wait-for-codex.yml`, the `trusted_authors=( … )` line
  in both "Decide whether this revision may run CI in the default-branch
  context" steps (their bodies must stay byte-identical)
- `.github/workflows/ci-full.yml`, the `fromJSON('[…]')` list in the
  `test` and `test-soapy` job conditions
