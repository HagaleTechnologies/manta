# MAN-537: the Docker image pins its base images by digest, Dependabot moves them, and the image names its commit

Date: 2026-11-09. Ticket: MAN-537.

## Context

The published image built from two floating tags, `rust:1-slim-bookworm`
and `debian:bookworm-slim`, and `.github/dependabot.yml` had no `docker`
entry. Of the inputs the shipped image is built from, these were the only
ones with no pin.

The image also reported `unknown` as its commit. `.dockerignore` keeps
`.git` out of the build context and nothing passed `MANTA_GIT_SHA`, so
`crates/manta-cli/build.rs` fell back to `unknown`. `manta --version`,
`decoderVersion` on the JSON stream and `manta_build_info` on `/metrics`
read the same compile-time constant (`crates/manta-cli/src/build_info.rs`),
so all three said `unknown`. A build of a tree with no `.git` reproduced it:
`manta 0.1.0 (git unknown; features: none)`. The same tree built with
`MANTA_GIT_SHA=7020ede76042fe0ec9c1704f3780840df1ed5b11` printed
`manta 0.1.0 (git 7020ede76042; features: none)`.

## Decisions

- **D1. Both `FROM` lines are `tag@sha256:<index digest>`.** The digest is
  the manifest-list (OCI index) digest, never a per-platform one, because
  one `FROM` serves `linux/amd64`, `linux/arm64` and `linux/arm/v7`. The tag
  stays: Dependabot tracks it, and a person reads it. Digests used,
  resolved from Docker Hub's registry API on 2026-11-09 (both
  `application/vnd.oci.image.index.v1+json`):
  - `rust:1-slim-bookworm@sha256:f2c004e3d2d8b547e8cf7ffbc29205385f4e71d23e53e4e383065db5e9f822cd`
  - `debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc`
- **D2. The builder stage takes `ARG MANTA_GIT_SHA` and
  `ENV MANTA_GIT_SHA=${MANTA_GIT_SHA}`**, after `COPY . .` and before the
  `cargo build` `RUN`. They follow the apt `RUN`, so a new commit keeps that
  layer cached. The runtime stage has neither. An unset arg is empty, which
  `build.rs` treats as no override, so a local `docker build` without the
  arg still reports `unknown`. The image reports the 12-hex prefix, the
  same string a native build of that commit prints, because `build.rs` cuts
  a 40- or 64-hex value to 12 lower-case characters.
- **D3. All four `docker/build-push-action` steps in
  `release-publish.yml` pass `build-args: MANTA_GIT_SHA=${{ github.sha }}`.**
  The arm64 check build and the multi-platform push in each job pass the
  same value, so the push can reuse the check build's arm64 layers (MAN-539
  A4). On a tag push `github.sha` is the release commit, the one
  `validate-tag` checks for ancestry and green CI. `.git` stays out of the
  context: the Docker jobs' checkout keeps a `packages: write` token in
  `.git/config`.
- **D4. Dependabot docker bumps do not auto-merge.**
  `dependabot-auto-merge.yml` and `scripts/verify-dependabot-diff-safe.py`
  are unchanged. The verifier accepts only Cargo manifests and the lockfile,
  and a diff that changes only the two digests gets
  `NOT SAFE: the diff touches "Dockerfile", outside the enumerated
  Cargo.toml/Cargo.lock paths`, so such a PR is labelled `needs-review`, as
  every Actions bump already is. `/Dockerfile` is a CODEOWNERS path, and no
  PR job builds the image, so a base-image bump would otherwise merge
  unbuilt. Auto-merge for these needs two things first: a verifier mode
  that accepts a diff confined to `FROM` lines whose repository and tag are
  unchanged and only the digest differs, and a PR-time image build.
- **D5. The `docker` entry matches the other two:** `directory: /`,
  `interval: weekly`, no `groups`. The owner reviews such a PR after a
  `publish=false` dispatch of `release-publish.yml` on its branch passes
  `docker-build-dispatch` (`docs/RUNBOOKS/release.md`, "Reviewing a
  Dependabot base-image bump").
- **D6. Out of scope: the images `release-publish.yml` starts with
  `docker run`** (`ubuntu:24.04` and `debian:bookworm` for the clean-runtime
  checks, `rust:1-slim-bookworm` for the arm64 SoapySDR build). Dependabot
  does not read `docker run` lines, so digests there would be kept by hand,
  and `scripts/tests/test_soapy_release.py` and
  `scripts/tests/test_package_release.py` pin their literal names. These
  are the remaining floating images in the release workflow; the ticket's
  "the one unpinned supply-chain input" holds for the image that ships.
- **D7. The release checks the commit before it publishes.**
  `scripts/check-soapy-release.py` image mode takes `--expect-git-sha SHA`,
  normalises a 40- or 64-hex value as `build.rs` does, and fails unless
  `manta --version` names that commit. Both Docker jobs run it with
  `"$GITHUB_SHA"` on the arm64 image before the push, so a dropped
  build-arg fails the release instead of publishing `unknown`. It checks
  `--version` only: `decoderVersion` and `manta_build_info` read the same
  constant.

## Supersedes and closes

- Supersedes the "Not hard-pinning `Dockerfile`'s `rust:1-slim-bookworm`
  base tag" bullet in
  [MAN-36](2026-09-04-man36-exact-rust-toolchain-pin.md). That record left
  the base tag as a separate image-hygiene decision; this is it.
  `rust-toolchain.toml` still picks the compiler inside the builder.
- Closes [MAN-83](2026-10-10-man83-build-identity-and-decoder-versioning.md)
  deferred item 1 and
  [MAN-128](2026-10-05-man128-node-health-metrics.md) deferred item 2. The
  `release.yml` that MAN-83 also named no longer exists (MAN-241), so only
  `release-publish.yml` changes.

## Follow-ups

- Pin, or move into a Dependabot-read file, the three `docker run` images
  in `release-publish.yml` (D6).
- A PR-time image build, and with it a docker-aware verifier mode, before
  docker digest bumps may auto-merge (D4).

## Verification limits

No Docker daemon was available. The image was not built, so BuildKit
passing the `ARG` to `cargo`, and the arm64 layer reuse between the check
and push builds, are untested here. The first `publish=false` dispatch of
`release-publish.yml` shows both: its check step prints
`OK: image commit <12 hex> matches --expect-git-sha`. Dependabot was not
run either. Its first docker PRs should move only digests, or the
`1-slim-bookworm` tag and its digest together.
