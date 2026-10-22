# 2026-10-20 — MAN-340: a dispatched CI run tests the PR head merged with the main commit its workflow came from

**Status:** Implemented (branch `MAN-340`). It takes effect on the first
dispatch after merge, because a dispatched run reads `ci-full.yml` from
`main`. This PR's own dispatched run still uses the old file.

**Ticket:** MAN-340. It builds on MAN-243
(`docs/DECISIONS/2026-10-10-man243-ci-trust-boundary.md`, D1 and D3).
Which PRs are dispatched does not change. What a dispatched run checks out
does.

## Problem

`wait-for-codex.yml` dispatches `ci-full.yml` at `main` with
`inputs.head_sha=<PR head>` for a trusted, in-repository, ready PR. GitHub
reads the workflow file from `github.sha`, which is `main`'s tip at
dispatch, and the `test` and `test-soapy` jobs then check out the bare PR
head. When `main` gains a CI step after a branch was cut, that step runs
against a tree that lacks its files, and every OS fails on code the PR
never touched. Only merging `main` into the branch cleared it.

| Event | Workflow file read from | Tree tested before this change | Can go stale this way? |
|---|---|---|---|
| `push` | `main` tip | `main` tip | no |
| `merge_group` | queue merge commit | PR + current `main` | no |
| `pull_request` (untrusted, fork, Dependabot) | PR merge ref | `refs/pull/N/merge` = PR + base | no |
| `workflow_dispatch` (trusted, MAN-243) | `github.sha` = `main` tip at dispatch | bare PR head | **yes** |

## Evidence

Measured 2026-10-20 while planning, against `7d63be7` (`main`).

Searching every failed dispatched run of 2026-10-10..12 for
`No module named 'scripts.tests.test_packag` gave three runs, all with
`github.sha` = `99c70e4` (MAN-268's merge, which added the
`test_packaging` and `test_package_release` steps):

| Run | `github.sha` (workflow source) | Checked-out head | PR |
|---|---|---|---|
| 38045561387 | 99c70e4 | e6ec89e | #231 MAN-270 |
| 38046070391 | 99c70e4 | c591626 | none found (head is on no ref) |
| 38053658487 | 99c70e4 | c7a01d2 | #233 MAN-79 |

Run 38045561387 is `event=workflow_dispatch`, `headBranch=main`,
`headSha=99c70e46…`, conclusion `failure`. Its macOS leg checked out
e6ec89e at 10:38:00Z and failed at 10:55:43Z with
`ModuleNotFoundError: No module named 'scripts.tests.test_packaging'`.
Its Windows leg failed at 11:02:51Z on
`No module named 'scripts.tests.test_package_release'`.

A worktree at e6ec89e reproduces it:
`python3 -m unittest scripts.tests.test_packaging -v` gives the same
`ModuleNotFoundError`. c7a01d2 gives the same error.

