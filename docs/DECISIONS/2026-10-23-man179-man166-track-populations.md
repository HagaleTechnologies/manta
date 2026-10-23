# MAN-179: the MAN-166 writeup's 41,174 and 409,675 count different track populations

Date: 2026-10-23. Ticket: MAN-179, found by Codex review on PR #152, round 12.

## Context

The audio-corpus README called the B2 baseline's 41,174 "distinct tracks
opened" and said `TrackManager::close_counts()` root-caused it. That cannot
hold: each `track_id` closes once, and the three reasons the MAN-166 record
lists (286,196 `unconfirmed`, 71,149 `evicted`, 52,330 `merged`) total 409,675.

## Decision

Both figures stand, labelled as different populations in the README, in a
dated annotation on the MAN-166 record, and here.

- **41,174 is promoted tracks in the decoder output**, taken from the
  2026-09-08 baseline's `manta decode --json` report (PR #144 calls it "41,174
  tracks opened"). The command that counted it was not recorded. The run
  predates `TrackPromoted` (PR #145; `02863cd` has it only because PR #144 was
  rebased onto #145 before merging). Its report carried events only from tracks
  that were promoted and then emitted decoder output, and a CANDIDATE emitted
  nothing, so a track count read from it, most likely its distinct
  `track_id`s, is a lower bound on promotions.
- **409,675 is three of five close reasons** from `close_counts()` on the same
  configuration (`track_cap=500`, `confirm_hops=19`): every closed track,
  CANDIDATEs included. The shares (68.8%, 29.7%) put the full total near
  416,000; the rest is `hang_expired` + `silent`, listed in neither document.

**No double count.** `next_id` never reuses an id, and a track that
`step_hop` closes passes exactly one `close_counts.record` site in
`crates/manta-engine/src/track.rs`: the lifecycle close, the RTTY close,
`merge_converged()` or `evict_over_cap()`. `finish()` and
`finish_for_discontinuity()` (a live source's reconnect) close the tracks
still open through `close_all()`, which records nothing, so the total
slightly undercounts tracks opened. That is HEAD; September's tree had the
same property with three sites (no RTTY close) and `finish()` alone, which
recorded nothing either. Two checks made while planning,
not committed: a scratch test held `sum(close_counts) + tracks.len() ==
total_spawns()` after every one of 40,000 hops of random keyed bursts on 64
channels (`track_cap=4`: 407,433 spawns; `track_cap=40`: 3,613); and a 30 s,
1,000-signal synthetic scene on today's defaults gave 1,879 `TrackPromoted`
(its distinct `track_id`s) against 62,464 closes, the same kind of gap.

**Not re-measured on B2.** Only the RBN ground-truth CSV is in the tree; the
audio stays out of git and off public hosts until K5TR's redistribution
permission is recorded. Today's tree would not reproduce September's numbers
anyway: `track_cap` is now 1200, and MAN-313 defers a promoted track's decoder
lease and `TrackPromoted` until it qualifies as CW. A
like-for-like run takes both counts from the PCM16 conversion (SHA-256
`d2ea5247…d4d4a621`) at `track_cap=500`, `confirm_hops=19`: `TrackPromoted`
from `manta decode --json --config` (`[detector] track_cap = 500`), and
`final_close_counts` from `cargo run --release -p manta-engine --example
close_counts -- <wav>` with a scratch edit of the `PipelineConfig::default()`
it runs (it has no config flag).

This changes only docs, with no change to code or defaults.
