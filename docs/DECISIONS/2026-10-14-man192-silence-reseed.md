# MAN-192: the HSMM decoder reseeds after a committed Silence, not only on a gate re-onset

## Decision

SPEC v2 §4.7 names two seeding triggers: "When `present` rises (§1.4), or
after any committed `Silence`, seed tokens … Seeds compete with any live
tokens on equal terms; the first character's evidence prunes them." Only
the first was implemented: `HsmmDecoder::seed` had exactly one call site,
`push`'s `present` false-to-true branch. When interference keeps the
keying-present gate asserted across a long gap, the Silence that closes
the gap committed without ever reseeding, so a transmission resuming at a
different speed had no fresh hypothesis to compete with the surviving
root token (found by Codex review on PR #161, round 18, deferred to this
ticket). Both triggers are now implemented; the spec already required
this — no SPEC text changed.

### What "committed as Silence" means

A word boundary commits as Silence when partial traceback
(`commit::commit`) actually emits a winning boundary head whose closing
segment was `Silence`. The carrier is a new `HistEntry::closing_segment:
Option<SegType>` field, stamped by `Token::successor` on every entry a
real gap transition creates (`Some(CGap)` on the glyph entry CGap appends;
`Some(WGap)`/`Some(Silence)` on both entries those transitions append;
`None` for an entry with no closing segment, which also accommodates a
future synthetic unclosed EOF glyph). Provenance is metadata: it never
feeds `token_order`, `merge_key`, confidence, consensus, or committed-
history deduplication, and `None` never triggers reseeding.

Glyph consensus alone cannot establish a committed long gap — CGap, WGap
and Silence interpretations of the same gap can agree on the glyph — so
the signal reads only the winning head, after the existing sort and
sealed-duplicate stripping, never an agreeing or losing candidate. A
losing Silence hypothesis must not reseed. If a glyph and its boundary
commit in separate calls, only the call that emits the boundary reports
Silence; several qualifying boundaries in one commit batch share the
same seed state and anchor, so one seed set suffices. EOF does not
reseed (no subsequent input exists to decode).

The public `commit()` signature is unchanged: it wraps a
`pub(super) commit_outcome()` whose `CommitOutcome { entries,
committed_silence }` is the signal `push` consumes.

### The reseed itself

In `push`, after `commit_outcome` filters `merged`: if the outcome
committed a Silence and `ev.present` is true, subtract the first
survivor's score (zero for an empty survivor set) from every retained
anchor token and every current survivor, then append `self.seed(ev.hop)`
once, after the existing beam truncation. Survivors keep their node,
phase, speed, history and anchor identity; only a common score offset
changes, so `successor`'s cumulative sums and `margin`'s differences —
every ordering, tie-break and confidence — are untouched. This is the
same uniform-shift normalization the gate-rise reseed has applied since
PR #161 round 5, extended to the survivors about to become `live` and
the next anchor snapshot. Placement after the truncate is deliberate:
injected as candidates, zero-score seeds would be pruned before any
evidence could act, defeating §4.7's "the first character's evidence
prunes them". When `present` is false the injection is skipped per
§1.4; the subsequent gate rise supplies the seeds. If a gate rise and a
Silence commit land on the same hop, `live` ends with exactly one
appended configured set.

### No cooldown

The research's D3 proposed throttling reseeds to one per
`10 * u_max_live` hops. Rejected: a fast hypothesis legitimately
completes a valid Silence inside that interval (the committed test
scene has two eligible Silence boundary commits 117 hops apart with
`u_max_live` 13, i.e. inside the proposed 130-hop window), and traceback
can commit delayed entries close together, so the throttle would violate
"after any committed Silence". The unbounded-growth concern it answered
is bounded structurally instead: every productive anchor prunes to
`beam` before at most `seed_units_hops.len()` seeds are appended, so any
snapshot holds at most `beam + seed set` tokens (17 with defaults);
`manta_repeated_recovery_is_bounded_and_deterministic` asserts the bound
over a run spanning more than `80 * u_max` hops with fallback anchors,
along with anchor expiry and byte-identical replay.

### Scored-prefix feedback and EOF protection

`HsmmDecoder` now carries `scored_live_len`: `live[..scored_live_len]`
is the sorted surviving candidate beam and any suffix is solely the raw
seeds the last reseed appended. This replaces `best_u`'s old "skip
tokens that look exactly like a fresh seed" heuristic, which the
normalization broke twice over: a real root can legitimately sit at
exactly score 0.0 with empty history after a Silence reseed (its
entries were just committed and stripped) and would have been skipped,
while a raw seed could not be told apart by score at all. `best_u`
returns the first scored survivor's speed — the survivor's, never a
seed's, so the `best_u → Evidence::set_u_ref` feedback loop
(`push_hop_hsmm`) cannot emit a spurious `SpeedUpdate` from a fresh
seed, the historical PR #161 round-2 bug. `finish()` truncates the raw
suffix before its empty check and sort: an empty-history seed sorts
ahead of a normalized zero-score incumbent carrying pending history and
would otherwise displace real pending text and depress its confidence.
Every assignment to `live` maintains the count (test fixtures that
build `live` directly set it too).

