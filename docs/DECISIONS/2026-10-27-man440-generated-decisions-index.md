# MAN-440: the decision-record index is generated; record PRs add only their file

Date: 2026-10-27. Ticket: MAN-440.

**Status:** accepted. The generator is `scripts/decisions-index.py`, the
refresh workflow is `.github/workflows/decisions-index.yml`, and both are
tested by `scripts/tests/test_decisions_index.py`.

## Problem

Every PR that added a decision record also added a row at the top of the
table in `docs/DECISIONS/README.md`. Two such PRs both insert a line after
the same `|---|---|---|` separator, so once the first merges the second
conflicts and the merge queue ejects it.

- 38 of the 57 commits on main between 2026-10-21 and 2026-10-27 touched
  the index.
- On 2026-10-24, six PRs (MAN-120/170/220/266/279/280) cost about four extra
  re-merge and 45-minute CI cycles from this collision alone.
- Reproduced on the real file: two branches that each added a record and a
  row gave `CONFLICT (content): Merge conflict in docs/DECISIONS/README.md`
  when the second merged after the first.
- The flow below reproduced clean: two branches that each added only a
  record file merged with `Merge made by the 'ort' strategy.` in either
  order, and the regenerated index listed the newer one first.
- Hand ordering had already drifted: main at `95b13fa` listed a 2026-10-25
  record above six 2026-10-26 ones because it landed last.

## Decision

1. **The table is generated.** `scripts/decisions-index.py --write`
   rewrites the table between the `<!-- BEGIN decisions-index … -->` and
   `<!-- END decisions-index -->` comments and leaves the hand-written text
   around them alone. Each row's date and ticket come from the file name,
   `YYYY-MM-DD-manNNN-slug.md` (or `man-NNN`). The title is the record's
   line-1 `# ` heading, minus a leading `YYYY-MM-DD — ` (em dash, en dash or
   hyphen). Records have no front matter, and the name and heading already
   hold what the index needs, so no record is migrated.
2. **A ticketless name** takes the first `MAN-NNN` in its heading, else `—`.
   Only `2026-09-03-cross-source-dedup-algorithm-spike.md` (MAN-53) needs
   that today; renaming it would break three inbound links.
3. **Order:** date descending, then ticketed before ticketless, then ticket
   number descending, then file name. It depends only on the files, so
   `--write` is idempotent and the refresh cannot loop. Same-day records had
   no meaningful order before either.
4. **A `|` in a title** is written as `\|` so it cannot split the cell.
   Nothing else is escaped.
