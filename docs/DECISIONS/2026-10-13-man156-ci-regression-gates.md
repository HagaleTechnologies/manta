# 2026-10-13 — MAN-156: CI catches dependency, rustdoc, Windows-release and PR-title regressions

**Status:** Implemented (branch `MAN-156`). The owner steps below need
hosted runs or repository-admin rights and are not done by the change
itself.

## Problem

The 2026-09-05 broad review (lens 5, items #26, #27 and #39) found four
kinds of regression that CI let through:

1. **Dependencies.** The required "Supply Chain" check ran `cargo audit`,
   which reads RustSec advisories only and reports an unmaintained crate as
   a warning. Nothing checked licenses, dependency sources or wildcard
   requirements, and an advisory published against an unchanged
   `Cargo.lock` was never reported, because the check ran only on PRs that
   changed a manifest.
2. **Rustdoc.** No workflow ran `cargo doc`. Its warnings scrolled past
   unseen.
3. **Windows.** A Windows leg existed (MAN-212), but a trusted PR is tested
   through `workflow_dispatch`, and only `test (ubuntu-latest)` and
   `test (macos-latest)` were re-posted on the PR head. A Windows failure
   never showed on the PR, and nothing stopped a release built from a
   commit whose Windows tests had failed.
4. **PR titles.** The merge queue squash-merges, so a PR's title becomes
   the commit subject on `main`. Nothing checked it.

## Evidence

Measured on `de525d1` (`main`), 2026-10-13.

### Rustdoc

`cargo doc --no-deps --workspace --locked` exited 0 with **48 warnings**,
not the review's 9:

| Crate | Warnings | Kind |
|---|---|---|
| `manta-cli` (bin `manta`) | 21 | `[spot]`-style table names read as links, `<TABLE>`/`<KEY>`/`<call>` read as HTML tags, 2 bare URLs, all in clap `///` comments |
| `manta-spot` | 15 | `<call>`/`<CALL>` placeholders in quoted CW examples |
| `manta-decode` | 8 | `[decode]`, `**[DEVIATION]**`, `C[t] − C[s]`, `"<AR>"` |
| `manta-dsp` | 3 | the formula `lift[m] = … P[k,m] − R[k]` |
| `manta-server` | 1 | a redundant explicit link target |

By kind: 25 unclosed HTML tags, 20 unresolved links, 2 bare URLs and 1
redundant link target. After this change,
`RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace --locked`
exits 0 with no warning. Adding ``/// See [`NoSuchItem`].`` to a public
item makes it exit 101 with `error: unresolved link to `NoSuchItem``.

### Dependencies

`cargo deny 0.20.2` against the workspace with this change's `deny.toml`:

| Config | Result |
|---|---|
| `deny.toml` as committed | `advisories ok, bans ok, licenses ok, sources ok`, exit 0. Only `warning[duplicate]` for `jni-sys`, `syn`, `thiserror` and `thiserror-impl`. |
| Without the RUSTSEC-2024-0449 ignore | exit 1: `error[unmaintained]: unsafe-libyaml is unmaintained`, `ID: RUSTSEC-2024-0449`, reached through `serde_yaml_ng v0.10.0`, a `manta-cli` dev-dependency. This was the only advisory finding. |
| Without `BSL-1.0` allowed | exit 4: `licenses FAILED`, `soapysdr-sys v0.8.1` (`soapy` feature). |
| Without the coppa `allow-git` | exit 8: `sources FAILED`, `error[source-not-allowed]` for the coppa git crates. |
| A scratch crate depending on `smallvec = "=1.6.0"` (planning) | exit 1: `error[vulnerability]: Buffer overflow in SmallVec::insert_many`, `ID: RUSTSEC-2021-0003`, with the fixed versions. |

cargo-deny's exit code is a bitmask: advisories 1, bans 2, licenses 4,
sources 8. None of the 239 registry crates in `Cargo.lock` was yanked
(checked against `index.crates.io` in planning), so `yanked = "deny"`
starts green.

### Windows

- MAN-301's final head `659f8da` (PR #271) failed `test (windows-latest)`
  and `test-hpsdr (windows-latest)` in dispatched run 38244914017 with CRLF
  fixtures. The PR head showed only the two green required checks and
  auto-merged. `main`'s push runs for `3fe5205` and `fe8aabb` stayed
  Windows-red until `f67a75d` pinned the fixtures to LF.
- Of 46 push runs on `main` from 2026-10-10T08:22Z to 2026-10-13T10:21Z, 6
  were red: `3fe5205` and `fe8aabb` (the MAN-301 regression); `cb4fff9`
  and `2c7a2f5` (`test-hpsdr (windows-latest)`, a `health_command` flake);
  `b7fb51c` (`test-soapy (ubuntu-latest)`); `891a66e`
  (`test (macos-latest)`).
- In the 25 most recent merge-queue runs, `test (windows-latest)` failed 6
  times: 3 were the MAN-301 regression and 3 were `health_command` tests
  failing with `os error 10054` ("An existing connection was forcibly
  closed by the remote host") while reading the health body. That is a 12%
  flake rate on one fake HTTP server.
- Durations on those runs: `test (windows-latest)` 16–41 min (median about
  24), `test (ubuntu-latest)` 12–28 min, `test (macos-latest)` 2–27 min.
- No release or tag existed yet, so a gate added now covers v0.1.0.
- `scripts/check-release-ci.py`, run read-only against live data, passes
  `de525d1` (run 38382769742), refuses `cb4fff9` naming
  `test-hpsdr (windows-latest)=failure`, refuses `3fe5205` naming
  `test (windows-latest)=failure, test-hpsdr (windows-latest)=failure`,
  and refuses an all-zero SHA with `no ci-full.yml push run exists`.

### PR titles

Of all 261 PR titles, 251 match the rule. The 10 that do not are #1, #2,
#3, #5, #7, #13, #47, #68, #70 and #89, all from before #90.

## Decisions

1. **cargo-deny replaces cargo audit inside the existing "Supply Chain"
   job.** cargo-deny reads the same RustSec database. Running both would
   mean two allowlists that can disagree. The job keeps its name, so it
   stays required with no ruleset change, and `queue-retry-handler.yml`
   already listens to the workflow. `checks: write` is dropped: only
   `rustsec/audit-check` needed it.
2. **`deny.toml` policy.** `all-features = true`, so the soapy and hpsdr
   trees are checked. No `[graph].targets`. cargo-deny's default advisory
   severities plus `yanked = "deny"`. One `ignore`, with a reason, for
   RUSTSEC-2024-0449: it is dev-only and has no fixed release; a future
   unmaintained advisory should still stop a dependency PR for a human
   decision. A permissive license allowlist (no copyleft without a
   deliberate edit). `wildcards = "deny"` with `allow-wildcard-paths =
   true`, which works because every workspace crate is `publish = false`
   (MAN-154). Sources: crates.io and exactly the coppa git URL. The file is
   code-owned.
3. **cargo-deny is installed with `taiki-e/install-action` pinned by SHA**
   (`tool: cargo-deny@0.20.2`). Its manifest carries the binary's SHA-256,
   it takes seconds, and it uses the job's pinned toolchain. Building from
   source adds about 4 minutes per run, and no workflow outside
   `ci-full.yml` may keep a build cache (MAN-243).
4. **"Supply Chain" also runs daily on `main`** (`17 6 * * *`). A
   scheduled run has no diff, so it always scans. It turns `main`'s tip red
   and blocks nothing.
5. **The rustdoc gate rides the required `test (ubuntu-latest)` leg,
   before `cargo test`.** Rustdoc's lints do not depend on the host, so
   one leg is enough, and riding a required check makes it blocking with
   no ruleset change, as MAN-65, MAN-80 and MAN-243 did. Default features
   only: soapy needs native SoapySDR.
6. **`manta-cli` is exempt from rustdoc's link and HTML lints by one
   crate-level `#![allow]`; its help text is not rewritten.** It is a
   binary with no public API, and clap prints its `///` text verbatim as
   `--help` and man-page text. Escaping would put visible backslashes or
   backticks into operator help. A crate-level allow also covers future
   subcommands, and the crate has only 4 intra-doc links. The library
   crates get no allow: their 27 warnings, on 20 lines, are fixed in place.
   `manta --help`, the man pages and the completions are unchanged.
7. **Windows is not made a required merge check here.** It failed 3 of
   the last 25 merge-group runs on one fake-server flake; requiring it now
   would bounce about one merge-queue entry in eight. Instead, every
   dispatched PR run now posts a `test (windows-latest)` check-run on the
   PR head, so the result is visible before merge; a release cannot ship
   from a commit with a failed test leg; and the flake is fixed (the fake
   server half-closes and drains before dropping its socket). Promotion is
   step O5, triggered by measured results. This keeps MAN-230's "Windows
   stays advisory" until the data supports changing it.
8. **The release gate lives only in `release-publish.yml`,** the workflow
   that publishes; `release.yml` is build-only. `validate-tag` runs
   `scripts/check-release-ci.py` after the ancestry check and before any
   build. It requires the newest `ci-full.yml` push run for the tag's
   commit to be completed and successful, with
   `test (ubuntu|macos|windows-latest)` and
   `test-hpsdr (ubuntu|macos|windows-latest)` present and green, so
   dropping Windows from the matrix cannot pass it silently. A failed leg
   re-run to green counts, because the API reports the latest attempt.
   `validate-tag` gains `actions: read` for this.
9. **The `PR title` check uses `pull_request`** (`opened`, `edited`,
   `reopened`, `synchronize`) plus a `merge_group` pass-through, a repo
   script and no third-party action. It needs no secret, matches "Supply
   Chain"'s trust model and runs on the PR that introduces it. The title
   reaches the script only through `env:`, never `${{ }}` in `run:`. The
   allowed types are those the repository already uses: `build chore ci
   docs feat fix perf refactor revert style test`; GitHub's default
   `Revert "…"` title is accepted too. It cannot fail on `merge_group`,
   so `queue-retry-handler.yml` is unchanged, as for "Codex review clean".
   Making it blocking is step O4.

## Owner steps

These need hosted runs or repository-admin rights; the change does not
perform them.

- **O1.** Confirm the first merge-group or push run after merge: its
  `test (ubuntu-latest)` ran the `cargo doc` step green.
- **O2.** Confirm the next trusted PR's head carries `test (windows-latest)`
  from `hagale-agent`, completed with the Windows leg's real conclusion:
  `gh api repos/HagaleTechnologies/manta/commits/<head>/check-runs --jq '.check_runs[].name'`.
- **O3.** Confirm the next day's scheduled "Supply Chain" run on `main`
  succeeded: `gh run list --workflow supply-chain.yml --event schedule --limit 1`.
- **O4.** Make `PR title` blocking once it has reported success on at
  least three PRs, one of them from Dependabot:

  ```sh
  RS=repos/HagaleTechnologies/manta/rulesets/18658768
  gh api "$RS" --jq '{rules: [.rules[]
      | if .type == "required_status_checks"
        then .parameters.required_status_checks += [{"context": "PR title"}] else . end
      | if .parameters == null then del(.parameters) else . end]}' > /tmp/man156-rules.json
  gh api -X PUT "$RS" --input /tmp/man156-rules.json \
    --jq '.rules[] | select(.type == "required_status_checks") | .parameters.required_status_checks[].context'
  ```

  Expected output: the four current contexts plus `PR title`.
- **O5.** Promote `test (windows-latest)` to required only once the 20
  consecutive merge-group runs after this merge show it green, or red only
  for a real regression and never `os error 10054`. Use the same command
  with `{"context": "test (windows-latest)"}`. The check-run minting in
  `ci-full.yml` must already be on `main` first, or trusted PRs wait
  forever on "Expected". `test-hpsdr (windows-latest)` stays advisory, like
  its ubuntu and macOS siblings.

## Not done

- Changing the ruleset (O4 and O5 above).
- Requiring `test-hpsdr (*)` or `test-soapy (*)` for merge, or adding a
  Windows `test-soapy` leg.
- Running the rustdoc gate on macOS or Windows, or with `--features
  hpsdr`/`soapy`.
- Rewriting `manta-cli`'s help text.
- Keeping `rustsec/audit-check` or an `audit.toml` (none existed).
- Linting individual commit messages: squash merge uses only the title.
- Fixing the ubuntu `doctor_checks` NTP flake (`2c7a2f5`) or other fake
  servers, none observed flaking.
- Touching `queue-retry-handler.yml`, `wait-for-codex.yml`, `release.yml`
  or `CHANGELOG.md`: decoder output is unchanged.
- MAN-230 Phase 2 (`--no-fail-fast`).

## Rollback

Revert the squash commit. For partial relief, delete the `cargo doc` step,
restore `rustsec/audit-check`, or remove the release-gate step; each is
independent. If O4 or O5 was applied, remove the context from the ruleset
with the same `jq` pattern using `map(select(.context != "…"))`.
