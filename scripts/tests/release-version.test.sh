#!/usr/bin/env bash
# MAN-65 findings 1 and 3 (PR #78, chatgpt-codex-connector round): the release
# workflow copied a Git tag's suffix straight into an OCI tag, so a tag that
# is perfectly valid as a Git ref and as SemVer -- v1.2.3+linux -- produced
# ghcr.io/...:1.2.3+linux, which Buildx rejects outright, taking the
# dependent GitHub Release down with it. Separately, every tag push got its
# own concurrency group yet all of them wrote the SAME :latest tag, so two
# tags pushed close together could overlap and the OLDER build, finishing
# last, would leave :latest pointing at the older image. These cases pin the
# grammar and the recency logic the pipeline now enforces.
#
# bash 3.2 compatible on purpose: this runs in ci.yml's `test` job, whose
# matrix includes macos-latest, where /bin/bash is 3.2 (no mapfile, no
# associative arrays, no ${x,,}).
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SCRIPT="$REPO_ROOT/scripts/release-version.sh"

failures=0

fail() {
  printf 'FAIL: %s\n' "$1" >&2
  failures=$((failures + 1))
}

accept() {  # accept <tag> <expected-version>
  local got
  if ! got="$("$SCRIPT" validate "$1" 2>/dev/null)"; then
    fail "$1: expected accept, got a rejection"
  elif [ "$got" != "$2" ]; then
    fail "$1: expected version '$2', got '$got'"
  fi
}

reject() {  # reject <tag>
  local out
  out="$("$SCRIPT" validate "$1" 2>&1 1>/dev/null)"
  rc=$?
  if [ "$rc" -eq 0 ]; then
    fail "$1: expected rejection, got accept"
  elif [ -z "$out" ]; then
    fail "$1: rejected with no stderr message"
  fi
}

accept_oci() {  # accept_oci <tag>
  if ! "$SCRIPT" assert-oci-tag "$1" >/dev/null 2>&1; then
    fail "assert-oci-tag $1: expected accept, got a rejection"
  fi
}

reject_oci() {  # reject_oci <tag>
  if "$SCRIPT" assert-oci-tag "$1" >/dev/null 2>&1; then
    fail "assert-oci-tag $1: expected rejection, got accept"
  fi
}

echo "== validate: accept =="
accept v1.2.3         1.2.3
accept v0.1.0         0.1.0
accept v10.20.30      10.20.30
accept v1.2.3-rc.1    1.2.3-rc.1
accept v1.2.3-alpha-1 1.2.3-alpha-1

echo "== validate: reject =="
reject v1.2.3+linux           # the finding's own example
reject v1.2.3-rc.1+build.7    # prerelease AND build metadata
reject v1.2.3+                # trailing separator
reject v1.2                   # not a full triple
reject v1.2.3.4
reject v01.2.3                # leading zero -- not SemVer
reject v1.2.3-                # empty prerelease
reject vfoo
reject 1.2.3                  # no leading v
reject ""
reject "v1.2.3 "               # trailing whitespace must not be tolerated

echo "== assert-oci-tag =="
# The SemVer check is the POLICY; this is the GUARANTEE. It also covers the
# workflow_dispatch path's `dispatch-<run_id>` tag, which never goes through
# `validate` at all.
accept_oci dispatch-1234567890
accept_oci 1.2.3-rc.1
reject_oci "1.2.3+linux"
reject_oci "-1.2.3"                          # OCI tags may not start with '-'
reject_oci ".1.2.3"
reject_oci "$(printf 'a%.0s' $(seq 1 129))"  # 129 chars: over the 128 limit

echo "== is-newest-stable =="
# MAN-65 finding 3: every tag push got its own concurrency group (the group
# included github.ref) yet every one of them wrote the SAME :latest tag, so
# two tags pushed close together could overlap and the OLDER build, finishing
# last, would leave :latest pointing at the older image -- README's install
# command then hands users a stale release.
#
# Each case builds a throwaway git repo, tags it, and asks the question the
# publish-latest job asks.
newest_case() {  # newest_case <expected:true|false> <ref-under-test> <tag>...
  local expected="$1" ref="$2"
  shift 2
  local repo got
  repo="$(mktemp -d)"
  (
    cd "$repo" || exit 2
    git init -q .
    git -c user.email=t@example.invalid -c user.name=t commit -q --allow-empty -m x
    for t in "$@"; do
      git tag "$t"
    done
  ) >/dev/null
  got="$(
    cd "$repo" || exit 2
    "$SCRIPT" is-newest-stable "$ref" 2>/dev/null
  )"
  rm -rf "$repo"
  if [ "$got" != "$expected" ]; then
    fail "is-newest-stable $ref (tags: $*): expected '$expected', got '$got'"
  fi
}

newest_case true  v1.2.3 v1.2.3
newest_case true  v1.2.4 v1.2.3 v1.2.4          # the newer of two
newest_case false v1.2.3 v1.2.3 v1.2.4          # THE FINDING'S RACE: the
                                                 # older tag's build declines
newest_case true  v1.10.0 v1.9.0 v1.10.0        # numeric, not lexicographic
newest_case true  v2.0.0  v1.99.99 v2.0.0
newest_case false v1.3.0-rc.1 v1.2.0 v1.3.0-rc.1   # a PRE-RELEASE never
                                                    # becomes :latest, even
                                                    # when it is the newest tag
newest_case true  v1.2.3 v1.2.3 v1.3.0-rc.1     # ...and a pre-release does
                                                 # not stop a stable release
newest_case true  v1.2.3 v1.2.3 not-a-release   # junk tags are ignored
newest_case false v1.2.3 v1.2.3 v1.2.4+meta     # a tag this pipeline would
                                                 # refuse to publish must not
                                                 # be treated as a release

if [ "$failures" -ne 0 ]; then
  echo "$failures failure(s)" >&2
  exit 1
fi
echo "OK: all release-version.sh cases passed."
