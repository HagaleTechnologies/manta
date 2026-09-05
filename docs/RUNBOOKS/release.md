# Cutting a manta release

This is the operational runbook for the release pipeline
(`.github/workflows/release.yml`, `.github/workflows/release-publish.yml`).
It exists so a maintainer who has never cut a manta release can do so from
this document alone. See
`docs/DECISIONS/2026-09-05-man65-release-pipeline-hardening.md` for the
design rationale behind the behaviour described here.

## Cutting a release

1. If the release changes it, bump the workspace `version` in `Cargo.toml`
   and commit that change on `main` first — the release tag and the crate
   version are independent today (the Docker/artifact version comes solely
   from the Git tag), but keeping them in step avoids confusion later.
2. Tag the commit `vX.Y.Z` (see "Accepted tag grammar" below) and push the
   tag: `git tag v1.2.3 && git push origin v1.2.3`.
3. Pushing the tag triggers both workflows:
   - **`release.yml`** builds and packages all five targets (macOS
     x86_64/arm64, Windows x86_64, Linux x86_64/arm64) and validates the
     multi-arch Docker build (`push: false`) — this run never publishes
     anything; it exists so a tag push gets the same build-validation a PR
     would have gotten.
   - **`release-publish.yml`** does the real work, in order:
     `validate-tag` (rejects an unpublishable tag in seconds, before any
     platform build starts) → `build` (rebuilds the same five targets) →
     `docker-publish` (pushes the multi-arch image to GHCR as
     `ghcr.io/hagaletechnologies/manta:X.Y.Z`) → `publish-latest` (see
     below) and, in parallel, `release` (creates the GitHub Release from
     the five build artifacts).
4. Watch the `release-publish.yml` run's summary for the GHCR visibility
   warning — see below.

## Accepted tag grammar

Accepted: `vX.Y.Z` with an optional SemVer pre-release, e.g. `v1.2.3`,
`v0.1.0`, `v1.2.3-rc.1`.

**Rejected: SemVer build metadata** (`v1.2.3+linux`) and anything else that
isn't the shape above. `+` is legal in a Git ref and in SemVer, but is not
in Docker/OCI's tag grammar (`[A-Za-z0-9_][A-Za-z0-9._-]{0,127}`) — pushing
such a tag used to reach Buildx 30 minutes into the build, get rejected
there, and silently skip the GitHub Release along with it (MAN-65 finding
1). Today, `validate-tag` rejects it within seconds of the push, before any
of the five platform builds start, and the job's own log names the
accepted grammar. If this happens: delete the bad tag
(`git push origin :refs/tags/v1.2.3+linux`) and re-tag without the
suffix.

The tag *trigger* itself is also narrower than a bare `v*` glob
(`v[0-9]*`), so an unrelated tag like `vendor-freeze` never invokes either
release workflow at all — but the trigger glob can't express the full
SemVer grammar, so `validate-tag` is still what gives a near-miss release
tag its explicit, readable failure.

## Pre-releases never become `:latest`

A pre-release tag (`v1.3.0-rc.1`) still publishes its own version tag
(`ghcr.io/hagaletechnologies/manta:1.3.0-rc.1`) but `publish-latest`
deliberately leaves `:latest` untouched — README's `docker run
ghcr.io/hagaletechnologies/manta:latest` install command must always hand
users a release, never a release candidate.

## The one-time GHCR visibility step

GitHub Container Registry creates a brand-new container package with
**private** visibility on its first push, and there is no supported REST
endpoint reachable from a workflow's `GITHUB_TOKEN` to change that. This is
deliberately **not automated** — the only way to close it with the token
this workflow holds would be storing a long-lived personal access token
with `admin:packages` in the repo purely to flip one switch once, which is
a worse security posture than a single manual click (the same "flag it to
a human, don't silently work around it" disposition this repo already
applies to MAN-66, `.github/workflows/release-publish.yml`'s own header
comment).

What *is* automated is noticing: the `publish-latest` job's "Verify the
image is anonymously pullable" step performs an anonymous pull probe on
every release and writes to the run's own step summary:

- If the probe succeeds, an "OK" line.
- If it fails, a warning block with the exact click-path:
  1. `https://github.com/HagaleTechnologies/manta/pkgs/container/manta`
  2. **Package settings** → **Danger Zone** → **Change visibility** →
     **Public**

This step never fails the job — the release itself is fine even when the
package is still private; the fix is the out-of-band human action above.
Do this once, the first time a real tag is published; subsequent releases
push to the same already-public package and the probe reports "OK".

## If `:latest` ends up wrong

`publish-latest` re-checks, immediately before writing `:latest`, whether
its own tag is still the newest published stable release (comparing
against every `vX.Y.Z` tag in the repo, numerically, ignoring
pre-releases) — this closes the race where two tags pushed close together
used to let the older build's `:latest` write win if it finished last
(MAN-65 finding 3). The remaining window is narrow (seconds: the check
happens right after the multi-arch image is already pushed, and the
`:latest` write is a manifest-only copy, not a rebuild) but not zero — two
tags pushed within that window could still resolve out of order.

If `:latest` is ever wrong (from that narrow window, or a manual mistake),
recovery is one command:

```sh
docker buildx imagetools create \
  -t ghcr.io/hagaletechnologies/manta:latest \
  ghcr.io/hagaletechnologies/manta:X.Y.Z   # the version that SHOULD be latest
```

Verify with:

```sh
docker buildx imagetools inspect ghcr.io/hagaletechnologies/manta:latest
docker buildx imagetools inspect ghcr.io/hagaletechnologies/manta:X.Y.Z
```

Both should report the same digest.

## What each artifact contains

- **Windows** (`manta-windows-x86_64.zip`): `manta.exe`, `README.md`, and
  the two license files. The binary links the MSVC C runtime **statically**
  (MAN-65 finding 2), so it needs no Visual C++ Redistributable installed —
  unpack and run.
- **macOS** (`manta-macos-{x86_64,arm64}.tar.gz`) and **Linux**
  (`manta-linux-{x86_64,arm64}.tar.gz`): the `manta` binary, `README.md`,
  and the two license files. **Linux binaries need `libasound2` installed**
  (`sudo apt install libasound2` on Debian/Ubuntu/Raspberry Pi OS, or the
  equivalent ALSA runtime package elsewhere) — audio input is an
  unconditional dependency even for file/KiwiSDR/HPSDR-only use, and
  without it the binary fails to start.
- **Docker image** (`ghcr.io/hagaletechnologies/manta`): multi-arch
  (`linux/amd64`, `linux/arm64`), tagged `:X.Y.Z` for every release and
  `:latest` for the newest stable release only.
