# MAN-530: live audio capture-ring overruns are counted, warned and reported to the engine at the exact sample

Date: 2026-11-09. Ticket: MAN-530. Source: review 2026-11-07 (bugs);
MAN-202 follow-up 6 (audio part); recorded out of scope by MAN-56.

**Status:** Implemented (branch `MAN-530`). A physical-device 500 ms decode
hold, a live `/metrics` scrape and a `manta doctor` overrun run remain
owner smoke tests: the implement container has no sound card. The
deterministic transport, wrapper, rendered-metric and doctor tests carry
both ticket scenarios.

## Problem

`AudioIqSource::from_device` read live rig audio through coppa-audio's
`CpalSource`, whose cpal callback pushes into an 8192-frame (~170 ms at
48 kHz) ring. When the decode thread fell behind, the ring rejected the
newest frames. coppa's ring counts them, but `CpalSource` keeps the count
private, and `AudioIqSource` implemented neither `health_counters()` nor
any gap hook. The loss had no counter, no log line and no gap report, and
the decoder spliced the audio either side of the hole as if it were
continuous Morse timing.

## Measured counterexample: a count is not a boundary

The research proposed exposing coppa's cumulative overflow count and
inferring where the hole sits from the count plus ring occupancy. Planning
measured that against the pinned coppa `audio_ring(8192)`:

- Parked consumer: fifty writes of 480 frames (24000 frames, 500 ms)
  leave 8192 buffered and count 15808 dropped.
- Concurrent capture: read 2048 pre-loss frames, then let the producer
  publish 2048 post-loss frames before the count/occupancy snapshot. The
  snapshot places the boundary at retained offset 10240; the true
  boundary is 8192 (6144 old frames followed by 2048 new ones, confirmed by
  sample values).

Two independent snapshots of a counter and an occupancy cannot locate a hole
while the producer runs. The boundary has to travel with the samples.

## What changed

