#!/usr/bin/env bash
# Detect dependencies that Dependabot's cargo updater cannot move.
#
# Dependabot runs `cargo update -p <name>:<locked-version>` and then asserts the new
# version landed in Cargo.lock (dependabot-core
# cargo/lib/dependabot/cargo/file_updater/lockfile_updater.rb, validate_dependency_update).
# When a crate's release pins a sibling with `=`, that single-package unlock scope cannot
# move the sibling, cargo declines silently, and Dependabot raises "Failed to update <name>!"
# -> reported as `unknown_error` / null. See docs/DECISIONS/2026-08-05-dependabot-cargo-
# single-package-unlock.md and thoughts/shared/research/2026-08-05-ski-1.md.
#
# Requires network access to the crates.io index. Not wired into required CI on purpose.
#
# Usage: scripts/check-dependabot-cargo-unlock.sh [crate ...]
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# Version of PKG in the Cargo.lock at DIR.
locked_version() {
  local dir="$1" pkg="$2"
  awk -v pkg="$pkg" '
    /^\[\[package\]\]/ { name=""; next }
    /^name = / { gsub(/[",]/, "", $3); name=$3; next }
    /^version = / { if (name == pkg) { gsub(/[",]/, "", $3); print $3; exit } }
  ' "$dir/Cargo.lock"
}

# Copy the worktree (tracked files + Cargo.lock) into a scratch dir so we never
# mutate the real lockfile.
scratch_copy() {
  local dest="$1"
  mkdir -p "$dest"
  git ls-files -z | xargs -0 -I{} cp --parents {} "$dest" 2>/dev/null \
    || tar -cf - $(git ls-files) | (cd "$dest" && tar -xf -)
}

crates=("$@")
if [[ ${#crates[@]} -eq 0 ]]; then
  # Every [workspace.dependencies] entry that is not a path/git dep.
  mapfile -t crates < <(
    awk '/^\[workspace\.dependencies\]/ {inblk=1; next}
         /^\[/ {inblk=0}
         inblk && /^[a-zA-Z0-9_-]+ *=/ && !/path *=/ && !/git *=/ { print $1 }' Cargo.toml
  )
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
scratch_copy "$tmp/single"
cp -R "$tmp/single" "$tmp/full"

# Full unlock: everything moves to the latest semver-compatible version.
(cd "$tmp/full" && cargo update --quiet)

failed=0
for pkg in "${crates[@]}"; do
  full="$(locked_version "$tmp/full" "$pkg" || true)"
  [[ -z "$full" ]] && continue

  rm -rf "$tmp/one"; cp -R "$tmp/single" "$tmp/one"
  (cd "$tmp/one" && cargo update --quiet -p "$pkg" >/dev/null 2>&1 || true)
  single="$(locked_version "$tmp/one" "$pkg" || true)"

  if [[ "$single" != "$full" ]]; then
    echo "FAIL ${pkg}: single-package unlock reaches ${single:-<none>}, full unlock reaches ${full}"
    echo "     Dependabot will raise \"Failed to update ${pkg}!\" for this crate."
    echo "     Remedy: cargo update -p ${pkg} --precise ${full}"
    failed=1
  fi
done

if [[ $failed -ne 0 ]]; then
  echo
  echo "One or more dependencies are unreachable via Dependabot's single-package unlock."
  exit 1
fi
echo "OK: every checked dependency is reachable via a single-package unlock."