5. **One heading was wrapped** onto line 2 (MAN-120's), so GitHub rendered
   half of it as a paragraph. It is joined into one line with no words
   changed. Every other record is untouched, and its index title is now its
   own heading: 13 rows read differently, and two hand annotations
   (`; MAN-226 correction, 2026-10-23`) are dropped because their records
   already carry the note.
6. **What a PR's own CI checks.** The record PR does not list itself, so
   the checks tolerate an index that lags:
   - `decision_records_have_indexable_names_and_headings` in
     `crates/manta-cli/tests/docs_consistency.rs` requires every record to
     have a parseable name, a `# ` line 1 and a blank line 2. It replaces
     `decisions_index_links_every_record`, which failed until the PR added
     its row and was what forced the conflict.
   - `python3 scripts/decisions-index.py` (the default check, and
     `RealIndexTests` in CI) requires every listed row to link an existing
     record with the right date and ticket, in order, with no duplicates.
     It names unlisted records and passes. It does not compare titles, so a
     PR that edits an old record's heading is not forced to edit the table.
   - `--strict` requires the README to equal what `--write` produces. The
     refresh PR meets it.
   - `reference_docs_relative_links_resolve` still fails on a row whose
     file is gone.
7. **The refresh workflow is the table's only writer.** `Decisions Index`
   runs on every push to main that touches `docs/DECISIONS/**`, daily at
   05:23 UTC, and on demand. It checks out main's commit without
   credentials, runs `--write`, and fails if anything but the README
   changed. When the README changed, it points `automation/decisions-index`
   at that commit and writes the README through the contents API with
   `CODEX_REVIEW_PAT`, then opens `docs(decisions): refresh the
   decision-record index`. That PR touches only the README, which no other
   PR touches, so it never conflicts with a record PR.
8. **The PAT author takes the existing trusted path.** A PR opened through
   `CODEX_REVIEW_PAT` is authored by `thagale`, so
   `auto-merge-trigger.yml` arms it and `wait-for-codex.yml` dispatches its
   CI, with no change to the trusted-author list. The contents API keeps a
   git credential off disk and out of any URL. The PAT is read only by the
   step that opens the PR, in the main-only `codex-review` environment.
9. **At most one refresh PR is open.** An open one that is not
   `CONFLICTING` is left alone: updating a queued PR's head restarts its CI
   and drops it from the queue. The push its merge makes re-runs the
   workflow, which adds whatever landed meanwhile. A `CONFLICTING` one is
   closed and replaced by one built from the current main.
10. **Exit codes:** `0` ok, `1` the index is wrong (or, under `--strict`,
    incomplete or stale), `2` a record or the README is malformed.

## What changes for an author

- Add the record file and nothing else under `docs/DECISIONS/`. Name it
  `YYYY-MM-DD-manNNN-slug.md`, keep the `# ` title on line 1 and leave
  line 2 blank.
- Your record appears in the index once the refresh PR after yours lands.
  The directory listing is complete at all times.
- A PR that renames or deletes a record must run
  `python3 scripts/decisions-index.py --write` in the same change, because
  the dangling-row checks fail otherwise.
- If `docs/DECISIONS/README.md` conflicts in a merge, take either side and
  run `python3 scripts/decisions-index.py --write`; never hand-merge the
  table.

## Supersedes

- The "its row in `docs/DECISIONS/README.md`" clause of
  [2026-10-16-man238-standing-brief.md](2026-10-16-man238-standing-brief.md):
  a ticket's outcome goes in its decision record, and its row is generated
  after it lands.
- The "every decision record is linked from `docs/DECISIONS/README.md`"
  check described in
  [2026-10-11-man149-docs-reference-and-index.md](2026-10-11-man149-docs-reference-and-index.md):
  the index is complete once the refresh lands, not in the record's own
  change.

## Rejected

- **`merge=union` on the README.** It merges two prepended rows in merge
  order, not date order, and hosted merges have historically ignored
  `.gitattributes` merge drivers
  ([rubocop#3594](https://github.com/rubocop-hq/rubocop/pull/3594),
  [gitlab-foss#17325](https://gitlab.com/gitlab-org/gitlab-foss/-/issues/17325),
  [BSERV-19161](https://jira.atlassian.com/browse/BSERV-19161)).
- **Dropping the table** for the dated file names. It loses the title and
  the ticket a reader scans for.
- **Regenerating the table inside the record PR.** It still inserts one line
  at the same place, so it still conflicts.
- **Front matter on each record.** None exists; adding it to every record
  would recover data the name and heading already hold.
- **A new bot identity for the refresh PR.** It would widen the trusted
  list in five places (`scripts/tests/test_ci_trust_boundary.py`'s
  `TRUSTED`, `auto-merge-trigger.yml`, both `wait-for-codex.yml` trust
  steps and `ci-full.yml`).

## Lag and failure modes

- Main's index lags a new record by one refresh PR: a Codex review, a
  dispatched CI run and a queue run, roughly one to two hours (not
  measured).
- A record PR opened before this landed that still adds a row conflicts
  once on the README. Resolve it with
  `git checkout origin/main -- docs/DECISIONS/README.md`.
- The refresh step fails, for example because the PAT lacks Contents write:
  `main-alert.yml` watches `Decisions Index` and opens
  `main is red: Decisions Index`.
- A refresh PR stalls, for example on a Codex findings review: close it, and
  the next run (push, schedule or `workflow_dispatch`) opens a fresh one.
- The PAT's user turns out not to be `thagale`: the refresh PR is then
  authored off the trusted list. It still gets CI through the native
  `pull_request` path, but not auto-merge, and a maintainer arms it by hand.

## Owner step

- **O1:** confirm `CODEX_REVIEW_PAT` has Contents: write and Pull requests:
  write on manta. It already arms auto-merge and dispatches workflows; the
  agent that built this cannot read secret scopes.

## Follow-up candidates

Not filed by this change: `wiki/INDEX.md` (18 of the same 57 commits) and
`CHANGELOG.md` (8 of 57) collide the same way.
