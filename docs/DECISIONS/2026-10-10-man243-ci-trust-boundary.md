# 2026-10-10 — MAN-243: unreviewed pull-request code stays out of main's CI context and build cache

**Status:** Implemented (branch `MAN-243`). Fixes audit finding
MAN-SEC-001 (repository audit 2026-10-05, manta @ `187192a8`). The owner
steps that need hosted GitHub, a second account or admin rights are in
`docs/RUNBOOKS/ci-trust-boundary.md`.

## Problem

The Codex review gate (`.github/workflows/wait-for-codex.yml`) dispatched
the full test workflow (`ci-full.yml`) on the default branch for a
pull-request head SHA. It did so for any author and any head repository,
and for a draft it did so at once, before any review. A dispatched run
lives in `main`'s Actions cache scope: it restored `main`'s Rust build
cache and saved new entries into it, and later `main` push runs,
merge-queue runs and release builds restored those entries. Unreviewed
code could therefore shape the build state that trusted runs reuse, and
the fork-approval setting does not cover this path, because the dispatched
run is a `workflow_dispatch` run in the base repository.

## Evidence

Traced from hosted logs with read-only `gh api` / `gh run view` calls
(planning session, 2026-10-10). This is a trusted, same-repository PR; the
outside-account variant was not exercised.

| Step | Evidence |
|---|---|
| A dispatched run builds a PR branch head in `main`'s context | run `37835179902` (event `workflow_dispatch`, `head_branch` `main`) logs `HEAD is now at 0b51556`, the MAN-44 PR branch head, not a `main` commit |
| It restores `main`'s cache | same log: `Cache hit for restore-key: v0-rust-test-Linux-x64-d0c4c80e-43a3dc50` |
| It saves a new entry into `main`'s scope | same log: `Cache Key: v0-rust-test-Linux-x64-d0c4c80e-14209615`, then `... Saving cache ...`; the cache listing shows id `8698189881`, ref `refs/heads/main`, created `2026-10-08T20:09:08Z` |
| Trusted runs restore that entry later | merge-group run `37992000346` and push run `37993751581` (2026-10-09): `Restored from cache key "v0-rust-test-Linux-x64-d0c4c80e-14209615" full match: true.` |
| The dispatched run's cache config | run `37990101053`: `save-if: true`, `lookup-only: false`; paths include `~/.cargo/bin`, `.crates.toml`, `.crates2.json` and `target` |

The cache key hashes `Cargo.lock`, every `Cargo.toml`,
`rust-toolchain.toml` and `.cargo/config.toml` from the checked-out PR
tree. So any PR that touches a manifest misses the exact key, restores by
prefix and saves a fresh `main`-scoped entry built from its own code. All
43 entries in the cache listing had ref `refs/heads/main`.

## Decisions

- **D1 — trust rule.** `wait-for-codex.yml` dispatches `ci-full.yml` only
  when the PR's head repository `full_name` equals `github.repository`,
  the PR opener's login is `thagale` or `catalyst-cloud-connector[bot]`,
  and draft is exactly `false`. Every input fails closed: a null head
  repository (deleted fork), a case-variant name, an empty draft value or
  an unknown login all mean no dispatch. The identity is the PR opener
  (`pull_request.user.login`), not `github.actor`, which is the Codex bot
  on the response path and whoever pushed on `synchronize`. The list is
  the one `auto-merge-trigger.yml` already admits on (MAN-217). Both
  dispatch sites are gated: the producer's pass-through dispatch by a
  step `if:`, the response job's clean-verdict dispatch by an in-script
  guard after the check-run is completed. The response job reads the trust
  inputs from the same live `GET /pulls/{n}` that supplies the dispatched
  SHA, because an `issue_comment` payload carries none of them.
- **D2 — drafts are never dispatched, trusted or not.** A trusted draft
  gets CI when it is marked ready for review: `ready_for_review` → Codex
  request → clean verdict → dispatch. The existing-label pass-through also
  dispatches once the PR is not a draft. The ticket names pre-review draft
  dispatch as part of the defect, and this keeps `main`'s context for
  revisions that have passed a review gate. Rejected alternatives:
  - *Run trusted drafts through the native `pull_request` path.* It keeps
    early CI, but a draft → ready flip on the same SHA would leave two
    apps' `test (…)` check-runs on that commit (hagale-agent's from the
    dispatch, GitHub Actions' from the native run), the ambiguity this repo
    has hit before (HAG-106/108, CQD-336).
  - *Keep trusted-draft dispatch with restore-only caching.* It leaves
    unreviewed code in `main`'s context, where `save-if` is not a boundary
    (see below).
