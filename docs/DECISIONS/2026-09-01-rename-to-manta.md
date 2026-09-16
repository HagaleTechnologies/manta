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
  five remaining comment references in the two workflow files were renamed to
  `manta` (a `manta#45` issue ref resolves through GitHub's rename redirect), and
  `crates/manta-cli/tests/synced_ci_files_repo_name.rs` fails CI if a future
  manual port reintroduces the old name. Editing the upstream template as well
  (see Follow-ups) is what stops a port from carrying it back in the first place.

  `.mergify.yml` carried a sixth reference and was guarded alongside them until
  the native-merge-queue cutover (#185, 2026-09-11) deleted the file outright;
  its replacement, `.github/workflows/auto-merge-trigger.yml`, never carried the
  old name. No *comment or prose* in this repo names the pre-rename project any
  more, so the ticket's acceptance grep (`git grep -i skimmer -- .github
  .mergify.yml`) still returns nothing.

  **Do not read that as "nothing names the old project" — one live value still
  does, deliberately.** `crates/manta-server/src/spot_message.rs:228` emits
  `source: "skimmer"` on every JSON spot, and three tests pin that exact string
  (`crates/manta-server/tests/json_stream_acceptance.rs:120` and `:609`,
  `crates/manta-server/src/spot_message.rs:314`). It is not an oversight of this
  rename: per `CLAUDE.md`, the JSON spot schema is an ecosystem contract shared
  with `dispensa`/`cqdx`, so the field is a wire value that cannot be renamed in
  manta alone -- changing it here unilaterally breaks consumers. It is therefore
  out of scope for MAN-25 (a comment-only ticket) and tracked as a coordinated
  cross-repo follow-up below. When this ADR is used as the rename inventory,
  that field is the one known outstanding item.

## Follow-ups

- `gh repo rename manta` after merge; GitHub redirects the old URL.
- Set the GitHub repository description and topics.
- Sibling repos that link to `HagaleTechnologies/skimmer` (cqdx, coppa,
  dispensa) rely on the redirect until touched for other reasons.
- **Outstanding, cross-repo:** `crates/manta-server/src/spot_message.rs`'s
  `source: "skimmer"` JSON field (and the three tests pinning it) still carries
  the old project name. Renaming it is a wire-protocol change to the shared spot
  schema, so it needs a `dispensa`/`cqdx` co-ordinated ticket -- consumers must
  accept the new value before manta emits it (or accept both during a
  transition). Not filed from the MAN-25 session, which held no Linear
  credential; recorded here so the inventory is not lost.
- **Deferred under the review-convergence policy**
  (`docs/DECISIONS/2026-08-07-pr-review-convergence-policy.md`), captured
  verbatim from PR #93's round-4 Codex review so it is not lost:

  > **Pin the trusted-author gate as well** -- If a future edit removes or
  > broadens the workflow's job-level author `if:` while leaving the `gh pr
  > merge --auto` invocation intact, this assertion still passes even though
  > the shared bypass-capable `CODEX_REVIEW_PAT` would let untrusted PRs merge
  > without approval. Since the test describes the successor as the
  > trusted-author admission boundary, it should also verify the two-author
  > allow-list and the credential used to invoke `gh`.

  Not fixed in MAN-25: `merge_policy_successor_to_mergify_exists` exists to
  prove merge policy *moved* to `auto-merge-trigger.yml` after #185, not to
  guard that workflow's trust boundary, and MAN-25 is a comment-rename
  ticket. Guarding the `if:` allow-list membership and the
  `secrets.CODEX_REVIEW_PAT` binding is a separate, worthwhile invariant --
  it belongs with the auto-merge policy ADR
  (`docs/DECISIONS/2026-07-25-pr-auto-merge-policy.md`), which is what would
  have to be amended alongside any change to that list. Not filed as a Linear
  ticket from the MAN-25 session, which held no credential; recorded here so
  the inventory is not lost.
- MAN-25: apply the same rename in the upstream `wait-for-codex.yml` /
  `ci.yml` template (canonical copy cited as `credenza`, PR #30) so the next
  manual port carries the new name. Requires access to that repo; tracked
  separately from the manta-side fix. The upstream `.mergify.yml` template is
  moot for manta after #185 retired Mergify here, but still worth renaming for
  the sibling repos that keep using it.
