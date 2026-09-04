#!/usr/bin/env bash
# Rename a fleet machine's manta checkout from its legacy `skimmer` directory name,
# move its worktree parent, repair linked worktrees, and repoint origin (MAN-27).
#
# Two modes:
#   --check   read-only audit + health verification. Mutates nothing. This is the
#             acceptance evidence for MAN-27's Gherkin scenarios.
#   --apply   perform the migration, then run the same verification.
#
# Idempotent: --apply on an already-migrated host is a verified no-op.
#
# Exit codes: 0 = healthy / nothing to do, 1 = migration needed or verification
# failed, 2 = refused (ambiguous or unsafe state needing a human).
#
# Requires bash + git. `jq` is required only for the registry step (Phase 3) and is
# degraded to a warning when absent.
#
# See docs/RUNBOOKS/fleet-checkout-rename.md and
#     docs/DECISIONS/2026-09-04-man27-fleet-checkout-rename.md
set -uo pipefail

OLD_NAME=skimmer
NEW_NAME=manta
ORG_DIR="$HOME/code-repos/github/HagaleTechnologies"
REMOTE_URL="https://github.com/HagaleTechnologies/manta.git"
CATALYST_DIR="${CATALYST_DIR:-$HOME/catalyst}"
TEAM_NEW=MAN
MODE=""
SCAN_ROOTS=()
ALLOW_TOOLING_HITS=0

die()  { printf 'ERROR: %s\n' "$*" >&2; exit 2; }
say()  { printf '%s\n' "$*"; }
pass() { printf 'PASS  %s\n' "$*"; }
fail() { printf 'FAIL  %s\n' "$*"; FAILED=1; }
info() { printf 'info  %s\n' "$*"; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --check|--apply) MODE="${1#--}";;
    --old)          OLD_NAME="$2"; shift;;
    --new)          NEW_NAME="$2"; shift;;
    --org-dir)      ORG_DIR="${2/#\~/$HOME}"; shift;;
    --remote-url)   REMOTE_URL="$2"; shift;;
    --catalyst-dir) CATALYST_DIR="${2/#\~/$HOME}"; shift;;
    --scan-root)    SCAN_ROOTS+=("${2/#\~/$HOME}"); shift;;
    --allow-tooling-hits) ALLOW_TOOLING_HITS=1;;
    -h|--help) sed -n '2,25p' "$0"; exit 0;;
    *) die "unknown argument: $1";;
  esac
  shift
done
[[ -n $MODE ]] || die "one of --check or --apply is required"

OLD_MAIN="$ORG_DIR/$OLD_NAME"
NEW_MAIN="$ORG_DIR/$NEW_NAME"
OLD_WTP="$ORG_DIR/${OLD_NAME}-worktrees"
NEW_WTP="$ORG_DIR/${NEW_NAME}-worktrees"

# Host identity, mirroring lib/host-identity.sh:9-40 (env -> Layer-2 config -> hostname).
host_name() {
  if [[ -n ${CATALYST_HOST_NAME:-} ]]; then printf '%s' "$CATALYST_HOST_NAME"; return; fi
  local cfg="$HOME/.config/catalyst/config.json" n=""
  if [[ -r $cfg ]] && command -v jq >/dev/null 2>&1; then
    n="$(jq -r '.catalyst.host.name // empty' "$cfg" 2>/dev/null)"
  fi
  [[ -n $n ]] && printf '%s' "$n" || hostname | cut -d. -f1
}

say "== MAN-27 fleet checkout rename =="
say "host:        $(host_name)"
say "mode:        $MODE"
say "org dir:     $ORG_DIR"
say "old -> new:  $OLD_NAME -> $NEW_NAME"
say ""

# ---- classify -------------------------------------------------------------
if [[ -d $OLD_MAIN && -d $NEW_MAIN ]]; then
  die "both '$OLD_NAME' and '$NEW_NAME' are present in $ORG_DIR — ambiguous.
     A human must decide which is authoritative (compare 'git -C <dir> log -1' and
     'git -C <dir> status') and remove or archive the other before re-running."
elif [[ -d $NEW_MAIN ]]; then
  STATE=migrated; MAIN="$NEW_MAIN"
elif [[ -d $OLD_MAIN ]]; then
  STATE=legacy;   MAIN="$OLD_MAIN"
else
  say "VERDICT: NOT PRESENT — no '$OLD_NAME' or '$NEW_NAME' checkout under $ORG_DIR."
  say "Nothing to do on this host. (Record it as 'not present' in the runbook table.)"
  exit 0
