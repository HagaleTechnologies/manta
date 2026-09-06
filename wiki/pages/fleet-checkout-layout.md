# Where does the manta checkout live on a fleet machine, and what breaks if it moves?

Two independently-keyed path systems both care about the checkout's on-disk
location, and they respond to a directory rename differently.

## 1. The manual fleet clone — `~/code-repos/github/<org>/<repo>`

Each fleet machine has a plain `git clone` under
`~/code-repos/github/HagaleTechnologies/manta` (as of MAN-27; historically
`.../skimmer` — see `docs/DECISIONS/2026-09-04-man27-fleet-checkout-rename.md`),
with an optional `manta-worktrees/` sibling directory holding linked
worktrees created either by hand (`git worktree add`) or via
`create-worktree.sh --worktree-dir <override>`. Naming here is literal and
directory-name-driven: nothing renames these automatically when the repo is
renamed upstream on GitHub. `direnv-provision.sh` treats
`~/code-repos/github/<org>/` as a first-class, org-keyed directory it may
write an `.envrc` into — that convention is unaffected by a per-repo rename.

## 2. Catalyst's own orchestrator worktree pool — `~/catalyst/wt/<key>/<ticket>`

`create-worktree.sh` resolves its worktree base path via a priority chain:
`--worktree-dir` flag → `catalyst.orchestration.worktreeDir` config →
`catalyst.projectKey` config → `REPO_NAME=$(basename "$REPO_ROOT")` fallback.

This project's committed `.catalyst/config.json` sets `catalyst.projectKey`
to `HagaleTechnologies` (the org, not the repo name), so this pool resolves
to `~/catalyst/wt/HagaleTechnologies/<ticket>` — **keyed by org, not by the
clone directory's name**, and therefore unaffected by a `skimmer` → `manta`
directory rename on any host whose local config matches the committed one.
A host whose config is missing `projectKey` (or carries a stale override)
falls through to the `basename`-of-checkout fallback instead, and *is*
affected — worktrees would keep minting under whatever the directory is
named at the time.

## What actually breaks on a rename, and why

`git worktree repair` is the fix for a moved checkout and/or worktree
parent, but two things about it are easy to get wrong — both measured
against real git fixtures while building `scripts/fleet-rename-checkout.sh`
for MAN-27:

- **Bare `git worktree repair`, run after both sides have moved, is a
  silent no-op** — exits 0, prints nothing, leaves the worktree `prunable`.
  The new paths must be passed to it explicitly.
- **`git worktree list` cannot detect a main-checkout-only move.** If only
  the clone moves (worktrees live elsewhere, e.g. the `~/catalyst/wt/…`
  shape above), `git worktree list` reports no `prunable` flag at all —
  the worktree is broken only when accessed *from inside itself*
  (`git -C <wt> rev-parse --git-dir` fails).

Full detail, measurements, and the decisions built on them:
`docs/DECISIONS/2026-09-04-man27-fleet-checkout-rename.md`. The runbook for
running the migration on a real host: `docs/RUNBOOKS/fleet-checkout-rename.md`.