Both real failing heads also conflict with `99c70e4`. The `merge-main`
step body on e6ec89e + 99c70e4 gives
`CONFLICT (content): Merge conflict in crates/manta-cli/Cargo.toml` and
`::error title=PR head conflicts with main::… (crates/manta-cli/Cargo.toml)`.
On c7a01d2 + 99c70e4 the conflict is in
`crates/manta-cli/tests/config_command.rs`. Each PR needed a manual merge
of `main` anyway (PR #231 has `fix(cli): MAN-270 — merge main and
reconcile output style …`).

A synthetic stale PR (13131ca plus a one-line docs commit) through the
same step:

- merged with 99c70e4: `HEAD^1` is the PR head, `HEAD^2` is 99c70e4, and
  `test_packaging`, `test_package_release` and `test_ci_trust_boundary`
  pass;
- merged with 7d63be7: every Python suite `ci-full.yml` runs passes, as do
  `release-version.test.sh` and `release-ancestry.test.sh`;
- `cargo test -p manta-cli --test build_identity --locked` on the merged
  tree: `6 passed; 0 failed; 2 ignored` (`HEAD` is the local merge
  commit).

## Decisions

### D1. Merge `main` into the PR head inside the dispatched run's own checkout

This tests what the merge queue and the native path already test: the PR
plus its base. The executed workflow definition stays `main`'s (MAN-243's
guarantee), and no credential is added: the merge commit is never pushed
and `persist-credentials: false` stays.

Rejected:

- **Skip steps whose inputs are missing.** It silently drops coverage,
  every future step would need its own guard, and it covers only missing
  files, not a new feature flag, crate or lint level.
- **Run the PR head's own workflow for trusted authors.** That reopens
  what MAN-243 closed: a same-repository PR could replace `ci-full.yml`.
  Dispatching at a PR branch also fails the `codex-review` environment's
  main-only deployment rule that `create-ci-check` and `relay-check-runs`
  need, so the required checks would never be created.
- **Push a merge of `main` onto the PR branch, or only label it "behind
  main".** Pushing needs a write credential in the gate and writes onto
  someone else's branch. A new head SHA restarts the Codex round and the
  dispatch. A label alone leaves the checks red for the same wrong reason.
- **Check out `refs/pull/N/merge`.** It needs a third dispatch input, and
  GitHub computes that ref asynchronously, so it may not match `head_sha`
  or the `main` commit whose workflow runs.

### D2. Merge exactly `github.sha`, not `main`'s live tip

`github.sha` is the commit the running workflow file came from, so every
file a step expects is in the tree by construction, unless the PR itself
deleted it. The result is deterministic for a given dispatch and needs no
network call. If `main` moves after dispatch, the merge queue re-tests
against the newer tip.

### D3. The step is inline and byte-identical in `test` and `test-soapy`

A script under `scripts/` would be missing from every PR head cut before
this change, which is the same bug. `test_ci_trust_boundary.py` keeps the
two bodies identical, as it does for the two MAN-243 `trust` steps.
Inputs arrive through `env:`, never `${{ }}` inside `run:`.

### D4. A conflict fails the leg; there is no fallback to the bare head

A fallback would bring back the `ModuleNotFoundError` this ticket removes.
A conflicting head cannot land anyway: the merge queue refuses it, and
GitHub does not run `pull_request` workflows on a conflicted PR. The
annotation is titled `PR head conflicts with main` and names the files.

### D5. Full history only on dispatch

The merge needs a merge base, so the checkout's `fetch-depth` is
`${{ github.event_name != 'workflow_dispatch' && 1 || 0 }}`: 0 (all
history) on dispatch, 1 otherwise. The inverted form is needed because
`cond && 0 || 1` is always 1 (0 is falsy in expressions). Native, push and
merge-group checkouts stay as they were. The repository is about 9 MB with
290 commits on `main`.

### D6. Fail closed on an unexpected checkout

The step stops with its own annotation title when `HEAD` is not
`head_sha` (`Unexpected checkout`), the clone is shallow
(`Shallow checkout`) or `github.sha` is not present
(`Default-branch commit missing`). Each is a broken assumption, not a PR
defect, and none can be mistaken for one.

### D7. `--no-ff` with a fixed local identity

When a merge happens, `HEAD` is always a two-parent commit (`HEAD^1` the PR
head, `HEAD^2` `github.sha`). The identity is `github-actions[bot]` with its
noreply address, passed by `-c`, with `commit.gpgsign=false`. A head that
already contains `github.sha` (a branch rebased onto `main`, or a manual
dispatch with no `head_sha`) is tested as it is, with the log line
`… already contains …`. A clean merge logs the notice
`Tested with main merged in`.

### D8. The relay and the check-run output stay as they are

The reason for a conflict is in the leg's error annotation and log, and
GitHub's PR page already shows "This branch has conflicts". Changing
`create-ci-check` or `relay-check-runs` (MAN-371's fail-closed code)
would add nothing for this ticket.

### D9. The tests go into `test_ci_trust_boundary.py`

That module is MAN-243's and already runs in the required `test` job, so
no new CI step or module is needed and the pre-push command list does not
change.

## Tests

`scripts/tests/test_ci_trust_boundary.py`:

- `CiFullDispatchMergeWiringTests`: the step follows checkout in both jobs,
  with its `if:`, `shell:`, name and exactly two `env:` keys; both
  checkouts use the dispatch-only `fetch-depth` (and a Python mirror of the
  expression checks all four events and the `&& 0 ||` trap); the two
  bodies are byte-identical; the body names no secret, token, push, fetch,
  remote or `scripts/` path, and no permission is added; the step runs only
  on `workflow_dispatch`.
- `CiFullDispatchMergeBehaviourTests` runs the real body from
  `ci-full.yml` under `bash -eo pipefail` against throwaway repositories: a
  PR cut before `main` gained `scripts/tests/test_packaging.py` fails with
  `No module named 'scripts.tests.test_packaging'` before the step and
  passes after it; the PR's own changes and deletions survive; a head that
  already contains `main`, and a manual dispatch, are left as they are; a
  conflict fails with `PR head conflicts with main` naming the file and
  aborts the merge; a shallow clone, a wrong checkout and a missing `main`
  commit each fail closed.
- `DocsReferenceTests`: `ci-full.yml` and the runbook name this record.

## Residual risks

- A clean textual merge can still fail on a semantic conflict. That is
  intended: the PR as it would land is broken, and the merge queue would
  fail it too.
- The checks reflect `main` as of dispatch. A later `main` is re-tested by
  the merge queue.
- `wiki_manifest_head_commit_contains_every_page_stamp` in
  `crates/manta-cli/tests/docs_consistency.rs` skips in a shallow clone,
  so it now runs on dispatched runs whenever the manifest's `head_commit`
  is fetchable. That makes those runs stricter, never looser.
- The annotation lives on the dispatched run, whose native check suite
  attaches to `main`'s commit, not on the PR's relayed check-run (D8).
- The Windows leg runs the step under Git Bash. Only the first dispatch
  after merge exercises it (`docs/RUNBOOKS/ci-trust-boundary.md`, O9).

## Verifying after merge

`docs/RUNBOOKS/ci-trust-boundary.md`, "When a trusted PR's dispatched test
check fails" and owner step O9.

## Rollback

Revert the squash commit. The next dispatch reads the reverted
`ci-full.yml` from `main`.
