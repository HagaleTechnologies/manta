#!/usr/bin/env bash
# MAN-36: assert this checkout is compiling with the EXACT Rust release
# rust-toolchain.toml pins, on whatever machine is running.
#
# Why a guard at all: rustup honours rust-toolchain.toml silently. A pin that
# stops being honoured (someone sets `channel = "stable"`, a CI step runs
# `cargo +stable`, a runner image ships a rustc outside rustup) degrades back to
# the floating behaviour MAN-36 removed, with no visible symptom until two
# builds disagree. This turns that into a failed required check.
#
# Deliberately needs NO network and NO jq -- unlike
# scripts/check-dependabot-cargo-unlock.sh, so it IS wired into required CI
# (.github/workflows/ci.yml, `test` job). Runs identically on ubuntu-latest and
# macos-latest: BRE sed and awk only, no `sort -V` (not portable to BSD sort).
#
# See docs/DECISIONS/2026-09-04-man36-exact-rust-toolchain-pin.md.
#
# Usage: scripts/check-rust-toolchain-pin.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

fail() {
  echo "ERROR: $*" >&2
  exit 1
}

PIN_FILE="rust-toolchain.toml"
[[ -f $PIN_FILE ]] ||
  fail "$PIN_FILE not found -- this repo pins an exact Rust release (MAN-36); without it every build floats."

# First `channel = "..."` line. TOML in this file is a fixed three-key literal
# written by us, so a full parser would be more machinery than the input needs.
pinned="$(sed -n 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$PIN_FILE" | head -n 1)"
[[ -n $pinned ]] || fail "$PIN_FILE declares no [toolchain] channel."

[[ $pinned =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] ||
  fail "$PIN_FILE pins channel=\"$pinned\", which is not an exact MAJOR.MINOR.PATCH release. A floating channel (stable/beta/nightly) reintroduces the drift MAN-36 removed."

msrv="$(sed -n 's/^[[:space:]]*rust-version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' Cargo.toml | head -n 1)"
[[ -n $msrv ]] || fail "Cargo.toml declares no [workspace.package] rust-version."

# `sort -V` is absent/divergent on BSD sort (macos-latest leg), so compare
# component-wise in awk instead.
awk -v a="$pinned" -v b="$msrv" '
  BEGIN {
    na = split(a, x, "."); nb = split(b, y, ".");
    n = (na > nb ? na : nb);
    for (i = 1; i <= n; i++) {
      xi = (i <= na ? x[i] + 0 : 0); yi = (i <= nb ? y[i] + 0 : 0);
      if (xi > yi) exit 0;
      if (xi < yi) exit 1;
    }
    exit 0;
  }' || fail "$PIN_FILE pins $pinned, below Cargo.toml's rust-version MSRV floor of $msrv."

# rustc, not cargo: cargo's own version numbering is a separate series (the
# release-channel manifest lists [pkg.cargo] version = "0.99.0" for Rust
# 1.98.1), so asserting on `cargo --version` would be asserting on a coincidence.
actual="$(rustc --version | awk '{print $2}')"
[[ $actual == "$pinned" ]] ||
  fail "active rustc is $actual but $PIN_FILE pins $pinned. If rustup is managing this checkout, run any cargo command from the repo root to let it install the pin; if rustc is not rustup-managed, install $pinned."

echo "OK: rustc $actual matches $PIN_FILE's pinned channel (MSRV floor $msrv)."
