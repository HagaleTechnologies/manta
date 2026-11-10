#!/usr/bin/env bash
# MAN-522: run the pre-push gate CLAUDE.md lists, with one command, in the order
# .github/workflows/ci-full.yml's ubuntu-support job runs it, and stop at the
# first failure with the failing command named.
#
#   bash scripts/pre-push.sh          the full gate (Linux and macOS)
#   bash scripts/pre-push.sh --fast   fmt, then clippy on the crates this branch changed
#   bash scripts/pre-push.sh --list   print the full gate's commands for this host; run nothing
#
# Order: the support job's steps, then `cargo test --workspace --locked` last. CI
# runs the workspace tests in its separate *-shards jobs and the doctests as
# `cargo test --workspace --doc --locked`; one local `cargo test --workspace`
# covers both, and running it last puts the slowest step after the quick ones.
# On macOS the three suites CI runs only on Linux (ci_gates, decisions_index,
# verify_dependabot_diff_safe) are skipped. scripts/tests/test_ci_gates.py
# (PrePushScriptTests) fails when this list and ci-full.yml or CLAUDE.md drift.
#
# Bash 3.2 (macOS /bin/bash): no mapfile, no associative arrays, no ${x,,},
# no "${arr[@]}" on an array that can be empty under set -u.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

usage() {
  echo "Usage: bash scripts/pre-push.sh [--fast | --list | --help]"
}

mode=full
if [ $# -gt 1 ]; then
  usage >&2
  exit 2
fi
case "${1:-}" in
  "") ;;
  --fast) mode=fast ;;
  --list) mode=list ;;
  -h | --help)
    usage
    exit 0
    ;;
  *)
    echo "pre-push: unknown argument: $1" >&2
    usage >&2
    exit 2
    ;;
esac

os="$(uname -s)"
steps=()
step() { steps[${#steps[@]}]="$1"; }
suite() { step "python3 -m unittest scripts.tests.test_$1 -v"; }
linux_suite() { if [ "$os" = Linux ]; then suite "$1"; fi; }

if [ "$mode" = fast ]; then
  step "cargo fmt --all --check"
  pkgs=""
  whole=no
  if base="$(git merge-base HEAD origin/main 2>/dev/null)"; then
    changed="$(git diff --name-only "$base"; git ls-files --others --exclude-standard)"
    while IFS= read -r path; do
      case "$path" in
        Cargo.toml | Cargo.lock) whole=yes ;;
        crates/*/*)
          crate="${path#crates/}"
          crate="${crate%%/*}"
          case " $pkgs " in *" -p $crate "*) ;; *) pkgs="$pkgs -p $crate" ;; esac
          ;;
      esac
    done <<<"$changed"
  else
    echo "pre-push: no origin/main to compare with; clippy runs on the whole workspace" >&2
    whole=yes
  fi
  if [ "$whole" = yes ] || [ -z "$pkgs" ]; then
    step "cargo clippy --workspace --all-targets --locked -- -D warnings"
  else
    step "cargo clippy$pkgs --all-targets --locked -- -D warnings"
  fi
else
  step "scripts/check-rust-toolchain-pin.sh"
  step "cargo fmt --all --check"
  step "cargo clippy --workspace --all-targets --locked -- -D warnings"
  step 'RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace --locked'
  step "cargo clippy -p manta-engine -p manta-cli --all-targets --features manta-cli/ftx --locked -- -D warnings"
  step "cargo test -p manta-engine --features ftx --locked --lib ftx"
  step "cargo test -p manta-engine --features ftx --locked --test ftx_band_stream"
  step "cargo test -p manta-cli --features ftx --locked --test ftx_acceptance"
  step "bash scripts/tests/release-version.test.sh"
  step "bash scripts/tests/release-ancestry.test.sh"
  suite ci_trust_boundary
  suite release_checksums
  suite soapy_release
  suite packaging
  suite package_release
  suite score_against_rbn_replay
  suite corpus
  suite score_transcript
  suite engine_overlap
  linux_suite ci_gates
  linux_suite decisions_index
  suite field_node
  linux_suite verify_dependabot_diff_safe
  suite ci_shards
  step "cargo test --workspace --locked"
fi

total=${#steps[@]}
i=0
if [ "$mode" = list ]; then
  while [ "$i" -lt "$total" ]; do
    printf '%s\n' "${steps[$i]}"
    i=$((i + 1))
  done
  exit 0
fi

while [ "$i" -lt "$total" ]; do
  cmd="${steps[$i]}"
  i=$((i + 1))
  printf '\n==> [%d/%d] %s\n' "$i" "$total" "$cmd"
  if eval "$cmd"; then
    :
  else
    rc=$?
    printf '\npre-push: FAILED (exit %d) at step %d/%d: %s\n' "$rc" "$i" "$total" "$cmd" >&2
    exit "$rc"
  fi
done
printf '\npre-push: all %d %s-mode checks passed\n' "$total" "$mode"
