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
#
# shellcheck disable=SC2015
# `A && B || C` is used throughout as pass/fail/info reporting, never as
# if/then/else control flow: pass()/fail()/info()/say() are printf wrappers
# that always return 0, so B never fails in a way that would let C run
# unintentionally.
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
    --old)          [[ $# -ge 2 ]] || die "missing value for $1"; OLD_NAME="$2"; shift;;
    --new)          [[ $# -ge 2 ]] || die "missing value for $1"; NEW_NAME="$2"; shift;;
    --org-dir)      [[ $# -ge 2 ]] || die "missing value for $1"; ORG_DIR="${2/#\~/$HOME}"; shift;;
    --remote-url)   [[ $# -ge 2 ]] || die "missing value for $1"; REMOTE_URL="$2"; shift;;
    --catalyst-dir) [[ $# -ge 2 ]] || die "missing value for $1"; CATALYST_DIR="${2/#\~/$HOME}"; shift;;
    --scan-root)    [[ $# -ge 2 ]] || die "missing value for $1"; SCAN_ROOTS+=("${2/#\~/$HOME}"); shift;;
    --allow-tooling-hits) ALLOW_TOOLING_HITS=1;;
    -h|--help)
      sed -n '2,19p' "$0" | sed 's/^# \{0,1\}//'
      cat <<'USAGE'

Flags:
  --check                read-only audit; mutates nothing
  --apply                perform the migration, then run the same verification
  --old NAME             legacy directory name (default: skimmer)
  --new NAME             target directory name (default: manta)
  --org-dir DIR          parent directory holding the clone
                         (default: ~/code-repos/github/HagaleTechnologies)
  --remote-url URL       desired origin URL
                         (default: https://github.com/HagaleTechnologies/manta.git)
  --catalyst-dir DIR     where execution-core/registry.json lives
                         (default: ~/catalyst, or $CATALYST_DIR)
  --scan-root DIR        add DIR to the tooling-preflight scan roots
                         (repeatable; default roots: ~/code-repos/github,
                         ~/bin, ~/.local/bin)
  --allow-tooling-hits   proceed with --apply despite tooling-preflight hits
  -h, --help             show this help
USAGE
      exit 0;;
    *) die "unknown argument: $1";;
  esac
  shift
done
[[ -n $MODE ]] || die "one of --check or --apply is required"

# Normalize before deriving the path variables below: a trailing slash (what
# shell tab-completion produces) or a symlinked path component (macOS
# /tmp -> /private/tmp, $TMPDIR -> /private/var/..., or any host where
# ~/code-repos sits on a symlinked volume) otherwise breaks the string-prefix
# matching against paths `git worktree list` reports canonically (C-2).
# Kept alongside the canonicalized form for the tooling preflight (F6): a tool
# that hardcodes the $HOME-spelled path (never having resolved the symlink
# itself) won't contain the canonical spelling, so both must be scanned for.
RAW_ORG_DIR="${ORG_DIR%/}"
if [[ -d $ORG_DIR ]]; then
  ORG_DIR="$(cd "$ORG_DIR" && pwd -P)"
else
  ORG_DIR="${ORG_DIR%/}"
fi

OLD_MAIN="$ORG_DIR/$OLD_NAME"
NEW_MAIN="$ORG_DIR/$NEW_NAME"
OLD_WTP="$ORG_DIR/${OLD_NAME}-worktrees"
NEW_WTP="$ORG_DIR/${NEW_NAME}-worktrees"
RAW_OLD_MAIN="$RAW_ORG_DIR/$OLD_NAME"

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
  local baseline="${1:-}" line path flag unhealthy=0 mainpath candidate root reg bn all_paths
  mainpath="$(git -C "$MAIN" rev-parse --show-toplevel)"
  all_paths="$(inventory | cut -f1)"

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
    # `git worktree list` reports the path git has recorded for this worktree,
    # which is stale if a host was renamed by hand (`mv` on both the checkout and
    # the worktree parent) without `git worktree repair`: the recorded path no
    # longer exists, so it is reported prunable even though the worktree is alive
    # and well at its corresponding new-named path. Treating that as benign
    # "pre-existing" cruft is the false-all-PASS this check exists to catch — a
    # candidate directory that actually exists on disk under the new worktree
    # parent is the thing to check for health, not the stale recorded path.
    candidate="$path"
    if [[ ! -e $path && $path == "$OLD_WTP"/* ]]; then
      candidate="$NEW_WTP/${path#"$OLD_WTP"/}"
    elif [[ ! -e $path && $path == "$OLD_MAIN"/* ]]; then
      candidate="$NEW_MAIN/${path#"$OLD_MAIN"/}"
    fi
    # C-3: even when the prefix remap above misses — a path shape it does not
    # know about — a prunable entry whose recorded directory is gone but whose
    # basename reappears live under the new worktree parent is still a fresh
    # regression, not baseline cruft. This check is independent of OLD_WTP/
    # OLD_MAIN prefix math on purpose, so it still catches the regression even
    # if ORG_DIR normalization above ever falls short of some other host's
    # path shape.
    # F3: only take the basename fallback when the candidate isn't itself a
    # separate, already-recorded worktree — otherwise a dead entry sharing a
    # basename with an unrelated, distinct live worktree gets misreported as
    # that live worktree's "unrepaired" stale copy.
    if [[ $candidate == "$path" && ! -e $path ]]; then
      bn="$(basename -- "$path")"
      if [[ -d "$NEW_WTP/$bn" ]] && ! grep -qxF "$NEW_WTP/$bn" <<<"$all_paths"; then
        candidate="$NEW_WTP/$bn"
      fi
    fi
    if [[ $flag == prunable ]]; then
      if [[ $candidate != "$path" && -d $candidate ]]; then
        fail "worktree recorded as prunable at stale path $path, but $candidate exists on disk and was not repaired (run: git -C $MAIN worktree repair $candidate)"
        unhealthy=1
      elif grep -qxF "$path" <<<"$baseline"; then
        info "pre-existing prunable worktree (unrelated to this rename): $path"
      else
        fail "worktree is prunable after the move: $path"
        unhealthy=1
      fi
    fi
    # Gherkin 2, part B: the check `worktree list` CANNOT make (KD 2).
    [[ $candidate == "$mainpath" ]] && continue
    if [[ -d $candidate ]] && ! git -C "$candidate" rev-parse --git-dir >/dev/null 2>&1; then
      fail "unhealthy worktree — its .git file points at a stale location: $candidate"
      unhealthy=1
    fi
  done <<<"$(inventory)"
  [[ $unhealthy -eq 0 ]] && pass "every linked worktree resolves its git dir"

  # Desired End State: registry.json's MAN entry, when present, must resolve to
  # the new checkout. Guarded on jq (the only tool that can parse it); downgrade
  # to informational rather than passing silently when it cannot be confirmed, so
  # a run never ends all-PASS with a dangling registry entry (C4).
  reg="$CATALYST_DIR/execution-core/registry.json"
  if [[ ! -f $reg ]]; then
    info "no registry.json at $reg — nothing to verify there"
  elif ! command -v jq >/dev/null 2>&1; then
    info "jq not installed — cannot verify registry.json's '$TEAM_NEW' entry"
  elif ! jq -e . "$reg" >/dev/null 2>&1; then
    info "registry.json at $reg does not parse as JSON — cannot verify the '$TEAM_NEW' entry"
  else
    root="$(jq -r --arg team "$TEAM_NEW" '[.projects[]? | select(.team==$team) | .repoRoot][0] // empty' "$reg" 2>/dev/null)"
    if [[ -z $root ]]; then
      fail "registry.json has no '$TEAM_NEW' entry (want repoRoot $NEW_MAIN)"
    elif [[ $root != "$NEW_MAIN" ]]; then
      fail "registry.json '$TEAM_NEW' repoRoot = $root (want $NEW_MAIN)"
    else
      pass "registry.json '$TEAM_NEW' repoRoot = $NEW_MAIN"
    fi
  fi
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
  # --scan-root ADDS to the default roots rather than replacing them (C-5) —
  # the runbook tells operators to point it at wherever magazzino or
  # link-build-cache.sh live "if they're outside the default roots", which
  # only makes sense as a widening of the scan, not a narrowing of it. A root
  # that does not exist or is not readable is reported, not silently dropped:
  # grep would otherwise exit 2 for it and the caller's `|| true` below would
  # swallow that with no trace (also C-5).
  local roots=() r
  while IFS= read -r r; do [[ -d $r ]] && roots+=("$r"); done < <(default_scan_roots)
  if [[ ${#SCAN_ROOTS[@]} -gt 0 ]]; then
    for r in "${SCAN_ROOTS[@]}"; do
      if [[ -d $r && -r $r ]]; then
        roots+=("$r")
      else
        say "warning: --scan-root '$r' does not exist or is not readable — skipping it" >&2
      fi
    done
  fi
  [[ ${#roots[@]} -eq 0 ]] && return 0
  # Literal old checkout path, and the directory name as a path segment. Skip:
  #   - .git dirs and the checkout/worktree-parent trees themselves (their own
  #     history, and a linked worktree's *own* copy of this script/tests,
  #     legitimately mention the old name — C3);
  #   - files literally named `.git` (a linked worktree's gitdir pointer file
  #     necessarily contains `gitdir: <OLD_MAIN>/.git/worktrees/<id>` by
  #     construction, and that is not "tooling hardcoding a path" — C2).
  # `-R` (not `-r`) so a symlinked file — e.g. a stow/dotfiles-style `~/bin`
  # symlink farm, the layout link-build-cache.sh normally lives in — is
  # actually followed and scanned rather than silently skipped (C-4).
  #
  # F6: scan for BOTH the canonicalized $OLD_MAIN and its raw, $HOME-spelled
  # form. On a host where `--org-dir` (or its default) sits on a symlinked
  # volume, a tool that hardcodes the un-canonicalized spelling — the form a
  # human would actually type — never contains the canonical one, so scanning
  # only for $OLD_MAIN reports "no hits" when it should report "cannot tell".
  grep -RIn --exclude-dir=.git --exclude=.git \
       --exclude-dir=node_modules --exclude-dir=target \
       -e "$OLD_MAIN" -e "$RAW_OLD_MAIN" -e "/${OLD_NAME}-worktrees" \
       "${roots[@]}" 2>/dev/null \
    | grep -v "^${OLD_MAIN}/" \
    | grep -v "^${NEW_MAIN}/" \
    | grep -v "^${OLD_WTP}/" \
    | grep -v "^${NEW_WTP}/" \
    | grep -v "^${RAW_OLD_MAIN}/" || true
}

# On an already-migrated host nothing is going to move, so a stray hardcoded
# reference elsewhere on the host is not this run's concern — the preflight
# exists to protect a MOVE, and forcing it to gate the apply-is-a-no-op path too
# broke the documented idempotent no-op (C3). `--check` still reports hits
# unconditionally, since it never moves anything and the report is informational.
if [[ $MODE == apply && $STATE == migrated ]]; then
  HITS=""
else
  HITS="$(tooling_hits)"
  if [[ -n $HITS ]]; then
    say "tooling reference(s) to the legacy path found:"
    printf '%s\n' "$HITS" | sed 's/^/  /' | head -50
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
  # `ps -Ao pid=,args=` is portable to both GNU and BSD/macOS ps; `pgrep -af`
  # is procps-only and macOS's pgrep has no `-a` (CR-2) — with stderr silenced
  # it would just return nothing there, quietly dropping this half of the
  # preflight on exactly the hosts the plan flags as macOS candidates. Filter
  # in bash (quoted case pattern, not a grep subprocess) so the filter itself
  # never becomes a process whose own argv contains $OLD_MAIN and matches.
  if command -v ps >/dev/null 2>&1; then
    while IFS= read -r procline; do
      case "$procline" in
        *fleet-rename-checkout*) ;;
        *"$OLD_MAIN"*) printf 'process referencing the checkout: %s\n' "$procline";;
      esac
    done < <(ps -Ao pid=,args= 2>/dev/null)
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
  # Explicit template: bare `mktemp`/`mktemp -d` is a GNU-coreutils-only
  # extension. BSD/macOS mktemp requires a template operand and exits 1 with a
  # usage error without one (CR-1) — which on a fleet host would abort this
  # function with a raw shell error instead of the degrade-to-warning message
  # every other failure path here uses.
  if ! tmp="$(mktemp "${TMPDIR:-/tmp}/fleet-rename-registry.XXXXXX")"; then
    info "mktemp failed — skipping registry reconciliation."
    info "  Edit $reg by hand so team \"$TEAM_NEW\" has repoRoot \"$NEW_MAIN\"."
    return 0
  fi
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
    # C-1: this branch used to only call verify() and exit, so the one fleet
    # state the ticket itself describes — "the git remote was repointed at
    # the new URL on one machine only" (i.e. directory already renamed by
    # hand, but worktrees/origin/registry left stale) — could never be
    # remediated by --apply; it just reported the same FAIL lines forever.
    # Reconcile in place instead: nothing needs to *move* (both directories
    # are already at their new names by definition of STATE=migrated), but
    # worktrees can still be registered at a stale legacy path, origin can
    # still point at the old URL, and the registry can still be stale.
    say "checkout is already named '$NEW_NAME'; reconciling any stale worktrees/origin/registry."

    busy="$(busy_reasons)"
    [[ -n $busy ]] && die "checkout is in use; refusing to touch it.
$busy
     Stop the Catalyst daemon and any open session on this checkout, then re-run."

    # F1: capture the prunable baseline before touching anything below, so
    # pre-existing cruft (KD 4 / ADR Decision 3) stays informational in
    # verify() instead of permanently failing a host that carries any.
    BASELINE="$(inventory | awk -F'\t' '$2=="prunable"{print $1}')"

    # F2: a host where only the main clone was hand-renamed leaves $OLD_WTP
    # behind forever otherwise — move it now, before repairing worktrees,
    # guarded the same way the legacy-apply path guards against merging two
    # worktree parents.
    if [[ -d $OLD_WTP ]]; then
      if [[ -e $NEW_WTP ]]; then
        die "both '${OLD_NAME}-worktrees' and '${NEW_NAME}-worktrees' exist under $ORG_DIR.
     Refusing to merge two worktree parents. Resolve by hand, then re-run."
      fi
      say "moving $OLD_WTP -> $NEW_WTP"
      mv "$OLD_WTP" "$NEW_WTP" || die "mv of the worktree parent failed"
    fi

    all_paths="$(inventory | cut -f1)"
    REPAIR=()
    while IFS=$'\t' read -r p _; do
      [[ -z $p ]] && continue
      cand="$p"
      if [[ ! -e $p && $p == "$OLD_WTP"/* ]]; then
        cand="$NEW_WTP/${p#"$OLD_WTP"/}"
      elif [[ ! -e $p && $p == "$OLD_MAIN"/* ]]; then
        cand="$NEW_MAIN/${p#"$OLD_MAIN"/}"
      fi
      # F3: same collision guard as verify() — don't treat an unrelated, live,
      # already-recorded worktree as this entry's repaired counterpart.
      if [[ $cand == "$p" && ! -e $p ]]; then
        bn="$(basename -- "$p")"
        if [[ -d "$NEW_WTP/$bn" ]] && ! grep -qxF "$NEW_WTP/$bn" <<<"$all_paths"; then
          cand="$NEW_WTP/$bn"
        fi
      fi
      [[ $cand != "$p" && -d $cand ]] && REPAIR+=("$cand")
    done <<<"$(inventory)"

    if [[ ${#REPAIR[@]} -gt 0 ]]; then
      say "repairing ${#REPAIR[@]} linked worktree(s) recorded at a stale legacy path"
      git -C "$MAIN" worktree repair "${REPAIR[@]}" || \
        info "git worktree repair reported a non-zero status; the verifier below is authoritative"
    else
      info "no linked worktrees recorded at a stale legacy path"
    fi

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
    [[ $FAILED -eq 0 ]] && { say "VERDICT: ALREADY MIGRATED — all checks pass."; exit 0; }
    say "VERDICT: MIGRATION INCOMPLETE — see FAIL lines above."; exit 1
  fi

  busy="$(busy_reasons)"
  [[ -n $busy ]] && die "checkout is in use; refusing to move.
$busy
     Stop the Catalyst daemon and any open session on this checkout, then re-run."

  # F4: only refuse when there's actually something to merge — a host with a
  # destination worktree parent but no legacy one (nothing under $OLD_MAIN
  # ever had linked worktrees) has one worktree parent, not two, and the move
  # below is safe.
  if [[ -d $OLD_WTP && -e $NEW_WTP ]]; then
    die "both '${OLD_NAME}-worktrees' and '${NEW_NAME}-worktrees' exist under $ORG_DIR.
     Refusing to merge two worktree parents. Resolve by hand, then re-run."
  fi

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
