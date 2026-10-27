# MAN-423: hops buffered before a spliced loss keep their timestamps; a parked protocol-2 backlog keeps its anchor

Date: 2026-10-25. Ticket: MAN-423, two findings deferred from Codex review
of PR #357 (MAN-305) under
`docs/DECISIONS/2026-08-07-pr-review-convergence-policy.md`: round 3,
comment `4340566529` (`crates/manta-engine/src/listen.rs`), and round 4,
comment `4340858510` (`crates/manta-input/src/hpsdr/proto2/mod.rs`), which
Tony added to the ticket on 2026-10-21. This record partly supersedes
[MAN-305](2026-10-19-man305-hpsdr-protocol-2-input.md) D10-D12, as set out
under Consequences.

## Context

**Pre-loss hops stamped late.** When `IqSource::take_missing_samples()`
reported a spliced loss of `k` samples, `listen_with_observers` and
`run_input_front` added `k` to `seg_base` before feeding the post-loss chunk
to the channelizer. `Channelizer::process` keeps a `filter_len` (8N = 16,384
samples at 192 kHz) sliding window across calls, and hop `m`'s timestamp
`seg_base + (m - pad_hops)·hop` is the start of its window. A hop whose
window reaches past the loss is emitted only after the post-loss read, so it
was mapped from the bumped `seg_base` and came out `k` samples late. MAN-305
D12 claimed the main-loop add "re-times nothing already buffered"; that was
wrong. Its calibration-fill fix split the fill at each loss, but splitting a
`process()` call does not split the channelizer's buffered hop timeline, so
events within one filter length before a fill loss stayed late too.

Measured at `40d275a` on the V1 192 kHz fixture, 50 datagrams (11,900
samples) lost, event times against the loss-free run:

| Loss | Event | Before | After |
|---|---|---|---|
| 2.095 s (402,240), single- and multi-input, whole-chunk or 238-sample reads | 397,312 | 409,212 | 397,312 |
| 50 at 400,000 and 20 at 404,000 | 397,312 | 413,972 | 397,312 |
| 211,312, inside the calibration fill, both paths | 205,312 | 217,212 | 205,312 |

The existing spot tests allow one hop (512 samples), so they passed with
the bug. The new tests assert exact event times.

**A parked source's overflow read as a zero-length restart.** A protocol-2
source that is open but not being read keeps receiving datagrams: a
multi-input run parks every opened source until all openers report
(`open_inputs`), and a stalled back end can pause one mid-stream. Once the
socket buffer is full, further datagrams are dropped. When reading resumes,
the backlog drains in microseconds, so the wall clock since the last
accepted datagram (D10's plausible-jump bound and D11's restart estimate) is
near zero. The real jump after the backlog then exceeded the fixed
8,192-datagram allowance, counted as a restart, and reported about 0
missing samples, leaving every later timestamp early by the dropped span.
Against the fake radio: open, ten datagrams queued, a 1 s park, then a jump
of 8,492 gave `Some(0)` and no dropped datagrams.

## Decision

