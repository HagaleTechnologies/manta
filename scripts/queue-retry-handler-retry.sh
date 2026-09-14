#!/usr/bin/env bash
set -eo pipefail

# Shared by both fail-closed branches below: a bare needs-human POST with `|| true`
# would let this whole step report success even when the ONE signal meant to get a
# human's attention never landed (label deleted, transient API failure). Verify it, and
# propagate failure via this function's own exit status so each call site can fail the
# step -- mark-handled's gate on `steps.retry.conclusion == 'success'` then correctly
# withholds the dedup marker, leaving the attempt eligible for a rerun to actually
# deliver the signal, instead of silently declaring victory.
attach_needs_human_or_fail() {
  # GH_TOKEN="$READ_TOKEN" on the create call too: this step's default GH_TOKEN is
  # CODEX_REVIEW_PAT, not github.token -- and the scenario that lands us in a
  # fail-closed branch calling this helper in the first place can be EXACTLY
  # "CODEX_REVIEW_PAT is broken" (the merge call itself failed on that same token).
  # Without this, the create call would inherit the broken PAT while the POST and
  # verify below correctly use READ_TOKEN, so on a repo where needs-human doesn't
  # already exist, create silently fails and POST has nothing to attach -- the one
  # signal meant to page a human never lands, for the same reason it needed paging.
  # READ_TOKEN (github.token) already has issues: write per this workflow's own
  # top-level permissions block, which covers label creation.
  GH_TOKEN="$READ_TOKEN" gh label create needs-human --color d73a4a \
    --description "Needs a human decision -- an automated handler could not safely proceed" \
    --repo "$REPO" >/dev/null 2>&1 || true
  GH_TOKEN="$READ_TOKEN" gh api --method POST "repos/$REPO/issues/${PR}/labels" -f "labels[]=needs-human" >/dev/null 2>&1 || true
  local existing_labels
  existing_labels=$(GH_TOKEN="$READ_TOKEN" gh api --paginate "repos/$REPO/issues/${PR}/labels" --jq '.[].name')
  if grep -qxF "needs-human" <<< "$existing_labels"; then
    # Staleness marker (Codex round 4): needs-human has no automatic clearing
    # mechanism -- it's a human decision point by design (see auto-merge-trigger.yml's
    # own header comment) -- but without recording WHICH revision it was attached
    # for, a later push that fixes the problem stays silently stalled: the label is
    # PR-wide, not SHA-scoped, so a fresh commit reads the exact same "needs-human
    # present" state the failed one did. auto-merge-trigger.yml/dependabot-auto-merge.yml
    # now scan for this marker to tell a genuinely-stale handoff (the head has since
    # moved) from one still covering the PR's current head. Best-effort (`|| true`):
    # if this fails to post, staleness detection just stays conservative (keeps
    # pausing), the same safe default as before this feature existed -- it never
    # makes the pause LESS safe, only potentially longer-lived than necessary.
    GH_TOKEN="$READ_TOKEN" gh pr comment "$PR" --repo "$REPO" --body "<!-- queue-retry-handler:needs-human sha=$HEAD_SHA12 -->" >/dev/null 2>&1 || true
    return 0
  fi
  echo "::error::Could not attach needs-human to PR #$PR -- the handoff comment posted, but the label signal did not land."
  return 1
}

# The three `gh pr comment` calls below carry $NO_MORE_RETRIES_MARKER / a head-mismatch
# marker -- that's machine-readable retry STATE (check-retried's fallback consumes it),
# not merely a notification, so silently losing it on a transient comment failure would
# recreate the unbounded-retry bug the marker exists to prevent: the next merge-group
# failure on this head would find neither the retry-used label (why we're in this branch
# at all) nor the comment marker (lost here), and re-arm again. Verify the marker
# actually landed and fail closed if it didn't; every OTHER `gh pr comment` call in this
# file stays best-effort (`|| true`), since only these three carry load-bearing state.
verify_marker_or_fail() {
  local marker="$1"
  local bodies
  bodies=$(GH_TOKEN="$READ_TOKEN" gh api --paginate "repos/$REPO/issues/${PR}/comments" --jq '.[] | select(.user.login == "github-actions[bot]") | .body')
  if [[ "$bodies" == *"$marker"* ]]; then
    return 0
  fi
  echo "::error::Could not confirm the no-more-retries marker was recorded for PR #$PR -- this is load-bearing retry state, not just a notification."
  return 1
}

