# 2026-09-01 — Rename skimmer → manta

**Status:** accepted, implemented in the same PR as this document.

## Decision

The project, GitHub repository, binary, and all workspace crates are renamed
from `skimmer` to `manta`. Crates are `manta-{decode,dsp,input,testkit,engine,
spot,cli}`; the future server crate is `manta-server`.

**Update (MAN-24, 2026-09-02):** the Linear team key and ticket prefix were
originally kept as `SKI` per this decision, but Linear's own team key was
separately renamed to `MAN` shortly after (confirmed live via the API — same
team UUID, `26e8448d-…`). `.catalyst/config.json`'s `linear.teamKey` and
`project.ticketPrefix` now say `MAN` to match; see MAN-24.

## Why

- `skimmer` collided with the generic noun ("a CW skimmer") and with **CW
  Skimmer**, the closed-source product this project exists to replace. Prose
  could not distinguish the project from the category.
- A manta ray is a filter feeder with a wide wingspan: it strains everything
  out of one wide pass of water, which is what the polyphase channelizer does
  to a passband. The logo (`assets/logo*.svg`) is a manta skimming over a
  spectrum with a few narrow CW carriers in it.
- Alternatives considered: `baleen` (same filter-feeder metaphor, quieter
  name), `dragnet`. `manta` is a crowded word on GitHub (storage, genomics,
  crypto, an invoicing app), but nothing in the SDR or amateur-radio space
  uses it, and the crate names are prefixed regardless.

## What changed

- Crate directories, package names, `use` paths, `CARGO_BIN_EXE_*` in tests,
  CI clippy/test targets, Cargo `repository` URL.
- Prose in `README.md`, `ARCHITECTURE.md`, `ROADMAP.md`, `CLAUDE.md`,
  `SECURITY.md`, `docs/`, `wiki/`. The generic phrase "CW skimmer" is kept
  wherever it means the category.
- `.catalyst/config.json` repository, project, and `thoughts.directory`
  values. The thoughts checkout itself (`repos/skimmer` in the
  `HagaleTechnologies/thoughts` repo) was moved to `repos/manta` separately
  in MAN-24, once `thoughts.directory` here already said `manta`.
- Historical DECISIONS and plan documents keep their original filenames.
- Comments inside the fleet-synced workflow files (`.github/workflows/
  wait-for-codex.yml`, `ci.yml`) and `.mergify.yml` were initially left as they
  were, to avoid drift against their upstream source.

  **Superseded by MAN-25 (2026-09-04):** those copies have no automated sync --
  `wait-for-codex.yml`'s own note reads "this repo's copy has no auto-sync, so
  port future template fixes here manually too", and `.github/workflows/`
  contains no sync workflow -- so a local edit is not reverted by machinery. The
  six remaining comment references were renamed to `manta` (a `manta#45` issue
  ref resolves through GitHub's rename redirect), and
  `crates/manta-cli/tests/synced_ci_files_repo_name.rs` fails CI if a future
  manual port reintroduces the old name. Editing the upstream template as well
  (see Follow-ups) is what stops a port from carrying it back in the first place.

## Follow-ups

- `gh repo rename manta` after merge; GitHub redirects the old URL.
- Set the GitHub repository description and topics.
- Sibling repos that link to `HagaleTechnologies/skimmer` (cqdx, coppa,
  dispensa) rely on the redirect until touched for other reasons.
- MAN-25: apply the same rename in the upstream `wait-for-codex.yml` /
  `ci.yml` / `.mergify.yml` template (canonical copy cited as `credenza`,
  PR #30) so the next manual port carries the new name. Requires access to
  that repo; tracked separately from the manta-side fix.