- **D3 — every other PR uses GitHub's isolated `pull_request` path.**
  `ci-full.yml`'s `test`, `test-soapy` and `test-hpsdr` jobs also run on
  `pull_request` when the PR is not dispatch-eligible by D1's identity
  rule: head outside the repository, or author not on the list
  (Dependabot included, as before). That run's cache scope is
  `refs/pull/N/merge`; for a fork it has a read-only token, no secrets,
  and waits on the fork-approval setting. A trusted in-repository PR is
  skipped there, so the two paths are mutually exclusive per PR. The
  native run's `test (…)` check-runs satisfy the required checks because
  the ruleset pins no `integration_id`, as already happens for Dependabot.
  The gate's Codex check behaviour (request, verdict, label) is unchanged.
  Auto-merge never arms for these authors, so the owner merges them
  deliberately.
- **D4 — only a push to `main` writes the cache.** Every `ci-full.yml`
  `Swatinem/rust-cache` step has
  `save-if: ${{ github.event_name == 'push' && github.ref == 'refs/heads/main' }}`.
  Dispatched, merge-group and native `pull_request` runs restore only.
  Merged code writes; nothing else does.
- **D5 — release builds start cold.** The `rust-cache` step is removed
  from the build job of both `release.yml` and `release-publish.yml`, with
  no replacement. A restored entry would carry `target/` objects and
  `~/.cargo/bin` + `.crates.toml` from whichever run wrote it, including a
  `cross` that `cargo install cross --version 0.2.5 --locked` would then
  treat as already installed, so a poisoned entry could supply the tool
  that builds release artifacts (inferred from cargo's documented
  behaviour, not exercised). The Docker jobs set no `cache-from`/`cache-to`
  and were already cold.
- **D6 — the trusted list is hard-coded in four places.**
  `auto-merge-trigger.yml`, the two byte-identical trust steps in
  `wait-for-codex.yml`, and the `fromJSON` list in `ci-full.yml`'s three
  test-job conditions. `scripts/tests/test_ci_trust_boundary.py` fails if
  they differ. A repository variable was rejected: it adds an owner-only
  setting, its availability on fork and Dependabot runs is a further
  variable, and it hides the list from review.
- **D7 — the cache purge happens after merge.** See the runbook. Purging
  before merge would let the still-unfixed gate re-poison the cache.
- **D8 — not blocked on MAN-241.** MAN-241's release-workflow restructure
  had no PR on 2026-10-10. The cold-build change applies to the release
  build jobs that exist now, and the test globs `release*.yml`, so it still
  holds whichever of the two lands first.
- **D9 — the invariants ride the required `test` check.**
  `python3 -m unittest scripts.tests.test_ci_trust_boundary -v` runs in
  `ci-full.yml`'s `test` job on the non-Windows legs, beside
  `release-version.test.sh`. It executes both trust-step bodies against
  an 11-row decision table under GitHub's bash flags, and checks that both
  dispatch sites are gated, the native-path condition, `save-if` on every
  cache step, the trusted list's equality, that no release workflow
  restores a cache, and that the doc paths the workflows cite exist.

## What `save-if` does and does not protect

`save-if` is hygiene, not the security boundary. Code that runs inside a
job on a GitHub-hosted runner can reach that job's cache credentials
(published cache-poisoning research, Adnan Khan 2024), so an untrusted
revision running in `main`'s context could still write `main`-scoped
entries with `save-if: false`. **The trust gate (D1) is the boundary.**
`save-if` keeps reviewed but unmerged trusted revisions out of `main`'s
cache, which is the case reproduced above.

## Residual risks

- Someone with push access can push onto a trusted author's in-repository
  branch and get dispatched. Anyone with push access is already inside
  the boundary (they can edit workflows); this matches MAN-66's accepted
  risk in `release-publish.yml`'s header.
- Trusted drafts no longer get early CI (D2).
- A dependency-bump PR's dispatched runs restore by prefix and recompile
  more until it merges and the push run saves its exact key.
- GitHub expression comparisons ignore case while the trust step's bash
  comparison does not. GitHub reports repository and login names in their
  canonical case, so this cannot split a real PR. If it ever did, the PR
  would get neither path and its required checks would stay missing, which
  fails closed.

## Owner steps

`docs/RUNBOOKS/ci-trust-boundary.md`: the pre-fix second-account check,
the fork-approval setting, the one-time cache purge and the post-merge
verifications.

## Rollback

Revert the squash commit (workflows, test and docs only). No cache action
is needed to roll back. Rolling back re-opens the dispatch and cache-save
paths; if the fix is re-landed later, repeat the purge.