# Ordering matters: if the label were applied before this call and the call then failed
# (transient API error, PAT problem), the revision would be permanently marked as having
# used its retry despite never actually being re-enqueued -- no new merge-group run
# would ever be created, so this handler could never even reach a handoff for it. Re-arm
# first; only mark the retry as spent (and tell the human it worked) once the merge call
# itself has actually succeeded.
#
# --match-head-commit: without it, a race between the pr-state fetch above and this call
# -- a contributor pushes a new commit in that window -- would silently arm auto-merge
# on the NEW head while the retry-used label still gets computed and applied against the
# OLD head_sha12. The new revision would then look "never retried" on its own eventual
# failure and get a second automatic retry, exceeding the one-retry bound this whole
# handler exists to enforce. Pinning the mutation to the exact SHA fetched in pr-state
# makes it fail explicitly instead of silently arming a revision the retry budget was
# never computed for.
#
# Needs-human live check: pr-state's own fetch runs, then a 10s debounce, then
# dedup and check-retried each make their own API round-trips before this step
# even starts -- a maintainer can attach needs-human in that gap, after pr-state
# already read the PR clean. auto-merge-trigger.yml and dependabot-auto-merge.yml
# both revalidate needs-human against LIVE labels right before their own merge
# calls for exactly this reason; this retry path re-arms via the same bypass-capable
# credential below and needs the identical guard, or a maintainer's just-issued
# pause could be silently overridden by this handler's own automatic retry.
#
# Staleness-aware, not a bare presence check (Codex round 5, defense-in-depth):
# auto-merge-trigger.yml/dependabot-auto-merge.yml now CLEAR needs-human when they
# determine it's stale for a new head, which is what normally keeps this PR's own
# eventual retry from ever seeing a stale label at all -- but if that clear call
# itself failed transiently, a bare presence check here would wrongly refuse to
# retry a revision that was never actually paused, silently stalling a corrected
# commit after its very first queue failure (the exact class of bug Codex found in
# auto-merge-trigger.yml, applied here as the second line of defense). Same
# provenance rule as those files: only treat it as stale when a marker is provably
# tied to the label's own most recent application AND names an older sha than this
# retry's HEAD_SHA12; fail closed (still pause) whenever that can't be confirmed.
needs_human_is_stale() {
  local current_head_sha12="$1"
  local last_labeled_at marker_info marker_created_at recorded_sha12
  local raw
  # --slurp + flatten (`add`) before selecting the latest event, fetch and filter as
  # two explicit &&-chained steps rather than `--jq` passed to `gh api` itself (Codex
  # round 6, corrected round 7): this CLI version rejects `--slurp` combined with
  # `--jq`/`--template` outright ("the --slurp option is not supported with --jq or
  # --template") -- round 6's own fix silently failed on every call via the
  # `2>/dev/null` swallow, always returning empty and therefore always fail-closed
  # (permanently pausing, never actually detecting staleness) -- the exact same
  # incompatibility this repo's own ci.yml already hit and documented (its codex-clean
  # gate, PR #56). Deliberately not a bare `cmd1 | cmd2` pipe either: this step's
  # default shell has no `pipefail`, so a pipe's exit status reflects only the last
  # command -- a genuine `gh api` failure could still slip through if `jq` happened to
  # accept whatever partial/empty output leaked past it, silently masking a real fetch
  # error the same way the `--slurp`/`--jq` bug did. Two explicit `&&`-chained
  # assignments preserve `gh api`'s own exit status visibility.
  if raw=$(GH_TOKEN="$READ_TOKEN" gh api --paginate --slurp "repos/$REPO/issues/${PR}/timeline" 2>/dev/null); then
    last_labeled_at=$(printf '%s' "$raw" | jq -r '(add // []) | [.[] | select(.event == "labeled" and .label.name == "needs-human")] | if length > 0 then (last | .created_at) else "" end' 2>/dev/null)
  else
    last_labeled_at=""
  fi
  [ -z "$last_labeled_at" ] && return 1
  if raw=$(GH_TOKEN="$READ_TOKEN" gh api --paginate --slurp "repos/$REPO/issues/${PR}/comments" 2>/dev/null); then
    marker_info=$(printf '%s' "$raw" | jq -r '(add // []) | [.[] | select(.user.login == "github-actions[bot]") | select(.body | test("queue-retry-handler:needs-human sha=[0-9a-f]{12}")) | [.created_at, (.body | capture("queue-retry-handler:needs-human sha=(?<s>[0-9a-f]{12})").s)] | @tsv] | if length > 0 then last else "" end' 2>/dev/null)
  else
    marker_info=""
  fi
  [ -z "$marker_info" ] && return 1
  marker_created_at=$(cut -f1 <<< "$marker_info")
  recorded_sha12=$(cut -f2 <<< "$marker_info")
  [ -z "$recorded_sha12" ] && return 1
  [[ "$marker_created_at" < "$last_labeled_at" ]] && return 1
  [ "$recorded_sha12" = "$current_head_sha12" ] && return 1
  return 0
}

