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

1. **`origin/main` is clean and up to date.** Check *this checkout* for
   uncommitted work **first** — before touching the working tree at all:

   ```sh
   git fetch origin
   git status --porcelain   # must print nothing; stop here if it does
   ```

   If that printed anything, **stop**: commit or stash it, or cut the
   release from a scratch worktree instead
   (`git worktree add /tmp/manta-release main`, then work there). Do
   **not** reach for `git reset --hard origin/main` to "get clean" — it
   destroys uncommitted tracked work irreversibly, and it does it *before*
   any later cleanliness check can tell you what was lost. Once the tree is
   clean, fast-forward instead, which fails loudly rather than discarding
   local commits:

   ```sh
   git switch main
   git merge --ff-only origin/main
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

Resolve the run by **the commit your tag points at** and the `push` event —
not by global recency. `--limit 1` alone returns the newest run of the
workflow, which is somebody else's run if a second tag push or a
`workflow_dispatch` publish started around the same time, and every later
step in this runbook (including the recovery block under "After") reuses
this run id:

```sh
TAG=v0.1.0
TAG_SHA="$(git rev-list -n 1 "$TAG")"   # annotated tag -> the commit it points at
RUN_ID="$(gh run list --workflow release-publish.yml --event push \
            --commit "$TAG_SHA" --limit 1 --json databaseId --jq '.[0].databaseId')"
echo "watching run $RUN_ID"
gh run watch "$RUN_ID"
```

`--commit` and `--event` are both `gh run list` filters (`gh run list
--help`). If the tag has been deleted and re-pushed against the *same*
commit, this matches the old run as well — `--limit 1` takes the newest,
which is the one you just started.

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
would, from a directory that is **not** a clone of this repo.

Because that directory has no manta remote, `gh` has nothing to infer the
repository from and every command below would abort before checking a
single artifact. So each one names the repo explicitly with `-R/--repo`
(`gh release view --help`); if you'd rather not repeat it, export it once
for the shell instead and drop the flags:

```sh
export GH_REPO=HagaleTechnologies/manta   # alternative to --repo below
```

```sh
gh release view v0.1.0 --repo HagaleTechnologies/manta \
  --json assets --jq '.assets[].name'
```

Expect exactly five archives: `manta-macos-x86_64.tar.gz`,
`manta-macos-arm64.tar.gz`, `manta-linux-x86_64.tar.gz`,
`manta-linux-arm64.tar.gz`, `manta-windows-x86_64.zip`.

```sh
gh release view v0.1.0 --repo HagaleTechnologies/manta \
  --json body --jq .body | head -5
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
CHECK_DIR="$(mktemp -d)" && cd "$CHECK_DIR" \
  && gh release download v0.1.0 --repo HagaleTechnologies/manta \
       --pattern 'manta-linux-x86_64.tar.gz' \
  && tar xzf manta-linux-x86_64.tar.gz \
  && cd manta-linux-x86_64 \
  && ./manta --version \
  && ./manta gen v1 --out ./v1 && ./manta decode ./v1/v1.wav