fi
git -C "$MAIN" rev-parse --git-dir >/dev/null 2>&1 \
  || die "$MAIN exists but is not a git repository"

# ---- worktree inventory ---------------------------------------------------
# Emits "<path>\t<prunable|ok>" per worktree, main tree first.
inventory() {
  local wt="" pr=""
  while IFS= read -r line; do
    case "$line" in
      worktree\ *) [[ -n $wt ]] && printf '%s\t%s\n' "$wt" "${pr:-ok}"
                   wt="${line#worktree }"; pr="";;
      prunable*)   pr="prunable";;
      "")          ;;
    esac
  done < <(git -C "$MAIN" worktree list --porcelain 2>/dev/null)
  [[ -n $wt ]] && printf '%s\t%s\n' "$wt" "${pr:-ok}"
}

FAILED=0

# ---- verify (shared by --check and the tail of --apply) --------------------
# $1 = newline-separated baseline of paths that were ALREADY prunable before any
#      move (empty for --check).
verify() {
  local baseline="${1:-}" line path flag unhealthy=0 mainpath
  mainpath="$(git -C "$MAIN" rev-parse --show-toplevel)"

  # Gherkin 1: directory named <new> present, no <old> directory remains.
  [[ -d $NEW_MAIN ]] && pass "directory '$NEW_NAME' present in $ORG_DIR" \
                     || fail "directory '$NEW_NAME' absent in $ORG_DIR"
  [[ -e $OLD_MAIN ]] && fail "legacy '$OLD_NAME' directory still present: $OLD_MAIN" \
                     || pass "no '$OLD_NAME' directory remains in $ORG_DIR"
  [[ -e $OLD_WTP ]]  && fail "legacy worktree parent still present: $OLD_WTP" \
                     || pass "no '${OLD_NAME}-worktrees' directory remains"

  # Gherkin 1: origin points at the new URL.
  local url; url="$(git -C "$MAIN" remote get-url origin 2>/dev/null || echo '<none>')"
  [[ $url == "$REMOTE_URL" ]] && pass "origin = $url" \
                              || fail "origin = $url (want $REMOTE_URL)"

  # Gherkin 2, part A: no NEW prunable worktrees (KD 4 — baseline-relative).
  while IFS=$'\t' read -r path flag; do
    [[ -z $path ]] && continue
    if [[ $flag == prunable ]]; then
      if grep -qxF "$path" <<<"$baseline"; then
        info "pre-existing prunable worktree (unrelated to this rename): $path"
      else
        fail "worktree is prunable after the move: $path"
      fi
    fi
    # Gherkin 2, part B: the check `worktree list` CANNOT make (KD 2).
    [[ $path == "$mainpath" ]] && continue
    if [[ -d $path ]] && ! git -C "$path" rev-parse --git-dir >/dev/null 2>&1; then
      fail "unhealthy worktree — its .git file points at a stale location: $path"
      unhealthy=1
    fi
  done <<<"$(inventory)"
  [[ $unhealthy -eq 0 ]] && pass "every linked worktree resolves its git dir"
}

