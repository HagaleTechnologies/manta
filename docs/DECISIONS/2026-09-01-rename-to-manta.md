# 2026-09-01 — Rename skimmer → manta

**Status:** accepted, implemented in the same PR as this document.

## Decision

The project, GitHub repository, binary, and all workspace crates are renamed
from `skimmer` to `manta`. Crates are `manta-{decode,dsp,input,testkit,engine,
spot,cli}`; the future server crate is `manta-server`. The Linear team key and
ticket prefix stay `SKI`.

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
- `.catalyst/config.json` repository and project names. `thoughts.directory`
  stays `skimmer` until the thoughts checkout is moved.
- Historical DECISIONS and plan documents keep their original filenames.
- Comments inside the fleet-synced workflow files (`.github/workflows/
  wait-for-codex.yml`, `ci.yml`) and `.mergify.yml` are left as they are to
  avoid drift against their upstream source.

## Follow-ups

- `gh repo rename manta` after merge; GitHub redirects the old URL.
- Set the GitHub repository description and topics.
- Sibling repos that link to `HagaleTechnologies/skimmer` (cqdx, coppa,
  dispensa) rely on the redirect until touched for other reasons.
