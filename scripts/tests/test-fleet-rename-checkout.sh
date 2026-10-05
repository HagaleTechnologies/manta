#!/usr/bin/env bash
# Fixture-driven tests for scripts/fleet-rename-checkout.sh.
# bash + git only, no CI wiring — see docs/DECISIONS/2026-09-04-man27-fleet-checkout-rename.md.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SUT="$REPO_ROOT/scripts/fleet-rename-checkout.sh"
PASS=0; FAIL=0; SKIP=0

ok()   { PASS=$((PASS+1)); printf 'ok   %s\n' "$1"; }
bad()  { FAIL=$((FAIL+1)); printf 'FAIL %s\n     %s\n' "$1" "${2:-}"; }
skip() { SKIP=$((SKIP+1)); printf 'SKIP %s\n' "$1"; }
check(){ if [[ "$2" == "$3" ]]; then ok "$1"; else bad "$1" "expected [$3], got [$2]"; fi; }

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
# CR-2 (validation round): newroot() is invoked as `R=$(newroot)` at every
# call site below. Command substitution forks a subshell, so an in-process
# array append inside newroot() only ever mutates that subshell's copy —
# the parent shell's ALL_ROOTS never grows past whatever was appended at
# top level. Track roots in a file instead: appends from inside the
# subshell are visible to the EXIT trap that reads it back to clean up.
ALL_ROOTS_FILE="$(mktemp "${TMPDIR:-/tmp}/fleet-rename-test-roots.XXXXXX")"
newroot() {
  local d
  d="$(mktemp -d "${TMPDIR:-/tmp}/fleet-rename-test.XXXXXX")"
  d="$(cd "$d" && pwd -P)"
  printf '%s\n' "$d" >> "$ALL_ROOTS_FILE"
  printf '%s\n' "$d"
}
cleanup_roots() {
  local d
  if [[ -f $ALL_ROOTS_FILE ]]; then
    while IFS= read -r d; do rm -rf "$d"; done < "$ALL_ROOTS_FILE"
    rm -f "$ALL_ROOTS_FILE"
  fi
}
trap cleanup_roots EXIT

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
printf '%s\n' "$TESTHOME" >> "$ALL_ROOTS_FILE"
export HOME="$TESTHOME"

