# Dependabot Cargo single-package unlock failures

## Context

Four consecutive weekly `cargo in /.` Dependabot runs failed: `29701682936`,
`29708341690`, `30225803112`, and `30772807189`. Each reported
`Failed to update serde!` and summarized the dependency as `unknown_error` /
`null`. Targeted dependency runs continued succeeding, which made the broken
full-workspace update path easy to miss.

Dependabot runs `cargo update -p <name>:<previous-version>` separately for each
dependency, then checks that the requested version landed. That check raises at
`lockfile_updater.rb:127` in `validate_dependency_update`. For the affected
release, `serde 1.0.229` requires `serde_core =1.0.229`, which in turn requires
`serde_derive =1.0.229`. The single-package unlock cannot move the exact-pinned
companions, so `cargo update -p serde:1.0.228` exits successfully without
changing the lockfile and Dependabot raises afterward.

The complete investigation and source trace are in
`thoughts/shared/research/2026-08-05-ski-1.md`.

## Decisions

1. When this failure shape occurs, maintainers widen Cargo's unlock scope with
   a precise update. For this incident the remedy was:

   ```text
   cargo update -p serde --precise 1.0.229
   ```

   Cargo then moved `serde`, `serde_core`, and `serde_derive` together. Once the
   lockfile is current, Dependabot reports no update needed instead of testing
   an unreachable target.

2. Do not add `ignore` or `groups` to `.github/dependabot.yml` for this failure.
   Ignoring suppresses a real update. Grouping changes PR presentation, but
   Dependabot's Cargo lockfile updater still runs one `cargo update -p <spec>`
   per dependency and therefore does not widen the unlock scope.

3. Use `scripts/check-dependabot-cargo-unlock.sh` as the on-demand detector. It
   compares each dependency's single-package result with a full semver-compatible
   resolution in disposable worktree copies. It is deliberately not a required
   CI check: it queries live crates.io state and could fail without a repository
   change, blocking unrelated pull requests.

## Verification log

Environment for all observations: `cargo 1.94.1 (29ea6fb6a 2026-03-24)`,
`rustc 1.94.1 (e408947bf 2026-03-25)`.

Red, at base commit `361f030ce1d66e256406d4cf44f5a82df77ddc8e` before changing
`Cargo.lock`:

```text
$ scripts/check-dependabot-cargo-unlock.sh serde
FAIL serde: single-package unlock reaches 1.0.228, full unlock reaches 1.0.229
     Dependabot will raise "Failed to update serde!" for this crate.
     Remedy: cargo update -p serde --precise 1.0.229

One or more dependencies are unreachable via Dependabot's single-package unlock.
exit 1
```

Green, after applying the precise update from the same base (committed as
`04552c9`):

```text
$ scripts/check-dependabot-cargo-unlock.sh serde
OK: every checked dependency is reachable via a single-package unlock.
$ cargo update -p serde:1.0.229 --dry-run
Locking 0 packages to latest compatible versions
warning: not updating lockfile due to dry run
```

Workspace sweep, after the portable crate-list loop was added on top of
`04552c9` (committed as `b2ac450`):

```text
$ scripts/check-dependabot-cargo-unlock.sh
Checking: cpal ctrlc libc anyhow num-complex hound serde serde_json clap criterion rand_core rand_chacha proptest approx tempfile rayon tungstenite rubato soapysdr regex
OK: every checked dependency is reachable via a single-package unlock.
```

The workspace passed `cargo fmt --all --check`,
`cargo clippy --workspace --all-targets -- -D warnings`, and
`cargo test --workspace` after both implementation phases.

## Recurrence

This can recur whenever a crate publishes an allowed release that exact-pins a
companion package while this repository remains on the previous family version.
The signal is a failed `Dependabot Updates` run titled `cargo in /. - Update #…`
whose dependency table reports `unknown_error` / `null`. Run the guard for the
named crate, then use its reported precise-update command and re-run the guard.