- `crates/manta-input/src/audio_capture.rs` (private): a manta-owned cpal
  stream feeding a private `rtrb` ring of tagged frames,
  `{ sample: f32, capture_index: u64 }`. The callback advances the index
  for every frame it is handed, accepted or rejected. It snapshots free
  slots once per callback, accepts that prefix and rejects the tail (the
  same drop-newest policy as coppa's ring), adds the tail's length to
  `InputHealthCounters::queue_dropped_samples` and records one gap when a
  new contiguous loss begins. Atomics only: no allocation, lock, logging,
  sleep or channel send in the callback.
- The reader peeks the next tagged frame. An index jump after frames were
  copied ends the read before it; a jump at the start of a read sets that
  read's hooks, then the read copies only the contiguous run. No position
  is inferred from counters or occupancy.
- `AudioIqSource`: `health_counters()`, `take_missing_samples()`,
  `take_unknown_gap()`, `take_discontinuity()` and
  `with_overrun_notice()`, live path only. File-backed sources keep their
  `AudioSource` path, EOF behaviour and absent health series.
- `manta-cli`'s `LiveSourceSpec::AudioDevice` injects the warning below, so
  single-input, `[[input]]` and reopened sources use the same wording.
- `manta doctor`: two report fields and one text line (below).
- Docs: METRICS.md, CLI-JSON.md, ARCHITECTURE §3/§8/threading,
  `manta-engine::soak`'s module note, CHANGELOG, wiki/INDEX.md.

## Decisions

1. **Own the capture locally; keep coppa pinned at
   `f8a4d16df7e5776a0756943c05712038774e6c70`.** No pin bump and no
   upstream write. A cumulative count accessor in coppa would not be
   enough (see the counterexample): a future coppa integration would need
   ordered boundary metadata carried with the samples, and could then
   replace this module. That is a possible later change, not a criterion
   here. Golden vectors and fixtures are unaffected: coppa's FFT, WAV and
   Watterson code is unchanged.
2. **Tagged frames, not snapshots.** Each frame carries its capture index.
   A frame is 16 bytes on a 64-bit target, so the ring holds about 128 KiB
   instead of 32 KiB; capacity in audio time stays 8192 frames, ~170 ms.
   Indices use wrapping arithmetic; the bounded retained horizon keeps a
   forward delta unambiguous, and a test covers the wrap.
3. **A dedicated owner thread holds the cpal lifetime.** Device lookup,
   config query, stream build, `play` and destruction all run on one
   thread per opened device. Only the device request, the ring consumer,
   the health `Arc` and a startup result cross threads; startup waits for
   success or a fully contextualised error, and a failed or panicked
   startup closes the handshake and returns an error rather than leaving
   the caller waiting. Drop signals shutdown and joins outside the
   callback. No `unsafe impl Send` for cpal objects. The acquisition
   offset (MAN-259) is measured just before `play`, from `from_device`'s
   entry instant.
4. **Threshold: half a second, as HPSDR protocol 2.** A hole of `k`
   frames with `k < 24000` (0.5 s at 48 kHz, the
   `hpsdr/proto2` `sample_rate_hz / 2` rule) is reported as
   `take_missing_samples() == Some(k)` **and** `take_unknown_gap() == true`
   on the first read that starts after the hole: the clock advances by the
   count and every track's continuity-dependent decode state is
   invalidated (MAN-202 decision 7, both apply). `k >= 24000` is
   `take_discontinuity() == Some(k)` alone; the discontinuity subsumes the
   rest. Each hook drains once.
5. **"The next read" is the next read that starts after the hole.**
   Reporting on the first resumed read would advance timestamps and reset
   decode state before the buffered pre-hole audio is decoded. Every
   returned buffer lies entirely on one side of a hole, so the hooks
   always describe the gap immediately before the current read, as the
   `IqSource` contract says.
6. **Hilbert state is kept across short gaps.** Resetting the 511-tap
   filter would add its own startup transient; MAN-202 already accepts a
   filter transient at a gap because the unknown-gap invalidation drops the
   character the gap cut. A long gap (a discontinuity) starts a new
   segment, so there the source restarts the filter: the fresh segment's
   first analytic samples carry no audio from before the hole.
7. **Metric definitions.** No new family, label, port or option.
   `manta_input_queue_dropped_samples_total` is the exact number of frames
   rejected from the full ring (one mono frame is one input sample).
   `manta_input_gaps_detected_total` counts contiguous runs of rejected
   frames: adjacent dropping callbacks with no accepted frame between
   them are one gap, so the count is independent of the consumer's read
   sizes. `manta_input_dropped_packets_total` and
   `manta_input_malformed_packets_total` are 0 for audio. Counters update
   at capture time and can become visible before the engine reaches the
   buffered boundary; a loss with no later captured frame is still
   counted. `source="audio"` with one input, the configured name with
   `[[input]]`. A parked 500 ms capture at 48 kHz with an empty ring reads
   15808 and 1.
8. **Warning: first loss, then at most once per 60 s per opened source.**
   The reader observes counter growth and reuses `OVERFLOW_WARN_INTERVAL`,
   not `account_overflow`, whose eager latch would report the engine gap
   at the wrong sample. Wording, for a device quoted as `"USB Audio"`:

   ```text
   warning: audio input "USB Audio" capture ring overran because the decode thread fell behind; lost samples are counted in manta_input_queue_dropped_samples_total and gaps in manta_input_gaps_detected_total for this input (warning limited to once per 60 s)
   ```

   The device label goes through the existing quoting; a raw device name
   never reaches the terminal. Warning timing and engine gap timing are
   separate: the first read after counters grow may warn while it still
   returns pre-loss audio.
9. **Doctor reports its own run, source-generically.** `manta doctor`
   keeps the source's `health_counters()` before its wrappers consume the
   source and snapshots them after listening, so loss during startup
   calibration counts. When either count is nonzero it prints one line
   immediately before `verdict:`:

   ```text
   input: 15808 samples dropped from the capture queue; 1 input gap detected
   ```

   `sample` and `gap` pluralise by their own counts. The wording does not
   claim every gap is a cpal overrun, because a SoapySDR or HPSDR source
   reports gaps through the same counters. `DoctorReport` gains
   `queue_dropped_samples` and `input_gaps_detected`, flattened into
   `manta doctor --json` as `queueDroppedSamples` and `inputGapsDetected`,
   each omitted at zero. A source with no counters behaves as zero.
   Verdict and exit status are unchanged. Doctor never reads a running
   daemon's counters.

## Departures from the research

- D1 (a public coppa ring plus two snapshots) → tagged frames, because of
  the measured counterexample.
- D2 (one gap per read that observed growth) → one gap per contiguous
  loss episode, so counts do not depend on read-poll frequency.
- D3 (an unsafe `Send` wrapper around the stream) → the owner thread;
  health updates at capture time and gap hooks at the sample boundary.

## Supersedes

- MAN-56's exclusion of audio from `health_counters()`
  (`2026-09-04-man56-input-health-metrics.md`): audio now has counters.
- `manta-engine::soak`'s former "ring overrun needs a coppa-audio API
  addition" deviation: the ring is manta's own now. Loss inside a sound
  card or driver, before the callback, stays unmeasurable.
- MAN-202 follow-up 6 is resolved **for audio only**. Its HPSDR protocol 1
  part (outer sequence numbers instead of arrival timing) remains open.

## Not done

No resampling, no cpal error-callback redesign, no recording-format change,
no decoder promotion, no global gap-API redesign and no dashboard or alert
change.
