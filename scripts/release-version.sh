#!/usr/bin/env bash
# Release tag -> Docker/OCI version tag, and release-recency questions.
#
# MAN-65 findings 1 and 3 (PR #78 review, chatgpt-codex-connector round).
# Lives in a script rather than inline `run:` blocks so it can be tested
# without pushing a tag -- see scripts/tests/release-version.test.sh, run
# by ci.yml's `test` job on every PR.
#
# bash 3.2 compatible (ci.yml's test matrix includes macos-latest).
#
# Usage:
#   release-version.sh validate <git-ref-name>          # -> version on stdout
#   release-version.sh assert-oci-tag <tag>
#   release-version.sh is-newest-stable <git-ref-name>   # -> true|false on stdout
set -euo pipefail

# SemVer 2.0.0, MINUS the build-metadata group -- deliberately not accepted:
# `+` is not in the OCI tag grammar, and manta publishes one version across
# all five targets from one tag, so a per-build suffix has no meaning here.
SEMVER_RE='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-((0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)(\.(0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*))*))?$'
# https://github.com/opencontainers/distribution-spec -- the grammar Buildx
# enforces and this pipeline previously violated.
OCI_TAG_RE='^[A-Za-z0-9_][A-Za-z0-9._-]{0,127}$'

die() { printf '%s\n' "$*" >&2; exit 2; }

cmd_assert_oci_tag() {
  local tag="${1-}"
  [[ $tag =~ $OCI_TAG_RE ]] || die \
    "'${tag}' is not a legal OCI/Docker tag ([A-Za-z0-9_][A-Za-z0-9._-]{0,127})"
}

cmd_validate() {
  local ref="${1-}"
  case "$ref" in
    v?*) ;;
    *) die "release tag must start with 'v' (got '${ref}')" ;;
  esac
  local version="${ref#v}"
  [[ $version =~ $SEMVER_RE ]] || die \
"'${ref}' is not an accepted release tag.

Accepted: v<major>.<minor>.<patch> with an optional pre-release, e.g.
  v1.2.3        v0.1.0        v1.2.3-rc.1
Rejected: SemVer BUILD METADATA (v1.2.3+linux) and anything else. '+' is
valid in a Git ref and in SemVer but NOT in Docker's tag grammar, so such a
tag would be rejected by Buildx mid-build and would silently take the
GitHub Release down with it (MAN-65 finding 1). Re-tag without the '+...'
suffix; see docs/RUNBOOKS/release.md."
  cmd_assert_oci_tag "$version"
  printf '%s\n' "$version"
}

# Numeric triple comparison. Only STABLE releases are compared: a
# pre-release is never :latest (README's install command must hand users a
# release, not a release candidate -- a defect the original finding did not
# name but which lived in the same three lines), so prerelease ordering
# rules are not needed here.
version_gt() {  # version_gt A B -> 0 if A > B
  local a="$1" b="$2" i
  local -a A B          # bash 3.2: indexed arrays are fine, associative are not
  IFS=. read -r A0 A1 A2 <<< "$a"
  IFS=. read -r B0 B1 B2 <<< "$b"
  A=("$A0" "$A1" "$A2")
  B=("$B0" "$B1" "$B2")
  for i in 0 1 2; do
    if [ "${A[$i]}" -gt "${B[$i]}" ]; then return 0; fi
    if [ "${A[$i]}" -lt "${B[$i]}" ]; then return 1; fi
  done
  return 1
}

cmd_is_newest_stable() {
  local version newest="" tag candidate triple rest tags
  version="$(cmd_validate "$1")" || exit 2
  case "$version" in
    *-*) printf 'false\n'
         printf 'pre-release %s is never published as :latest\n' "$version" >&2
         return 0 ;;
  esac
  # `git tag --list` over the tags fetched by the CALLER, immediately
  # before this call -- see the workflow step's own comment for why the
  # fetch must not happen at job start.
  #
  # Captured via command substitution, NOT `< <(git tag --list ...)`: a
  # process substitution's exit status is invisible to the loop and to
  # `set -e`, so a failing `git tag --list` used to look identical to "no
  # tag is newer" -- printing `false` and exiting 0. Command substitution
  # makes the failure observable and `die`-able below.
  tags="$(git tag --list 'v[0-9]*')" || die \
    "'git tag --list' failed -- cannot determine release recency for ${version}"
  while IFS= read -r tag; do
    [ -n "$tag" ] || continue
    candidate="${tag#v}"
    # Extract a leading X.Y.Z numeric triple regardless of what follows it
    # -- even a tag this pipeline would refuse to PUBLISH outright (e.g.
    # trailing build metadata: v1.2.4+meta) still names a version that
    # could become the real next release once re-tagged correctly, so it
    # must still be able to block an older tag from wrongly claiming to be
    # newest. A trailing '-...' (pre-release) is the one deliberate
    # exception: a pre-release is recognized and never blocks a stable
    # release, or tagging v-next-rc.1 early would freeze :latest until the
    # final release ships.
    if [[ $candidate =~ ^([0-9]+\.[0-9]+\.[0-9]+)(.*)$ ]]; then
      triple="${BASH_REMATCH[1]}"
      rest="${BASH_REMATCH[2]}"
    else
      continue
    fi
    case "$rest" in -*) continue ;; esac
    if [ -z "$newest" ] || version_gt "$triple" "$newest"; then
      newest="$triple"
    fi
  done <<< "$tags"
  # The caller's own tag was fetched and pushed before this ever runs, so
  # it must always appear in `$tags` itself -- an empty `$newest` here
  # means the fetch/listing silently saw nothing, not that "nothing is
  # newer". That is a hard failure, not a legitimate `false`: the old
  # behaviour printed `false` with a message naming `$version` as its own
  # blocker (`${newest:-$version}`), which is self-contradictory rather
  # than informative.
  [ -n "$newest" ] || die \
    "no stable release tags found while checking recency for ${version} -- expected at least ${version} itself to be visible after 'git fetch --tags'; refusing to silently decide"
  if [ "$newest" = "$version" ]; then
    printf 'true\n'
  else
    printf 'false\n'
    printf 'a release with version prefix %s exists or is newer; leaving :latest alone\n' "$newest" >&2
  fi
}

main() {
  local sub="${1-}"
  [ $# -ge 1 ] && shift
  case "$sub" in
    validate) cmd_validate "${1-}" ;;
    assert-oci-tag) cmd_assert_oci_tag "${1-}" ;;
    is-newest-stable) cmd_is_newest_stable "${1-}" ;;
    *) die "usage: $0 {validate|assert-oci-tag|is-newest-stable} <arg>" ;;
  esac
}

main "$@"