# ---- tooling preflight ----------------------------------------------------
# MAN-27's technical note asks that magazzino's clone/worktree tooling and
# link-build-cache.sh be checked for path assumptions before moving. Neither is
# reachable from a dispatch container (the planning session searched exhaustively),
# so the check runs HERE, on the host where they exist.
default_scan_roots() {
  printf '%s\n' "$HOME/code-repos/github" "$HOME/bin" "$HOME/.local/bin"
}
tooling_hits() {
  local roots=("${SCAN_ROOTS[@]}")
  if [[ ${#roots[@]} -eq 0 ]]; then
    while IFS= read -r r; do [[ -d $r ]] && roots+=("$r"); done < <(default_scan_roots)
  fi
  [[ ${#roots[@]} -eq 0 ]] && return 0
  # Literal old checkout path, and the directory name as a path segment. Skip .git
  # and the checkout itself (its own history legitimately mentions the old name).
  grep -rIn --exclude-dir=.git --exclude-dir=node_modules --exclude-dir=target \
       -e "$OLD_MAIN" -e "/${OLD_NAME}-worktrees" \
       "${roots[@]}" 2>/dev/null \
    | grep -v "^${OLD_MAIN}/" | grep -v "^${NEW_MAIN}/" || true
}

HITS="$(tooling_hits)"
if [[ -n $HITS ]]; then
  say "tooling reference(s) to the legacy path found:"
  printf '  %s\n' "$HITS" | head -50
  say ""
fi
if [[ $MODE == apply && -n $HITS ]]; then
  if [[ $ALLOW_TOOLING_HITS -eq 1 ]]; then
    say "OVERRIDE: --allow-tooling-hits given; proceeding despite the references above."
    say "          Fix them by hand after the move."
    say ""
  else
    die "refusing to move while tooling still hardcodes the legacy path (listed above).
     Fix those references (magazzino's clone/worktree tooling, link-build-cache.sh,
     shell rc files), or re-run with --allow-tooling-hits to proceed anyway."
  fi
fi

# ---- busy preflight -------------------------------------------------------
# Moving a checkout out from under a live git operation corrupts it. Check the
# cheap, dependency-free signals rather than reaching for lsof (absent on some
# fleet hosts and pathologically slow with +D on a large tree).
busy_reasons() {
  local d
  for d in "$MAIN/.git" "$MAIN"/.git/worktrees/*; do
    [[ -e $d ]] || continue
    [[ -e $d/index.lock    ]] && printf 'git index.lock present: %s\n' "$d/index.lock"
    [[ -e $d/HEAD.lock     ]] && printf 'git HEAD.lock present: %s\n'  "$d/HEAD.lock"
    [[ -e $d/rebase-merge  ]] && printf 'rebase in progress: %s\n'     "$d/rebase-merge"
    [[ -e $d/rebase-apply  ]] && printf 'rebase in progress: %s\n'     "$d/rebase-apply"
    [[ -e $d/MERGE_HEAD    ]] && printf 'merge in progress: %s\n'      "$d/MERGE_HEAD"
    [[ -e $d/BISECT_LOG    ]] && printf 'bisect in progress: %s\n'     "$d/BISECT_LOG"
  done
  # A process whose command line mentions the old path is very likely holding it.
  if command -v pgrep >/dev/null 2>&1; then
    pgrep -af "$OLD_MAIN" 2>/dev/null | grep -v "fleet-rename-checkout" \
      | sed 's/^/process referencing the checkout: /'
  fi
  return 0
}

# ---- registry.json ----------------------------------------------------------
# Schema per lib/worktree-resolve.sh:8-16 — {"projects":[{"team","repoRoot"}]}.
reconcile_registry() {
  local reg="$CATALYST_DIR/execution-core/registry.json" tmp
  if [[ ! -f $reg ]]; then
    info "no registry.json at $reg — nothing to reconcile on this host"
    return 0
  fi
  if ! command -v jq >/dev/null 2>&1; then
    info "jq not installed — skipping registry reconciliation."
    info "  Edit $reg by hand so team \"$TEAM_NEW\" has repoRoot \"$NEW_MAIN\"."
    return 0
  fi
  if ! jq -e . "$reg" >/dev/null 2>&1; then
    info "could not parse $reg as JSON — leaving it untouched. Fix it by hand."
    return 0
  fi
  tmp="$(mktemp)"
  # Correct an existing MAN entry in place, or append one. A stale SKI entry is
  # left alone on purpose: checkout-sync.mjs drops entries whose repoRoot is
  # missing (CTL-854), so it is already inert, and removing registry rows is a
  # wider change than MAN-27 needs.
  jq --arg team "$TEAM_NEW" --arg root "$NEW_MAIN" '
    .projects = ((.projects // []) | map(if .team == $team then .repoRoot = $root else . end))
    | if any(.projects[]; .team == $team) then .
      else .projects += [{team: $team, repoRoot: $root}] end
  ' "$reg" > "$tmp" && mv "$tmp" "$reg" \
    && say "registry.json: team $TEAM_NEW -> $NEW_MAIN" \
    || { rm -f "$tmp"; info "registry.json update failed — edit it by hand"; }
  local stale
  stale="$(jq -r --arg old "$OLD_NAME" '.projects[]? | select((.repoRoot|type=="string") and (.repoRoot|endswith("/" + $old))) | .team' "$reg" 2>/dev/null)"
  [[ -n $stale ]] && info "stale registry entries left in place (inert, dropped on read per CTL-854): $stale"
  return 0
}

# ---- apply ----------------------------------------------------------------
if [[ $MODE == apply ]]; then
  if [[ $STATE == migrated ]]; then
    say "nothing to move — checkout is already named '$NEW_NAME'; verifying only."
    verify ""
    say ""
    [[ $FAILED -eq 0 ]] && { say "VERDICT: ALREADY MIGRATED — all checks pass."; exit 0; }
    say "VERDICT: MIGRATION NEEDED — see FAIL lines above."; exit 1
  fi

  busy="$(busy_reasons)"
  [[ -n $busy ]] && die "checkout is in use; refusing to move.
$busy
     Stop the Catalyst daemon and any open session on this checkout, then re-run."

  [[ -e $NEW_WTP ]] && die "destination worktree parent already exists: $NEW_WTP
     Refusing to merge two worktree parents. Resolve by hand, then re-run."

  # Baseline: which worktrees were ALREADY prunable before we touched anything (KD 4).
  BASELINE="$(inventory | awk -F'\t' '$2=="prunable"{print $1}')"

  # Inventory BEFORE the move — afterwards `worktree list` reports stale paths.
  WTS="$(inventory | cut -f1)"
  MAINPATH="$(git -C "$MAIN" rev-parse --show-toplevel)"

  say "moving $OLD_MAIN -> $NEW_MAIN"
  mv "$OLD_MAIN" "$NEW_MAIN" || die "mv of the main checkout failed"
  if [[ -d $OLD_WTP ]]; then
    say "moving $OLD_WTP -> $NEW_WTP"
    mv "$OLD_WTP" "$NEW_WTP" || die "mv of the worktree parent failed (main checkout
     is already at $NEW_MAIN — re-run --apply after fixing the cause)"
  else
    info "no '${OLD_NAME}-worktrees' directory on this host — nothing to move there"
  fi
  MAIN="$NEW_MAIN"

  # Map every pre-move worktree path onto its post-move location and hand the list
  # to `git worktree repair` EXPLICITLY. Bare `git worktree repair` is a silent
  # no-op when both sides moved: it exits 0 and leaves the worktree prunable
  # (measured, git 2.47.3) — see the plan's Key Discovery 1.
  REPAIR=()
  while IFS= read -r p; do
    [[ -z $p || $p == "$MAINPATH" ]] && continue
    case "$p" in
      "$OLD_WTP"/*)  p="$NEW_WTP/${p#"$OLD_WTP"/}";;
      "$OLD_MAIN"/*) p="$NEW_MAIN/${p#"$OLD_MAIN"/}";;
    esac
    # `git worktree repair` exits 1 if ANY argument path is invalid, while still
    # repairing the rest (measured) — so filter dead paths out here and let the
    # verifier report them as pre-existing prunable entries instead.
    [[ -d $p ]] && REPAIR+=("$p")
  done <<<"$WTS"

  if [[ ${#REPAIR[@]} -gt 0 ]]; then
    say "repairing ${#REPAIR[@]} linked worktree(s)"
    git -C "$MAIN" worktree repair "${REPAIR[@]}" || \
      info "git worktree repair reported a non-zero status; the verifier below is authoritative"
  else
    info "no linked worktrees to repair"
  fi

  # One set-url covers every worktree — they share the main checkout's config
  # (measured).
  cur="$(git -C "$MAIN" remote get-url origin 2>/dev/null || echo '')"
  if [[ $cur != "$REMOTE_URL" ]]; then
    say "repointing origin: ${cur:-<none>} -> $REMOTE_URL"
    git -C "$MAIN" remote set-url origin "$REMOTE_URL" || die "git remote set-url failed"
  else
    info "origin already correct"
  fi

  reconcile_registry

  say ""
  verify "$BASELINE"
  say ""
  [[ $FAILED -eq 0 ]] && { say "VERDICT: MIGRATED — all checks pass."; exit 0; }
  say "VERDICT: MIGRATION INCOMPLETE — see FAIL lines above."; exit 1
fi

# ---- check ------------------------------------------------------------------
if [[ $MODE == check ]]; then
  say "worktrees currently registered:"
  inventory | while IFS=$'\t' read -r p f; do say "  - $p [$f]"; done
  say ""
  # --check never mutates anything, so there is no before/after distinction to
  # draw: every prunable entry it observes is by definition pre-existing, not
  # something this run caused. Pass the full current-prunable set as baseline
  # (mirrors --apply's pre-move baseline capture) so verify() reports it as
  # informational rather than a failure (KD 4).
  BASELINE="$(inventory | awk -F'\t' '$2=="prunable"{print $1}')"
  verify "$BASELINE"
  say ""
  if [[ $STATE == legacy ]]; then
    say "VERDICT: MIGRATION NEEDED — checkout is still named '$OLD_NAME'."
    exit 1
  elif [[ $FAILED -eq 1 ]]; then
    say "VERDICT: MIGRATION NEEDED — checkout is named '$NEW_NAME' but checks failed above."
    exit 1
  fi
  say "VERDICT: ALREADY MIGRATED — all checks pass."
  exit 0
fi