# expect: `manta 0.1.0`, then the W1AW text and `spots: 1`
```

Use `mktemp -d`, not a fixed path like `/tmp/manta-release-check`, and keep
the `&&` chain. A reused directory still holds the *previous* run's archive
and unpacked binary; `gh release download` refuses to overwrite an existing
file unless you pass `--clobber` (`gh release download --help`), so on a
second run the download fails — and in an interactive shell, with no
`set -e` to stop it, the unpack-and-run steps then happily validate the
*old* asset and report success for a release you never downloaded. A fresh
directory each time makes that impossible; the `&&` chain means a failed
download never reaches `./manta --version` even if you do reuse one.

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
  this pipeline is deliberately configured to allow), fix GHCR without
  re-tagging. Do **not** delete and re-push the tag for a GHCR-only failure
  — the GitHub Release binaries are the deliverable that matters.

  Two ways, and the cheap-looking one isn't:

  **a. Re-run the job in CI — but this is not a job-only retry.** `gh run
  rerun --job` reruns that job *"including dependencies"* (`gh run rerun
  --help`), and `release-publish.yml` declares `docker-publish` as
  `needs: [verify-version, build]`. So every GHCR retry this way also
  repeats `verify-version` **and the whole five-leg `build` matrix** — the
  full 10–25 minute release build, not a targeted 10–20 minute Docker leg.
  Budget for that before you reach for it. `--job` also wants the job's
  **`databaseId`**, not the job number in the Actions URL — `gh run rerun
  --help` warns that the URL's number returns `404 NOT FOUND` — so look the
  id up first, resolving the run by the tagged commit (same lookup as
  "Watch the run", never `--limit 1` on its own):

  ```sh
  TAG=v0.1.0
  TAG_SHA="$(git rev-list -n 1 "$TAG")"
  RUN_ID="$(gh run list --workflow release-publish.yml --event push \
              --commit "$TAG_SHA" --limit 1 --json databaseId --jq '.[0].databaseId')"
  gh run view "$RUN_ID" --json jobs --jq '.jobs[] | {name, databaseId}'
  # NOT a docker-publish-only rerun: this also re-runs verify-version and
  # all five build legs (`--job` reruns the job "including dependencies"),
  # so budget the full 10-25 minute release build, not a Docker-only leg.
  gh run rerun --job <the docker-publish databaseId from the line above>
  ```

  **b. Push the image by hand.** Usually faster, and it touches nothing but
  GHCR. From any machine with Docker and a `write:packages` token, at the
  tagged commit:

  ```sh
  IMAGE=ghcr.io/hagaletechnologies/manta
  echo "$GHCR_TOKEN" | docker login ghcr.io -u "$GITHUB_USER" --password-stdin
  docker buildx build --platform linux/amd64,linux/arm64 \
    -t "$IMAGE:0.1.0" -t "$IMAGE:latest" --push .
  ```

  A `workflow_dispatch` publish is **not** a third option: that path tags
  the image `dispatch-<run_id>` and never `:latest` (see `docker-publish`'s
  "Determine version, image, and tag list" step), so it cannot repair
  `:0.1.0` or `:latest`.

## If the release is wrong

If the tag was pushed against the wrong commit, or `verify-version` should
have failed but didn't, or the release needs to be pulled entirely:

```sh
gh release delete v0.1.0 --yes      # removes the GitHub Release + its assets
git push origin :refs/tags/v0.1.0   # removes the tag from origin
git tag -d v0.1.0                   # removes the local tag
```

**None of these remove the Docker image tags already pushed to GHCR** — a
`docker-publish` run that succeeded before you noticed the problem leaves
both `ghcr.io/hagaletechnologies/manta:0.1.0` **and `:latest`** pointing at
the bad image. `:latest` is republished on *every* real tag push, not just
the first one (`release-publish.yml`'s `docker-publish` job appends
`$IMAGE:latest` to its tag list for any `push` event), and README's install
command is `docker run ghcr.io/hagaletechnologies/manta:latest` — so
leaving `:latest` alone hands the bad image to every reader who follows the
README. Always deal with both:

1. Delete the bad version tag from the package's GitHub UI (Package
   settings → Manage versions).
2. **Fix `:latest` — every time, not only after the first release.** A
   `:latest` left pointing at a withdrawn image is the failure this step
   exists to prevent, and it is the *normal* case: every real tag push
   republishes `:latest`, so the second and every later bad release leaves
   it stale exactly as the first one does. Which of the two branches below
   applies depends only on whether an earlier good release exists — never
   on skipping the step.

   **a. An earlier good release exists — re-point `:latest` at it.** Do this
   from any machine with Docker and `buildx`; a `workflow_dispatch` publish
   will *not* do it for you, since the workflow pushes `:latest` only for
   real tag pushes. The login is required even to *read* the existing tag,
   for as long as the package is private (MAN-66), and the token needs
   `read:packages` plus `write:packages` to copy one tag onto another:

   ```sh
   IMAGE=ghcr.io/hagaletechnologies/manta
   echo "$GHCR_TOKEN" | docker login ghcr.io -u "$GITHUB_USER" --password-stdin
   docker buildx imagetools create -t "$IMAGE:latest" "$IMAGE:<last-good-version>"
   docker buildx imagetools inspect "$IMAGE:latest"   # expect BOTH platforms
   ```

   `docker buildx imagetools create` copies the **manifest list** between
   tags server-side, which is what keeps `:latest` multi-platform. Do
   **not** restore it with `docker pull` + `docker tag` + `docker push`:
   on an ordinary single-platform Docker engine a pull resolves a
   multi-platform image down to the host's own OS/architecture
   ([Multi-platform builds](https://docs.docker.com/build/building/multi-platform/)),
   so re-pushing that local image would republish `:latest` as amd64-only
   or arm64-only while `docker-publish` and README both promise
   `linux/amd64` *and* `linux/arm64` — silently breaking `docker run` for
   every reader on the other architecture. The `imagetools inspect` line
   is the check: it must list both platforms.

   **b. No good release exists yet (the bad one was the first) — delete
   `:latest`.** There is nothing to point it at: delete the `latest` version
   in the same Package settings → Manage versions UI, and expect the
   README's `docker run` command to fail until the next good tag
   republishes it. Deleting is the correct outcome here — leaving the tag
   alive and bad is not.

   Either way, confirm before you walk away: `docker manifest inspect
   ghcr.io/hagaletechnologies/manta:latest` must either resolve to the
   good image's digest **with both `linux/amd64` and `linux/arm64` in its
   manifest list** (branch a) or fail with `manifest unknown` (branch b).
   Anything else — including a digest that resolves but lists only one
   platform — means `:latest` is still not what README promises.

Then re-tag once the underlying problem is fixed.
