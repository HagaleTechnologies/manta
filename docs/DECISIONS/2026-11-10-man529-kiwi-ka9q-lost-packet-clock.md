# MAN-529: KiwiSDR and ka9q-radio report lost packets to the sample clock

Date: 2026-11-10
Ticket: MAN-529 (review 2026-11-07; MAN-202 follow-up 3)
Status: decided and built; proved with loopback fakes and the real `manta`
binary against fake receivers, no live KiwiSDR or radiod run.
Extends: D12 of [the MAN-140 ka9q-radio input record](2026-10-13-man140-ka9q-radio-input.md)
and [MAN-305 D10](2026-10-19-man305-hpsdr-protocol-2-input.md). Neither
record is rewritten.

## Context

`KiwiIqSource` and `Ka9qIqSource` detected lost packets and added them to
their loss counters (`manta_input_dropped_packets_total`,
`manta_input_gaps_detected_total`), but neither implemented
`IqSource::take_missing_samples` or `take_discontinuity`. The engine never
learnt about a loss, so its sample clock ran early by the lost span and every
later spot was stamped early. Losses add up over a session: 100 lost Kiwi
frames made every later spot 4.3 s early.

Reproduced on commit `de39a71` (plan phase):

- Kiwi driver: a loopback fake sent 130 SND frames of 512 pairs at 12000 Hz
  with a pulse at frames 20 and 60; the gapped run omitted `seq` 40-42. No
  report reached `take_missing_samples()`, and the post-gap pulse sat
  12,288 samples (128 ms) early. At 11999 Hz it sat 12,289 samples early.
- ka9q driver: a `FakeRadiod` sent 180-sample packets with `seq` 1, 2, 5, 6.
  `read()` returned 720 samples for a stream span of 1,080, with no report.
- ka9q end to end: `manta run --ka9q-group ... --json` against the CLI fake
  radiod at 96 kHz, compared with the file replay. With 33 packets (5,940
  samples) skipped at packet 1600, the W1AW spot's `sampleTs` was 5,888
  samples early.
- Kiwi end to end: `manta run --kiwi-host ... --json` against a fake KiwiSDR
  streaming a 12 kHz V1 vector at 8x real time, W1AW keyed 63 s into the
  stream. With `seq` 60-62 lost, the spot's `sampleTs` was 6,810,368 against
  6,822,656 gap-free: 12,288 samples (128 ms) early, 60 s after the loss.

## Decision

**D1. Follow MAN-305 D10.** A lost span under `sample_rate / 2` output
samples (0.5 s) goes to `take_missing_samples()`, and the engine advances its
clock without closing tracks. A loss of 0.5 s or more goes to
`take_discontinuity()` with the same count, and the engine closes its tracks
and starts a new segment. Nothing is zero-filled. This is the "starts a new
segment or pads" choice the ticket's second scenario asks for: short losses
splice, long ones start a new segment, none pad. At 96 kHz, 11 lost Kiwi
frames at 12000 Hz (45,056 samples) splice and 12 (49,152) start a new
segment; 266 lost ka9q packets of 180 samples (47,880) splice and 267
(48,060) start a new segment.

**D2. One shared mark queue, `LossMarks`** (`crates/manta-input/src/loss_marks.rs`).
Both drivers need the same bookkeeping: marks in output-stream coordinates, a
delivered counter, a read cap, promotion of due marks into pending reports,
and the 0.5 s split. Marks at one position merge and classify on their total.

**D3. A read ends at the next mark.** The `IqSource` contract places a report
immediately before the samples the most recent `read()` returned, and both
`HeldLosses` in the engine and `DecimatingSource` anchor it at the first
sample of that read. Both drivers buffer ahead (Kiwi holds a resampler chunk,
ka9q a packet queue), so a read could straddle a loss. Each driver now caps a
read at the next mark, and a report becomes pending only when a read returns
its first post-loss sample. A read that returns `Ok(0)` (a MAN-201 or MAN-489
stall) promotes nothing.

**D4. Kiwi counts the clock from the raw skipped-frame count.** The tracker's
`Gap` carries `skipped` (the `seq` delta minus one) next to `missing` (net of
malformed frames). The counters keep using `missing`, so their values do not
change for the same input. A malformed SND frame is shorter than its 17-byte
header and carries no samples, so its span is absent from the stream and the
clock counts it. A lost frame is taken to hold the last valid non-empty
frame's pair count, else the arriving frame's; with neither, no mark is
recorded.

**D5. Kiwi tracks the mark in raw coordinates and converts.** The driver
stores the negotiated `rate_in_hz` (the same rounded value the resampler
uses) and `resampler.output_delay()`, counts raw pairs pushed since connect,
and places the mark at `floor(raw x 96000 / rate_in) + output_delay`. The
plan phase measured that position exact at 12000 Hz and within 5 samples at
11999 Hz. The lost count is `skipped x pairs x 96000 / rate_in` with a
carried remainder, so three one-frame gaps at 11999 Hz report 4,096, 4,096
and 4,097, exactly the 12,289 of one three-frame gap. When a loss is
detected every output sample already produced is pre-loss, so the mark is
never behind what has been delivered.

**D6. Each tracker's existing window bounds one report.** Kiwi: 65,536
frames (`MAX_PLAUSIBLE_SEQ_JUMP`), 46.6 min at 12 kHz. ka9q: the count comes
from the RTP timestamp, `(ts - last_ts) - last_len` with wrapping arithmetic.
RFC 3550 section 5.1 and RFC 3551 section 4.1 define the RTP clock as one tick
per sampling instant, which for 2-channel IQ is one tick per complex sample.
The count is used when it is at least one sample per lost packet and at most
10 s of samples (`SEQ_WINDOW_S`); otherwise it falls back to
`missing_packets x last_len`. No wall-clock bound: protocol 2 needed one only
because it has no timestamp and a paused reader's backlog looks like a
restart (MAN-423 D5). A Kiwi `seq` gap is always a server-side drop of time
that really passed.

