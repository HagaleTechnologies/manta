# 2026-10-16 — MAN-259: a reconnect's outage ends where acquisition began

**Status:** Implemented.

**Ticket:** MAN-259. Corrects the outage anchor of
[MAN-73's reconnect design](2026-10-05-man73-source-reconnect.md); deferred
from the Codex review of PR #207 ("Anchor recovery before acquisition
starts").

## Problem

`ReconnectingSource` reports a reconnect's outage, in samples, through
`IqSource::take_discontinuity()`, and `manta_engine::listen` advances the
sample clock by that many samples before the reopened source's first
sample. The outage ended where the reopened source's first `read()` began
(PR #207 round 3), which is after the opener has returned. Every live
driver tells its hardware to start streaming *inside* the opener:

| source | start trigger inside `open` | work after it, before `open` returns |
|---|---|---|
| KiwiSDR | `SET mod=iq` in the post-handshake `SET` loop | the FFT resampler build |
| SoapySDR | `stream.activate` | none of note; unbounded in a network driver |
| HPSDR | the start command, after the C&C burst | building the per-DDC handles |
| audio | `cpal` `start` | wrapping the ring |
| ka9q-radio | the data-group join | the status wait, up to `settle` (1500 ms default) with no SSRC, then a stash replay |

Samples the device captured between the trigger and the opener's return
were counted twice: once as outage, and again as data stamped to start
after that outage. Every spot after a reconnect was stamped late by that
span: for ka9q-radio by the span its startup stash actually replayed -- up
to the whole settle window at rates the stash holds, and less on faster
channels (the stash caps itself; see the residual).

Reproduced at `91c9b72` with a fake clock: an opener that takes 2 s and
returns a source streaming from the start of that call produced a gap of
3.0 s (288 000 samples at 96 kS/s); the true signal-silent gap is the 1 s
backoff (96 000 samples).

## Decision

1. **The opener reports where acquisition began.** A new trait method,
   `IqSource::acquisition_start_offset() -> Duration` (default `ZERO`),
   is the offset from the connection's `open` call starting to
   immediately before its start trigger: the last instant certainly
   signal-silent. Each driver takes `Instant::now()` on entry and again
   just before the trigger in the table above, and stores the
   difference. The offset is relative, like `take_discontinuity`'s sample
   count, so `manta-input` stays independent of `manta-cli`'s injected
   `ReconnectEnv` clock. Widening the `Opener` type to return
   `(source, Instant)` was rejected: the signal is read off the returned
   source, like `health_counters` and the sample-rate, centre and passband
   checks already are.
2. **The outage ends at the reopen worker's entry into the opener, plus
    that offset.** The reopen worker timestamps its own entry into the
    open call (`ReconnectEnv::clock()`, a `Send` read on the injected
    clock) and sends that base back with its result, after the backoff
    sleep (the sleep is real outage). When it installs the reopened
    source it computes the anchor as that base plus the offset, clamped
    to the install instant so no driver can end the
    outage in the future. The first productive read uses this anchor
    instead of its own start. The anchor is never later than that read's
    start, so PR #207 round 3's rule (the first read's capture time is not
    outage) still holds. Handshake time before the trigger (Kiwi's
    WebSocket handshake and `sample_rate` wait, HPSDR's C&C burst,
    ka9q-radio's group resolve) stays in the outage.
3. **Wrappers forward it.** `DecimatingSource` and `manta-cli`'s
   `FixedCenterFreqSource` sit between the driver and
   `ReconnectingSource` inside the opener, and both forward
   `acquisition_start_offset`. The trait default would otherwise move the
   anchor back to the start of `open`, dropping the handshake from the
   outage. The same review found `DecimatingSource` did not forward
   `take_discontinuity`; it now does, rescaled from the inner rate to its
   own `sample_rate()` (the unit the trait reports). That was harmless
   until now only because no raw driver reports a gap.
4. **ka9q-radio anchors later than its join when the stash evicts**
   (validate round 1). The startup stash keeps only its newest
   `RCVBUF_BYTES` (4 MiB), so on a channel fast enough to overflow it
   during the wait the replay starts at the first packet of the chosen
   channel the stash still holds, and the offset reports that packet's
   arrival (each stashed packet keeps its receive `Instant`); with nothing
   of the channel retained, the join remains the anchor. The first
   implementation anchored at the join on the false belief that the stash
   replays everything queued from it: an evicted opening never reaches
   `read`, so anchoring at the join stamped post-reconnect spots *early*
   by the evicted span -- at 1.536 MS/s `f32le` with no SSRC the stash
   holds ~0.32 s of a >= 1.5 s wait, >= 1.18 s early, past the ticket's
   one-second tolerance (at 768 kS/s, >= 0.85 s). At the project's
   reference 192 kS/s the stash holds the whole window and the join and
   first-retained anchors coincide.
5. **The PR review round tightened the anchor's edges** (PR #342). Four
   second-order gaps, each dropping signal-silent time from the outage
   and so stamping later spots early: the base instant moved from the
   reading thread's spawn-side sample into the worker itself (decision 2
   above), so the OS's scheduling delay for the `reopen-<kind>` thread
   stays counted; KiwiSDR sends `SET ident_user` before sampling its
   offset, immediately before the SET loop whose first command is the
   `SET mod=iq` trigger, so a blocking `ident_user` send on a slow or
   backpressured connection stays outage; ka9q-radio's fallback anchor is
   the data socket's first landed group membership rather than an
   instant taken before the socket even existed, so socket setup and the
   interface attempts stay outage; and its retained-packet anchor
   accepts only a packet the replay would accept
   (`packet_yields_samples` applies `accept_rtp`'s own checks: SSRC,
   RTP payload type, at least one decodable IQ sample), so a packet with
   an empty or undecodable payload or a foreign payload type -- which
   `read` never returns -- cannot end the outage before the first
   replayable sample.

## Residual

A delay between the start trigger and the device's first captured
sample is excluded from the gap, so a spot after a reconnect can be
stamped early by that delay; for the other four drivers it is far
smaller than the opener time this record removes from the gap. The time
a first read spends waiting for data was excluded before this change
too.

For ka9q-radio that residual covers two cases decision 4 does not close.
When the stash retained nothing of the chosen channel (the channel sent
no RTP during the open wait, or a foreign channel's flood evicted all of
it), the anchor falls back to the join and the delay from the join to
the channel's first packet stays excluded. And the anchor is the first
retained packet's *receive* time, which is late by at most one drain
pass (~1 ms) after its true arrival. Neither approaches one second on a
single-channel group; a shared group whose foreign traffic dominates the
stash can stretch the first, and is a decision for the field-node ticket
if it ever shows up.

The opener's own runtime is still unbounded for SoapySDR (MAN-73's
documented gap); this record only re-anchors the outage around it.

