#!/usr/bin/env bash
# MAN-244: reject release commits outside the freshly fetched default branch.
# Bash 3.2 compatible. Called by validate-tag before any release build.
set -uo pipefail

die() { printf 'release ancestry: %s\n' "$*" >&2; exit 2; }

if [ "$#" -ne 2 ] || [ -z "${1-}" ] || [ -z "${2-}" ]; then
  die 'usage: check-release-ancestry.sh <event-commit-object-id> <default-branch>'
fi
event_commit="$1"
default_branch="$2"
git rev-parse --git-dir >/dev/null 2>&1 || die 'not a Git repository'
# Require an immutable full object ID, never a mutable ref or revision expression.
if [[ ! "$event_commit" =~ ^([0-9a-fA-F]{40}|[0-9a-fA-F]{64})$ ]]; then
  die 'event commit must be a full hexadecimal object ID'
fi
git check-ref-format "refs/heads/$default_branch" >/dev/null 2>&1 ||
  die "invalid default branch: $default_branch"
release_commit="$(git rev-parse --verify --end-of-options "${event_commit}^{commit}")" ||
  die "cannot resolve event object $event_commit to a commit"
shallow="$(git rev-parse --is-shallow-repository)" || die 'cannot inspect shallow history'
fetch_args=(--no-tags)
case "$shallow" in
  true) fetch_args+=(--unshallow) ;;
  false) ;;
  *) die "unexpected shallow-history state: $shallow" ;;
esac
# An old tracking ref must never turn a failed refresh into acceptance.
git fetch "${fetch_args[@]}" origin \
  "+refs/heads/${default_branch}:refs/remotes/origin/man244-default" ||
  die "cannot fetch default branch $default_branch from origin"
default_tip="$(git rev-parse --verify --end-of-options 'refs/remotes/origin/man244-default^{commit}')" ||
  die 'cannot resolve fetched default-branch tip'
git merge-base --is-ancestor "$release_commit" "$default_tip"
status=$?
case "$status" in
  0) printf 'OK: release commit %s is an ancestor of %s at %s\n' "$release_commit" "$default_branch" "$default_tip" ;;
  1) printf 'release ancestry: %s is not an ancestor of %s at %s\n' "$release_commit" "$default_branch" "$default_tip" >&2; exit 1 ;;
  *) die "Git ancestry check failed (exit $status); cannot prove release membership" ;;
esac
