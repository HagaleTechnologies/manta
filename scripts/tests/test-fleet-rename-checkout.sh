#!/usr/bin/env bash
# Fixture-driven tests for scripts/fleet-rename-checkout.sh.
# bash + git only, no CI wiring — see docs/DECISIONS/2026-09-04-man27-fleet-checkout-rename.md.
#
# shellcheck disable=SC2015
# `A && B || C` is used throughout as ok/bad reporting, never as if/then/else
# control flow: ok()/bad()/skip() are printf wrappers that always return 0.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SUT="$REPO_ROOT/scripts/fleet-rename-checkout.sh"
PASS=0; FAIL=0; SKIP=0

ok()   { PASS=$((PASS+1)); printf 'ok   %s\n' "$1"; }
bad()  { FAIL=$((FAIL+1)); printf 'FAIL %s\n     %s\n' "$1" "${2:-}"; }
skip() { SKIP=$((SKIP+1)); printf 'SKIP %s\n' "$1"; }
check(){ [[ "$2" == "$3" ]] && ok "$1" || bad "$1" "expected [$3], got [$2]"; }

# A fixture org dir: <root>/org/<name> clone, optional <name>-worktrees siblings.
mkfixture() { # $1=root $2=clone-name; echoes the clone path
  local root="$1" name="$2"
  mkdir -p "$root/org"
  git -c init.defaultBranch=main init -q "$root/org/$name"
  git -C "$root/org/$name" config user.email t@example.invalid
  git -C "$root/org/$name" config user.name  test
  : > "$root/org/$name/README"
  git -C "$root/org/$name" add README
  git -C "$root/org/$name" -c commit.gpgsign=false commit -qm init
  git -C "$root/org/$name" remote add origin \
    "https://github.com/HagaleTechnologies/$name.git"
  printf '%s\n' "$root/org/$name"
}
addwt() { git -C "$1" worktree add -q -b "$3" "$2"; }   # $1=clone $2=path $3=branch
# Explicit template: bare `mktemp -d` is a GNU-coreutils-only extension and
# exits 1 with a usage error on BSD/macOS mktemp, which has no default
# template (CR-1) — this harness must itself run on a macOS fleet host.
#
# F5: canonicalize through `pwd -P` immediately, matching the SUT's own
# --org-dir canonicalization (fleet-rename-checkout.sh normalizes via
# `cd "$ORG_DIR" && pwd -P`). Without this, a symlinked $TMPDIR (macOS
# /var -> /private/var) makes fixture paths and the SUT's derived paths
# diverge, silently disabling assertions built from the raw path.
ALL_ROOTS=()
newroot() {
  local d
  d="$(mktemp -d "${TMPDIR:-/tmp}/fleet-rename-test.XXXXXX")"
  d="$(cd "$d" && pwd -P)"
  ALL_ROOTS+=("$d")
  printf '%s\n' "$d"
}
trap 'rm -rf "${ALL_ROOTS[@]}"' EXIT

# V-5: hermetic default $HOME. Every SUT invocation below that does not
# explicitly override HOME (T23 and T33 do, specifically to exercise the
# default-scan-root behavior) must not resolve the SUT's default scan roots
# ($HOME/code-repos/github, $HOME/bin, $HOME/.local/bin) against the real
# invoking user's home. On a fleet host that tree literally contains this
# checkout (and this very test file) under the legacy name, which the
# fixture-relative exclusions elsewhere in this harness know nothing about —
# so without this, the harness only passes in an environment shaped nothing
# like the fleet hosts it exists to validate. Point HOME at an empty,
# throwaway directory with none of those subdirectories so the harness
# behaves identically on a laptop, CI runner, or a real fleet host.
TESTHOME="$(mktemp -d "${TMPDIR:-/tmp}/fleet-rename-testhome.XXXXXX")"
ALL_ROOTS+=("$TESTHOME")
export HOME="$TESTHOME"

