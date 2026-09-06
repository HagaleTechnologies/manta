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
EXPLICIT_REMOTE_URL=0

die()  { printf 'ERROR: %s\n' "$*" >&2; exit 2; }
say()  { printf '%s\n' "$*"; }
pass() { printf 'PASS  %s\n' "$*"; }
fail() { printf 'FAIL  %s\n' "$*"; FAILED=1; }
info() { printf 'info  %s\n' "$*"; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --check|--apply)
      # C1 (validation round): --check and --apply used to share this one
      # case arm with no mutual-exclusion guard, so the mode was last-wins —
      # `--check --apply` silently performed the migration under a flag
      # whose entire safety story, in this script's own header, the
      # runbook, and the ADR, is "--check mutates nothing". Refuse instead
      # of picking a winner.
      NEW_MODE="${1#--}"
      [[ -n $MODE && $MODE != "$NEW_MODE" ]] && die "--check and --apply are mutually exclusive; pass exactly one"
      MODE="$NEW_MODE"
      ;;
    --old)          [[ $# -ge 2 ]] || die "missing value for $1"; OLD_NAME="$2"; shift;;
    --new)          [[ $# -ge 2 ]] || die "missing value for $1"; NEW_NAME="$2"; shift;;
    --org-dir)      [[ $# -ge 2 ]] || die "missing value for $1"; ORG_DIR="${2/#\~/$HOME}"; shift;;
    --remote-url)   [[ $# -ge 2 ]] || die "missing value for $1"; REMOTE_URL="$2"; EXPLICIT_REMOTE_URL=1; shift;;
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
# V-2: raw ($HOME-spelled, pre-canonicalization) counterparts of NEW_MAIN,
# OLD_WTP and NEW_WTP too — a scan root under a symlinked path component (the
# case the comment above already handles for OLD_MAIN) makes grep emit
# raw-spelled lines for ANY of the four directories, not just the old main
# checkout, so all four need a raw exclusion below or the checkout's own
# worktree files become false-positive "tooling hits".
RAW_NEW_MAIN="$RAW_ORG_DIR/$NEW_NAME"
RAW_OLD_WTP="$RAW_ORG_DIR/${OLD_NAME}-worktrees"
RAW_NEW_WTP="$RAW_ORG_DIR/${NEW_NAME}-worktrees"

# CR-2: a shell script a human actually writes hardcodes the checkout as
# either the literal characters `$HOME/…` (unexpanded, e.g.
# CACHE_SRC="$HOME/code-repos/.../skimmer") or a literal `~/…` — neither
# spelling appears as a substring of $OLD_MAIN or $RAW_OLD_MAIN, which are
# always the shell-expanded absolute path, so the tooling preflight never
# saw either of the two forms a human is most likely to write. Derive both
# literal spellings when the org dir sits under $HOME so tooling_hits() can
# scan for them too.
HOME_OLD_MAIN=""
TILDE_OLD_MAIN=""
if [[ $RAW_ORG_DIR == "$HOME" || $RAW_ORG_DIR == "$HOME"/* ]]; then
  HOME_OLD_MAIN="\$HOME${RAW_ORG_DIR#"$HOME"}/$OLD_NAME"
  TILDE_OLD_MAIN="~${RAW_ORG_DIR#"$HOME"}/$OLD_NAME"
fi

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
# C3 (validation round): classification and the move code paths tested
# $OLD_MAIN/$OLD_WTP with `-d`, while verify() tests the very same paths
# with `-e`. A stray FILE (not a directory) at any of these four paths was
# therefore invisible here — classification proceeded as if it did not
# exist — but failed verify() on every subsequent run with no way to ever
# converge, since nothing moves or removes a thing classification never
# saw. Refuse loudly instead, the same "needs a human" shape already used
# for both-present below, before STATE is ever derived.
for stray in "$OLD_MAIN" "$NEW_MAIN" "$OLD_WTP" "$NEW_WTP"; do
  [[ -e $stray && ! -d $stray ]] && die "$stray exists but is not a directory — a human must remove or rename it before this tool can proceed."
done
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
# CR-1 (validation round): `rev-parse --git-dir` succeeds by walking
# UPWARD to find an enclosing repository, so on a git-managed ancestor of
# --org-dir (e.g. a yadm/dotfiles-managed $HOME) it silently returns that
# enclosing repo's git dir instead of failing — every subsequent
# `git -C "$MAIN" …` (worktree inventory, worktree repair, set_origin) then
# operates on the wrong repository. Compare --show-toplevel against $MAIN
# itself instead: that only ever reports $MAIN when $MAIN is truly its own
# repository's root. Canonicalize $MAIN the same way ORG_DIR already is
# (cd && pwd -P) so a symlinked path component does not produce a spurious
# mismatch against git's own canonicalized --show-toplevel output.
MAIN_TOPLEVEL="$(git -C "$MAIN" rev-parse --show-toplevel 2>/dev/null)" \
  || die "$MAIN exists but is not a git repository"
MAIN_REAL="$MAIN"
[[ -d $MAIN ]] && MAIN_REAL="$(cd "$MAIN" && pwd -P)"
[[ $MAIN_TOPLEVEL == "$MAIN_REAL" ]] \
  || die "$MAIN exists but is not a git repository (found enclosing repository at $MAIN_TOPLEVEL instead)"

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

# C2 (validation round): the basename-collision remap below used to accept
# any directory at the candidate path that merely HAD a .git and was not
# already in this repo's own worktree list — it never checked that .git
# belonged to $MAIN at all. A foreign worktree (of some unrelated repo)
# parked at the colliding basename was therefore adopted as the dead
# entry's "repaired" counterpart: `git worktree repair` correctly declines
# to rewrite a foreign .git, so the host can never reach an all-PASS state,
# and the FAIL line's suggested remedy is a command that does nothing.
# Compare git-common-dir (the shared .git backing every worktree of one
# repo) rather than a mere is-there-a-.git existence check.
worktree_belongs_to_main() {
  local d="$1" main_gcd cand_gcd
  main_gcd="$(git -C "$MAIN" rev-parse --path-format=absolute --git-common-dir 2>/dev/null)" \
    || main_gcd="$(git -C "$MAIN" rev-parse --git-common-dir 2>/dev/null)"
  [[ -n $main_gcd ]] || return 1
  [[ $main_gcd == /* ]] || main_gcd="$MAIN/$main_gcd"
  main_gcd="$(cd "$main_gcd" 2>/dev/null && pwd -P)" || return 1
  cand_gcd="$(git -C "$d" rev-parse --path-format=absolute --git-common-dir 2>/dev/null)" \
    || cand_gcd="$(git -C "$d" rev-parse --git-common-dir 2>/dev/null)"
  [[ -n $cand_gcd ]] || return 1
  [[ $cand_gcd == /* ]] || cand_gcd="$d/$cand_gcd"
  cand_gcd="$(cd "$cand_gcd" 2>/dev/null && pwd -P)" || return 1
  [[ $main_gcd == "$cand_gcd" ]]
}

FAILED=0
REGISTRY_UNVERIFIED=0

# CR-4: a plain "all checks pass" verdict line is what the runbook's
# acceptance step tells an operator to look for — printing that exact line
# when the registry.json check never actually ran (jq absent, or the file
# didn't parse) makes a dangling registry entry indistinguishable from a
# genuinely clean host. Route every "all clear" verdict through here so the
# distinction always surfaces.
report_pass_verdict() {
  local label="$1"
  if [[ $REGISTRY_UNVERIFIED -eq 1 ]]; then
    say "VERDICT: $label (registry.json's '$TEAM_NEW' entry NOT verified — see the info line above; this is not the same as confirmed clean) — all other checks pass."
  else
    say "VERDICT: $label — all checks pass."
  fi
}

# ---- verify (shared by --check and the tail of --apply) --------------------
# $1 = newline-separated baseline of paths that were ALREADY prunable before any
#      move (empty for --check).
verify() {
  local baseline="${1:-}" line path flag unhealthy=0 mainpath candidate root root_canon reg bn all_paths
  mainpath="$(git -C "$MAIN" rev-parse --show-toplevel)"
  all_paths="$(inventory | cut -f1)"

  # Gherkin 1: directory named <new> present, no <old> directory remains.
  [[ -d $NEW_MAIN ]] && pass "directory '$NEW_NAME' present in $ORG_DIR" \
                     || fail "directory '$NEW_NAME' absent in $ORG_DIR"
  [[ -e $OLD_MAIN ]] && fail "legacy '$OLD_NAME' directory still present: $OLD_MAIN" \
                     || pass "no '$OLD_NAME' directory remains in $ORG_DIR"
  [[ -e $OLD_WTP ]]  && fail "legacy worktree parent still present: $OLD_WTP" \
                     || pass "no '${OLD_NAME}-worktrees' directory remains"

  # Gherkin 1: origin points at the new URL. CR-3: accept any origin URL that
  # resolves to the same host/org/repo as $REMOTE_URL (SSH or HTTPS) — the
  # ticket's own Gherkin only requires "points at HagaleTechnologies/manta",
  # not a specific protocol, and a byte-exact HTTPS-only comparison reports a
  # healthy SSH-origin clone as MIGRATION NEEDED.
  local url; url="$(git -C "$MAIN" remote get-url origin 2>/dev/null || echo '<none>')"
  if [[ $url == '<none>' ]]; then
    fail "origin = <none> (want $REMOTE_URL)"
  elif [[ "$(normalize_origin "$url")" == "$(normalize_origin "$REMOTE_URL")" ]]; then
    pass "origin = $url"
  else
    fail "origin = $url (want $REMOTE_URL)"
  fi

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
    # V-1: also require that the candidate actually IS a worktree (has a
    # .git of its own), not merely a same-named plain directory that happens
    # to sit under the new worktree parent — otherwise the script hands git a
    # directory with no .git to repair, which fails and can never converge.
    # C2 (validation round): "has a .git" alone accepted a FOREIGN worktree
    # (of some unrelated repo) parked at the colliding basename — require it
    # to actually be a worktree of $MAIN, or `git worktree repair` correctly
    # declines it and the host can never converge.
    if [[ $candidate == "$path" && ! -e $path ]]; then
      bn="$(basename -- "$path")"
      if [[ -d "$NEW_WTP/$bn" && -e "$NEW_WTP/$bn/.git" ]] \
         && ! grep -qxF "$NEW_WTP/$bn" <<<"$all_paths" \
         && worktree_belongs_to_main "$NEW_WTP/$bn"; then
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
  #
  # CR-4: "informational" here still let the run's final VERDICT line print
  # bare "all checks pass" — indistinguishable from a host where the registry
  # was actually confirmed clean. Flag REGISTRY_UNVERIFIED so callers can
  # print a verdict that says the registry line never actually ran.
  REGISTRY_UNVERIFIED=0
  reg="$CATALYST_DIR/execution-core/registry.json"
  if [[ ! -f $reg ]]; then
    info "no registry.json at $reg — nothing to verify there"
  elif ! command -v jq >/dev/null 2>&1; then
    info "jq not installed — cannot verify registry.json's '$TEAM_NEW' entry"
    REGISTRY_UNVERIFIED=1
  elif ! jq -e . "$reg" >/dev/null 2>&1; then
    info "registry.json at $reg does not parse as JSON — cannot verify the '$TEAM_NEW' entry"
    REGISTRY_UNVERIFIED=1
  else
    root="$(jq -r --arg team "$TEAM_NEW" '[.projects[]? | select(.team==$team) | .repoRoot][0] // empty' "$reg" 2>/dev/null)"
    if [[ -z $root ]]; then
      fail "registry.json has no '$TEAM_NEW' entry (want repoRoot $NEW_MAIN)"
    else
      # V-6: accept either spelling. A host whose org dir sits behind a
      # symlink (F6) can have a correct, host-native entry written in
      # either the canonicalized form ($NEW_MAIN) or the raw, $HOME-spelled
      # form ($RAW_NEW_MAIN) — comparing canonical-only reports a false
      # "MIGRATION NEEDED" on a healthy host. Also canonicalize $root itself
      # when it resolves on this host, so a THIRD spelling that happens to
      # point at the same real directory is accepted too.
      root_canon="$root"
      [[ -d $root ]] && root_canon="$(cd "$root" && pwd -P)"
      if [[ $root == "$NEW_MAIN" || $root == "$RAW_NEW_MAIN" || $root_canon == "$NEW_MAIN" ]]; then
        pass "registry.json '$TEAM_NEW' repoRoot = $root"
      else
        fail "registry.json '$TEAM_NEW' repoRoot = $root (want $NEW_MAIN)"
      fi
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
  while IFS= read -r r; do
    # CR-3 (validation round): default roots were only checked with `-d`,
    # unlike --scan-root's `-d && -r` — an existing-but-unreadable default
    # root used to enter roots[] and have grep's permission error swallowed
    # by tooling_hits()'s own `2>/dev/null` and trailing `|| true`, reporting
    # a silent clean scan indistinguishable from one that actually ran.
    if [[ ! -e $r ]]; then
      # F7: a missing DEFAULT root used to be dropped with no trace at all,
      # indistinguishable downstream from "scanned it, found nothing" —
      # warn here the same way a missing --scan-root already does below.
      say "warning: default scan root '$r' does not exist — skipping it" >&2
    elif [[ ! -r $r ]]; then
      say "warning: default scan root '$r' exists but is not readable — skipping it" >&2
    else
      roots+=("$r")
    fi
  done < <(default_scan_roots)
  if [[ ${#SCAN_ROOTS[@]} -gt 0 ]]; then
    for r in "${SCAN_ROOTS[@]}"; do
      # CR-3: accept a FILE, not just a directory — the runbook tells
      # operators to point --scan-root at wherever magazzino or
      # link-build-cache.sh live, and grep handles a file operand fine.
      # Report "does not exist" and "not readable" as distinct messages
      # (previously one combined message covered both).
      if [[ ! -e $r ]]; then
        say "warning: --scan-root '$r' does not exist — skipping it" >&2
      elif [[ ! -r $r ]]; then
        say "warning: --scan-root '$r' exists but is not readable — skipping it" >&2
      else
        roots+=("$r")
      fi
    done
  fi
  if [[ ${#roots[@]} -eq 0 ]]; then
    # F7: an empty effective root set used to `return 0` exactly like a
    # clean scan of real roots — "scanned nothing" and "scanned everything,
    # found nothing" were the same string to the caller, silently voiding
    # the ticket's tooling-preflight instruction (ADR Decision 6) whenever
    # every default and every --scan-root was absent. Loudly warn instead of
    # returning silently so an operator or the runbook's evidence table
    # cannot mistake this for a clean scan.
    say "warning: no tooling-preflight scan roots exist or are readable — nothing was checked for hardcoded legacy-path references (this is NOT the same as 'checked and found none')" >&2
    return 0
  fi
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
  #
  # CR-2: also scan for the literal `$HOME/…` and `~/…` spellings — the two
  # forms a human actually writes by hand in a shell script — which match
  # neither $OLD_MAIN nor $RAW_OLD_MAIN because both of those are always
  # already shell-expanded to a real path.
  local patterns=(-e "$OLD_MAIN" -e "$RAW_OLD_MAIN" -e "/${OLD_NAME}-worktrees")
  [[ -n $HOME_OLD_MAIN  ]] && patterns+=(-e "$HOME_OLD_MAIN")
  [[ -n $TILDE_OLD_MAIN ]] && patterns+=(-e "$TILDE_OLD_MAIN")
  grep -RIn --exclude-dir=.git --exclude=.git \
       --exclude-dir=node_modules --exclude-dir=target \
       "${patterns[@]}" \
       "${roots[@]}" 2>/dev/null \
    | grep -v "^${OLD_MAIN}/" \
    | grep -v "^${NEW_MAIN}/" \
    | grep -v "^${OLD_WTP}/" \
    | grep -v "^${NEW_WTP}/" \
    | grep -v "^${RAW_OLD_MAIN}/" \
    | grep -v "^${RAW_NEW_MAIN}/" \
    | grep -v "^${RAW_OLD_WTP}/" \
    | grep -v "^${RAW_NEW_WTP}/" || true
}

# On an already-migrated host with no legacy worktree parent left over,
# nothing is going to move, so a stray hardcoded reference elsewhere on the
# host is not this run's concern — the preflight exists to protect a MOVE,
# and forcing it to gate the apply-is-a-no-op path too broke the documented
# idempotent no-op (C3). But a migrated host CAN still have a move pending:
# the migrated-apply branch below moves $OLD_WTP -> $NEW_WTP when a legacy
# worktree parent is left over from a hand-rename (F2), so the skip must be
# conditioned on there being no such move, not on STATE==migrated alone
# (V-3). `--check` still reports hits unconditionally, since it never moves
# anything and the report is informational.
if [[ $MODE == apply && $STATE == migrated && ! -d $OLD_WTP ]]; then
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

# ---- origin -----------------------------------------------------------------
# CR-3: normalize protocol/user/.git-suffix away so an SSH origin
# (git@github.com:org/repo(.git)?) is recognized as equivalent to the HTTPS
# default rather than being flagged as drift or silently rewritten.
normalize_origin() {
  local u="${1%.git}" host path
  case "$u" in
    git@*:*)
      u="${u#git@}"
      host="${u%%:*}"
      path="${u#*:}"
      u="$host/$path"
      ;;
    ssh://git@*)  u="${u#ssh://git@}";;
    https://*)    u="${u#https://}";;
    http://*)     u="${u#http://}";;
  esac
  printf '%s' "$u"
}
# CR-1/CR-3: `git remote set-url` cannot create a remote that does not exist —
# on a checkout with no origin it dies with "No such remote 'origin'" after
# the directories have already moved, leaving the host half-migrated and
# every subsequent --apply failing identically (never converging). Add the
# remote when absent instead. CR-3: when origin already resolves to the same
# repo as $REMOTE_URL but via a different protocol (SSH vs HTTPS), leave it
# alone — rewriting it silently changes push authentication — unless the
# operator explicitly asked for this exact URL via --remote-url.
set_origin() {
  local cur
  if cur="$(git -C "$MAIN" remote get-url origin 2>/dev/null)"; then
    if [[ $cur == "$REMOTE_URL" ]]; then
      info "origin already correct"
    elif [[ "$(normalize_origin "$cur")" == "$(normalize_origin "$REMOTE_URL")" ]]; then
      if [[ $EXPLICIT_REMOTE_URL -eq 1 ]]; then
        say "repointing origin: $cur -> $REMOTE_URL (explicit --remote-url)"
        git -C "$MAIN" remote set-url origin "$REMOTE_URL" || die "git remote set-url failed"
      else
        info "origin already resolves to the right repo ($cur) — leaving its protocol alone; pass --remote-url to force $REMOTE_URL"
      fi
    else
      say "repointing origin: $cur -> $REMOTE_URL"
      git -C "$MAIN" remote set-url origin "$REMOTE_URL" || die "git remote set-url failed"
    fi
  else
    say "adding origin: <none> -> $REMOTE_URL"
    git -C "$MAIN" remote add origin "$REMOTE_URL" || die "git remote add failed"
  fi
}

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
  # V-4: the template lives in $reg's OWN directory (not $TMPDIR), so the
  # final `mv` below is a same-filesystem rename (atomic) instead of a
  # cross-filesystem copy. mktemp also always creates the file mode 0600 —
  # capture the registry's real mode first and restore it on the temp file
  # before the swap, or the `mv` silently narrows registry.json's
  # permissions and a daemon running under a different uid loses read access.
  if ! tmp="$(mktemp "${reg}.XXXXXX")"; then
    info "mktemp failed — skipping registry reconciliation."
    info "  Edit $reg by hand so team \"$TEAM_NEW\" has repoRoot \"$NEW_MAIN\"."
    return 0
  fi
  local reg_mode
  reg_mode="$(stat -c '%a' "$reg" 2>/dev/null || stat -f '%Lp' "$reg" 2>/dev/null || true)"
  [[ -n $reg_mode ]] && chmod "$reg_mode" "$tmp" 2>/dev/null
  # Correct an existing MAN entry in place, or append one. A stale SKI entry is
  # left alone on purpose: checkout-sync.mjs drops entries whose repoRoot is
  # missing (CTL-854), so it is already inert, and removing registry rows is a
  # wider change than MAN-27 needs.
  # V-6: deliberately write the canonicalized spelling ($NEW_MAIN), not the
  # raw $HOME-spelled one — it is unambiguous across a symlinked org dir,
  # and verify()'s own read side now accepts either spelling, so a
  # pre-existing entry in the raw form is never falsely flagged as drift.
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
      # V-1: also require the candidate to actually be a worktree of its own
      # (has a .git), not just a plain directory sharing the basename.
      # C2 (validation round): same fix as verify() — require the candidate
      # to be a worktree of $MAIN specifically, not merely any repo's .git.
      if [[ $cand == "$p" && ! -e $p ]]; then
        bn="$(basename -- "$p")"
        if [[ -d "$NEW_WTP/$bn" && -e "$NEW_WTP/$bn/.git" ]] \
           && ! grep -qxF "$NEW_WTP/$bn" <<<"$all_paths" \
           && worktree_belongs_to_main "$NEW_WTP/$bn"; then
          cand="$NEW_WTP/$bn"
        fi
      fi
      if [[ $cand != "$p" && -d $cand ]]; then
        REPAIR+=("$cand")
      elif [[ -d $p ]] && ! git -C "$p" rev-parse --git-dir >/dev/null 2>&1; then
        # V-5: a worktree living OUTSIDE org-dir entirely (its own directory
        # never moved, e.g. the ~/catalyst/wt/<key>/<ticket> shape) keeps its
        # RECORDED path unchanged when only the main clone is hand-renamed —
        # cand stays == p, so the remap above never queues it — even though
        # its .git file still targets $OLD_MAIN's now-nonexistent
        # .git/worktrees dir. Queue it directly whenever it exists on disk
        # but fails to resolve its own git-dir, regardless of path remapping.
        REPAIR+=("$p")
      fi
    done <<<"$(inventory)"

    if [[ ${#REPAIR[@]} -gt 0 ]]; then
      say "repairing ${#REPAIR[@]} linked worktree(s) recorded at a stale legacy path"
      git -C "$MAIN" worktree repair "${REPAIR[@]}" || \
        info "git worktree repair reported a non-zero status; the verifier below is authoritative"
    else
      info "no linked worktrees recorded at a stale legacy path"
    fi

    set_origin

    reconcile_registry

    say ""
    verify "$BASELINE"
    say ""
    [[ $FAILED -eq 0 ]] && { report_pass_verdict "ALREADY MIGRATED"; exit 0; }
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

  # One set_origin call covers every worktree — they share the main checkout's
  # config (measured).
  set_origin

  reconcile_registry

  say ""
  verify "$BASELINE"
  say ""
  [[ $FAILED -eq 0 ]] && { report_pass_verdict "MIGRATED"; exit 0; }
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
  report_pass_verdict "ALREADY MIGRATED"
  exit 0
fi