# --- T1: old layout present, new absent -> exit 1, reports MIGRATION NEEDED ---
R=$(newroot); C=$(mkfixture "$R" skimmer)
mkdir -p "$R/org/skimmer-worktrees"; addwt "$C" "$R/org/skimmer-worktrees/w1" w1
out=$("$SUT" --check --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T1 exit"          "$rc" "1"
if grep -q "MIGRATION NEEDED" <<<"$out"; then ok "T1 verdict"; else bad "T1 verdict" "$out"; fi
if grep -q "skimmer-worktrees/w1" <<<"$out"; then ok "T1 lists worktree"; else bad "T1 lists worktree" "$out"; fi
if [[ -d "$R/org/skimmer" ]]; then ok "T1 --check mutated nothing"; else bad "T1 --check mutated nothing"; fi

# --- T2: already migrated and healthy -> exit 0, all PASS ---
R=$(newroot); C=$(mkfixture "$R" manta)
mkdir -p "$R/org/manta-worktrees"; addwt "$C" "$R/org/manta-worktrees/w1" w1
git -C "$C" remote set-url origin https://github.com/HagaleTechnologies/manta.git
out=$("$SUT" --check --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T2 exit" "$rc" "0"
if grep -q "ALREADY MIGRATED" <<<"$out"; then ok "T2 verdict"; else bad "T2 verdict" "$out"; fi

# --- T3: no checkout at all on this host -> exit 0, NOT PRESENT (not a failure) ---
R=$(newroot); mkdir -p "$R/org"
out=$("$SUT" --check --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T3 exit" "$rc" "0"
if grep -q "NOT PRESENT" <<<"$out"; then ok "T3 verdict"; else bad "T3 verdict" "$out"; fi

# --- T4: BOTH skimmer and manta present -> exit 2, refuses (ambiguous) ---
R=$(newroot); mkfixture "$R" skimmer >/dev/null; mkfixture "$R" manta >/dev/null
out=$("$SUT" --check --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T4 exit" "$rc" "2"
if grep -qi "both .* present" <<<"$out"; then ok "T4 refuses"; else bad "T4 refuses" "$out"; fi

# --- T5: health predicate catches the invisible main-only-move breakage (KD 2) ---
R=$(newroot); C=$(mkfixture "$R" skimmer); mkdir -p "$R/wt"
addwt "$C" "$R/wt/MAN-9" MAN-9
mv "$R/org/skimmer" "$R/org/manta"          # main moved, worktree did not
out=$("$SUT" --check --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T5 exit" "$rc" "1"
if grep -q "unhealthy worktree" <<<"$out"; then ok "T5 detects invisible breakage"
else bad "T5 detects invisible breakage" "$out"; fi
if grep -q "prunable" <<<"$out"; then bad "T5 must not rely on prunable" "$out"
else ok "T5 not relying on prunable"; fi

# --- T6: pre-existing prunable entry is informational, not a failure (KD 4) ---
R=$(newroot); C=$(mkfixture "$R" manta)
git -C "$C" remote set-url origin https://github.com/HagaleTechnologies/manta.git
mkdir -p "$R/wt"; addwt "$C" "$R/wt/dead" dead; rm -rf "$R/wt/dead"
out=$("$SUT" --check --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T6 exit" "$rc" "0"
if grep -q "pre-existing prunable" <<<"$out"; then ok "T6 informational"; else bad "T6 informational" "$out"; fi

# --- T7: wrong origin on an otherwise-migrated clone -> exit 1 ---
R=$(newroot); C=$(mkfixture "$R" manta)
git -C "$C" remote set-url origin https://github.com/HagaleTechnologies/skimmer.git
out=$("$SUT" --check --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T7 exit" "$rc" "1"
if grep -q "origin" <<<"$out"; then ok "T7 flags origin"; else bad "T7 flags origin" "$out"; fi

# --- T8: full happy path — both dirs move, worktree healthy, origin repointed ---
R=$(newroot); C=$(mkfixture "$R" skimmer)
mkdir -p "$R/org/skimmer-worktrees"; addwt "$C" "$R/org/skimmer-worktrees/w1" w1
echo dirty > "$R/org/skimmer-worktrees/w1/scratch"          # KD 6
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T8 exit"        "$rc" "0"
if [[ -d "$R/org/manta" ]]; then ok "T8 clone renamed"; else bad "T8 clone renamed" "$out"; fi
if [[ ! -e "$R/org/skimmer" ]]; then ok "T8 old gone"; else bad "T8 old gone"; fi
if [[ -d "$R/org/manta-worktrees/w1" ]]; then ok "T8 wt parent moved"; else bad "T8 wt parent moved"; fi
if [[ -f "$R/org/manta-worktrees/w1/scratch" ]]; then ok "T8 dirty state survived"
else bad "T8 dirty state survived"; fi
check "T8 origin" \
  "$(git -C "$R/org/manta" remote get-url origin)" \
  "https://github.com/HagaleTechnologies/manta.git"
# The regression that bare `git worktree repair` would leave behind (KD 1):
if git -C "$R/org/manta" worktree list --porcelain | grep -q '^prunable'
then bad "T8 no prunable"; else ok "T8 no prunable"; fi
if git -C "$R/org/manta-worktrees/w1" rev-parse --git-dir >/dev/null 2>&1
then ok "T8 worktree healthy from inside"; else bad "T8 worktree healthy from inside"; fi

# --- T9: idempotent — a second --apply is a clean no-op (KD 7) ---
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T9 exit" "$rc" "0"
if grep -q "ALREADY MIGRATED\|nothing to move" <<<"$out"; then ok "T9 no-op"; else bad "T9 no-op" "$out"; fi

# --- T10: worktrees outside the org dir (~/catalyst/wt shape) are repaired too (KD 2) ---
R=$(newroot); C=$(mkfixture "$R" skimmer); mkdir -p "$R/wt/HagaleTechnologies"
addwt "$C" "$R/wt/HagaleTechnologies/MAN-9" MAN-9
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T10 exit" "$rc" "0"
if git -C "$R/wt/HagaleTechnologies/MAN-9" rev-parse --git-dir >/dev/null 2>&1
then ok "T10 external worktree repaired"; else bad "T10 external worktree repaired" "$out"; fi

# --- T11: a dead worktree path must not turn the run red (KD 3) ---
R=$(newroot); C=$(mkfixture "$R" skimmer); mkdir -p "$R/wt" "$R/org/skimmer-worktrees"
addwt "$C" "$R/org/skimmer-worktrees/live" live
addwt "$C" "$R/wt/dead" dead; rm -rf "$R/wt/dead"
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T11 exit" "$rc" "0"
if grep -q "pre-existing prunable" <<<"$out"; then ok "T11 dead wt informational"
else bad "T11 dead wt informational" "$out"; fi
if git -C "$R/org/manta-worktrees/live" rev-parse --git-dir >/dev/null 2>&1
then ok "T11 live wt repaired"; else bad "T11 live wt repaired"; fi

# --- T12: refuses to move while an index.lock / rebase is in flight ---
R=$(newroot); C=$(mkfixture "$R" skimmer); : > "$C/.git/index.lock"
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T12 exit" "$rc" "2"
if grep -qi "in use\|lock" <<<"$out"; then ok "T12 busy refusal"; else bad "T12 busy refusal" "$out"; fi
if [[ -d "$R/org/skimmer" ]]; then ok "T12 nothing moved"; else bad "T12 nothing moved"; fi

# --- T13: a worktree parent that does not exist is fine (clone-only host) ---
R=$(newroot); mkfixture "$R" skimmer >/dev/null
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T13 exit" "$rc" "0"
if [[ -d "$R/org/manta" && ! -e "$R/org/manta-worktrees" ]]
then ok "T13 no spurious wt parent"; else bad "T13 no spurious wt parent"; fi

# --- T14: refuses if the destination worktree parent already exists ---
R=$(newroot); mkfixture "$R" skimmer >/dev/null
mkdir -p "$R/org/skimmer-worktrees" "$R/org/manta-worktrees"
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T14 exit" "$rc" "2"
if [[ -d "$R/org/skimmer" ]]; then ok "T14 nothing moved"; else bad "T14 nothing moved"; fi

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
if grep -q "no registry.json" <<<"$out"; then ok "T17 notes absence"; else bad "T17 notes absence" "$out"; fi

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
  if grep -qi "could not parse" <<<"$out"; then ok "T18 warns"; else bad "T18 warns" "$out"; fi
else
  if grep -qi "jq not installed" <<<"$out"; then ok "T18 warns (no jq)"; else bad "T18 warns (no jq)" "$out"; fi
fi

# --- T19: tooling preflight blocks --apply on a hardcoded old path ---
R=$(newroot); mkfixture "$R" skimmer >/dev/null
mkdir -p "$R/tools"
printf 'CACHE_SRC="%s/org/skimmer/target"\n' "$R" > "$R/tools/link-build-cache.sh"
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" \
      --scan-root "$R/tools" 2>&1); rc=$?
check "T19 exit" "$rc" "2"
if grep -q "link-build-cache.sh" <<<"$out"; then ok "T19 names the file"; else bad "T19 names the file" "$out"; fi
if [[ -d "$R/org/skimmer" ]]; then ok "T19 nothing moved"; else bad "T19 nothing moved"; fi

# --- T20: --allow-tooling-hits overrides, and says so ---
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" \
      --scan-root "$R/tools" --allow-tooling-hits 2>&1); rc=$?
check "T20 exit" "$rc" "0"
if grep -q "OVERRIDE" <<<"$out"; then ok "T20 records override"; else bad "T20 records override" "$out"; fi
if [[ -d "$R/org/manta" ]]; then ok "T20 moved"; else bad "T20 moved"; fi

# --- T21: --check reports tooling hits without blocking (exit reflects rename only) ---
R=$(newroot); mkfixture "$R" manta >/dev/null
git -C "$R/org/manta" remote set-url origin https://github.com/HagaleTechnologies/manta.git
mkdir -p "$R/tools"; printf 'x=%s/org/skimmer\n' "$R" > "$R/tools/link-build-cache.sh"
out=$("$SUT" --check --org-dir "$R/org" --catalyst-dir "$R/catalyst" --scan-root "$R/tools" 2>&1)
rc=$?
check "T21 exit" "$rc" "0"
if grep -q "tooling reference" <<<"$out"; then ok "T21 reports"; else bad "T21 reports" "$out"; fi

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
if [[ -z $missing ]]; then ok "T22 runbook flags all exist"; else bad "T22 runbook flags all exist" "$missing"; fi

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
if [[ -d "$ORG/manta" ]]; then ok "T23 moved despite default-scan-root self-reference"
else bad "T23 moved despite default-scan-root self-reference" "$out"; fi

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
if grep -q "ALREADY MIGRATED\|nothing to move" <<<"$out"; then ok "T24 no-op despite worktree content"
else bad "T24 no-op despite worktree content" "$out"; fi

# --- T25: --check must not report ALREADY MIGRATED when a worktree was moved by
#     hand without `git worktree repair` (C1) ---
R=$(newroot); C=$(mkfixture "$R" skimmer)
mkdir -p "$R/org/skimmer-worktrees"; addwt "$C" "$R/org/skimmer-worktrees/w1" w1
mv "$R/org/skimmer" "$R/org/manta"
mv "$R/org/skimmer-worktrees" "$R/org/manta-worktrees"
git -C "$R/org/manta" remote set-url origin https://github.com/HagaleTechnologies/manta.git
out=$("$SUT" --check --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T25 exit" "$rc" "1"
if grep -qi "was not repaired" <<<"$out"; then ok "T25 detects unrepaired hand-move"
else bad "T25 detects unrepaired hand-move" "$out"; fi

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
  if grep -qi "registry.json" <<<"$out"; then ok "T26 flags stale registry"; else bad "T26 flags stale registry" "$out"; fi
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
  if grep -qi "usage: mktemp\|No such file or directory" <<<"$out"
  then bad "T28 no raw mktemp/shell error" "$out"; else ok "T28 no raw mktemp/shell error"; fi
  if grep -q "mktemp failed" <<<"$out"; then ok "T28 prints actionable message"
  else bad "T28 prints actionable message" "$out"; fi
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
if git -C "$R/org/manta-worktrees/w1" rev-parse --git-dir >/dev/null 2>&1
then ok "T29 worktree repaired"; else bad "T29 worktree repaired" "$out"; fi
if git -C "$R/org/manta" worktree list --porcelain | grep -q '^prunable'
then bad "T29 no prunable" "$out"; else ok "T29 no prunable"; fi
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
if git -C "$R/org/manta-worktrees/w1" rev-parse --git-dir >/dev/null 2>&1
then ok "T30 worktree repaired despite trailing slash"
else bad "T30 worktree repaired despite trailing slash" "$out"; fi

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
if git -C "$R/real/org/manta-worktrees/w1" rev-parse --git-dir >/dev/null 2>&1
then ok "T31 worktree repaired through symlinked org-dir"
else bad "T31 worktree repaired through symlinked org-dir" "$out"; fi

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
if grep -q "link-build-cache.sh" <<<"$out"; then ok "T32 finds symlinked file"
else bad "T32 finds symlinked file" "$out"; fi
if [[ -d "$R/org/skimmer" ]]; then ok "T32 nothing moved"; else bad "T32 nothing moved"; fi

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
if grep -q "uses-old-path.sh" <<<"$out"
then ok "T33 default root hit still found alongside --scan-root"
else bad "T33 default root hit still found alongside --scan-root" "$out"; fi

# --- T34: a --scan-root that does not exist is reported as skipped rather
#     than silently absorbed by grep's own exit 2 (validation-round finding
#     C-5) ---
R=$(newroot); mkfixture "$R" skimmer >/dev/null
out=$("$SUT" --check --org-dir "$R/org" --catalyst-dir "$R/catalyst" \
      --scan-root "$R/does-not-exist" 2>&1); rc=$?
if grep -qi "does not exist" <<<"$out"
then ok "T34 warns about missing --scan-root"; else bad "T34 warns about missing --scan-root" "$out"; fi

# --- T35: --apply on an already-migrated host with an unrelated pre-existing
#     dead worktree must still exit 0 (validation-round finding F1) — the
#     migrated branch must pass a real baseline into verify(), not an empty
#     one, or a host with any unrelated worktree cruft can never pass ---
R=$(newroot); C=$(mkfixture "$R" manta)
git -C "$C" remote set-url origin https://github.com/HagaleTechnologies/manta.git
mkdir -p "$R/wt"; addwt "$C" "$R/wt/dead" dead; rm -rf "$R/wt/dead"
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T35 exit" "$rc" "0"
if grep -q "ALREADY MIGRATED" <<<"$out"; then ok "T35 verdict"; else bad "T35 verdict" "$out"; fi
if grep -q "pre-existing prunable" <<<"$out"; then ok "T35 dead wt informational"
else bad "T35 dead wt informational" "$out"; fi

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
if [[ -d "$R/org/manta-worktrees" ]]; then ok "T36 worktree parent moved"
else bad "T36 worktree parent moved" "$out"; fi
if [[ ! -e "$R/org/skimmer-worktrees" ]]; then ok "T36 legacy worktree parent gone"
else bad "T36 legacy worktree parent gone"; fi
if git -C "$R/org/manta-worktrees/w1" rev-parse --git-dir >/dev/null 2>&1
then ok "T36 worktree repaired"; else bad "T36 worktree repaired"; fi

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
if grep -q "pre-existing prunable" <<<"$out"; then ok "T37 dead entry informational"
else bad "T37 dead entry informational" "$out"; fi
if grep -q "was not repaired" <<<"$out"; then bad "T37 must not conflate basename collision" "$out"
else ok "T37 does not conflate basename collision"; fi

# --- T38: --apply is not refused merely because <new>-worktrees already
#     exists when there is no <old>-worktrees to merge it with (validation-
#     round finding F4) — only TWO worktree parents is an actual conflict ---
R=$(newroot); mkfixture "$R" skimmer >/dev/null
mkdir -p "$R/org/manta-worktrees"
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T38 exit" "$rc" "0"
if [[ -d "$R/org/manta" ]]; then ok "T38 clone renamed"; else bad "T38 clone renamed" "$out"; fi
if [[ -d "$R/org/manta-worktrees" ]]; then ok "T38 pre-existing worktree parent survives"
else bad "T38 pre-existing worktree parent survives"; fi

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
if grep -q "link-build-cache.sh" <<<"$out"; then ok "T40 blocks pending worktree-parent move"
else bad "T40 blocks pending worktree-parent move" "$out"; fi
if [[ -d "$R/org/skimmer-worktrees" ]]; then ok "T40 nothing moved"; else bad "T40 nothing moved"; fi

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
if grep -q "tooling reference" <<<"$out"
then bad "T41 no false-positive tooling hit through symlinked org-dir" "$out"
else ok "T41 no false-positive tooling hit through symlinked org-dir"; fi

# --- T42: --apply on the ALREADY-MIGRATED branch repairs an external
#     worktree (the ~/catalyst/wt/<key>/<ticket> shape) whose own directory
#     never moved — its recorded path is unchanged (cand == p) so the
#     prefix-remap that queues stale-legacy-path entries for repair never
#     fires, yet its .git file still targets the old main clone's now-gone
#     .git/worktrees dir once the main clone alone is hand-renamed
#     (validation-round finding 1: infinite non-convergence) ---
R=$(newroot); C=$(mkfixture "$R" skimmer); mkdir -p "$R/wt"
addwt "$C" "$R/wt/MAN-9" MAN-9
mv "$R/org/skimmer" "$R/org/manta"          # hand-rename: already STATE=migrated
git -C "$R/org/manta" remote set-url origin https://github.com/HagaleTechnologies/manta.git
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T42 exit" "$rc" "0"
if git -C "$R/wt/MAN-9" rev-parse --git-dir >/dev/null 2>&1
then ok "T42 external worktree repaired on already-migrated host"
else bad "T42 external worktree repaired on already-migrated host" "$out"; fi
out2=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc2=$?
check "T42 second apply exit" "$rc2" "0"
if grep -q "ALREADY MIGRATED" <<<"$out2"; then ok "T42 converges (no re-run loop)"
else bad "T42 converges (no re-run loop)" "$out2"; fi

# --- T43: registry.json repoRoot written in the raw, $HOME-spelled form
#     through a symlinked org dir is accepted as correct, not flagged as
#     drift against the canonicalized spelling (validation-round finding 2)
#     ---
R=$(newroot); C=$(mkfixture "$R/real" manta)
git -C "$C" remote set-url origin https://github.com/HagaleTechnologies/manta.git
ln -s "$R/real" "$R/link"
mkdir -p "$R/catalyst/execution-core"
printf '{"projects":[{"team":"MAN","repoRoot":"%s/link/org/manta"}]}\n' "$R" \
  > "$R/catalyst/execution-core/registry.json"
out=$("$SUT" --check --org-dir "$R/link/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
if command -v jq >/dev/null 2>&1; then
  check "T43 exit" "$rc" "0"
  if grep -qi "FAIL.*registry.json" <<<"$out"; then bad "T43 raw-spelled repoRoot not flagged as drift" "$out"
  else ok "T43 raw-spelled repoRoot not flagged as drift"; fi
else
  skip "T43 skipped (no jq)"
fi

# --- T44: the tooling preflight loudly warns rather than silently returning
#     "no hits" when the effective scan-root set is empty — distinct from
#     genuinely scanning real roots and finding nothing (validation-round
#     finding 3) ---
R=$(newroot); mkfixture "$R" skimmer >/dev/null
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T44 exit" "$rc" "0"
if [[ -d "$R/org/manta" ]]; then ok "T44 still moves (warns, does not refuse)"
else bad "T44 still moves (warns, does not refuse)" "$out"; fi
if grep -qi "no tooling-preflight scan roots" <<<"$out"
then ok "T44 warns when the scan-root set is empty"
else bad "T44 warns when the scan-root set is empty" "$out"; fi

# --- T45: --apply on a checkout with no origin remote at all ADDS one
#     instead of calling `git remote set-url`, which cannot create a remote
#     that does not exist — previously this died mid-run ("No such remote
#     'origin'") after the directories had already moved, leaving the host
#     half-migrated and every subsequent --apply failing identically
#     (validation-round finding CR-1) ---
R=$(newroot); C=$(mkfixture "$R" skimmer)
git -C "$C" remote remove origin
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T45 exit" "$rc" "0"
check "T45 origin added" \
  "$(git -C "$R/org/manta" remote get-url origin)" \
  "https://github.com/HagaleTechnologies/manta.git"
if grep -q "adding origin" <<<"$out"; then ok "T45 logs an add, not a repoint"
else bad "T45 logs an add, not a repoint" "$out"; fi
if grep -q "VERDICT: MIGRATED" <<<"$out"; then ok "T45 converges"; else bad "T45 converges" "$out"; fi

# --- T46: the tooling preflight catches the literal `$HOME/…` and `~/…`
#     spellings of the old path — the two forms a human actually writes by
#     hand in a shell script — which matched neither $OLD_MAIN nor
#     $RAW_OLD_MAIN before, since both of those are always already
#     shell-expanded to a real path (validation-round finding CR-2) ---
R=$(newroot); mkfixture "$R" skimmer >/dev/null
mkdir -p "$R/tools"
# Literal $HOME text on purpose: the tool under scan hardcodes the unexpanded spelling.
printf '%s\n' "CACHE_SRC=\"\$HOME/org/skimmer/target\"" > "$R/tools/link-build-cache-home.sh"
out=$(HOME="$R" "$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" \
      --scan-root "$R/tools" 2>&1); rc=$?
check "T46a exit" "$rc" "2"
if grep -q "link-build-cache-home.sh" <<<"$out"
then ok "T46a catches literal \$HOME-spelled path"
else bad "T46a catches literal \$HOME-spelled path" "$out"; fi
if [[ -d "$R/org/skimmer" ]]; then ok "T46a nothing moved"; else bad "T46a nothing moved"; fi

R=$(newroot); mkfixture "$R" skimmer >/dev/null
mkdir -p "$R/tools"
printf 'CACHE_SRC=~/org/skimmer/target\n' > "$R/tools/link-build-cache-tilde.sh"
out=$(HOME="$R" "$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" \
      --scan-root "$R/tools" 2>&1); rc=$?
check "T46b exit" "$rc" "2"
if grep -q "link-build-cache-tilde.sh" <<<"$out"
then ok "T46b catches literal ~-spelled path"
else bad "T46b catches literal ~-spelled path" "$out"; fi
if [[ -d "$R/org/skimmer" ]]; then ok "T46b nothing moved"; else bad "T46b nothing moved"; fi

# --- T47: an SSH origin (git@github.com:org/repo.git) resolving to the same
#     repo as the desired HTTPS URL is accepted as already-correct by
#     --check's comparison, and --apply must not silently rewrite it to
#     HTTPS (changing push authentication) unless the operator explicitly
#     asks via --remote-url (validation-round finding CR-3) ---
R=$(newroot); C=$(mkfixture "$R" manta)
git -C "$C" remote set-url origin git@github.com:HagaleTechnologies/manta.git
out=$("$SUT" --check --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T47a exit" "$rc" "0"
if grep -q "ALREADY MIGRATED" <<<"$out"; then ok "T47a SSH origin accepted by --check"
else bad "T47a SSH origin accepted by --check" "$out"; fi

R=$(newroot); C=$(mkfixture "$R" skimmer)
git -C "$C" remote set-url origin git@github.com:HagaleTechnologies/manta.git
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T47b exit" "$rc" "0"
check "T47b origin protocol preserved" \
  "$(git -C "$R/org/manta" remote get-url origin)" \
  "git@github.com:HagaleTechnologies/manta.git"
if grep -qi "leaving its protocol alone" <<<"$out"; then ok "T47b explains why it did not rewrite"
else bad "T47b explains why it did not rewrite" "$out"; fi

out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" \
      --remote-url https://github.com/HagaleTechnologies/manta.git 2>&1); rc=$?
check "T47c exit" "$rc" "0"
check "T47c explicit --remote-url forces the rewrite" \
  "$(git -C "$R/org/manta" remote get-url origin)" \
  "https://github.com/HagaleTechnologies/manta.git"

# --- T48: when registry.json cannot be verified (here: present but not
#     valid JSON — the same code path a jq-absent host takes), the run's own
#     VERDICT line says so instead of the bare "all checks pass" the
#     runbook's acceptance step treats as proof the registry is clean
#     (validation-round finding CR-4) ---
R=$(newroot); mkfixture "$R" skimmer >/dev/null
mkdir -p "$R/catalyst/execution-core"
echo 'not json {' > "$R/catalyst/execution-core/registry.json"
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T48 exit" "$rc" "0"
if command -v jq >/dev/null 2>&1; then
  if grep -q "VERDICT: MIGRATED — all checks pass\.$" <<<"$out"
  then bad "T48 verdict must not claim bare all-checks-pass when registry unverified" "$out"
  else ok "T48 verdict must not claim bare all-checks-pass when registry unverified"; fi
  if grep -qi "registry.json's 'MAN' entry NOT verified" <<<"$out"
  then ok "T48 verdict names the unverified registry"; else bad "T48 verdict names the unverified registry" "$out"; fi
else
  skip "T48 skipped (no jq)"
fi

# --- T49: --org-dir sitting inside an enclosing git repository (e.g. a
#     yadm/dotfiles-managed $HOME) with a leftover NON-repo skimmer
#     directory must refuse rather than silently operating on the enclosing
#     repo's .git (validation-round finding CR-1: `rev-parse --git-dir`
#     walks upward and succeeds against the enclosing repo, so the old
#     guard never fired; every subsequent `git -C "$MAIN" …` — worktree
#     inventory, worktree repair, set_origin — then targeted the wrong
#     repository and reported "VERDICT: MIGRATED" while having repointed
#     the enclosing repo's origin) ---
R=$(newroot)
git -c init.defaultBranch=main init -q "$R"
git -C "$R" config user.email t@example.invalid
git -C "$R" config user.name  test
: > "$R/README"; git -C "$R" add README
git -C "$R" -c commit.gpgsign=false commit -qm init
git -C "$R" remote add origin https://github.com/me/dotfiles.git
mkdir -p "$R/org/skimmer"
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T49 exit" "$rc" "2"
if grep -qi "is not a git repository" <<<"$out"; then ok "T49 refuses non-repo checkout"
else bad "T49 refuses non-repo checkout" "$out"; fi
check "T49 enclosing repo origin untouched" \
  "$(git -C "$R" remote get-url origin)" \
  "https://github.com/me/dotfiles.git"

# --- T50: --scan-root pointing directly at a FILE (not its parent
#     directory) is scanned rather than dropped as "does not exist or is
#     not readable" (validation-round finding CR-3) — the runbook tells
#     operators to point --scan-root at wherever link-build-cache.sh lives,
#     and grep handles a file operand fine ---
R=$(newroot); mkfixture "$R" skimmer >/dev/null
mkdir -p "$R/tools"
printf 'CACHE_SRC="%s/org/skimmer/target"\n' "$R" > "$R/tools/link-build-cache.sh"
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" \
      --scan-root "$R/tools/link-build-cache.sh" 2>&1); rc=$?
check "T50 exit" "$rc" "2"
if grep -q "link-build-cache.sh" <<<"$out"; then ok "T50 accepts a file as --scan-root"
else bad "T50 accepts a file as --scan-root" "$out"; fi
if [[ -d "$R/org/skimmer" ]]; then ok "T50 nothing moved"; else bad "T50 nothing moved"; fi

# --- T51: --check --apply together is refused (validation-round finding
#     C1) — the two mode flags used to share one case arm with no
#     mutual-exclusion guard, so the mode was last-wins and this exact
#     invocation silently performed the migration despite --check's entire
#     safety story being "mutates nothing" ---
R=$(newroot); mkfixture "$R" skimmer >/dev/null
out=$("$SUT" --check --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T51 exit" "$rc" "2"
if grep -qi "mutually exclusive" <<<"$out"; then ok "T51 refuses conflicting mode flags"
else bad "T51 refuses conflicting mode flags" "$out"; fi
if [[ -d "$R/org/skimmer" ]]; then ok "T51 nothing moved"; else bad "T51 nothing moved"; fi
# Repeating the SAME flag twice is not a conflict.
out=$("$SUT" --check --check --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T51 repeated same flag exit" "$rc" "1"

# --- T52: a dead worktree entry sharing a basename with an unrelated
#     FOREIGN repository's worktree (not one of $MAIN's own) must not be
#     adopted as that dead entry's repaired counterpart (validation-round
#     finding C2) — `git worktree repair` correctly refuses to rewrite a
#     foreign .git, so the old fallback made the host FAIL identically on
#     every re-run with a remedy line that does nothing ---
R=$(newroot); C=$(mkfixture "$R" manta)
git -C "$C" remote set-url origin https://github.com/HagaleTechnologies/manta.git
mkdir -p "$R/org/manta-worktrees" "$R/wt" "$R/foreign"
addwt "$C" "$R/wt/w1" w1-dead; rm -rf "$R/wt/w1"
git -c init.defaultBranch=main init -q "$R/foreign/repo"
git -C "$R/foreign/repo" config user.email t@example.invalid
git -C "$R/foreign/repo" config user.name  test
: > "$R/foreign/repo/README"; git -C "$R/foreign/repo" add README
git -C "$R/foreign/repo" -c commit.gpgsign=false commit -qm init
addwt "$R/foreign/repo" "$R/org/manta-worktrees/w1" w1-foreign
out=$("$SUT" --check --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
if grep -q "was not repaired" <<<"$out"; then bad "T52 must not adopt a foreign worktree" "$out"
else ok "T52 does not adopt a foreign worktree"; fi
check "T52 exit" "$rc" "0"
if grep -q "pre-existing prunable" <<<"$out"; then ok "T52 dead entry reported informational"
else bad "T52 dead entry reported informational" "$out"; fi

# --- T53: a stray FILE (not a directory) named `skimmer` must not make the
#     host fail forever (validation-round finding C3) — classify() and the
#     move paths test $OLD_MAIN/$OLD_WTP with -d, so this file was invisible
#     to migration, yet verify() tested the same paths with -e and reported
#     "legacy directory still present" — misdescribing a file as a directory
#     — on every subsequent re-run with no way to converge. It must now
#     refuse once, clearly, instead of looping ---
R=$(newroot); C=$(mkfixture "$R" manta)
git -C "$C" remote set-url origin https://github.com/HagaleTechnologies/manta.git
touch "$R/org/skimmer"
out=$("$SUT" --check --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T53 exit" "$rc" "2"
if grep -qi "is not a directory" <<<"$out"; then ok "T53 refuses stray non-directory 'skimmer'"
else bad "T53 refuses stray non-directory 'skimmer'" "$out"; fi
if grep -qi "legacy .skimmer. directory still present" <<<"$out"
then bad "T53 must not loop with a misleading directory FAIL" "$out"
else ok "T53 must not loop with a misleading directory FAIL"; fi

# --- T54: when grep cannot read a path under a scan root (here a dangling
#     symlink, which `grep -R` follows and fails on with exit 2), the tooling
#     preflight warns that the scan was incomplete instead of reporting it as
#     a clean scan. Skipped where this platform's grep does not fail on it ---
R=$(newroot); mkfixture "$R" skimmer >/dev/null
mkdir -p "$R/tools"; ln -s "$R/does-not-exist" "$R/tools/dangling"
grep -RIn no-such-text "$R/tools" >/dev/null 2>&1; probe=$?
if [[ $probe -gt 1 ]]; then
  out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" \
        --scan-root "$R/tools" 2>&1); rc=$?
  check "T54 exit" "$rc" "0"
  if grep -q "scan did not complete cleanly" <<<"$out"; then ok "T54 warns about an incomplete scan"
  else bad "T54 warns about an incomplete scan" "$out"; fi
else
  skip "T54 skipped (this grep does not fail on a dangling symlink)"
fi

# --- T55: both scripts are shellcheck-clean (plan Phases 1-3) ---
if command -v shellcheck >/dev/null 2>&1; then
  out=$(shellcheck -x "$SUT" "$REPO_ROOT/scripts/tests/test-fleet-rename-checkout.sh" 2>&1); rc=$?
  if [[ $rc -eq 0 ]]; then ok "T55 shellcheck clean"; else bad "T55 shellcheck clean" "$out"; fi
else
  skip "T55 skipped (no shellcheck)"
fi

# --- T56: when neither GNU nor BSD stat can read registry.json's mode, the
#     run says the rewritten file is left at 0600 instead of staying silent,
#     and the registry is still updated ---
R=$(newroot); mkfixture "$R" skimmer >/dev/null
mkdir -p "$R/catalyst/execution-core" "$R/fakebin"
printf '{"projects":[]}\n' > "$R/catalyst/execution-core/registry.json"
printf '#!/usr/bin/env bash\nexit 1\n' > "$R/fakebin/stat"
chmod +x "$R/fakebin/stat"
out=$(PATH="$R/fakebin:$PATH" "$SUT" --apply --org-dir "$R/org" \
      --catalyst-dir "$R/catalyst" 2>&1); rc=$?
if command -v jq >/dev/null 2>&1; then
  check "T56 exit" "$rc" "0"
  if grep -q "could not read the permission bits" <<<"$out"; then ok "T56 reports unreadable mode"
  else bad "T56 reports unreadable mode" "$out"; fi
  if grep -q "registry.json: team MAN" <<<"$out"; then ok "T56 registry still updated"
  else bad "T56 registry still updated" "$out"; fi
else
  skip "T56 skipped (no jq)"
fi

# --- T57: a symlinked registry.json (a dotfiles/stow layout) must not come
#     out of --apply world-writable: the mode copied onto the rewritten file
#     is the link target's, not the symlink's own 0777 ---
if command -v jq >/dev/null 2>&1; then
  R=$(newroot); mkfixture "$R" skimmer >/dev/null
  mkdir -p "$R/catalyst/execution-core" "$R/dotfiles"
  printf '{"projects":[]}\n' > "$R/dotfiles/registry.json"
  chmod 644 "$R/dotfiles/registry.json"
  ln -s "$R/dotfiles/registry.json" "$R/catalyst/execution-core/registry.json"
  "$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" >/dev/null 2>&1
  after="$(stat -L -c '%a' "$R/catalyst/execution-core/registry.json" 2>/dev/null \
           || stat -L -f '%Lp' "$R/catalyst/execution-core/registry.json" 2>/dev/null)"
  check "T57 symlinked registry.json not left world-writable" "$after" "644"
else
  skip "T57 registry mode assertion skipped (no jq)"
fi

# --- T58: a registered worktree path now occupied by an unrelated standalone
#     repository is not a healthy worktree of the checkout. `git worktree
#     list` does not mark it prunable (its .git exists), and a bare
#     `rev-parse --git-dir` succeeds against the unrelated repo, so the
#     health check must compare the common git dir with the checkout's ---
R=$(newroot); C=$(mkfixture "$R" manta)
mkdir -p "$R/org/manta-worktrees"
addwt "$C" "$R/org/manta-worktrees/w1" w1
rm -rf "$R/org/manta-worktrees/w1"
git -c init.defaultBranch=main init -q "$R/org/manta-worktrees/w1"
out=$("$SUT" --check --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T58 exit" "$rc" "1"
if grep -q "unhealthy worktree.*$R/org/manta-worktrees/w1" <<<"$out"
then ok "T58 flags a worktree path reused by an unrelated repository"
else bad "T58 flags a worktree path reused by an unrelated repository" "$out"; fi

# --- T59: --apply rewrites a symlinked registry.json (a dotfiles/stow
#     layout) through the link: the link stays a link and its target gets
#     the MAN entry, instead of the link being replaced by a regular file ---
if command -v jq >/dev/null 2>&1; then
  R=$(newroot); mkfixture "$R" skimmer >/dev/null
  mkdir -p "$R/catalyst/execution-core" "$R/dotfiles"
  printf '{"projects":[]}\n' > "$R/dotfiles/registry.json"
  ln -s ../../dotfiles/registry.json "$R/catalyst/execution-core/registry.json"
  out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
  check "T59 exit" "$rc" "0"
  if [[ -L "$R/catalyst/execution-core/registry.json" ]]; then ok "T59 registry.json is still a symlink"
  else bad "T59 registry.json is still a symlink" "$out"; fi
  check "T59 link target updated" \
    "$(jq -r '.projects[]|select(.team=="MAN")|.repoRoot' "$R/dotfiles/registry.json")" \
    "$R/org/manta"
  if compgen -G "$R/catalyst/execution-core/registry.json.*" >/dev/null \
     || compgen -G "$R/dotfiles/registry.json.*" >/dev/null
  then bad "T59 no temp file left behind" "$(ls -la "$R/catalyst/execution-core" "$R/dotfiles")"
  else ok "T59 no temp file left behind"; fi
else
  skip "T59 skipped (no jq)"
fi

# --- T60: the T58 health check still passes a healthy worktree on git older
#     than 2.31, which has no `rev-parse --path-format`: emulated by a git
#     wrapper that echoes that option back the way an unknown option is ---
R=$(newroot); C=$(mkfixture "$R" manta)
mkdir -p "$R/org/manta-worktrees" "$R/oldgit"
addwt "$C" "$R/org/manta-worktrees/w1" w1
REAL_GIT="$(command -v git)"
cat > "$R/oldgit/git" <<EOF
#!/usr/bin/env bash
args=()
for a in "\$@"; do
  if [[ \$a == --path-format=* ]]; then printf '%s\n' "\$a"; else args+=("\$a"); fi
done
exec "$REAL_GIT" "\${args[@]}"
EOF
chmod +x "$R/oldgit/git"
out=$(PATH="$R/oldgit:$PATH" "$SUT" --check --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T60 exit" "$rc" "0"
if grep -q "unhealthy worktree" <<<"$out"
then bad "T60 healthy worktree passes without rev-parse --path-format" "$out"
else ok "T60 healthy worktree passes without rev-parse --path-format"; fi

# --- T61: `git worktree list` exiting non-zero (emulated by a git wrapper)
#     must not read as an empty, healthy inventory: --check fails verification instead of passing it, and
#     --apply refuses before moving anything instead of skipping every
#     repair and reporting MIGRATED ---
R=$(newroot); C=$(mkfixture "$R" manta)
BROKENGIT="$R/brokengit"
mkdir -p "$R/org/manta-worktrees" "$BROKENGIT"
addwt "$C" "$R/org/manta-worktrees/w1" w1
REAL_GIT="$(command -v git)"
cat > "$BROKENGIT/git" <<EOF
#!/usr/bin/env bash
if [[ \${3:-} == worktree && \${4:-} == list ]]; then
  echo "fatal: simulated unreadable worktree metadata" >&2; exit 128
fi
exec "$REAL_GIT" "\$@"
EOF
chmod +x "$BROKENGIT/git"
out=$(PATH="$BROKENGIT:$PATH" "$SUT" --check --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T61 --check exit" "$rc" "1"
if grep -q "PASS  every linked worktree" <<<"$out"
then bad "T61 --check does not pass an unreadable inventory" "$out"
else ok "T61 --check does not pass an unreadable inventory"; fi
out=$(PATH="$BROKENGIT:$PATH" "$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T61 --apply (already migrated) exit" "$rc" "2"
if grep -q "cannot enumerate this checkout's worktrees" <<<"$out"
then ok "T61 --apply (already migrated) refuses for the listing failure"
else bad "T61 --apply (already migrated) refuses for the listing failure" "$out"; fi
R=$(newroot); C=$(mkfixture "$R" skimmer)
mkdir -p "$R/org/skimmer-worktrees"; addwt "$C" "$R/org/skimmer-worktrees/w1" w1
out=$(PATH="$BROKENGIT:$PATH" "$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T61 --apply exit" "$rc" "2"
if [[ -d "$R/org/skimmer" && ! -e "$R/org/manta" ]] \
   && grep -q "cannot enumerate this checkout's worktrees" <<<"$out"; then ok "T61 --apply moved nothing"
else bad "T61 --apply moved nothing" "$out"; fi
# Same defect in the busy preflight: a failing `ps` must refuse the move, not
# read as "no process holds the checkout".
R=$(newroot); mkfixture "$R" skimmer >/dev/null; mkdir -p "$R/fakebin"
printf '#!/usr/bin/env bash\nexit 1\n' > "$R/fakebin/ps"; chmod +x "$R/fakebin/ps"
out=$(PATH="$R/fakebin:$PATH" "$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T61 --apply with failing ps exit" "$rc" "2"
if [[ -d "$R/org/skimmer" && ! -e "$R/org/manta" ]] && grep -q "could not list processes" <<<"$out"
then ok "T61 failing ps refuses the move"
else bad "T61 failing ps refuses the move" "$out"; fi

# --- T62: an origin spelled as the RELATIVE LOCAL PATH
#     github.com/HagaleTechnologies/manta.git is a directory name to git, not
#     the GitHub repository, so it must not normalize equal to the HTTPS URL:
#     --check flags it and --apply repoints it ---
R=$(newroot); C=$(mkfixture "$R" manta)
git -C "$C" remote set-url origin github.com/HagaleTechnologies/manta.git
out=$("$SUT" --check --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T62 --check exit" "$rc" "1"
if grep -q "FAIL  origin = github.com/HagaleTechnologies/manta.git" <<<"$out"
then ok "T62 --check flags a local-path origin"
else bad "T62 --check flags a local-path origin" "$out"; fi
out=$("$SUT" --apply --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T62 --apply exit" "$rc" "0"
check "T62 --apply repoints origin" "$(git -C "$C" remote get-url origin)" \
  "https://github.com/HagaleTechnologies/manta.git"
# A '/' before the first ':' also makes git read a git@ spelling as a local
# path rather than scp-style SSH.
git -C "$C" remote set-url origin git@github.com/HagaleTechnologies:manta.git
out=$("$SUT" --check --org-dir "$R/org" --catalyst-dir "$R/catalyst" 2>&1); rc=$?
check "T62 --check exit (git@ local path)" "$rc" "1"
if grep -q "FAIL  origin = git@github.com/HagaleTechnologies:manta.git" <<<"$out"
then ok "T62 --check flags a git@ local-path origin"
else bad "T62 --check flags a git@ local-path origin" "$out"; fi

printf '\n%d passed, %d failed, %d skipped\n' "$PASS" "$FAIL" "$SKIP"
[[ $FAIL -eq 0 ]]
