# Fleet checkout rename: `skimmer` → `manta` (MAN-27)

## What and why

The repo-side rename landed on 2026-09-01
(`docs/DECISIONS/2026-09-01-rename-to-manta.md`): project, GitHub repository,
binary, crates, and `.catalyst/config.json` all say `manta` now. What that
rename could not reach is **on-disk fleet state**: the literal directory name
of each machine's clone (`~/code-repos/github/HagaleTechnologies/skimmer`),
its worktree parent (`skimmer-worktrees/`), the clone's `origin` remote URL,
and Catalyst's per-host `execution-core/registry.json`. MAN-27 covers that
half. `docs/DECISIONS/2026-09-04-man27-fleet-checkout-rename.md` is the ADR
for the decisions this runbook and `scripts/fleet-rename-checkout.sh`
implement — read it first if a step here seems arbitrary.

This runbook was **not run against real fleet hardware** at the time it was
written — see "Evidence table" below. It was authored and its underlying
script fully exercised against synthetic git fixtures inside an ephemeral,
single-use container with no SSH access to any fleet host (see
`docs/DECISIONS/2026-09-04-man27-fleet-checkout-rename.md`'s "Constraints"
section). Whoever runs this on a real host is the first person to confirm it
against reality — fill in the evidence table as you go, same convention as
`docs/RUNBOOKS/m2-pi4-cpu-budget.md`'s "Runs" log.

## Preconditions

- bash + git ≥ 2.30 (`git worktree repair` landed in 2.30).
- `jq` is optional — the registry-reconciliation step degrades to a printed,
  actionable warning without it; the directory/worktree/origin rename still
  completes.
- Stop the Catalyst daemon (and close any open agent session) on that host
  before running `--apply` — the script refuses to move a checkout that
  looks busy (open `index.lock`, in-progress rebase/merge/bisect, or a live
  process whose command line references the checkout path), but "looks
  busy" is a best-effort heuristic, not a guarantee.
- Obtain `magazzino` (its clone/worktree tooling and `link-build-cache.sh`)
  on that host if it isn't already there — the ticket's technical note asks
  that both be checked for hardcoded old-path assumptions before moving
  anything. This runbook's procedure enforces that check automatically (see
  "Tooling preflight" below); it could not be done ahead of time because
  `magazzino` is not reachable from any dispatch container (see the ADR).

## Procedure

```bash
cd ~/code-repos/github/HagaleTechnologies/manta 2>/dev/null \
  || cd ~/code-repos/github/HagaleTechnologies/skimmer

scripts/fleet-rename-checkout.sh --check          # read-only; nothing is moved
scripts/fleet-rename-checkout.sh --apply
scripts/fleet-rename-checkout.sh --check          # must exit 0, all PASS
```