## Verification

- `reconnect.rs`: `discontinuity_ends_at_acquisition_start_not_opener_return`
  (2 s open, offset `ZERO`: the gap is the 1 s backoff alone),
  `pre_acquisition_handshake_time_still_counts_as_outage` (offset 500 ms:
  1.5 s), `an_offset_longer_than_the_open_call_clamps_to_the_install_instant`
  (offset 5 s, open 2 s: 3 s). The first two failed at `91c9b72` with
  288 000 samples. The MAN-73 and PR #207 round-3 tests pass unchanged.
- `ka9q`: `acquisition_start_offset_is_measured_before_the_settle_wait`
  opens against the mock radiod with no SSRC and asserts the offset is
  below the settle window. An offset taken where `open` returns measured
  452 ms against the fixture's 350 ms settle and failed it.
  `acquisition_start_offset_lands_at_the_first_packet_retained_past_the_stash_cap`
  (validate round 1) streams twice the stash cap's worth of 6.144 MS/s
  RTP through a no-SSRC open, asserts the cap evicted the opening, and
  pins the anchor past the first sent packet (a join anchor measured
  under 1 µs and failed it).
- Forwarding: `decimate.rs`
  `acquisition_start_offset_forwards_to_the_inner_source` and
  `take_discontinuity_forwards_to_the_inner_source`; `main.rs`
  `fixed_center_freq_source_forwards_acquisition_start_offset`.
- KiwiSDR, HPSDR and audio record the offset with the same two-`Instant`
  pattern; their test harnesses cannot reach a real start trigger, so
  only the build and their existing suites cover them. SoapySDR cannot be
  built here (no `libsoapysdr`); CI's soapy leg compiles and tests it.
- Review round (PR #342):
  `discontinuity_includes_the_delay_before_the_reopen_worker_enters_the_opener`
  (a worker 500 ms late to enter the opener keeps that delay in the gap;
  with the base restored to the spawn-side sample it failed with 96 000
  vs 144 000 samples) and
  `acquisition_start_offset_skips_packets_that_cannot_yield_samples`
  (empty-payload RTP with the right SSRC cannot anchor; the SSRC-only
  predicate anchored at the first empty packet, 150 ms into the open).
  The KiwiSDR and ka9q join-instant changes are structural -- no harness
  can make a `SET` send block or delay a socket's creation -- and are
  covered by the build and the suites above.