# --- T1: old layout present, new absent -> exit 1, reports MIGRATION NEEDED ---
R=$(newroot); C=$(mkfixture "$R" skimmer)
mkdir -p "$R/org/skimmer-worktrees"; addwt "$C" "$R/org/skimmer-worktrees/w1" w1
out=$("$SUT" --check --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T1 exit"          "$rc" "1"
grep -q "MIGRATION NEEDED" <<<"$out" && ok "T1 verdict" || bad "T1 verdict" "$out"
grep -q "skimmer-worktrees/w1" <<<"$out" && ok "T1 lists worktree" || bad "T1 lists worktree" "$out"
[[ -d "$R/org/skimmer" ]] && ok "T1 --check mutated nothing" || bad "T1 --check mutated nothing"

# --- T2: already migrated and healthy -> exit 0, all PASS ---
R=$(newroot); C=$(mkfixture "$R" manta)
mkdir -p "$R/org/manta-worktrees"; addwt "$C" "$R/org/manta-worktrees/w1" w1
git -C "$C" remote set-url origin https://github.com/HagaleTechnologies/manta.git
out=$("$SUT" --check --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T2 exit" "$rc" "0"
grep -q "ALREADY MIGRATED" <<<"$out" && ok "T2 verdict" || bad "T2 verdict" "$out"

# --- T3: no checkout at all on this host -> exit 0, NOT PRESENT (not a failure) ---
R=$(newroot); mkdir -p "$R/org"
out=$("$SUT" --check --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T3 exit" "$rc" "0"
grep -q "NOT PRESENT" <<<"$out" && ok "T3 verdict" || bad "T3 verdict" "$out"

# --- T4: BOTH skimmer and manta present -> exit 2, refuses (ambiguous) ---
R=$(newroot); mkfixture "$R" skimmer >/dev/null; mkfixture "$R" manta >/dev/null
out=$("$SUT" --check --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T4 exit" "$rc" "2"
grep -qi "both .* present" <<<"$out" && ok "T4 refuses" || bad "T4 refuses" "$out"

# --- T5: health predicate catches the invisible main-only-move breakage (KD 2) ---
R=$(newroot); C=$(mkfixture "$R" skimmer); mkdir -p "$R/wt"
addwt "$C" "$R/wt/MAN-9" MAN-9
mv "$R/org/skimmer" "$R/org/manta"          # main moved, worktree did not
out=$("$SUT" --check --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T5 exit" "$rc" "1"
grep -q "unhealthy worktree" <<<"$out" && ok "T5 detects invisible breakage" \
  || bad "T5 detects invisible breakage" "$out"
grep -q "prunable" <<<"$out" && bad "T5 must not rely on prunable" "$out" \
  || ok "T5 not relying on prunable"

# --- T6: pre-existing prunable entry is informational, not a failure (KD 4) ---
R=$(newroot); C=$(mkfixture "$R" manta)
git -C "$C" remote set-url origin https://github.com/HagaleTechnologies/manta.git
mkdir -p "$R/wt"; addwt "$C" "$R/wt/dead" dead; rm -rf "$R/wt/dead"
out=$("$SUT" --check --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T6 exit" "$rc" "0"
grep -q "pre-existing prunable" <<<"$out" && ok "T6 informational" || bad "T6 informational" "$out"

# --- T7: wrong origin on an otherwise-migrated clone -> exit 1 ---
R=$(newroot); C=$(mkfixture "$R" manta)
git -C "$C" remote set-url origin https://github.com/HagaleTechnologies/skimmer.git
out=$("$SUT" --check --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T7 exit" "$rc" "1"
grep -q "origin" <<<"$out" && ok "T7 flags origin" || bad "T7 flags origin" "$out"

# --- T8: full happy path — both dirs move, worktree healthy, origin repointed ---
R=$(newroot); C=$(mkfixture "$R" skimmer)
mkdir -p "$R/org/skimmer-worktrees"; addwt "$C" "$R/org/skimmer-worktrees/w1" w1
echo dirty > "$R/org/skimmer-worktrees/w1/scratch"          # KD 6
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T8 exit"        "$rc" "0"
[[ -d "$R/org/manta" ]]            && ok "T8 clone renamed"    || bad "T8 clone renamed" "$out"
[[ ! -e "$R/org/skimmer" ]]        && ok "T8 old gone"         || bad "T8 old gone"
[[ -d "$R/org/manta-worktrees/w1" ]] && ok "T8 wt parent moved" || bad "T8 wt parent moved"
[[ -f "$R/org/manta-worktrees/w1/scratch" ]] && ok "T8 dirty state survived" \
  || bad "T8 dirty state survived"
check "T8 origin" \
  "$(git -C "$R/org/manta" remote get-url origin)" \
  "https://github.com/HagaleTechnologies/manta.git"
# The regression that bare `git worktree repair` would leave behind (KD 1):
git -C "$R/org/manta" worktree list --porcelain | grep -q '^prunable' \
  && bad "T8 no prunable" || ok "T8 no prunable"
git -C "$R/org/manta-worktrees/w1" rev-parse --git-dir >/dev/null 2>&1 \
  && ok "T8 worktree healthy from inside" || bad "T8 worktree healthy from inside"

# --- T9: idempotent — a second --apply is a clean no-op (KD 7) ---
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T9 exit" "$rc" "0"
grep -q "ALREADY MIGRATED\|nothing to move" <<<"$out" && ok "T9 no-op" || bad "T9 no-op" "$out"

# --- T10: worktrees outside the org dir (~/catalyst/wt shape) are repaired too (KD 2) ---
R=$(newroot); C=$(mkfixture "$R" skimmer); mkdir -p "$R/wt/HagaleTechnologies"
addwt "$C" "$R/wt/HagaleTechnologies/MAN-9" MAN-9
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T10 exit" "$rc" "0"
git -C "$R/wt/HagaleTechnologies/MAN-9" rev-parse --git-dir >/dev/null 2>&1 \
  && ok "T10 external worktree repaired" || bad "T10 external worktree repaired" "$out"

# --- T11: a dead worktree path must not turn the run red (KD 3) ---
R=$(newroot); C=$(mkfixture "$R" skimmer); mkdir -p "$R/wt" "$R/org/skimmer-worktrees"
addwt "$C" "$R/org/skimmer-worktrees/live" live
addwt "$C" "$R/wt/dead" dead; rm -rf "$R/wt/dead"
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T11 exit" "$rc" "0"
grep -q "pre-existing prunable" <<<"$out" && ok "T11 dead wt informational" \
  || bad "T11 dead wt informational" "$out"
git -C "$R/org/manta-worktrees/live" rev-parse --git-dir >/dev/null 2>&1 \
  && ok "T11 live wt repaired" || bad "T11 live wt repaired"

# --- T12: refuses to move while an index.lock / rebase is in flight ---
R=$(newroot); C=$(mkfixture "$R" skimmer); : > "$C/.git/index.lock"
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T12 exit" "$rc" "2"
grep -qi "in use\|lock" <<<"$out" && ok "T12 busy refusal" || bad "T12 busy refusal" "$out"
[[ -d "$R/org/skimmer" ]] && ok "T12 nothing moved" || bad "T12 nothing moved"

# --- T13: a worktree parent that does not exist is fine (clone-only host) ---
R=$(newroot); mkfixture "$R" skimmer >/dev/null
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T13 exit" "$rc" "0"
[[ -d "$R/org/manta" && ! -e "$R/org/manta-worktrees" ]] \
  && ok "T13 no spurious wt parent" || bad "T13 no spurious wt parent"

# --- T14: refuses if the destination worktree parent already exists ---
R=$(newroot); mkfixture "$R" skimmer >/dev/null
mkdir -p "$R/org/skimmer-worktrees" "$R/org/manta-worktrees"
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T14 exit" "$rc" "2"
[[ -d "$R/org/skimmer" ]] && ok "T14 nothing moved" || bad "T14 nothing moved"

# --- T15: registry with a stale SKI repoRoot gains a correct MAN entry ---
R=$(newroot); mkfixture "$R" skimmer >/dev/null
mkdir -p "$R/catalyst/execution-core"
cat > "$R/catalyst/execution-core/registry.json" <<JSON
{"projects":[{"team":"SKI","repoRoot":"$R/org/skimmer"},
             {"team":"CTL","repoRoot":"$R/org/other"}]}
JSON
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T15 exit" "$rc" "0"
if command -v jq >/dev/null 2>&1; then
  check "T15 MAN repoRoot" \
    "$(jq -r '.projects[]|select(.team=="MAN")|.repoRoot' "$R/catalyst/execution-core/registry.json")" \
    "$R/org/manta"
  check "T15 SKI left alone" \
    "$(jq -r '[.projects[]|select(.team=="SKI")]|length' "$R/catalyst/execution-core/registry.json")" "1"
  check "T15 unrelated team untouched" \
    "$(jq -r '.projects[]|select(.team=="CTL")|.repoRoot' "$R/catalyst/execution-core/registry.json")" \
    "$R/org/other"
else
  skip "T15 registry assertions skipped (no jq)"
fi

# --- T16: an existing MAN entry with a stale repoRoot is corrected, not duplicated ---
R=$(newroot); mkfixture "$R" skimmer >/dev/null
mkdir -p "$R/catalyst/execution-core"
printf '{"projects":[{"team":"MAN","repoRoot":"%s/org/skimmer"}]}\n' "$R" \
  > "$R/catalyst/execution-core/registry.json"
"$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" >/dev/null 2>&1
if command -v jq >/dev/null 2>&1; then
  check "T16 single MAN entry" \
    "$(jq -r '[.projects[]|select(.team=="MAN")]|length' "$R/catalyst/execution-core/registry.json")" "1"
  check "T16 MAN corrected" \
    "$(jq -r '.projects[]|select(.team=="MAN")|.repoRoot' "$R/catalyst/execution-core/registry.json")" \
    "$R/org/manta"
else
  skip "T16 registry assertions skipped (no jq)"
fi

# --- T17: absent registry.json is not an error (host without execution-core) ---
R=$(newroot); mkfixture "$R" skimmer >/dev/null
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T17 exit" "$rc" "0"
grep -q "no registry.json" <<<"$out" && ok "T17 notes absence" || bad "T17 notes absence" "$out"

# --- T18: malformed registry.json warns, does not corrupt, does not fail the rename ---
R=$(newroot); mkfixture "$R" skimmer >/dev/null
mkdir -p "$R/catalyst/execution-core"
echo 'not json {' > "$R/catalyst/execution-core/registry.json"
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T18 exit" "$rc" "0"
check "T18 file untouched" "$(cat "$R/catalyst/execution-core/registry.json")" 'not json {'
# Malformed-JSON detection itself requires jq to attempt the parse; without jq the
# script correctly short-circuits earlier with its own "jq not installed" notice
# instead (same degrade-to-warning path T15-T17 rely on when jq is absent).
if command -v jq >/dev/null 2>&1; then
  grep -qi "could not parse" <<<"$out" && ok "T18 warns" || bad "T18 warns" "$out"
else
  grep -qi "jq not installed" <<<"$out" && ok "T18 warns (no jq)" || bad "T18 warns (no jq)" "$out"
fi

# --- T19: tooling preflight blocks --apply on a hardcoded old path ---
R=$(newroot); mkfixture "$R" skimmer >/dev/null
mkdir -p "$R/tools"
printf 'CACHE_SRC="%s/org/skimmer/target"\n' "$R" > "$R/tools/link-build-cache.sh"
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" \
      --scan-root "$R/tools" 2>&1); rc=$?
check "T19 exit" "$rc" "2"
grep -q "link-build-cache.sh" <<<"$out" && ok "T19 names the file" || bad "T19 names the file" "$out"
[[ -d "$R/org/skimmer" ]] && ok "T19 nothing moved" || bad "T19 nothing moved"

# --- T20: --allow-tooling-hits overrides, and says so ---
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" \
      --scan-root "$R/tools" --allow-tooling-hits 2>&1); rc=$?
check "T20 exit" "$rc" "0"
grep -q "OVERRIDE" <<<"$out" && ok "T20 records override" || bad "T20 records override" "$out"
[[ -d "$R/org/manta" ]] && ok "T20 moved" || bad "T20 moved"

# --- T21: --check reports tooling hits without blocking (exit reflects rename only) ---
R=$(newroot); mkfixture "$R" manta >/dev/null
git -C "$R/org/manta" remote set-url origin https://github.com/HagaleTechnologies/manta.git
mkdir -p "$R/tools"; printf 'x=%s/org/skimmer\n' "$R" > "$R/tools/link-build-cache.sh"
out=$("$SUT" --check --org-dir "$R/org" --catalyst-dir "$R/catalyst" --scan-root "$R/tools" 2>&1)
rc=$?
check "T21 exit" "$rc" "0"
grep -q "tooling reference" <<<"$out" && ok "T21 reports" || bad "T21 reports" "$out"

# --- T22: every long flag the runbook shows is a flag the parser's case block
#     accepts (matched against the parser only, so a prose comment mentioning a
#     flag name — e.g. "(empty for --check)." — cannot satisfy this vacuously) ---
RB="$REPO_ROOT/docs/RUNBOOKS/fleet-checkout-rename.md"
PARSER="$(awk '/^while \[\[ \$# -gt 0/,/^done$/' "$SUT")"
missing=""
while IFS= read -r f; do
  [[ -z $f ]] && continue
  grep -qE -- "(^[[:space:]]*${f}[)|])|(\\|${f}[)|])" <<<"$PARSER" || missing="$missing $f"
done < <(grep -o -- '--[a-z][a-z-]*' "$RB" | sort -u)
[[ -z $missing ]] && ok "T22 runbook flags all exist" || bad "T22 runbook flags all exist" "$missing"

# --- T23: default scan roots (no --scan-root given) must not block on the
#     checkout's own linked-worktree .git pointer file, which necessarily
#     contains the old path by construction (C2) ---
R=$(newroot)
ORG="$R/code-repos/github/HagaleTechnologies"
# mkfixture always nests under <root>/org, so build this fixture by hand: the
# default scan root ($HOME/code-repos/github) must be the org dir itself.
mkdir -p "$ORG"
git -c init.defaultBranch=main init -q "$ORG/skimmer"
git -C "$ORG/skimmer" config user.email t@example.invalid
git -C "$ORG/skimmer" config user.name  test
: > "$ORG/skimmer/README"; git -C "$ORG/skimmer" add README
git -C "$ORG/skimmer" -c commit.gpgsign=false commit -qm init
git -C "$ORG/skimmer" remote add origin https://github.com/HagaleTechnologies/skimmer.git
mkdir -p "$ORG/skimmer-worktrees"
addwt "$ORG/skimmer" "$ORG/skimmer-worktrees/w1" w1
out=$(HOME="$R" "$SUT" --apply --org-dir "$ORG" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T23 exit" "$rc" "0"
[[ -d "$ORG/manta" ]] && ok "T23 moved despite default-scan-root self-reference" \
  || bad "T23 moved despite default-scan-root self-reference" "$out"

# --- T24: --apply on an already-migrated host is a verified no-op even when a
#     scanned worktree contains literal old-path strings (this repo's own test
#     file, checked out under the new worktree parent) — C3 ---
R=$(newroot); C=$(mkfixture "$R" manta)
git -C "$C" remote set-url origin https://github.com/HagaleTechnologies/manta.git
mkdir -p "$R/org/manta-worktrees"
addwt "$C" "$R/org/manta-worktrees/w1" w1
mkdir -p "$R/org/manta-worktrees/w1/scripts/tests"
printf 'x=%s/org/skimmer-worktrees/w1\n' "$R" \
  > "$R/org/manta-worktrees/w1/scripts/tests/test-fleet-rename-checkout.sh"
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" \
      --scan-root "$R/org" 2>&1); rc=$?
check "T24 exit" "$rc" "0"
grep -q "ALREADY MIGRATED\|nothing to move" <<<"$out" && ok "T24 no-op despite worktree content" \
  || bad "T24 no-op despite worktree content" "$out"

# --- T25: --check must not report ALREADY MIGRATED when a worktree was moved by
#     hand without `git worktree repair` (C1) ---
R=$(newroot); C=$(mkfixture "$R" skimmer)
mkdir -p "$R/org/skimmer-worktrees"; addwt "$C" "$R/org/skimmer-worktrees/w1" w1
mv "$R/org/skimmer" "$R/org/manta"
mv "$R/org/skimmer-worktrees" "$R/org/manta-worktrees"
git -C "$R/org/manta" remote set-url origin https://github.com/HagaleTechnologies/manta.git
out=$("$SUT" --check --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T25 exit" "$rc" "1"
grep -qi "was not repaired" <<<"$out" && ok "T25 detects unrepaired hand-move" \
  || bad "T25 detects unrepaired hand-move" "$out"

# --- T26: a registry.json MAN entry left pointing at the old path fails --check
#     (never a silent all-PASS over a dangling registry entry) — C4 ---
R=$(newroot); C=$(mkfixture "$R" manta)
git -C "$C" remote set-url origin https://github.com/HagaleTechnologies/manta.git
mkdir -p "$R/catalyst/execution-core"
printf '{"projects":[{"team":"MAN","repoRoot":"%s/org/skimmer"}]}\n' "$R" \
  > "$R/catalyst/execution-core/registry.json"
out=$("$SUT" --check --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
if command -v jq >/dev/null 2>&1; then
  check "T26 exit" "$rc" "1"
  grep -qi "registry.json" <<<"$out" && ok "T26 flags stale registry" || bad "T26 flags stale registry" "$out"
else
  skip "T26 skipped (no jq)"
fi

# --- T27: a trailing flag with no operand is a usage error (exit 2), not the
#     unbound-variable exit 1 that reads as a real verdict (C6) ---
out=$("$SUT" --check --org-dir 2>&1); rc=$?
check "T27 exit" "$rc" "2"

# --- T28: mktemp failure during registry reconciliation degrades to the
#     script's own actionable message rather than a raw mktemp usage error /
#     shell "No such file or directory", and never corrupts registry.json
#     (CR-1/CR-6 — this is the path a bare, GNU-only `mktemp` call took on
#     BSD/macOS before it was given an explicit template) ---
R=$(newroot); C=$(mkfixture "$R" skimmer) >/dev/null
mkdir -p "$R/catalyst/execution-core" "$R/fakebin"
printf '{"projects":[]}\n' > "$R/catalyst/execution-core/registry.json"
printf '#!/usr/bin/env bash\nexit 1\n' > "$R/fakebin/mktemp"
chmod +x "$R/fakebin/mktemp"
out=$(PATH="$R/fakebin:$PATH" "$SUT" --apply --org-dir "$R/org" \
      --catalyst-dir "$R/catalyst" 2>&1); rc=$?
if command -v jq >/dev/null 2>&1; then
  check "T28 exit" "$rc" "1"
  grep -qi "usage: mktemp\|No such file or directory" <<<"$out" \
    && bad "T28 no raw mktemp/shell error" "$out" || ok "T28 no raw mktemp/shell error"
  grep -q "mktemp failed" <<<"$out" && ok "T28 prints actionable message" \
    || bad "T28 prints actionable message" "$out"
  check "T28 registry untouched" \
    "$(cat "$R/catalyst/execution-core/registry.json")" '{"projects":[]}'
else
  skip "T28 skipped (no jq)"
fi

# --- T29: --apply on a hand-renamed host (directories already renamed
#     outside the script, but a worktree still registered at the legacy path
#     and origin still pointing at the old URL — the "repointed on one
#     machine only" state the ticket itself describes) actually repairs and
#     repoints instead of verifying-only and looping at exit 1 forever
#     (validation-round finding C-1) ---
R=$(newroot); C=$(mkfixture "$R" skimmer)
mkdir -p "$R/org/skimmer-worktrees"; addwt "$C" "$R/org/skimmer-worktrees/w1" w1
mv "$R/org/skimmer" "$R/org/manta"
mv "$R/org/skimmer-worktrees" "$R/org/manta-worktrees"
# origin left stale on purpose
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T29 exit" "$rc" "0"
check "T29 origin repointed" \
  "$(git -C "$R/org/manta" remote get-url origin)" \
  "https://github.com/HagaleTechnologies/manta.git"
git -C "$R/org/manta-worktrees/w1" rev-parse --git-dir >/dev/null 2>&1 \
  && ok "T29 worktree repaired" || bad "T29 worktree repaired" "$out"
git -C "$R/org/manta" worktree list --porcelain | grep -q '^prunable' \
  && bad "T29 no prunable" "$out" || ok "T29 no prunable"
"$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" >/dev/null 2>&1; rc2=$?
check "T29 second apply exit" "$rc2" "0"

# --- T30: a trailing slash on --org-dir (what shell tab-completion produces)
#     must not stop --apply from finding and repairing a worktree still
#     registered at the legacy path (validation-round finding C-2) ---
R=$(newroot); C=$(mkfixture "$R" skimmer)
mkdir -p "$R/org/skimmer-worktrees"; addwt "$C" "$R/org/skimmer-worktrees/w1" w1
mv "$R/org/skimmer" "$R/org/manta"
mv "$R/org/skimmer-worktrees" "$R/org/manta-worktrees"
git -C "$R/org/manta" remote set-url origin https://github.com/HagaleTechnologies/manta.git
out=$("$SUT" --apply --org-dir "$R/org/" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T30 exit" "$rc" "0"
git -C "$R/org/manta-worktrees/w1" rev-parse --git-dir >/dev/null 2>&1 \
  && ok "T30 worktree repaired despite trailing slash" \
  || bad "T30 worktree repaired despite trailing slash" "$out"

# --- T31: a symlinked --org-dir component (macOS /tmp -> /private/tmp,
#     $TMPDIR -> /private/var/..., or ~/code-repos on a symlinked volume)
#     must not stop --apply from finding and repairing a stale worktree --
#     git canonicalizes worktree paths to the real (non-symlink) location, so
#     the script must normalize --org-dir the same way before deriving its
#     path variables (validation-round finding C-2) ---
R=$(newroot); C=$(mkfixture "$R/real" skimmer); ln -s "$R/real" "$R/link"
mkdir -p "$R/real/org/skimmer-worktrees"; addwt "$C" "$R/real/org/skimmer-worktrees/w1" w1
mv "$R/real/org/skimmer" "$R/real/org/manta"
mv "$R/real/org/skimmer-worktrees" "$R/real/org/manta-worktrees"
git -C "$R/real/org/manta" remote set-url origin https://github.com/HagaleTechnologies/manta.git
out=$("$SUT" --apply --org-dir "$R/link/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T31 exit" "$rc" "0"
git -C "$R/real/org/manta-worktrees/w1" rev-parse --git-dir >/dev/null 2>&1 \
  && ok "T31 worktree repaired through symlinked org-dir" \
  || bad "T31 worktree repaired through symlinked org-dir" "$out"

# --- T32: a symlinked file inside a scanned root (a stow/dotfiles-style
#     ~/bin symlink farm — the normal layout link-build-cache.sh lives in) is
#     followed and scanned, not silently skipped (validation-round finding
#     C-4: grep -r does not follow symlinks, -R does) ---
R=$(newroot); mkfixture "$R" skimmer >/dev/null
mkdir -p "$R/real-tools" "$R/bin"
printf 'CACHE_SRC=%s/org/skimmer/target\n' "$R" > "$R/real-tools/link-build-cache.sh"
ln -s "$R/real-tools/link-build-cache.sh" "$R/bin/link-build-cache.sh"
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" \
      --scan-root "$R/bin" 2>&1); rc=$?
check "T32 exit" "$rc" "2"
grep -q "link-build-cache.sh" <<<"$out" && ok "T32 finds symlinked file" \
  || bad "T32 finds symlinked file" "$out"
[[ -d "$R/org/skimmer" ]] && ok "T32 nothing moved" || bad "T32 nothing moved"

# --- T33: --scan-root ADDS to the default scan roots rather than replacing
#     them — a hit under a default root must still block --apply even when
#     --scan-root points somewhere else entirely (validation-round finding
#     C-5) ---
R=$(newroot)
mkdir -p "$R/code-repos/github/other-tool"
printf 'x=%s/org/skimmer\n' "$R" > "$R/code-repos/github/other-tool/uses-old-path.sh"
mkfixture "$R" skimmer >/dev/null
mkdir -p "$R/extra-scan-root"
out=$(HOME="$R" "$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" \
      --scan-root "$R/extra-scan-root" 2>&1); rc=$?
check "T33 exit" "$rc" "2"
grep -q "uses-old-path.sh" <<<"$out" \
  && ok "T33 default root hit still found alongside --scan-root" \
  || bad "T33 default root hit still found alongside --scan-root" "$out"

# --- T34: a --scan-root that does not exist is reported as skipped rather
#     than silently absorbed by grep's own exit 2 (validation-round finding
#     C-5) ---
R=$(newroot); mkfixture "$R" skimmer >/dev/null
out=$("$SUT" --check --org-dir "$R/org" --catalyst-dir "$R/catalyst" \
      --scan-root "$R/does-not-exist" 2>&1); rc=$?
grep -qi "does not exist or is not readable" <<<"$out" \
  && ok "T34 warns about missing --scan-root" || bad "T34 warns about missing --scan-root" "$out"

# --- T35: --apply on an already-migrated host with an unrelated pre-existing
#     dead worktree must still exit 0 (validation-round finding F1) — the
#     migrated branch must pass a real baseline into verify(), not an empty
#     one, or a host with any unrelated worktree cruft can never pass ---
R=$(newroot); C=$(mkfixture "$R" manta)
git -C "$C" remote set-url origin https://github.com/HagaleTechnologies/manta.git
mkdir -p "$R/wt"; addwt "$C" "$R/wt/dead" dead; rm -rf "$R/wt/dead"
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T35 exit" "$rc" "0"
grep -q "ALREADY MIGRATED" <<<"$out" && ok "T35 verdict" || bad "T35 verdict" "$out"
grep -q "pre-existing prunable" <<<"$out" && ok "T35 dead wt informational" \
  || bad "T35 dead wt informational" "$out"

# --- T36: --apply converges a half-hand-migrated host — main clone renamed
#     by hand, worktree parent left at its legacy name (validation-round
#     finding F2) — instead of failing forever on a leftover
#     <old>-worktrees directory the migrated branch never moved ---
R=$(newroot); C=$(mkfixture "$R" skimmer)
mkdir -p "$R/org/skimmer-worktrees"; addwt "$C" "$R/org/skimmer-worktrees/w1" w1
mv "$R/org/skimmer" "$R/org/manta"
git -C "$R/org/manta" remote set-url origin https://github.com/HagaleTechnologies/manta.git
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T36 exit" "$rc" "0"
[[ -d "$R/org/manta-worktrees" ]] && ok "T36 worktree parent moved" \
  || bad "T36 worktree parent moved" "$out"
[[ ! -e "$R/org/skimmer-worktrees" ]] && ok "T36 legacy worktree parent gone" \
  || bad "T36 legacy worktree parent gone"
git -C "$R/org/manta-worktrees/w1" rev-parse --git-dir >/dev/null 2>&1 \
  && ok "T36 worktree repaired" || bad "T36 worktree repaired"

# --- T37: a dead worktree entry and an unrelated, distinct, live worktree
#     that happens to share its basename must not be conflated (validation-
#     round finding F3) — the live one is not a "repaired copy" of the dead
#     one, and reporting it as such produces a no-op remedy ---
R=$(newroot); C=$(mkfixture "$R" manta)
git -C "$C" remote set-url origin https://github.com/HagaleTechnologies/manta.git
mkdir -p "$R/org/manta-worktrees" "$R/wt"
addwt "$C" "$R/org/manta-worktrees/w1" w1
addwt "$C" "$R/wt/w1" w1-dead; rm -rf "$R/wt/w1"
out=$("$SUT" --check --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T37 exit" "$rc" "0"
grep -q "pre-existing prunable" <<<"$out" && ok "T37 dead entry informational" \
  || bad "T37 dead entry informational" "$out"
grep -q "was not repaired" <<<"$out" && bad "T37 must not conflate basename collision" "$out" \
  || ok "T37 does not conflate basename collision"

# --- T38: --apply is not refused merely because <new>-worktrees already
#     exists when there is no <old>-worktrees to merge it with (validation-
#     round finding F4) — only TWO worktree parents is an actual conflict ---
R=$(newroot); mkfixture "$R" skimmer >/dev/null
mkdir -p "$R/org/manta-worktrees"
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T38 exit" "$rc" "0"
[[ -d "$R/org/manta" ]] && ok "T38 clone renamed" || bad "T38 clone renamed" "$out"
[[ -d "$R/org/manta-worktrees" ]] && ok "T38 pre-existing worktree parent survives" \
  || bad "T38 pre-existing worktree parent survives"

# --- T39: registry.json's file mode survives --apply un-narrowed — mktemp
#     creates 0600 by default, so mv-ing that over registry.json would leave
#     it unreadable to a daemon running under a different uid than the
#     operator (validation-round finding V-4) ---
if command -v jq >/dev/null 2>&1; then
  R=$(newroot); mkfixture "$R" skimmer >/dev/null
  mkdir -p "$R/catalyst/execution-core"
  printf '{"projects":[]}\n' > "$R/catalyst/execution-core/registry.json"
  chmod 644 "$R/catalyst/execution-core/registry.json"
  before="$(stat -c '%a' "$R/catalyst/execution-core/registry.json" 2>/dev/null \
            || stat -f '%Lp' "$R/catalyst/execution-core/registry.json" 2>/dev/null)"
  "$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" >/dev/null 2>&1
  after="$(stat -c '%a' "$R/catalyst/execution-core/registry.json" 2>/dev/null \
           || stat -f '%Lp' "$R/catalyst/execution-core/registry.json" 2>/dev/null)"
  check "T39 registry.json mode preserved" "$after" "$before"
else
  skip "T39 registry mode assertion skipped (no jq)"
fi

# --- T40: the tooling preflight still fires on a migrated host that has a
#     move actually pending — a half-hand-migrated host (clone already named
#     manta, worktree parent still skimmer-worktrees) with a
#     link-build-cache.sh hardcoding the legacy worktree path must be
#     refused, not silently skipped just because STATE==migrated
#     (validation-round finding V-3) ---
R=$(newroot); C=$(mkfixture "$R" manta)
git -C "$C" remote set-url origin https://github.com/HagaleTechnologies/manta.git
mkdir -p "$R/org/skimmer-worktrees" "$R/tools"
addwt "$C" "$R/org/skimmer-worktrees/w1" w1
printf 'CACHE_SRC=%s/org/skimmer-worktrees/w1/target\n' "$R" > "$R/tools/link-build-cache.sh"
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" \
      --scan-root "$R/tools" 2>&1); rc=$?
check "T40 exit" "$rc" "2"
grep -q "link-build-cache.sh" <<<"$out" && ok "T40 blocks pending worktree-parent move" \
  || bad "T40 blocks pending worktree-parent move" "$out"
[[ -d "$R/org/skimmer-worktrees" ]] && ok "T40 nothing moved" || bad "T40 nothing moved"

# --- T41: a symlinked --org-dir component must not turn the checkout's OWN
#     worktree files into false-positive "tooling hits" — only OLD_MAIN had a
#     raw-spelling exclusion; NEW_MAIN/OLD_WTP/NEW_WTP need raw exclusions
#     too, or a healthy, already-migrated host refuses to --check because its
#     own linked worktree's test-harness copy matches the tooling-preflight
#     scan under the raw, unresolved path (validation-round finding V-2) ---
R=$(newroot); C=$(mkfixture "$R/real" manta)
git -C "$C" remote set-url origin https://github.com/HagaleTechnologies/manta.git
ln -s "$R/real" "$R/link"
mkdir -p "$R/real/org/manta-worktrees"
addwt "$C" "$R/real/org/manta-worktrees/w1" w1
mkdir -p "$R/real/org/manta-worktrees/w1/scripts/tests"
printf 'x=%s/real/org/skimmer-worktrees/w1\n' "$R" \
  > "$R/real/org/manta-worktrees/w1/scripts/tests/test-fleet-rename-checkout.sh"
out=$("$SUT" --check --org-dir "$R/link/org" --catalyst-dir "$R/catalyst" \
      --scan-root "$R/link/org" 2>&1); rc=$?
check "T41 exit" "$rc" "0"
grep -q "tooling reference" <<<"$out" \
  && bad "T41 no false-positive tooling hit through symlinked org-dir" "$out" \
  || ok "T41 no false-positive tooling hit through symlinked org-dir"

printf '\n%d passed, %d failed, %d skipped\n' "$PASS" "$FAIL" "$SKIP"
[[ $FAIL -eq 0 ]]
