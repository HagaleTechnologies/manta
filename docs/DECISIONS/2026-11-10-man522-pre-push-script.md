# MAN-522: one command runs the pre-push gate in CI order

Date: 2026-11-10. Ticket: MAN-522.

## Context

CLAUDE.md lists about 14 pre-push commands. No script, Makefile or hook ran
them, so contributors copied them one at a time and only CI ran the whole list.
The ticket's hpsdr drift note was already fixed by MAN-547
(docs/DECISIONS/2026-11-08-man547-brief-matches-ci.md).

## Decisions

1. `bash scripts/pre-push.sh` runs every CLAUDE.md command in the order
   `ci-full.yml`'s `ubuntu-support` job runs them, then
   `cargo test --workspace --locked` last. CI's `cargo test --workspace --doc`
   step is dropped: the full local test run covers doctests, and the slowest
   step goes after the quick ones.
2. One `python3 -m unittest scripts.tests.test_<name> -v` per suite, as CI
   runs them, so a failure names the suite.
3. On macOS only the three Linux-only suites are skipped (`ci_gates`,
   `decisions_index`, `verify_dependabot_diff_safe`). Rustdoc still runs.
4. Each step prints `==> [i/N] <command>`. The first failure prints
   `pre-push: FAILED (exit <rc>) at step i/N: <command>` and exits with that code.
5. `--fast` runs fmt, then clippy with `-p` for each crate changed since
   `git merge-base HEAD origin/main` (uncommitted and untracked files count).
   It falls back to `--workspace` when `origin/main` is missing, root
   `Cargo.toml` or `Cargo.lock` changed, or no crate changed.
6. `--list` prints the full gate's commands for this host and runs nothing, so
   tests read the list without parsing bash. The script stays bash 3.2-compatible.
7. CLAUDE.md keeps its `sh` fence (four tests pin it) and names the script in
   the prose above it, at 100 lines.

## Rule

A PR that adds, removes or reorders an `ubuntu-support` step changes
`scripts/pre-push.sh` in the same PR. `PrePushScriptTests` in
`scripts/tests/test_ci_gates.py` fails until it does, and also fails when a
CLAUDE.md command is missing from the script. This extends MAN-547's rule.

## Cost

Measured in the Catalyst container: everything except the workspace tests took
614 s from a cold build. The workspace tests took 3586 s per the MAN-547
validation. Expect about an hour for the full gate.