## Consequences

- `HistEntry` gains a public field (source-compatibility change for any
  Rust consumer constructing one; `manta-decode` exports `hsmm` from its
  lib root, so the type is reachable). Callers obtaining histories
  through `Token::successor` get correct provenance automatically.
- Decoder output changes only for inputs with gate-stuck gaps under the
  opt-in `hsmm` engine (`decode.engine` remains `legacy` by default);
  same-build repeated-input determinism is unchanged and asserted.
- The `hsmm` engine's outstanding stage-2 acceptance gate (measured
  FAIL 2026-09-09) remains outstanding; this ticket claims no CER gain
  and no promotion.
- Each productive anchor generates candidates from at most
  `beam + seed_units_hops.len()` live tokens, a bounded additive cost
  over §10's model while seeds live. Before/after
  `hsmm_300_tracks` runs on this development container measured
  24.722 s median per 10 s of 300-track input at the merge base, then
  27.428 s and 30.800 s for two runs of the changed code under the
  identical command — but the two after-runs differ from each other by
  more than the base-to-after delta, so this host cannot attribute the
  slowdown to the change; the container's timing drifted upward across
  consecutive identical runs. The change's structural costs are
  `HistEntry` growing from 24 to 32 bytes (cloned per successor) and
  the seeds' bounded extra candidate evaluations after a committed
  Silence. This is not a Pi 4 or M-series acceptance result; the CPU
  budget gate remains to be measured on stable hardware.

## Verification

`cargo test -p manta-decode --lib --locked` — 156 passed at the merge
base, all preserved, plus these `man192`-prefixed regressions:

- `token::man192_gap_transitions_stamp_closing_segment` — CGap/WGap/
  Silence stamp every entry; inherited entries keep provenance.
- `commit::man192_commit_outcome_flags_only_a_silence_boundary_head` —
  consensus and the WGap/CGap/`None`-provenance negatives.
- `commit::man192_winning_head_provenance_decides_the_signal` — winner
  after sort decides; a losing Silence does not reseed; forced leg.
- `commit::man192_split_commit_reports_silence_only_on_the_boundary_call`
  — glyph first, boundary later; already-decided boundary re-derives to
  no entries and no signal.
- `commit::man192_public_wrapper_matches_internal_helper_entries` —
  wrapper and helper parity including confidence and alternatives.
- `tests::man192_committed_silence_with_gate_high_merges_seeds` — the
  ticket scenario: E + boundary commit with the gate never dropping and
  the configured seed set merged into live and the current snapshot.
- `tests::man192_default_set_reseeds_and_a_nondefault_seed_competes` —
  all configured seeds in order; a fresh nondefault speed takes a real
  successor and competes in the scored beam.
- `tests::man192_silence_reseed_shifts_all_scores_by_one_offset` — one
  offset everywhere, pairwise differences preserved, identities and
  histories untouched, root kept alongside seeds.
- `tests::man192_best_u_reports_a_zero_score_scored_root` — the
  normalized-root speed failure the old heuristic would have caused.
- `tests::man192_finish_ignores_raw_seed_suffix` — EOF parity with a
  scored-only control; seed-only, repeated and empty EOF.
- `tests::man192_no_reseed_for_wgap_uncommitted_or_gate_low_silence` —
  the three negative controls.
- `tests::man192_gate_rise_and_silence_commit_on_the_same_hop` — one
  appended set; ordinary drop/rise unchanged.
- `tests::man192_two_silence_commits_closer_than_the_rejected_cooldown_both_reseed`
  — the no-cooldown rule.
- `tests::man192_repeated_recovery_is_bounded_and_deterministic` —
  twelve cycles reseed; bounds, expiry, byte-identical replay over a
  > 80·u_max run.

The live/EOF `best_u → set_u_ref` wiring is covered by the existing
`decoder.rs` envelope tests (`hsmm_no_spurious_speed_update_on_reseed`
and the finish-loop feedback tests) over the new `best_u`; the
direct-HSMM EOF test above covers the state they feed on. Sibling work
(MAN-189's commit frontier, MAN-190's anchor scheduling, MAN-191's
Silence cadence) was not present in this implementation base and is
untouched.
