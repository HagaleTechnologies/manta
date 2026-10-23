# MAN-279: a planned-restart window counts only when the restart happened

Date: 2026-10-23. Ticket: MAN-279, deferred from PR #219 (MAN-96) review
round 4 (P2, `scripts/field-node.py:829`) under
`docs/DECISIONS/2026-08-07-pr-review-convergence-policy.md`. This refines
MAN-96 D-E (`2026-10-07-man96-secondary-skimmer-field-node.md`) without
rewriting that record.

## Context

`summarize` in `scripts/field-node.py` excluded a window for every
`planned` note, whether or not a restart followed. On 30-day synthetic
ledgers (600 s samples, note 60 s after sample 100) run through
`field-node.py report`: a note followed only by healthy samples excluded
the full 1800 s cap with `restarts` empty. A ledger 43 intervals down
elsewhere plus a 10 min outage of the same process at sample 101 FAILs
D-A without a note (exit 1, 0.9898); adding a note over that outage
excluded 1140 s, hid it, and turned the run into a PASS (exit 0, 0.9900).

## Decision

- **Gate, don't move.** The window's bounds stay as MAN-96 built them;
  only whether it is appended changes. Every report excludes a subset of
  what it excluded before, and time no longer excluded counts as observed
  (up, down or unobserved), so no outage is hidden that was not before.
- **A restart means a changed `start_time`.** Some sample from the note
  to the window end must carry a `start_time` (`manta_start_time_seconds`)
  other than the last non-null one before the note. A not-up sample alone
  is an outage, not a restart; the restart list uses the same rule.
- **Look past the cap only for evidence.** When the window runs to the
  cap, the check continues up to the first healthy sample at or after it,
  so a restart that outlasts the cap or leaves no samples inside it (host
  reboot, sampler offline) still counts. The window still ends at the cap.
- **No baseline means no restart.** A note with no earlier reachable
  sample has no `start_time` to compare, and excludes nothing.
- **Restart classification is unchanged**, including
  `RESTART_ATTRIBUTION_LOOKBACK_S`.

## Consequences

- A cancelled maintenance excludes 0 s, and any outage after its note
  counts against D-A. Tests: `test_planned_note_without_a_restart_excludes_nothing`,
  `test_outage_inside_a_cancelled_planned_window_counts_as_down` and
  `test_planned_window_capped_without_a_restart_excludes_nothing` in
  `scripts/tests/test_field_node.py`.
- A same-process recovery inside the window before a later restart now
  excludes 0 s (it excluded up to that recovery before), while the later
  restart is still listed `planned`. Extending the window to the later
  restart could exclude more than before and hide the earlier outage.
- A `report` run while a planned restart is still down excludes 0 s until
  a sample shows the new `start_time`; running it again restores the
  window. The day-30 report always follows a post-window sample (MAN-291).
- The 1800 s cap, the flags, the JSON and Markdown shape and every other
  D-* threshold are unchanged.