**D7. ka9q marks are queue-absolute.** The driver keeps `queued_total`
(samples ever pushed, counted after a `Stale` packet's truncation) and marks a
gap at the value before the post-gap packet's samples. Draining the queue
does not move a mark. A gap inside the startup stash replay is marked the
same way during `open`. `Stale` packets mark nothing and do not update the
last timestamp and length. `Baseline`, `InOrder` and `Resync` mark nothing.

**D8. Acceptance compares `sampleTs`, not wall-clock `timestamp`.** Two runs
have different session starts. A spot's `timestamp` is the session start plus
`sampleTs / fs`, so an equal `sampleTs` is the same UTC time under the same
start. Kiwi compares a gapped run with a gap-free run (both pass through the
same resampler); ka9q compares with the file replay, as the protocol 2
acceptance test does.

**D9. The ticket's "60 s later" is tested.** The Kiwi driver test
`a_spot_60_s_after_a_gap_keeps_the_gap_free_clock` places a pulse 60.2 s of
stream after a three-frame gap and requires its clock position (output index
plus every report carried at or before it) to equal the gap-free position
within 4 samples.

## Accepted risks

- A hostile or broken KiwiSDR can move the spot clock by up to 46.6 min with
  one frame, where before it could not move it at all. MAN-275 already trusts
  the IQ stream at face value, and the counters already accepted the same
  window.
- A recording still does not carry a spliced loss. A replay of a recorded
  lossy KiwiSDR or ka9q-radio session splices short losses and stamps later
  spots early, as it already does for HPSDR protocol 2. A loss of 0.5 s or
  more does reach the recording, as a `gap` record, through the existing
  `RecordingSource` discontinuity path.
- `manta check` already fails on any discontinuity. A KiwiSDR or ka9q-radio
  stream that loses 0.5 s or more inside the check window now fails it with
  `source discontinuity (N missing samples)`, as HPSDR protocol 2 does. A
  shorter loss still passes.
- Kiwi's integer-rounded resampler rate stays a constant 5 to 12 ppm clock
  error (MAN-423 and MAN-407 territory). A loss is converted at the same
  rounded rate, so it is consistent with the stream it interrupts.

## Open live-hardware item

Confirm on a live radiod that the RTP timestamp advances one tick per complex
IQ sample and that a sequence gap carries the lost samples in its timestamp.
Both repository fakes advance it that way; the research phase's reading of
radiod's source agrees, second-hand. Until then the 10 s bound and the
packet-count fallback limit what a wrong timestamp can do.

## Follow-ups

- The trackers' `Resync` outcomes (duplicate, backward or out-of-window
  jumps) still move nothing.
- radiod's mute (an RTP timestamp jump with no sequence gap and the marker bit
  set) is not handled; this change triggers on a sequence gap only.
- The recording half of MAN-202 follow-up 3 (and follow-up 9): carry a
  spliced loss in the IQ recording format so a replay keeps its clock.
- Per-driver `note_input_gap` calls for splice-damage measurement stay with
  MAN-202 follow-up 4.

## Consequences

- Every spot after a lost KiwiSDR SND frame or ka9q-radio RTP packet keeps its
  true `sampleTs`, `timestamp` and `id`. Telnet's `HHMMZ` changes only when a
  correction crosses a minute.
- A loss of 0.5 s or more now closes the tracks in progress and starts a new
  segment.
- The loss counters keep their values for the same input.
- No config, schema or recording-format change. No engine change.

## Verification

| Ticket scenario | Pinned by |
|---|---|
| 1. A Kiwi sequence gap advances the sample clock | `loss_marks.rs` tests; `kiwi.rs` `three_lost_frames_at_12000_hz_report_12288_samples_at_the_gap`, `three_lost_frames_at_11999_hz_report_12289_samples`, `a_spot_60_s_after_a_gap_keeps_the_gap_free_clock`, `small_gaps_sum_to_one_large_gap_at_a_fractional_rate`, `a_malformed_frame_inside_a_gap_counts_for_the_clock_but_not_the_counters` |
| 2. A ka9q RTP gap is reported the same way | `ka9q/mod.rs` `lost_samples_come_from_the_rtp_timestamp`, `a_lost_packet_is_reported_after_the_read_that_starts_past_it`, `a_gap_inside_the_startup_stash_replay_is_reported_at_its_position`, `the_lost_count_follows_the_timestamp_not_the_packet_size`, `stale_packets_add_no_report_and_no_samples` |
| 1, end to end through the real `manta` binary | `crates/manta-cli/tests/kiwi_lost_frames_acceptance.rs` `a_kiwi_seq_gap_keeps_a_spot_60_s_later_on_the_gap_free_clock` (gapped and gap-free `sampleTs` within 256 samples, 60 s after the gap) |
| 2, end to end through the real `manta` binary | `crates/manta-cli/tests/ka9q_input.rs` `ka9q_lost_packets_keep_the_spot_clock_true` (33 packets dropped at 3.0 s; live `sampleTs` within 256 samples of the file replay) |
| Gap policy split at 0.5 s | `kiwi.rs` `eleven_lost_frames_splice_and_twelve_start_a_new_segment`; `ka9q/mod.rs` `a_half_second_loss_is_a_discontinuity` |

No live KiwiSDR or radiod was run.
