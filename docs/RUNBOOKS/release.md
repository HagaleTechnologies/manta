# Cutting a manta release

MAN-84 / decision D7 (`docs/DECISIONS/2026-09-06-broad-review-decisions.md`):
tag now, labeled **pre-stability alpha, expect breakage** — manta has not
cleared its own M2/M3 acceptance gates yet, and the tag says so rather than
implying otherwise.

The tag push is a deliberate human act, not something CI can do for you —
`.github/workflows/release-publish.yml` only reacts to a tag that already
exists on `origin`; it never creates one. Whoever runs this needs push access
to `HagaleTechnologies/manta` and a working `gh` CLI authenticated against it.

## Before you push the tag

1. `origin/main` is clean and CI is green on the commit you're about to tag:

   ```sh
   git fetch origin && git switch main && git reset --hard origin/main
   git status --porcelain          # must be empty
   ```

2. The tag version and the workspace version must agree —
   `release-publish.yml`'s `verify-version` job checks this in CI too, but
   check it yourself first so a mismatch doesn't cost you a full matrix run:

   ```sh
   sed -n '/^\[workspace.package\]/,/^\[/p' Cargo.toml | grep '^version ='
   # version = "0.1.0"  ->  the tag must be v0.1.0, not anything else
   ```

3. No release has been cut yet — confirm both the local and remote tag
   namespace are actually empty before assuming this is a first release:

   ```sh
   git tag -l 'v*'
   git ls-remote --tags origin 'v*'
   ```

## Push the tag

Annotated, not lightweight — the message becomes part of the tag's own
record, independent of whatever the GitHub Release body ends up saying:

```sh
git tag -a v0.1.0 -m "manta v0.1.0 — pre-stability alpha, expect breakage"
git push origin v0.1.0
```

This single push triggers **two** workflows on the same ref: `release.yml`
(build-and-validate, no write permissions) and `release-publish.yml` (the
publish path — GHCR image + GitHub Release). That's by design, not a
duplicate run to cancel; see `release.yml`'s header comment.

## Watch the run

```sh
gh run list --workflow release-publish.yml --limit 1
gh run watch "$(gh run list --workflow release-publish.yml --limit 1 --json databaseId --jq '.[0].databaseId')"
```

Rough expected durations, so a long run doesn't look stuck when it isn't:

- `verify-version` — well under a minute; it only reads `Cargo.toml`.
- `build` — 10–25 minutes. The two `cross`-based Linux legs are the long
  poles, since each installs `cross` itself before building.
- `docker-publish` — 10–20 minutes; the multi-arch (`linux/amd64` +
  `linux/arm64`) build runs the arm64 leg under QEMU emulation, not native
  hardware.
- `release` — a couple of minutes once its dependencies finish; it only
  downloads artifacts and calls `action-gh-release`.

## Verify the release is real

Don't trust the green checkmarks alone — check the artifact an operator
would actually get, from a directory that is **not** a checkout of this
repo (that's the whole point of the ticket this runbook exists for):

```sh
gh release view v0.1.0 --json assets --jq '.assets[].name'
# expect exactly five: manta-macos-x86_64.tar.gz, manta-macos-arm64.tar.gz,
# manta-linux-x86_64.tar.gz, manta-linux-arm64.tar.gz,
# manta-windows-x86_64.zip

gh release view v0.1.0 --json body --jq '.body' | head -5
# expect the pre-stability-alpha banner ABOVE the auto-generated changelog

mkdir -p /tmp/manta-release-check && cd /tmp/manta-release-check
gh release download v0.1.0 --pattern 'manta-linux-x86_64.tar.gz'
tar xzf manta-linux-x86_64.tar.gz && cd manta-linux-x86_64
./manta --version                            # expect: manta 0.1.0
./manta gen v1 --out ./v1 && ./manta decode ./v1/v1.wav   # expect: spots: 1
```

If the banner is missing from the release body, or the auto-generated
changelog replaced it instead of following it, GitHub's documented
prepend-on-generate behavior didn't happen as expected: edit the published
notes by hand to unblock the release, then fix `release-publish.yml` as a
follow-up (either move the banner into the tag annotation and drop
`generate_release_notes`, or compose the body directly instead of relying
on prepend order).

At least one non-Linux archive should be spot-checked by someone who
actually has that platform — the macOS, Windows, and `aarch64` Linux legs
only ever get built in CI; nobody has run the binary they produce outside
of it yet.

Finally, confirm the README's release badge now resolves to `v0.1.0`
instead of "no releases found" (`README.md:17`).

## After

- **GHCR package visibility** — a brand-new GHCR package is private by
  default. Flip it to public in the repo's package settings (GitHub UI:
  Packages → manta → Package settings → Danger Zone → Change visibility).
  **This is explicitly not a release blocker** (MAN-65 finding 4 / MAN-66) —
  if you don't get to it, or it's still private, close MAN-84 anyway and
  leave MAN-66 open to track it.
- If `docker-publish` failed but `release` still published (the behavior
  Phase 2 of MAN-84 deliberately enables — the GitHub Release no longer
  waits on GHCR succeeding), re-run just that job rather than re-tagging:

  ```sh
  gh run rerun <run-id> --job docker-publish
  ```

  Do **not** delete and re-push the tag for a GHCR-only failure — the
  GitHub Release binaries are the deliverable this ticket cares about, and
  they're already published.

## If the release is wrong

```sh
gh release delete v0.1.0 --yes
git push origin :refs/tags/v0.1.0
git tag -d v0.1.0
```

None of those three commands remove the Docker image tag already pushed to
GHCR — if `docker-publish` succeeded before you noticed the problem, the
image stays in the registry under that tag until someone deletes it
separately from the package settings UI.

## Related

- `docs/DECISIONS/2026-09-06-broad-review-decisions.md` §D6 (Pi4 CPU-budget
  gate paused, does not block this), §D7 (tag now, pre-stability alpha
  wording — the decision this whole runbook exists to execute).
- `.github/workflows/release.yml`, `.github/workflows/release-publish.yml` —
  the two-file pipeline this runbook drives.
- MAN-65's release-pipeline hardening plan extends this file rather than
  creating a second release runbook — see that ticket for the tag/OCI
  validation and `:latest`-race work not covered here.
