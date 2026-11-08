# MAN-547: CLAUDE.md's pre-push list matches ci-full.yml and the brief stays at 100 lines

Date: 2026-11-08. Ticket: MAN-547.

## Context

`CLAUDE.md` had grown back to 150 lines against the rule in
docs/DECISIONS/2026-10-16-man238-standing-brief.md. Its pre-push list still
named a `test-hpsdr` job and a `--features hpsdr` clippy/test pair. MAN-239
deleted that job when HPSDR input joined every build, and both crates keep
`hpsdr = []` only as a compatibility name, so the two commands rebuilt the
default build and never failed. The list also missed three suites CI runs
(`test_corpus`, `test_score_transcript`, `test_ci_shards`), and the brief held
four ticket narratives (MAN-272, MAN-273, MAN-319, MAN-14) that already have
records. `ARCHITECTURE.md` called `manta-cli → manta-dsp` dev-only; MAN-310
made it a normal dependency.

## Decision

- The pre-push list names only commands a `ci-full.yml` job runs, and every
  `scripts/tests` suite CI runs. Three tests in
  `crates/manta-cli/tests/docs_consistency.rs` pin this and the 100-line
  budget. A change that adds a unittest step to `ci-full.yml` adds the module
  to the brace list in `CLAUDE.md` in the same PR.
- The four narratives left the brief. Their records and index rows are
  unchanged.
- `hpsdr = []` stays, so `--features hpsdr` in an existing script still
  builds. Removing it is a separate decision.
- No "every `--features X` has a cfg site" test: the command test already
  rejects a feature leg CI does not run.
