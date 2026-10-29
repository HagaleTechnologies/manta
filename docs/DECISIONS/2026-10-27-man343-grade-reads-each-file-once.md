# MAN-343: `manta grade` reads each input file once, however it is named

Date: 2026-10-27. Ticket: MAN-343, a Codex P2 on PR #271 (MAN-301)
deferred under `docs/DECISIONS/2026-08-07-pr-review-convergence-policy.md`.

## Context

`expand_paths` in `crates/manta-cli/src/grade_cmd.rs` turned the `--spots`
and `--rbn` arguments into a file list without remembering what it had
already returned, and both loaders take their file list and their "N files"
count from it. A file named twice was read twice. At `95b13fa`, with
`dumps/20261011.csv` and `spots.jsonl` copied from
`crates/manta-cli/tests/fixtures/rbn_grade/` and `link.csv` a symlink to
the CSV:

| Run | Output |
|---|---|
| `--rbn dumps/` (baseline) | `rbn: 9 CW reports from 1 file` |
| `--rbn dumps/ --rbn dumps/20261011.csv` | `rbn: 18 CW reports from 2 files`; the 14025.1 kHz W1AW record's `rbnReports` 3 became 6 |
| `--spots spots.jsonl --spots ./spots.jsonl` | `spots: 8 from 2 files`; 8 grade records for 4 spot ids |
| `--rbn link.csv dumps/20261011.csv` | `rbn: 18 CW reports from 2 files` |

A whole duplicated input doubles every count, so the modal frequency and
the median WPM stay put and only the counts are wrong. A partly duplicated
input can flip a grade. With `dumps/rbn-a.csv` (two skimmers: W1AW at
14026.0 kHz, 22 WPM) and `dumps/rbn-b.csv` (three skimmers: 14025.0 kHz,
25 WPM), the fixture's 14025.1 kHz, 25 WPM W1AW spot graded `match` from
`--rbn dumps` and `disagree` (`frequency`, modal 14026000, median 22.0,
7 reports) from `--rbn dumps dumps/rbn-a.csv`.

## Decision

1. The fix is one seen-set in `expand_paths`. It covers both flags, the file
   list and the summary's file count. Spot lists and RBN lists are
   deduplicated separately.
2. The set is keyed on `std::fs::canonicalize`, which resolves `./`, `..`,
   symlinked files and symlinked directories. The list keeps the first
   spelling of each file, in argument order, so every error message still
   names the path as typed, and a macOS `/var` path is not rewritten to
   `/private/var`. `scripts/shadow-compare.py`'s `expand_paths` does the
   same with `os.path.realpath`.
3. A path that cannot be canonicalized, such as a missing file, keys as
   given. The loader's `cannot read <path>` error then reports it once, as
   typed.
4. A directory whose only matching file was already named is not an error:
   the `has no <what> files` check runs before deduplication.

The regression tests are `grade_reads_a_file_named_directly_and_through_its_directory_once`,
`grade_reads_a_symlinked_file_or_directory_once` (Unix only),
`grade_does_not_reweigh_one_rbn_file_named_twice` and
`grade_names_a_missing_input_as_typed_once` in
`crates/manta-cli/tests/grade_cmd.rs`, and
`expand_paths_keeps_each_file_once_at_its_first_spelling` in
`grade_cmd.rs` itself.

## Consequences

- Hard links are not detected: a hard link is a different path to the same
  inode, which canonicalization does not resolve, and std has no stable
  inode check on Windows, a CI leg. Two files with the same content, such
  as a re-downloaded dump, are also read twice; RBN rows carry no id to
  deduplicate on.
- Dropping a duplicate is silent. The summary's `from N file(s)` shows what
  was read, and output for inputs without duplicates is unchanged.
- The grading engine (`manta_server::rbn_compare::grade`) is unchanged; the
  live comparator takes no file list.
- `scripts/score-against-rbn.py --node-log` inherits the fix for CSV and
  directory arguments. The same `.zip` named twice is still read twice,
  because `_unzip_rbn` extracts each into a fresh temporary directory.
- Windows (`\\?\`) and macOS (`/private/var`) canonical forms were not run
  locally; CI's `test` matrix covers them.