current_labels=$(GH_TOKEN="$READ_TOKEN" gh api --paginate "repos/$REPO/issues/${PR}/labels" --jq '.[].name')
if grep -qxF "needs-human" <<< "$current_labels"; then
  if needs_human_is_stale "$HEAD_SHA12"; then
    echo "::notice::needs-human on PR #$PR is stale (provably tied to an earlier revision, not this retry's head $HEAD_SHA12) -- clearing it and proceeding with this head's retry."
    GH_TOKEN="$READ_TOKEN" gh api --method DELETE "repos/$REPO/issues/${PR}/labels/needs-human" >/dev/null 2>&1 || true
  else
    echo "::notice::PR #$PR now has needs-human covering its current head (or its provenance could not be confirmed) -- standing down without consuming this head's retry budget."
    exit 0
  fi
fi

# Trusted-author check (Codex P1 round 3): CODEX_REVIEW_PAT is a bypass actor on
# manta-review-gate ("always" mode) -- auto-merge-trigger.yml restricts its own use
# of this same credential to exactly the two authors that bypass is meant for
# (thagale, catalyst-cloud-connector[bot]). This retry step re-arms EVERY PR that
# reaches it, regardless of author, using the SAME credential -- for any other PR
# that somehow entered the queue (a manually-approved outside contributor, or
# Dependabot, whose own admission path already requires a real satisfied review
# rather than a bypass), an unconditional bypass-token re-arm would silently
# succeed even if that review were dismissed between the original queue entry and
# this retry, undoing the review requirement the PR was supposed to still need.
# Use the bypass token only for the two authors it's meant for; everyone else gets
# READ_TOKEN (github.token, not a bypass actor), so their retry only actually
# merges once the review requirement is STILL satisfied for real.
#
# GraphQL's Actor.login for a bot omits the "[bot]" suffix the REST/webhook
# payloads use elsewhere in this file (confirmed empirically against a real
# catalyst-cloud-connector[bot]-authored manta PR: `gh pr view --json author`
# returns "app/catalyst-cloud-connector", not "catalyst-cloud-connector[bot]") --
# AUTHOR_LOGIN came from pr-state's own `gh pr view --json author`, so it must be
# compared against THIS format, not the webhook one auto-merge-trigger.yml uses.
if [ "$AUTHOR_LOGIN" = "thagale" ] || [ "$AUTHOR_LOGIN" = "app/catalyst-cloud-connector" ]; then
  MERGE_TOKEN="$GH_TOKEN"
else
  MERGE_TOKEN="$READ_TOKEN"
fi

# Re-check the base immediately before mutating: --match-head-commit pins the head
# atomically at GitHub's own mutation boundary, but it doesn't pin the base -- `gh pr
# merge --auto` takes no base argument at all, it just arms auto-merge for the PR AS IT
# CURRENTLY EXISTS. If a contributor retargets the PR away from main during the
# debounce (which only re-checks head/auto-merge) or any later step in this job, this
# call would arm auto-merge against whatever base the PR now has -- one
# auto-merge-trigger.yml itself deliberately excludes, precisely because other bases
# lack the same required-status-checks ruleset main has. Re-fetching right here shrinks
# that window to the gap between this check and the mutation call itself.
current_base=$(GH_TOKEN="$READ_TOKEN" gh pr view "$PR" --repo "$REPO" --json baseRefName --jq .baseRefName)
if [ "$current_base" != "main" ]; then
  echo "::notice::PR #$PR's base changed to '$current_base' (was main) since pr-state ran -- auto-merge-trigger.yml itself would refuse this base too. Standing down without consuming this head's retry budget."
