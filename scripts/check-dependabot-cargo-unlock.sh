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
# Requires network access to the crates.io index and `jq`. Not wired into required CI on purpose.
#
# Usage: scripts/check-dependabot-cargo-unlock.sh [crate ...]
set -euo pipefail

command -v jq >/dev/null 2>&1 || {
  echo "ERROR: jq is required (install it before running this guard)." >&2
  exit 2
}

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# Version of the direct workspace dependency PKG resolved at DIR. Cargo.lock can
# contain several versions of the same package, so reading the first matching
# lockfile entry would make `cargo update -p PKG` ambiguous and could false-green.
#
# --all-features is required: `cargo metadata` prunes disabled optional
# dependencies from `.resolve.nodes`, so a default sweep would resolve nothing
# for feature-gated workspace deps (e.g. `soapysdr` behind `soapy`) and report
# them as errors.
#
# The node dep's `.name` is the *alias* Cargo exposes to the compiler — the
# normalized library name, with hyphens folded to underscores (`num-complex` is
# published under `.name == "num_complex"`). Matching on it misses every
# hyphenated crate, so resolve `.pkg` through `.packages[]` and compare the real
# package name. That lookup also yields the authoritative `.version`, which
# avoids parsing it back out of the package id.
direct_version() {
  local dir="$1" pkg="$2"
  (cd "$dir" && cargo metadata --format-version 1 --locked --all-features) | jq -r --arg pkg "$pkg" '
    .workspace_members as $members
    | (reduce .packages[] as $p ({}; .[$p.id] = {name: $p.name, version: $p.version})) as $byid
    | [.resolve.nodes[]
       | select(.id as $id | $members | index($id))
       | .deps[]
       | select($byid[.pkg].name == $pkg)
       | $byid[.pkg].version]
    | unique[]
  '
}

# Copy the worktree (tracked files + Cargo.lock) into a scratch dir so we never
# mutate the real lockfile.
scratch_copy() {
  local dest="$1"
  mkdir -p "$dest"
  git ls-files -z | xargs -0 -I{} cp --parents {} "$dest" 2>/dev/null \
    || git ls-files -z | tar --null -T - -cf - | (cd "$dest" && tar -xf -)
}

crates=("$@")
if [[ ${#crates[@]} -eq 0 ]]; then
  # Every [workspace.dependencies] entry that is not a path/git dep.
  while IFS= read -r crate; do
    crates+=("$crate")
  done < <(awk '/^\[workspace\.dependencies\]/ {inblk=1; next}
                /^\[/ {inblk=0}
                inblk && /^[a-zA-Z0-9_-]+ *=/ && !/path *=/ && !/git *=/ { print $1 }' Cargo.toml)
fi

echo "Checking: ${crates[*]}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
scratch_copy "$tmp/single"
cp -R "$tmp/single" "$tmp/full"

# Full unlock: everything moves to the latest semver-compatible version.
(cd "$tmp/full" && cargo update --quiet)

failed=0
for pkg in "${crates[@]}"; do
  if ! current="$(direct_version "$tmp/single" "$pkg")"; then
    echo "ERROR ${pkg}: could not resolve the current direct workspace version"
    failed=1
    continue
  fi
  if ! full="$(direct_version "$tmp/full" "$pkg")"; then
    echo "ERROR ${pkg}: could not resolve the full-unlock direct workspace version"
    failed=1
    continue
  fi
  if [[ -z "$current" || "$current" == *$'\n'* ]]; then
    echo "ERROR ${pkg}: expected one directly resolved workspace version, found ${current:-<none>}"
    failed=1
    continue
  fi
  if [[ -z "$full" || "$full" == *$'\n'* ]]; then
    echo "ERROR ${pkg}: expected one full-unlock direct workspace version, found ${full:-<none>}"
    failed=1
    continue
  fi

  rm -rf "$tmp/one"; cp -R "$tmp/single" "$tmp/one"
  if ! (cd "$tmp/one" && cargo update --quiet -p "${pkg}:${current}" >/dev/null 2>&1); then
    echo "ERROR ${pkg}: cargo update -p ${pkg}:${current} failed"
    failed=1
    continue
  fi
  if ! single="$(direct_version "$tmp/one" "$pkg")"; then
    echo "ERROR ${pkg}: could not resolve the single-package-update workspace version"
    failed=1
    continue
  fi
  if [[ -z "$single" || "$single" == *$'\n'* ]]; then
    echo "ERROR ${pkg}: expected one single-package-update workspace version, found ${single:-<none>}"
    failed=1
    continue
  fi

  if [[ "$single" != "$full" ]]; then
    echo "FAIL ${pkg}: single-package unlock reaches ${single:-<none>}, full unlock reaches ${full}"
    echo "     Dependabot will raise \"Failed to update ${pkg}!\" for this crate."
    # Qualify with the resolved version: an unqualified spec is ambiguous when
    # the lockfile carries several versions of the same crate (rand_core is
    # present at 0.6.4, 0.9.5 and 0.10.1 here), and Cargo rejects it outright.
    echo "     Remedy: cargo update -p ${pkg}:${current} --precise ${full}"
    failed=1
  fi
done

if [[ $failed -ne 0 ]]; then
  echo
  echo "One or more dependencies are unreachable via Dependabot's single-package unlock."
  exit 1
fi
echo "OK: every checked dependency is reachable via a single-package unlock."