**D1. Losses are held at their boundary.** `HeldLosses` (a queue of
`(at, k)`) lives in the engine's `Segment`, so a new segment starts with
none. `at` is the segment sample the post-loss samples begin at:
`seg_consumed` in the decode loop, `filled` in the calibration fill.
`seg_base` still takes `k` as soon as the loss is read, so every reader of
the clock at the read position (`seg_base + seg_consumed`: batch `end_ts`,
`rtty.restart`, the live tap's tick, the discontinuity rebase) is unchanged.
The hop-to-timestamp closure becomes `seg.held.hop_ts(..)`: a hop at
segment position `pos` is stamped `seg_base + pos - Σ{k : at > pos}`. Only
hops at or after a loss's boundary carry it, however the reads were split.

**D2. The calibration fill uses the same queue.** MAN-305's piece split and
`calib_losses` are gone; the fill is processed in one call again, so it
makes one per-batch callback per calibration block. Multi-input
`fill_end = seg_base + filled`, the same value as before because `seg_base`
now carries the losses, so the replay merge order does not change.

**D3. Discontinuities.** Held losses are already in `seg_base`, so the
decode loop's rebase `seg_base += seg_consumed + gap` keeps them on the
clock and the new segment holds none. A discontinuity during the fill clears
the queue and adds `filled + gap`, the same total as before.

**D4. Held losses are dropped by samples processed.** `record` drops
entries with `at + filter_len <= processed` before pushing; the decode loop
passes `seg_consumed`, the fill passes 0 (nothing is processed until it is
full). Dropping by boundary instead re-timed an event at 13,824 when two
fill losses were held. Correctness does not depend on dropping (an entry
with `at <= pos` subtracts nothing); it only bounds the queue to about
`filter_len / 238 + 1` entries.

**D5. A pause in reading anchors protocol 2's clock.** `read()` measures
the time since the previous `read()` returned (or since `open()`'s run = 1).
A pause of at least 100 ms (`PARKED_AFTER`) opens a window
`Parked { from, accounted: 0, left: park_window }` anchored where the pause
began; each pause opens its own. While a window is open, `handle_iq` judges
a gap or restart against `max(since_last, (now - from) - accounted / fs)`,
for both D10's plausible-jump bound and D11's restart estimate. Each
accepted datagram adds its 238 samples plus whatever it reported to
`accounted` and uses up one of `left`; at 0 (`MAX_BUFFERED_DATAGRAMS`, one
full socket buffer) the window closes. Stale datagrams count for nothing.
With no window open the bound is exactly the old one. 100 ms is below the
time the radio needs to outrun the allowance (81 datagrams at 192 kS/s)
and far above a decode loop's 1.24 ms between datagrams.

Measured with the fix: the 1 s park then a jump of 8,492 reports
`Some(2_021_096)` (8,492 × 238) and 8,492 dropped datagrams; a restart behind
the same park is measured from the pause, less the ten drained datagrams.

**D5a (PR review). The pause bounds a restart's outage; it is not the
outage.** A radio that restarts early in the pause keeps streaming, so its
post-restart datagrams are queued behind sequence 0, and reporting the whole
unaccounted pause as missing would run every later timestamp ahead by what
they hold (and count a later overflow gap again). On a restart while a
window is open, `restart_outage` receives the socket's backlog without
blocking (at most `MAX_BUFFERED_DATAGRAMS`, kept in `look_ahead` and handled
next as if just received), counts the radio time every queued DDC0
datagram represents -- judged by a copy of the sequence tracker, so a
plausible gap counts its lost datagrams, a stale one nothing, and a further
restart in the backlog only itself -- and reports `(now - from) - accounted /
fs` less that time. A later restart among those datagrams measures the same
way over what is still queued, so the pause is assigned once however many
restarts it holds (second review round:
`two_restarts_behind_a_park_assign_the_pause_once`, two 60-datagram runs; the
first version counted only the run up to the next restart, leaving the
second run's 14,280 samples ahead). Outside a window the estimate is the old `since_last`.
Test `an_early_restart_behind_a_park_does_not_count_its_backlog_twice`: five
pre-restart datagrams, then 0..=119 with 20 lost, behind a 0.5 s pause; every
sample accounted after the pause adds up to the pause's wall clock (before:
about 28,300 samples, the run's 119 datagrams, ahead). Real radios pace their
datagrams, so the queued run is radio time since the restart;
`radio_restart_resyncs_with_a_wall_clock_discontinuity`, whose fake sends
its ten restart datagrams in one burst after a 300 ms pause, now expects the
nine queued behind sequence 0 to come off the estimate.

## Consequences

- This record supersedes, in the MAN-305 record, D12's sentence "In the main
  loops the add lands between reads, so it re-times nothing already
  buffered", D12's calibration piece split, and D10/D11's "since the last
  valid datagram" for a source that was not being drained.
- `manta_input_dropped_packets_total{source="hpsdr"}` for protocol 2 now
  counts a socket-buffer overflow during a pause in reading as dropped
  datagrams ([docs/METRICS.md](../METRICS.md)). Names, labels and
  `manta_input_gaps_detected_total` are unchanged.
- File replay is unchanged: no file source reports losses, so decoder
  output under the CHANGELOG rule does not move.
- An event whose window straddles a splice can still fire on a different
  hop than without the loss. That is the splice changing the window's
  content, not the clock.
- Tests: `held_losses_map_only_hops_at_or_after_their_boundary`,
  `held_losses_are_dropped_only_once_processed_past`,
  `hops_buffered_before_a_loss_keep_their_timestamps` and the
  `*_keep_pre_loss_event_times` tests in `crates/manta-engine/src/listen.rs`;
  `a_gap_past_the_allowance_after_a_park_is_a_loss`,
  `a_restart_behind_a_park_is_measured_from_the_pause`,
  `a_drained_source_keeps_the_fixed_allowance` and
  `a_park_window_ends_after_its_datagrams` in
  `crates/manta-input/src/hpsdr/proto2/mod.rs`.