elif GH_TOKEN="$MERGE_TOKEN" gh pr merge "$PR" --repo "$REPO" --auto --squash --match-head-commit "$HEAD_SHA"; then
  # GH_TOKEN="$READ_TOKEN" here too: a successful merge call only proves CODEX_REVIEW_PAT
  # has pull-requests:write -- it says nothing about whether it ALSO has issues:write, a
  # genuinely separate scope for a fine-grained PAT. If it doesn't, these two calls
  # (inheriting the step's default GH_TOKEN) would fail every single time, the verify
  # below (correctly using READ_TOKEN) would always read "not attached," and every retry
  # -- including ones that just succeeded -- would immediately disarm and hand off to a
  # human, defeating this file's entire purpose. READ_TOKEN (github.token) has issues:
  # write per this workflow's own top-level permissions block, independent of whatever
  # CODEX_REVIEW_PAT is scoped for.
  GH_TOKEN="$READ_TOKEN" gh label create "$LABEL" --color d4c5f9 \
    --description "This revision already used its one automatic queue retry" \
    --repo "$REPO" >/dev/null 2>&1 || true
  GH_TOKEN="$READ_TOKEN" gh api --method POST "repos/$REPO/issues/${PR}/labels" -f "labels[]=$LABEL" >/dev/null 2>&1 || true

  # Verify the marker actually attached: a nonexistent label is silently dropped by the
  # add-labels endpoint rather than erroring, so the POST above can look successful
  # while recording nothing -- a subsequent failure on this same revision would then be
  # wrongly treated as a first failure, exceeding the one-retry bound.
  existing_labels=$(GH_TOKEN="$READ_TOKEN" gh api --paginate "repos/$REPO/issues/${PR}/labels" --jq '.[].name')
  attached=$(grep -qxF "$LABEL" <<< "$existing_labels" && echo true || echo false)
  if [ "$attached" != "true" ]; then
    # Recheck the head before disarming: the --match-head-commit merge call above
    # already succeeded, proving HEAD_SHA matched AT THAT MOMENT, but a new push can
    # still land in the gap between that success and reaching this disarm (the
    # label-verify work above takes real API round-trips). `--disable-auto` takes no
    # head-match guard of its own, so an unconditional call here would undo a legitimate
    # re-arm auto-merge-trigger.yml already performed for the replacement head --
    # silently stalling a commit that has nothing to do with this stale failure.
    current_head_sha=$(GH_TOKEN="$READ_TOKEN" gh pr view "$PR" --repo "$REPO" --json headRefOid --jq .headRefOid)
    if [ "$current_head_sha" != "$HEAD_SHA" ]; then
      echo "::notice::PR #$PR's head moved to ${current_head_sha:0:12} while verifying the retry-used label for $HEAD_SHA12 -- that commit's own retry budget is unaffected; standing down without touching auto-merge on the replacement revision."
      exit 0
    fi
    # Disarm rather than leave it silently active: if the label genuinely can't be
    # recorded (a persistent label-permission or API problem, not a one-off blip),
    # leaving auto-merge armed means EVERY subsequent failure on this PR reads "not yet
    # retried" and re-arms again, forever. Fail closed: disable auto-merge and hand off
    # to a human instead.
    echo "::error::Retry-used label failed to attach for PR #$PR (commit $HEAD_SHA12) -- disarming auto-merge and handing off instead of risking an unbounded retry loop."
    GH_TOKEN="$READ_TOKEN" gh pr merge "$PR" --repo "$REPO" --disable-auto >/dev/null 2>&1 || true

    # Verify the disarm actually took: the same underlying problem that stopped the
    # retry-used label from attaching (permission issue, transient API failure) can just
    # as easily make THIS call fail too. Left silently armed with no retry-used label
    # recorded, the NEXT merge-group failure on this head reads "not yet retried" and
    # gets ANOTHER automatic retry, defeating the entire one-retry bound this fail-closed
    # path exists to enforce. Re-check rather than trust the disable-auto call's own
    # reported success.
    still_armed=$(GH_TOKEN="$READ_TOKEN" gh pr view "$PR" --repo "$REPO" --json autoMergeRequest --jq '.autoMergeRequest != null')
    # Marker embedded below: check-retried falls back to scanning comments for this
    # exact string when the queue-retry-used label itself can't be found, so a rerun
    # doesn't undo this disarm and re-arm on a label-creation outage that persists
    # across attempts. Posting a comment doesn't require creating any new label, so it
    # keeps working even when label creation itself is what's broken.
    NO_MORE_RETRIES_MARKER="<!-- queue-retry-handler:no-more-retries sha=$HEAD_SHA12 -->"
    if [ "$still_armed" = "true" ]; then
      echo "::error::Auto-merge is STILL ARMED for PR #$PR (commit $HEAD_SHA12) despite the disable-auto call -- the one-retry bound cannot be relied on until this is fixed by hand."
      GH_TOKEN="$READ_TOKEN" gh pr comment "$PR" --repo "$REPO" --body "Merge-group CI failed for this revision (commit \`$HEAD_SHA12\`), and the retry-tracking label could not be recorded. Attempted to disarm auto-merge to prevent an unbounded retry loop, but could NOT confirm it's actually disabled -- this needs urgent human attention: run \`gh pr merge --disable-auto\` yourself and verify, or close/reopen the PR. $NO_MORE_RETRIES_MARKER" || true
    else
      GH_TOKEN="$READ_TOKEN" gh pr comment "$PR" --repo "$REPO" --body "Merge-group CI failed for this revision (commit \`$HEAD_SHA12\`), and the retry-tracking label could not be recorded. Disarmed auto-merge rather than risk retrying unboundedly -- this needs a human: push a fix, or manually run \`gh pr merge --auto --squash\` once the labeling problem is resolved. $NO_MORE_RETRIES_MARKER" || true
    fi
    verify_marker_or_fail "$NO_MORE_RETRIES_MARKER" || exit 1
    attach_needs_human_or_fail || exit 1
  else
    GH_TOKEN="$READ_TOKEN" gh pr comment "$PR" --repo "$REPO" --body "Merge-group CI failed for this revision (commit \`$HEAD_SHA12\`) -- automatically re-armed auto-merge for one retry. If this fails again for the same commit, it will NOT retry again; it needs a human to look." || true
  fi
