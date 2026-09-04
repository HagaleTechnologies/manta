# 2026-09-04 — MAN-27 fleet checkout rename (`skimmer` → `manta`, on-disk half)

**Status:** accepted, implemented in the same PR as this document.

## Context

`docs/DECISIONS/2026-09-01-rename-to-manta.md` renamed the project, the
GitHub repository, the binary, and every workspace crate. It explicitly could
not reach **on-disk fleet state**: each machine's clone directory name
(`~/code-repos/github/HagaleTechnologies/skimmer`), its worktree parent
(`skimmer-worktrees/`), the clone's `origin` remote URL, and Catalyst's
per-host `execution-core/registry.json`. MAN-27 is that remaining half.

No environment available to implement this ticket has SSH or filesystem
access to any of the five named fleet hosts (aldebaran, rigel, sophon, vega,
or a persistent "this machine" — every dispatch container is a fresh,
single-use target, not one of the fleet's long-lived hosts). This ADR
therefore documents **tooling and a runbook**, not a completed migration —
see `docs/RUNBOOKS/fleet-checkout-rename.md`'s evidence table, which records
"not run" against every host for that same reason. Each decision below is
scoped to what could be decided and built without host access, plus what a
future operator with host access needs to know before running it.

## Decision 1 — rename by `mv` + explicit-path `git worktree repair`, never re-clone

`scripts/fleet-rename-checkout.sh --apply` moves the existing directories in
place. Re-cloning would discard local branches, stashes, reflog, and any
dirty worktree state — and the ticket itself notes the `origin` remote was
already repointed on one machine only, meaning at least one host's checkout
has diverged from a clean mirror of `origin` in exactly the way a re-clone
would silently erase.

## Decision 2 — `git worktree repair` must be given the new worktree paths explicitly

Measured against real git fixtures (git 2.47.3) while building this
tooling: **bare `git worktree repair`, run after both the main checkout and
its worktree parent have moved, is a silent no-op.** It exits `0` and prints
nothing, while `git worktree list` continues to report the linked worktree
as `prunable`:

```
$ git worktree repair; echo $?
0
$ git worktree list
/…/org/manta                  1be23a7 [main]
/…/org/skimmer-worktrees/wt1  1be23a7 [wt1] prunable
```

Passing the destination paths explicitly repairs it correctly:

```
$ git worktree repair ../manta-worktrees/wt1
repair: gitdir incorrect: .git/worktrees/wt1/gitdir
repair: .git file broken: /…/org/manta-worktrees/wt1
```

This matters because the ticket's own technical note, and most of the
folklore around this git subcommand, says only "run `git worktree repair`
after moving both" — without this ADR, a future operator following that
literal instruction gets a green exit code and a still-broken worktree.
`scripts/fleet-rename-checkout.sh` maps every pre-move worktree path onto
its post-move location and passes that list to `git worktree repair`
explicitly; it never calls the bare form. A deliberate regression test
(temporarily reverting to the bare call, confirming the test suite goes red,
then reverting) is part of this change's own verification — see the test
file's header comment and its "Testing Strategy" note.

`git worktree repair` also exits `1` if *any* argument path is invalid, even
though it still repairs the valid ones — the script filters its argument
list to paths that currently exist before calling repair, so one
already-dead worktree doesn't turn an otherwise-successful run red.

## Decision 3 — worktree health is "no NEW prunable entries" AND "every listed worktree resolves its own git dir"

The ticket's Gherkin scenario 2 checks for the absence of a `prunable` flag.
That check is necessary but **not sufficient**, also measured against real
fixtures: when only the main checkout moves and the worktree lives elsewhere
(e.g. under `~/catalyst/wt/…`, the shape this project's `projectKey` config
actually produces), `git worktree list` run from the main checkout reports
**no `prunable` flag at all** — yet the worktree is broken from the inside:

```
$ git -C ~/catalyst/wt/…/MAN-9 rev-parse --git-dir
fatal: not a git repository: /…/org/skimmer/.git/worktrees/MAN-9
```

`scripts/fleet-rename-checkout.sh`'s `verify()` therefore runs `git -C <wt>
rev-parse --git-dir` inside every worktree the inventory lists, in addition
to checking for new `prunable` flags — the check `git worktree list` cannot
make on its own.

Pre-existing `prunable` entries (e.g. a worktree directory a human already
`rm -rf`'d) are unrelated to this rename and exist independently of it. The
verifier compares against a pre-move baseline (`--apply`) or the full
currently-`prunable` set (`--check`, which never mutates anything, so
everything it observes is by definition pre-existing) and reports those as
informational rather than failing the run — otherwise a host with unrelated
worktree cruft could never pass.

## Decision 4 — `SKI-*` Catalyst worker and archive state is historical and is NOT migrated

`execution-core/workers/<TICKET>/` and its archived form are keyed purely by
ticket-ID string, which already embeds whichever Linear prefix (`SKI` or
`MAN`) was live at filing time. Those directories, and the thoughts-pool
documents under `repos/manta/shared/*/SKI-1*`, record tickets that really
were filed under the `SKI` prefix before it moved to `MAN` (MAN-24, per the
2026-09-01 rename ADR's update note). Renaming them after the fact would
misrepresent which prefix was actually live when that ticket was filed — a
strictly worse outcome than leaving them alone.

**Precedent**: MAN-24 moved the thoughts-repo directory itself
(`repos/skimmer/` → `repos/manta/`) once, and left `SKI-1`'s ticket ID and
document filenames untouched inside the new tree. This ADR takes the
ticket's own "or documented as historical" branch for fleet-side `SKI-*`
state, and this document — plus the runbook's "What is deliberately not
migrated" section — is the artifact that satisfies it. No script step
touches `SKI-*` paths.

## Decision 5 — `registry.json`: write a correct `MAN` entry, leave a stale `SKI` entry in place

`${CATALYST_DIR:-$HOME/catalyst}/execution-core/registry.json` resolves a
ticket's team prefix to a `repoRoot` (`lib/worktree-resolve.sh:8-16`).
`--apply` corrects an existing `MAN` entry's `repoRoot` (or appends one) to
point at the new path. It does **not** delete a stale `SKI` entry: reading
`checkout-sync.mjs`'s own logic shows a registry entry whose `repoRoot` no
longer exists on the current host is dropped, not treated as an error
(CTL-854) — so a stale `SKI` entry is already inert once its `repoRoot`
(the old `skimmer` path) stops existing. Deleting registry rows is a wider
blast radius than this ticket needs; the script reports the stale entry and
leaves it. `jq` is required only for this step and is optional overall —
without it, the script prints the exact manual edit needed and the rest of
the rename still completes.

## Decision 6 — the `magazzino` / `link-build-cache.sh` check runs as an on-host preflight gate

The ticket's technical note asks that `magazzino`'s clone/worktree tooling
and `link-build-cache.sh` be checked for hardcoded path assumptions before
moving anything. Neither is reachable from any environment available to
implement this ticket — `magazzino` does not appear in the thoughts pool's
repo list, is not mirrored anywhere in any container's filesystem, and no
amount of further searching from a dispatch container will find it (it
presumably exists only as a clone on the fleet hosts themselves). Rather
than leave this as an unresolved dependency, `--apply` greps a configurable
set of scan roots (default: `~/code-repos/github`, `~/bin`, `~/.local/bin`)
for literal references to the legacy checkout path and refuses to proceed if
it finds any, printing each `file:line`. The operator fixes the reference or
passes `--allow-tooling-hits` to proceed anyway, with the override recorded
in the script's own output. This converts an unreachable dependency into a
decided, self-checking, host-side step instead of an open question.

## Decision 7 — the host list is enumerated explicitly, not derived from the Catalyst cluster roster

The live Catalyst dispatch cluster's `staticRoster` is `["aldebaran",
"sophon", "vega"]` — three hosts. `rigel` is a real, separate fleet host
(it runs `vanity`'s refinery service and is a Claude-in-Chrome runner host)
that sits outside that roster. Deriving the host list from `staticRoster`
would silently skip `rigel`. The runbook enumerates the ticket's five hosts
by name (aldebaran, rigel, sophon, vega, plus whichever host is "this
machine" at execution time) instead.

## Not wired into required CI

Same rationale as `scripts/check-dependabot-cargo-unlock.sh`
(`docs/DECISIONS/2026-08-05-dependabot-cargo-single-package-unlock.md`): this
is a fleet-ops guard whose real target is host state no CI runner has, and
its test harness (`scripts/tests/test-fleet-rename-checkout.sh`) is a
manually-invoked, dependency-free bash suite against synthetic fixtures —
runnable on demand, not gating merges.

## Constraints acknowledged, not resolved, by this document

- **The fleet-side migration itself has not been run.** This PR ships tested
  tooling and a runbook; `docs/RUNBOOKS/fleet-checkout-rename.md`'s evidence
  table records "not run" for all five hosts, for the reason stated above.
  Running `scripts/fleet-rename-checkout.sh --apply` on each host, and
  filling in that table, remains outstanding follow-up work requiring host
  access this implementation environment does not have.
- **Whether a fleet host's local `~/.config/catalyst/config.json` carries a
  `catalyst.orchestration.worktreeDir` override, or whether its
  `execution-core/registry.json` already contains a stale `SKI` entry, could
  not be confirmed empirically** — only the schema and the code that
  consumes it were verified by reading `create-worktree.sh` and
  `worktree-resolve.sh`. The script handles both the documented default
  shape and common overrides (via `--org-dir`, `--catalyst-dir`), but an
  operator should read its `--check` output on each host rather than assume
  uniform state across the fleet.
