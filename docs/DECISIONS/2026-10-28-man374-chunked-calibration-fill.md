# MAN-374: the startup calibration fill runs through `process_hops` in read-sized chunks, so a short unkeyed timeout keeps its callsign

**Status:** accepted. `Segment::process_fill`
(`crates/manta-engine/src/listen.rs`) feeds the fill in `CHUNK_SAMPLES`
pieces on both listen paths, pinned by two regression tests.

## Context

Deferred from PR #283 (MAN-313) review round 2, a Codex P2, per [the PR
review convergence policy](2026-08-07-pr-review-convergence-policy.md).

`listen()` buffers the first 2 s of input (`CALIBRATION_SECONDS`) and, until
this change, passed all of it to one `TrackManager::process_hops` call; the
multi-input front (`run_input_front`) did the same. `process_hops` steps
every hop through `step_hop` first and runs `drain_pool` once at the end.
MAN-313's `Unkeyed` rule sits in `step_hop` and reads `last_char_hop` and
`callsign_read`, which only the loop after `drain_pool` writes. So for the
whole ~750-hop fill the rule saw the evidence from before the fill.

With `[detector] unkeyed_close_ms` at its 1000 ms floor (375 hops), a
qualified track whose message ends and whose carrier starts within about
the first 0.85 s was closed inside the fill before its decoder ever ran. Its
`has_emitted` was still false, so the close path dropped it without a flush:
the queued message was discarded, no `TrackClosed` reached the validator,
and the channel took the 30 s respawn cooldown. From 2000 ms up (the 10 s
default included) the timeout cannot run out inside a 750-hop fill.

Measured on `main` at `95b13fa`, 96 kHz, a keyed message from t = 0 then a
steady carrier, through `listen_with_observers`:

| scene | carrier start | 375 hops: `Unkeyed` in the fill / text for the run / closes | 3750-hop control: text / closes |
|---|---|---|---|
| K5ARH 70 WPM | 0.96 s | 0 / `T5ARH T` / SignalEnded | `T5ARH T` / SignalEnded |
| K5ARH 80 WPM | 0.84 s | **1 / `""` / none** | `EESARH T` / SignalEnded |
| N1A 70 WPM | 0.69 s | **1 / `""` / none** | `JA T` / SignalEnded |
| N1A 80 WPM | 0.60 s | **1 / `""` / none** | `1A T` / SignalEnded |
| TK1A 70 WPM | 0.86 s | **1 / `""` / none** | `K1A T` / SignalEnded |

## Decision

Feed the fill through `process_hops` one `CHUNK_SAMPLES` (2048-sample)
piece at a time, as steady-state reads already are, and keep its events
one batch: one `emit` and one `on_tracks` in `listen_with_observers`, one
`Batch` with `tick` in `run_input_front`. So the daemon's "calibration read
has returned" signal (the second `on_tracks`, MAN-122), the replay merge and
`tick` all still see one fill batch. Hop timestamps do not move: `hop_ts`
maps each absolute hop, and MAN-423's held losses are pruned only in
`HeldLosses::record`, so a calibration loss applies exactly as before.

`decode_source` already chunks file decode at 4096 samples for the same
reason: `Silent`'s GC-timer reset also runs only after `drain_pool`, and a
whole-file call let it expire on a signal that never stopped.

`track.rs` is unchanged.

## Alternatives rejected

- **A′: gate `Unkeyed` on an empty `pending` queue.** One line in
  `step_hop`, and it reads literally as the ticket's second Then. It broke
  four per-hop unit tests that pin the close hop
  (`decoded_characters_restart_the_unkeyed_timer`,
  `unkeyed_active_track_closes_after_unkeyed_close_hops`,
  `unkeyed_close_starts_the_respawn_cooldown`,
  `woken_track_is_not_closed_unkeyed_immediately`). Because `note_char`
  stamps the batch-end hop, a fill that decodes any character would also
  restart the timer at hop ~750, moving the close about a second later. It
  leaves `Silent` exposed to the same cause.
- **A: move the whole `Unkeyed` evaluation after the drain.** It needs the
  close machinery (counts, removal, cooldown, flush) callable from
  `process_hops` after `drain_pool`, breaks the same four tests, and also
  leaves `Silent` exposed.
- **C: flush a qualified track's queue on any close.** It changes MAN-19's
  rule that a track that never emitted gets no `TrackClosed`, and
  `finish_decoder_speed_only` does not force a trailing mark, so the last
  character could still be lost.

## Consequences

- With `unkeyed_close_hops = 375`, every bold row above decodes the
  control's text (`EESARH`, `JA`, `1A`, `K1A`). The track still closes
  `Unkeyed`, but in steady state after its decoder has drained, so the close
  flushes it and emits `TrackClosed { closure: Bookkeeping {
  survivor_track_id: None } }` after its last `CharDecoded`. Rows that never
  closed early do not change.
- The `Silent` close below 2 s has the same cause and is covered by the same
  change: with `gc_hops = 300`, `CQ DE K5ARH K5ARH K` at 25 WPM no longer
  drops its first track in the fill (MAN-374 research).
- The evidence is still stale by up to one chunk (8 hops at 96 kHz, 16 at
  48 kHz), as in steady state (SPEC §2.8 G, "batch granularity"). For that
  to close a track before its first drain, the track would need a re-rise
  within about one chunk of promotion and a qualification in the last chunk
  before the deadline. That is reasoned, not measured.
- A callsign keyed straight into a carrier with no word gap still does not
  set `callsign_read`: MAN-313's completed-word rule is unchanged. A spot
  still needs the validator's repeated messages, so no 2 s window spots by
  itself; what this change restores is the callsign's characters reaching
  the validator.

## What shipped

`Segment::process_fill`, called at both fill sites; the comments there and
on `CALIBRATION_SECONDS`; one clause in SPEC §2.8 G naming what a batch is;
two regression tests in `crates/manta-engine/src/listen.rs`
(`short_unkeyed_close_keeps_a_callsign_decoded_in_the_calibration_fill`,
`short_unkeyed_close_keeps_a_callsign_decoded_in_a_multi_input_fill`); and
this record. Both tests fail with the fill forced back to one piece (decoded
text `""`).

The tests use `TK1A` at 70 WPM, not the ticket's `K5ARH`. To end by about
0.85 s, `K5ARH` needs 75 WPM or more, where this decoder misreads it at
start-up (`EESARH`) even at the default timeout. `TK1A` reads as the
callsign `K1A`: the leading `T` is lost at start-up, as signals keyed from
t = 0 misread or lose their first character in the control too.

## Related

- [MAN-313 non-CW rejection](2026-10-13-man313-non-cw-rejection.md): the
  `Unkeyed` close and its batch-granular `last_char_hop`.
- [MAN-423 pre-loss hops and parked backlogs](2026-10-25-man423-pre-loss-hops-and-parked-backlogs.md):
  the held calibration losses this change keeps on their timestamps.
- [PR review convergence policy](2026-08-07-pr-review-convergence-policy.md):
  the rule that made this a ticket instead of an inline PR #283 fix.