else
  # Disambiguate WHY the merge call failed: --match-head-commit makes this fail two
  # structurally different ways, and conflating them would raise a false alarm on the
  # common one. Re-fetch the PR's CURRENT head to tell them apart.
  current_head=$(GH_TOKEN="$READ_TOKEN" gh pr view "$PR" --repo "$REPO" --json headRefOid --jq .headRefOid)
  if [ "$current_head" != "$HEAD_SHA" ]; then
    # Benign: a new commit landed in the race window between the pr-state fetch and
    # this call. That new revision gets its own fresh retry budget on its own eventual
    # merge-group run (or is already covered by auto-merge-trigger.yml's synchronize
    # handler) -- nothing to do here, and no human needed for what's actually just a
    # normal push.
    echo "::notice::PR #$PR's head moved from $HEAD_SHA12 to ${current_head:0:12} during this run -- the retry for the OLD commit is moot; the new commit has its own retry budget. No action needed."
  else
    echo "::error::Could not re-arm auto-merge for PR #$PR (head unchanged at $HEAD_SHA12) -- treating as an immediate handoff instead of silently stalling."
    CURRENT_HEAD_MARKER="<!-- queue-retry-handler:no-more-retries sha=$HEAD_SHA12 -->"
    GH_TOKEN="$READ_TOKEN" gh pr comment "$PR" --repo "$REPO" --body "Merge-group CI failed for commit \`$HEAD_SHA12\`, and the automatic retry itself could not re-arm auto-merge (API error). This needs a human: push a fix, or manually run \`gh pr merge --auto --squash\`. $CURRENT_HEAD_MARKER" || true
    verify_marker_or_fail "$CURRENT_HEAD_MARKER" || exit 1
    attach_needs_human_or_fail || exit 1
  fi
fi