Flags (all optional; defaults match this ticket's shape):

| Flag | Default | Purpose |
|---|---|---|
| `--check` / `--apply` | (required, pick one) | read-only audit vs. perform the migration |
| `--old NAME` | `skimmer` | legacy directory name |
| `--new NAME` | `manta` | target directory name |
| `--org-dir DIR` | `~/code-repos/github/HagaleTechnologies` | parent directory holding the clone |
| `--remote-url URL` | `https://github.com/HagaleTechnologies/manta.git` | desired `origin` URL |
| `--catalyst-dir DIR` | `~/catalyst` (or `$CATALYST_DIR`) | where `execution-core/registry.json` lives |
| `--scan-root DIR` (repeatable) | `~/code-repos/github`, `~/bin`, `~/.local/bin` | roots the tooling preflight greps for hardcoded old-path references |
| `--allow-tooling-hits` | off | proceed with `--apply` despite tooling preflight hits (records the override in the output) |

**cwd caveat**: on a legacy host the script lives inside the directory being
moved. Running it from that directory is fine — bash has already read the
script file into memory, and every path the script touches is derived from
`--org-dir`/`--catalyst-dir`, not from `$PWD` — but the invoking shell's own
`cwd` will point at a now-renamed (or, briefly, nonexistent) path afterwards.
Run `cd "$OLDPWD"`-equivalent (i.e. `cd ~/code-repos/github/HagaleTechnologies/manta`)
or open a new shell before the final `--check`. **This is the one step to
re-verify empirically on the first host** — if it proves awkward in
practice, copy the script to `/tmp` first and run it from there instead.

## Tooling preflight

Before moving anything, `--apply` greps `--scan-root` directories (default:
`~/code-repos/github`, `~/bin`, `~/.local/bin`) for literal references to the
old checkout path or to `<old-name>-worktrees`, and refuses to proceed if it
finds any — this is the ticket's "check magazzino's clone/worktree tooling
and `link-build-cache.sh` for path assumptions" instruction, turned into an
executable gate that runs on the host where those tools actually exist
(rather than a step this document could complete in advance). Point
`--scan-root` at wherever `magazzino` and `link-build-cache.sh` live on that
host if they're outside the default roots.

## Evidence table

One row per host, filled in during execution. `–` = not yet run.

| Host | Checkout present? | `--check` before | `--apply` result | `--check` after | Notes |
|---|---|---|---|---|---|
| aldebaran | – | – | – | – | not run — no SSH/filesystem access to this host from the container this tooling was authored in (MAN-27 implementation phase, 2026-09-04) |
| rigel | – | – | – | – | not run — same reason; also outside the 3-host Catalyst dispatch roster, so may not have a checkout at all (research §G) |
| sophon | – | – | – | – | not run — same reason |
| vega | – | – | – | – | not run — same reason |
| *(implementation host)* | – | – | – | – | not run — the container this was authored in has no `~/catalyst/`, no `~/code-repos/`, and is not a persistent fleet host (research §F) |

## Troubleshooting

Each refusal mode below is a `die()` call in `scripts/fleet-rename-checkout.sh`;
the script's own message is the source of truth if this table and the script
ever disagree.

| Exit | Situation | Remedy |
|---|---|---|
| 2, "both … are present" | Both `skimmer` and `manta` directories exist under `--org-dir` | Compare `git -C <dir> log -1` and `git -C <dir> status` on each; decide which is authoritative; remove or archive the other; re-run |
| 2, "in use" | An `index.lock`/`HEAD.lock` file, an in-progress rebase/merge/bisect, or a live process referencing the checkout path was detected | Stop the Catalyst daemon and any open session on that checkout; clear the lock only if you've confirmed no process actually holds it; re-run |
| 2, "destination worktree parent already exists" | `<new-name>-worktrees` already exists alongside a still-legacy `<old-name>` clone | Resolve by hand — decide which worktree parent is authoritative, merge or remove the other — then re-run |
| 2, "refusing to move while tooling still hardcodes the legacy path" | The tooling preflight found a hit under a scanned root | Fix the reference (in `magazzino`'s tooling, `link-build-cache.sh`, or a shell rc file), or re-run with `--allow-tooling-hits` only if the hit is provably inert (e.g. a comment) — record that judgement in this table's Notes column |
| 1, "MIGRATION NEEDED" / "MIGRATION INCOMPLETE" | `--check` found the legacy name, or `--apply` completed but a verification check still failed | Re-run `--apply`; if it fails a second time, read the specific FAIL lines above the verdict — each one names exactly what's wrong |

## What is deliberately not migrated

`execution-core/workers/SKI-*/` and any archived `SKI-*` worker state, plus
the thoughts-pool documents under `repos/manta/shared/*/SKI-1*` — these
record tickets genuinely filed under the `SKI` prefix before the Linear
team key moved to `MAN` (MAN-24). Renaming them would misrepresent history.
See `docs/DECISIONS/2026-09-04-man27-fleet-checkout-rename.md`, Decision 4,
for the full rationale and precedent.
