#!/usr/bin/env bash
# Fixture-driven tests for scripts/fleet-rename-checkout.sh.
# bash + git only, no CI wiring — see docs/DECISIONS/2026-09-04-man27-fleet-checkout-rename.md.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SUT="$REPO_ROOT/scripts/fleet-rename-checkout.sh"
PASS=0; FAIL=0

ok()   { PASS=$((PASS+1)); printf 'ok   %s\n' "$1"; }
bad()  { FAIL=$((FAIL+1)); printf 'FAIL %s\n     %s\n' "$1" "${2:-}"; }
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
newroot() { mktemp -d; }

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

# --- T22: every long flag the runbook shows is a flag the script accepts ---
RB="$REPO_ROOT/docs/RUNBOOKS/fleet-checkout-rename.md"
missing=""
for f in $(grep -o -- '--[a-z][a-z-]*' "$RB" | sort -u); do
  grep -q -- "$f)" "$SUT" || missing="$missing $f"
done
[[ -z $missing ]] && ok "T22 runbook flags all exist" || bad "T22 runbook flags all exist" "$missing"

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[[ $FAIL -eq 0 ]]
