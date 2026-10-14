# MAN-174: deferred beacons survive merge chains and every loss is counted

## Context

PR #154 made a non-allowlisted Beacon candidate wait until its track
closes before it is judged
(`docs/DECISIONS/2026-09-09-beacon-emission-deferred-to-track-close.md`).
A `Bookkeeping { survivor_track_id: Some(s) }` close moves the track's
pending candidates to `s`; a `SignalEnded` close judges them against the
track's final speed. Codex's round-10 and round-11 reviews of that PR left
eight P2 findings, deferred to this ticket under the review-convergence
policy. Reproduced at `9636fe0` with temporary tests:

- **Merge chain (finding 1).** Three tracks converging in one hop make
  `merge_converged` return `[(2 → 1), (1 → 3)]`, in dependency order.
  `process_hops` then sorted every `TrackClosed` by `track_id`, so track 1's
  close reached the validator first. Track 2's close then migrated its
  pending beacon into a recreated track-1 state that no later close would
  ever free: 0 spots at track 3's close, and leaked state. The same loss
  happened when the survivor closed `SignalEnded` in the same batch
  (`[1 SignalEnded, 2 → 1]`), so collapsing survivor ids to the final live
  track, the review's other suggestion, would not have been enough.
- **Never-emitted survivor (found while reproducing finding 1).** A merge
  survivor that had produced no event yet was named to consumers by its
  loser's `TrackClosed`, but MAN-19's `has_emitted` filter suppressed its
  own later close. State migrated into it was never freed.
- **Uncounted losses (findings 2 and 6).** A pending beacon dropped by the
  WPM gate (implausible or never-measured final speed) or by the stale
  check (an emitted spot for the same identity is newer) changed no
  counter, against ARCHITECTURE §8.
- **Eviction order (finding 4).** Migration appended the loser's queue to
  the survivor's and evicted from the front, so a loser candidate older
  than all eight of the survivor's displaced the survivor's oldest newer
  one.
- **Equal timestamps (finding 5).** The stale check used `<=`, so a
  deferred beacon with the same `sample_ts` as an emitted spot, for example
  from two tracks completing a word on the same hop, was dropped even
  though it cannot rewind `Dedupe`'s watermark.
- **Docs (findings 3, 7, 8).** SPEC V22/V25 still said an exempt Beacon
  spots when `TrackMeta` arrives; SPEC §5 still showed
  `TrackClosed { track_id }` with no `closure`; the wiki never linked the
  2026-09-09 record and still described first-decode Beacon emission.

## Decision

- **Closure order.** `process_hops` sorts a batch's `TrackClosed` events by
  descending merge depth (the number of same-batch merge hops from a track
  to one that is not itself closed this batch), then by `track_id`
  (`sequence_batch`/`closure_depths` in `crates/manta-engine/src/track.rs`).
  A loser's depth is its survivor's plus one, so every loser closes before
  its survivor, whether the survivor merges further, ends `SignalEnded` or
  is evicted. Closures with no same-batch dependency keep `track_id` order,
  and every other event keeps its `(sample_ts, track_id, tier)` order, so
  output changes only in the cases that were broken. Records keep naming the
  immediate survivor; ids are not collapsed to the final root.
- **Survivors owe a close.** `merge_converged` marks a survivor
  `has_emitted` when an emitting loser names it, so the survivor gets its
  own `TrackClosed` on whatever path later closes it.
  `primary_track_id` (`manta_engine::decode_samples`'s reported track) now
  ignores `TrackClosed`, so a survivor that never decoded cannot become the
  reported track on its closure alone.
- **Validator backstop.** The `Bookkeeping` handler resolves the named
  survivor through `merged_into` before migrating, so closures that arrive
  in the wrong order still land on the live root.
- **Every loss is counted.** `SuppressionCounts` gains
  `pending_beacon_rejected_wpm` (one per candidate the WPM gate drops,
  unconfirmed and implausible speed alike; a candidate the operator lists
  suppress is counted under `blocklist`/`notch` instead, never both) and
  `pending_beacon_stale`. With `pending_beacon_overflow` and
  `pending_beacon_lost_to_eviction`, a pending beacon now leaves the queue
  only by being emitted, suppressed by `Dedupe::should_emit` like any other
  candidate, or counted. The counters stay off `/metrics`, like the existing
  pending-beacon counters.
- **Eviction by age.** `pending_beacons` is kept sorted by `sample_ts`
  (`insert_by_age`, used by capture and by migration), and the 8-entry cap
  drops the true oldest.
- **Equal timestamps go to dedupe.** Only a deferred beacon strictly older
  than the identity's last emitted spot is dropped (and counted). An equal
  timestamp goes through `Dedupe::should_emit`, which suppresses a
  same-type duplicate and admits a spot-type change or SNR jump. This
  supersedes the 2026-09-09 record's rule that "a deferred candidate whose
  own `sample_ts` is not strictly newer is discarded"; that record is not
  edited.

Not changed: `close_all` (end of stream and discontinuities have no merge
dependencies), the 45 WPM cutoff, the 8-entry cap, the decision log's
`reason` labels (`beaconSpeed`, `dedupe`) and every wire format.

## Spec status

`docs/SPEC-decode-core.md` §5 now gives `TrackClosed { track_id, closure }`
with both closure kinds, which close reasons map to each, and what a
consumer holding deferred evidence does with each. §6 rule 6 states the
closure order. V22 and V25 are narrowed to allowlisted candidates; a
non-allowlisted Beacon decoded before `TrackMeta` is captured and spots
only at `TrackClosed`, as V18/V30 already require.

## Tests

- `crates/manta-engine/src/track.rs`:
  `chained_merge_closures_are_sequenced_loser_before_survivor`,
  `a_merge_losers_closure_precedes_its_survivors_same_batch_close`,
  `chained_merge_resolves_the_deferred_beacon_at_the_final_survivor`
  (real `merge_converged` output into a real `Validator`),
  `a_never_emitted_merge_survivor_gets_its_own_closure_when_it_merges`,
  `a_never_emitted_merge_survivor_gets_its_own_closure_when_evicted`.
- `crates/manta-engine/src/lib.rs`: `primary_track_id_ignores_track_closed`.
- `crates/manta-spot/src/validator.rs`:
  `a_chained_merge_out_of_order_still_resolves_at_the_final_survivor`,
  `a_beacon_rejected_for_implausible_wpm_is_counted`,
  `a_beacon_rejected_for_unconfirmed_wpm_is_counted`,
  `migration_keeps_the_newest_pending_beacons_by_sample_ts`,
  `an_equal_timestamp_deferred_beacon_goes_through_dedupe`; and
  `resolved_beacon_never_rewinds_dedupe_state_backward` now sends a
  plausible final speed, so it reaches the stale check (it had been passing
  through the round-10 unconfirmed-WPM refusal instead) and asserts
  `pending_beacon_stale == 1`.
- `crates/manta-cli/tests/docs_consistency.rs`:
  `spec_section_5_track_closed_names_its_closure_kinds`.
