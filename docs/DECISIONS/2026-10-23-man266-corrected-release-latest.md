# MAN-266: a corrected release tag takes `:latest` beside its rejected leading-zero twin

Date: 2026-10-23. Ticket: MAN-266, deferred from PR #107's Codex review
(thread `PRRT_kwDOTQvU8M6qMMjj`) under the
[PR review convergence policy](2026-08-07-pr-review-convergence-policy.md).
This corrects the equality step of MAN-65's recency check
([MAN-65 decision](2026-09-05-man65-release-pipeline-hardening.md),
Finding 3); the rest of that record stands.

## Problem

`scripts/release-version.sh is-newest-stable` scans every `v[0-9]*` tag,
takes each tag's leading `X.Y.Z` with a regex that allows leading zeros,
and keeps the largest by `version_gt`, which strips leading zeros. It kept
the raw text of the first tag holding the maximum, then compared that text
to the requested version with string `=`.

`validate` rejects `v02.0.0`, but the tag can still exist after a mistaken
push. `git tag --list` sorts byte-wise, so a leading-zero twin always lists
before its corrected tag, in any component. The two tie under
`version_gt`, so the scan kept `"02.0.0"` and `"02.0.0" = "2.0.0"` failed.
With `v02.0.0` and `v2.0.0` both tagged and nothing higher, the check
printed `false` for `v2.0.0` every time, not just when a race hit.

Two steps in `.github/workflows/release-publish.yml` call this check, so
both declined:

- `publish-latest`'s "Is this still the newest stable release?" step, which
  gates the manifest copy to `:latest`;
- `release`'s "May this Release become Latest?" step, which feeds
  `make_latest`, so `releases/latest` stayed on the older release too.

The existing leading-zero test (`v1.02.2` beside `v1.2.3`) used unequal
versions, where the bigger canonical tag replaces the twin, so it passed.

## Decision

The final test is numeric equality through the same comparator the scan
uses: neither `version_gt "$newest" "$version"` nor
`version_gt "$version" "$newest"`. That reuses the zero stripping and the
arbitrary-length comparison PR #107 added for components past Bash's
64-bit integers. Testing both directions keeps a `false` when the requested
version is above everything the scan saw. Storing a normalized triple would
also work, but it repeats `version_gt`'s normalization in a second place.

Unchanged:

- `validate` still rejects leading zeros, so `v02.0.0` is never published.
- A malformed tag still counts in the scan. A twin equal to the requested
  version no longer blocks it, but a malformed tag that is numerically
  higher (`v03.0.0`, `v03.0.0+meta`) still declines `:latest` for
  `v2.0.0`, per MAN-65's conservative treatment of tags that may be
  re-pushed correctly.
- Pre-releases still never take `:latest` and never block a stable
  release. An empty or failed tag listing still exits 2.
- The workflows are unchanged. A `true` reaches both consumers as before.
  The runbook's recovery steps for a wrong `:latest` stay as they are, and
  operators have no new step.

`scripts/tests/release-version.test.sh` pins `reject v02.0.0`, the
ticket's `v02.0.0`/`v2.0.0` pair, minor and patch twins, an oversized
twin, an all-zero twin, a higher malformed tag with and without `+meta`,
and the requested-version-above-the-scan case.

## What this does not fix

- The window between the recency check and the registry write. Two tags
  pushed within it can still resolve out of order, the residual race
  MAN-65 accepted.
- Recency is still read from tags, not from what GHCR published (MAN-65's
  "second deliberate asymmetry").
