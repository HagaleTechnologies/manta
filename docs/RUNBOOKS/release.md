# Cutting a manta release

The release *pipeline* (`.github/workflows/release.yml` +
`.github/workflows/release-publish.yml`, MAN-21) is fully automated. The one
step that isn't, and can't safely be, is pushing the tag: that's a
deliberate human act — the moment someone decides "this commit is what we're
calling v0.1.0" — not an automation gap. `release-publish.yml`'s header
comment explains why the publish path has no `pull_request` trigger at all
(credential isolation); the same reasoning is why nothing in CI pushes tags
on your behalf.

Every release **must** carry the pre-stability-alpha statement (decision
[D7](../DECISIONS/2026-09-06-broad-review-decisions.md)) until manta clears
its M2/M3 acceptance gates (see `README.md` § Status and `ROADMAP.md`). CI
enforces this for the README and the release-notes template via
`crates/manta-cli/tests/release_copy.rs` (part of `cargo test --workspace`,
this repo's required check) — but the tag's *own* annotation message is not
checked by anything, so write it by hand as shown below.

## Before you push the tag

1. **`origin/main` is clean and up to date:**

   ```sh
   git fetch origin
   git switch main
   git reset --hard origin/main
   git status --porcelain   # must be empty
   ```

2. **The tag version matches `[workspace.package]` in `Cargo.toml`.** The
   release pipeline's own `verify-version` job re-checks this on the tag
   push and fails the release if it disagrees, but catching it here saves a
   failed run:

   ```sh
   sed -n '/^\[workspace.package\]/,/^\[/p' Cargo.toml | grep '^version ='
   ```

   If the workspace version needs to change, do that as a normal PR to
   `main` first, merge it, and only then tag the new merge commit — never
   bump the version in the same breath as tagging an old commit.

3. **CI is green on the commit you're about to tag.** Check the `test
   (ubuntu-latest)` / `test (macos-latest)` required checks on that commit
   in the GitHub UI or via `gh`.

4. **No tag or release already exists for this version:**

   ```sh
   git tag -l 'v*'
   git ls-remote --tags origin 'v*'
   ```

   Both should be empty (or, for a later release, should not already
   contain the version you're about to cut).

## Push the tag

Use an **annotated** tag, not a lightweight one — the annotation message is
what `git show <tag>` and several GitHub UI surfaces display, and it's the
one place in this whole procedure the pre-stability wording is written by a
human rather than templated:

```sh
git tag -a v0.1.0 -m "manta v0.1.0 — pre-stability alpha, expect breakage"
git push origin v0.1.0
```

Pushing the tag triggers both workflow files: `release.yml` (build-and-
validate, `pull_request`-safe, redundant with the publish build but
harmless) and `release-publish.yml` (the one that matters — `verify-version`,
the five-target `build` matrix, `docker-publish`, and `release`).

## Watch the run

```sh
gh run list --workflow release-publish.yml --limit 1
gh run watch "$(gh run list --workflow release-publish.yml --limit 1 --json databaseId --jq '.[0].databaseId')"
```

Expected shape and rough durations:

- `verify-version` — under a minute. If this fails, the tag and
  `Cargo.toml` disagree; see "If the release is wrong" below.
- `build` (5 legs: macOS x86_64, macOS arm64, Windows MSVC, Linux x86_64,
  Linux arm64) — 10–25 minutes. The two Linux legs are the long pole; they
  build via `cross`, which installs itself (`cargo install cross`) before
  it can build anything.
- `docker-publish` — 10–20 minutes; builds `linux/amd64` and `linux/arm64`
  under QEMU emulation for the non-native arch, which is slow by nature.
- `release` — seconds, once `build` and `verify-version` have both
  succeeded. Per this repo's decision to prioritize the GitHub Release
  binaries over the GHCR image (MAN-84, MAN-65 finding 4 / MAN-66), `release`
  does **not** wait on `docker-publish` to succeed — only to finish. A
  failed `docker-publish` does not block the Release.

## Verify the release is real

Don't trust the green checkmarks alone — check the artifact as an operator
would, from a directory that is **not** a clone of this repo:

```sh
gh release view v0.1.0 --json assets --jq '.assets[].name'
```

Expect exactly five archives: `manta-macos-x86_64.tar.gz`,
`manta-macos-arm64.tar.gz`, `manta-linux-x86_64.tar.gz`,
`manta-linux-arm64.tar.gz`, `manta-windows-x86_64.zip`.

```sh
gh release view v0.1.0 --json body --jq .body | head -5
```

Expect the pre-stability-alpha banner **above** the auto-generated
changelog (GitHub's `create-release` API prepends `body:` to
`generate_release_notes` output — not independently re-verified against a
real GitHub API response as of this writing; if the banner is missing or
the changelog replaced it instead of following it, that assumption was
wrong — edit the published notes by hand to unblock the release, then fix
`release-publish.yml`'s `release` job as a follow-up, e.g. by moving the
banner into the tag annotation or composing the body without
`generate_release_notes`).

Then, from a scratch directory with no manta checkout and no Rust
toolchain assumed:

```sh
mkdir -p /tmp/manta-release-check && cd /tmp/manta-release-check
gh release download v0.1.0 --pattern 'manta-linux-x86_64.tar.gz'
tar xzf manta-linux-x86_64.tar.gz
cd manta-linux-x86_64
./manta --version                          # expect: manta 0.1.0
./manta gen v1 --out ./v1 && ./manta decode ./v1/v1.wav   # expect W1AW text, spots: 1
```

Repeat the download-unpack-run check for at least one non-Linux archive on
a machine of that platform — the macOS, Windows, and `arm64` legs are built
in CI and were never run outside it before this release.

Finally, confirm `README.md`'s release badge
(`img.shields.io/github/v/release/HagaleTechnologies/manta`) now renders
`v0.1.0` instead of "no releases found".

## After

- **Make the GHCR package public** (one-time, per package, not per
  release): GitHub package settings → `manta` → Package settings → Change
  visibility → Public. This is tracked as MAN-66 and is explicitly **not a
  release blocker** — close out the release even if this step hasn't
  happened yet, and leave MAN-66 open until it has.
- If `docker-publish` failed while `release` still published (the behavior
  this pipeline is deliberately configured to allow), re-run **only** that
  job (`gh run rerun <run-id> --job <docker-publish-job-id>`) rather than
  re-tagging. Do not delete and re-push the tag for a GHCR-only failure —
  the GitHub Release binaries are the deliverable that matters.

## If the release is wrong

If the tag was pushed against the wrong commit, or `verify-version` should
have failed but didn't, or the release needs to be pulled entirely:

```sh
gh release delete v0.1.0 --yes      # removes the GitHub Release + its assets
git push origin :refs/tags/v0.1.0   # removes the tag from origin
git tag -d v0.1.0                   # removes the local tag
```

**None of these remove the Docker image tag already pushed to GHCR** — a
`docker-publish` run that succeeded before you noticed the problem leaves
`ghcr.io/hagaletechnologies/manta:0.1.0` (and, if this was the first tag,
`:latest`) in place. Delete those manually from the package's GitHub UI
(Package settings → Manage versions) if they need to go too, and re-tag
once the underlying problem is fixed.
