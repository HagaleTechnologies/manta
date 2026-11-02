# 2026-11-02 — MAN-199: a promoted track replays its recorded lead-in, and every track closes through one routine

**Status:** Implemented (branch `MAN-199`). The manual V8w legacy comparison
and the `cpu_budget` comparison named in the plan were not run in the
implementing session (see "Not measured").

**Ticket:** MAN-199. Source: investigation 5 of
`docs/DECISIONS/2026-09-09-decoder-recall-research.md` (rank 5 of 20).
Related: MAN-271 (which replaced MAN-3, per the ticket's 2026-10-13
comment), MAN-194, MAN-313, MAN-108, MAN-202, MAN-19.

## Problem

A track's decoder started at its promotion hop. Every hop of real energy
before it was discarded: on a +20 dB step after a warm floor, the candidate
is born 13 hops after the onset and confirmed 18 hops later, so 31 hops
(83 ms) never reached a decoder. That is a whole dah at 40 WPM. `W1AW`
decoded as `M1AW` at every speed from 20 to 40 WPM, `CQ` as `FQ` and `K1ABC`
as `U1ABC` at 35 and 40 WPM.

Separately, the close paths disagreed. Hang expiry, merge and eviction
checked `has_emitted` before draining, so a track that had not yet emitted
dropped its queued and buffered evidence, and got no `TrackClosed`, where
end of stream flushed the same state. `listen_with_observers` returned on a
read error without closing its tracks at all.

## What changed

- **Hop history.** `TrackManager` records each stepped hop's power spectrum
  in `HopHistory` (64 hops, recycled buffers), indexed by hop and stamped
  with the original `sample_ts`. A query answers `Recorded`,
  `NotRecorded(BeforeSegment | BeforeInputGap | Overwritten)` or
  `NotYetKnown`. The history is cleared at an unknown-length input gap
  (MAN-202) and is per segment, so it starts empty after a discontinuity.
  Pre-roll hops never reach `step_hop`, so they are never recorded.
- **Lead-in.** At promotion, and at a DORMANT wake, `Track::build_lead_in`
  takes the recorded hops before that hop, minus any hop a decoder on the
  same channels already consumed (`last_fed_hop`). Each entry is the
  max-power owned channel's power and its square root, through the MAN-312
  crash hold. The track keeps the lead-in in its own queue, which the drain
  pushes through `TrackDecoder::push_lead_in_hop` ahead of `pending`.
- **Decoder.** `push_lead_in_hop` feeds a hop exactly as `push_hop` does but
  does not count it in `hop_count`, so `TrackMeta`/`TimingUpdate` and the
  MAN-102 peak-hold window stay on post-promotion hops. A space before the
  decoder's first mark is neither classified nor learned.
- **One close routine.** `Track::close(CloseFlush)` drains the refiner
  backlog, the lead-in and `pending` through the decoder, then finishes it
  (`Forced`: `finish()`; `SpeedOnly`: `finish_speed_only()`), publishes
  garbles and sets `has_emitted` if the flush produced an event.
  `TrackManager::close_track` wraps it for every single-track close (all
  `CloseReason`s, merge and eviction); `close_all` (end of stream,
  discontinuity) and `go_dormant` call it directly. `finish_decoder` and
  `finish_decoder_speed_only` are gone. The `TrackClosed` gate now runs
  after the flush everywhere.
- **Read error.** `listen_with_observers` closes its segment's tracks with
  `finish_for_discontinuity()` before returning a read error, as
  `run_input_front` already did.
- **Ordering.** `effective_sort_ts` sorts every event of a track promoted
  in the batch (except its `TrackPromoted` and `TrackClosed`) at no earlier
  than that promotion's timestamp, so lead-in output stamped before the
  promotion hop still follows `TrackPromoted`.

## Measured on the branch

`crates/manta-engine/tests/regression_first_character.rs`,
`leading_character_survives_acquisition`: 3 s of noise (seed 11), the text
keyed once at +5 kHz, 5 ms rise, 4 s tail, seed 22, 20 dB, 96 kS/s, default
config.

| Lead-in depth | Exact text (15 cases) | Wrong decodes |
|---|---|---|
| 0 (the branch with `LEAD_IN_HOPS = 0`) | 6/15 | `M1AW` ×5, `FQ DE W1AW` ×2, `U1ABC` ×2 |
| 64 | 15/15 | none |

Depth 0 reproduces the pre-change baseline exactly, so the leading-space
rule changes nothing without a lead-in. The ignored `first_character_sweep`
(5 SNRs × 3 seeds × 15 cases) result is recorded below under "Sweep".

The close matrix, `every_close_site_flushes_the_same_way_for_the_same_state`
(a decoder fed `PARIS` with no trailing gap, so `S` is open; for
`has_emitted` true and false): every forced close (`finish`, `go_dormant`,
`HangExpired`) flushes exactly `S`, every speed-only close (discontinuity,
`Silent`, `Unkeyed`, `NotCw`, `Rtty`, `Merged`, `Evicted`) flushes no
character and the same final `SpeedUpdate`, and every row closes. Before
the change the `has_emitted = false` rows for `HangExpired`, `Merged` and
`Evicted` flushed nothing and did not close.

### Sweep

Run on the branch with `cargo test -p manta-engine --locked --test
regression_first_character -- --ignored --nocapture`; see the PR for the
printed table if it is not filled in here.

## Decisions

- **D1.** The history lives in `TrackManager`, stores power only, holds 64
  hops and recycles its buffers. A per-candidate buffer cannot hold the 13
  hops before birth.
- **D2.** Lead-in depth 64 (171 ms). On the plan's 225-case prototype grid
  it was the smallest depth that decoded every first character (48 missed
  one at 3 dB); 96 was equally clean but replays more of a possible earlier
  transmission.
- **D3.** A separate `lead_in` queue. In `pending`, MAN-313's D′
  qualification would count replayed marks (the research prototype made a
  swept-carrier bump qualify that way).
- **D4.** Lead-in hops do not advance `hop_count` or the SNR peak window.
  `lead_in_does_not_move_track_meta` pins it.
- **D5.** A space before the first mark is not timing evidence: it is a
  truncation, like the trailing space `finish()` already refuses to learn
  from (MAN-315).
- **D6.** "Not recorded" versus "not yet known" is a type (`HopRecord`),
  and a short lead-in carries its reason (`LeadInCut`). Nothing synthetic
  is replayed and no padding is counted as received silence.
- **D7.** A lead-in never re-reads a hop a decoder on the same channels
  consumed (`last_fed_hop`), which covers a DORMANT wake and an eviction
  respawn.
- **D8.** One close routine, with the `has_emitted` gate after the flush.
  This is MAN-374's rejected alternative C in its safe form: MAN-19's rule
  (a `TrackClosed` only for a track that produced another event first)
  holds literally because the flush's events come first, and a speed-only
  close still forces nothing (PR #154 round 7).
- **D9.** A track's `TrackPromoted` sorts first among its same-batch
  events (SPEC §6 rule 6).

## Rejected alternatives

- Pushing the lead-in into `pending`: counted in `hop_count` (moved
  `TrackMeta` timestamps) and in D′ qualification; the research prototype
  broke 9 lib tests and the `non_cw` sweep-bump guard.
- Lowering `confirm_hops`: MAN-166's V10 cliff pins it at 19, and it cannot
  recover the 13 pre-birth hops.
- Padding scenes or extending transmissions until goldens pass: excluded by
  the ticket.

## Known limits

- Streams shorter than `Demod`'s 375-hop init window (`EE`, `R`) still give
  "no signal found" (MAN-271's sub-1.1 s follow-up). The lead-in lowers
  that floor by up to 64 hops.
- A leading character longer than the lead-in (an all-dit `5` at 28 WPM or
  faster) can still lose its start.
- Failed `Demod` bootstrap windows are not remembered.
- With `decode.refine_bw_hz > 0` lead-in entries carry raw amplitude, as
  the refiner's own first outputs already do.
- An unqualified track still drops its queues at any close (MAN-313).
- `EdgeLegacy`/`Hsmm` `finish_speed_only` flushes nothing (MAN-169).

## Not measured

- The ignored V8w legacy vector and `cpu_budget` comparisons against
  `origin/main` were not run. Expected cost per hop: one 4–8 KB copy into a
  recycled buffer, one store per fed track; per lease, 64 extra decoder
  pushes.
- `soapy`/`hpsdr` features beyond the CI gate, live sources, real
  recordings and fading scenes.

## Follow-ups

- A deeper or adaptive lead-in for long leading characters.
- Remembering failed bootstrap windows (with the sub-1.1 s follow-up).
- A config key or metric for the lead-in, if field data asks for one.
