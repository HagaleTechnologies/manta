# MAN-533: a live input that delivers no usable samples for 30 s is reconnected

Date: 2026-11-09
Ticket: MAN-533 (review 2026-11-07; MAN-275 finding 9)
Status: decided and built; proved with fake sources, no hardware run.
Supersedes: the "Filed (recommend-file)" disposition of finding 9 and the
"unknown-tag frames are discarded without any counter" residue of finding 10
in [the MAN-275 threat model](2026-10-17-man275-kiwi-threat-model.md). That
record is not rewritten.

## Context

A KiwiSDR that keeps sending frames manta cannot use held
`KiwiIqSource::read` forever. Any frame reset the driver's timeout counter,
so only 40 consecutive 250 ms *silent* timeouts ended a connection.
Unknown-tag binary frames, text frames, unrecognised `MSG` keys, pings and
one-sample `SND` frames all counted as traffic. The MAN-489 soft stall is a
one-shot latch armed only by delivery, so it never reopened anything, and
`ReconnectingSource` never regained control while the driver looped.
HPSDR protocol 1 fed only malformed datagrams held its read the same way
until 10,000 had arrived.

Reproduced on commit `7020ede` (plan phase):

- Driver: a fake that completes the Kiwi handshake and then sends
  `XYZ\x00\x01\x02\x03` every 200 ms. After 12 s `read()` had not returned,
  and 60 discarded frames counted nothing
  (`malformed=0 dropped=0 gaps=0 queue_dropped=0`).
- Daemon: the real `manta run` against the same fake, sampled at 35 s.
  `/healthz` said `source kiwi: healthy` and `decode: stalled 34s`;
  `manta_source_health{source="kiwi"} 1`,
  `manta_source_outages_total{source="kiwi"} 0`,
  `manta_input_malformed_packets_total{source="kiwi"} 0`; one connection
  accepted; no `lost:` or `reconnect` line on stderr.

## Decision

**D1. Measure active read time with no usable sample, as a budget.** Time
spent inside a driver's receive loop is spent from the budget, and a usable
sample refills it. Time a consumer spends away from `read()` is never
spent: a multi-input daemon opening other inputs, or an HPSDR sibling
parked on the shared socket, cannot trip it (the pattern protocol 1's
`silent_waited` already used). The budget is independent of the soft-stall
latch and runs from open for a stream that never delivers.

**D2. "Usable" means samples that reach the driver's output.** For Kiwi,
resampler output appended to `pending`; for HPSDR protocol 1, a valid Metis
packet. Kiwi's MAN-489 stamp (`last_usable_iq`, `stall.note_delivery()`)
moves to the same point, so a one-sample `SND` trickle no longer re-arms
the soft stall forever.

**D3. The bound lives in the driver, through one shared helper, and raises
a typed error; the reconnect layer owns everything after that.**
`manta-input`'s `stall` module gains `NO_USABLE_SAMPLES_AFTER`,
`NoUsableSamples`, `is_no_usable_samples` and `UsableSampleBudget`.
`ReconnectingSource` and the grouped-HPSDR `GroupSource` classify the
error, record the reason and reopen through their existing `Err` paths.
This departs from the ticket's "generic detector in the reconnect layer":
a detector there cannot run while the driver's `read()` loops, which
MAN-275's own validation already noted. Rejected: an idle-return `Ok(0)`
contract (eight forwarding implementations, and grouped HPSDR, `doctor`,
`soak` and `source_check` read an unflagged `Ok(0)` as end of stream), and a
watchdog thread (it cannot interrupt a blocked read, leaks a socket per
cycle, and `IqSource` is not `Send`).

**D4. Every existing "no samples within the bound" error carries the same
type, with its text unchanged.** Kiwi silence, HPSDR protocol 1 silence and
its 10,000-malformed bound, protocol 2's stall and malformed bound, ka9q's
stall, audio's no-samples bound and SoapySDR's silence budget. Not typed:
SoapySDR's overflow bound, `HPSDR session retired`, a WebSocket close,
protocol or socket errors, and the reopen guards.

**D5. No `Ok(0)` counting in the wrapper.** An unflagged `Ok(0)` stays end
of stream, and flagged zeros are one-shot per outage.

**D6. One window, 30 s, a `pub const`, not configurable.** It exceeds the
4.5 s soft report, the 10 s silence bounds and the 20 s pauses in
`kiwi_stall_acceptance.rs`. Sources with shorter no-sample bounds (SoapySDR
about 2 s, audio 5 s, ka9q and protocol 2 10 s) keep them; they already
win.

**D7. Kiwi counts unknown-tag binary frames and text frames in
`malformed_packets`, nothing else.** A receiver never sends either on its
sound channel. Unknown `MSG` keys, pings, pongs and header-only `SND`
frames are not counted. The `SND` sequence tracker is untouched, so a real
gap around an unknown frame is still reported.

**D8. `/status` gains an additive `sources` array.** One row per source
registered through the health sink, sorted by name: `name`, `healthy`,
`losses_total`, `last_loss_reason` and `last_loss_age_seconds`. It is
omitted when empty, and `schema_version` stays 1. The reason is a `String`
on the wire, filled from the closed `SourceLossReason` vocabulary
(`no usable samples`, `read failed`), so an older `manta status` still
reads a newer daemon's label. `manta status` prints a Sources table; its
exit code is unchanged.

**D9. Losses travel through a new loss sink, not the health sink.**
`ReconnectingSource::with_loss_sink` is told every lost connection, before
the unhealthy edge, so a never-productive source's repeated losses show
while it stays unhealthy. A failed reopen attempt is not a loss.

**D10. Grouped HPSDR records the failing member's reason for every member
of the generation** (`failed_because`); a member's unflagged `Ok(0)` is
`read failed`.

**D11. Docs.** This record, plus one `wiki/INDEX.md` pointer row. No
`wiki/pages` edit, and no edit to the MAN-275 record.

## The window and the reasons

| Reason | When |
|---|---|
| `no usable samples` | A source kept reading for `NO_USABLE_SAMPLES_AFTER` (30 s) with no usable sample, or hit its own shorter no-sample bound (D4). |
| `read failed` | Any other lost connection: a closed socket, a protocol error, a device error. |

The log line keeps its format. The new causes read
`source kiwi lost: KiwiSDR delivered no usable samples for 30 s; reconnecting in 2s`
and
`source hpsdr lost: HPSDR device delivered no usable samples for 30 s: no valid Metis packet arrived; reconnecting in 2s`.
A never-productive receiver climbs the existing backoff ladder (2 s, 4 s,
up to 60 s); at the cap that is one reconnect about every 90 s instead of
one connection held forever.

## Consequences

- `MantaInputDown` now fires for a receiver that holds its connection open
  without usable samples; before, the source read healthy while `/healthz`
  said `decode: stalled`.
- `manta_input_malformed_packets_total{source="kiwi"}` widens to include
  unknown-tag and text frames ([METRICS.md](../METRICS.md)).
- Nothing changes for a stream that delivers samples. Interleaved
  unknown-tag frames are still discarded in place, and the MAN-489 soft
  stall still settles held calls first.

## Verification

| Ticket scenario | Pinned by |
|---|---|
| 1. Useless frames count as a stall | `kiwi.rs` `unknown_tag_frames_end_in_no_usable_samples_within_the_window`; `reconnect.rs` `a_no_usable_samples_error_reports_its_reason_and_reopens`; `tests/kiwi_no_usable_samples.rs` `a_kiwi_sending_only_unknown_tags_is_reconnected_with_reason_no_usable_samples` |
| 2. The detector covers every live source | `hpsdr.rs` `a_malformed_only_stream_ends_in_no_usable_samples_after_the_window`; the typed-bound assertions in the audio, ka9q, protocol 2 and SoapySDR tests; `hpsdr_group.rs` `a_member_no_usable_samples_error_fails_the_generation_with_its_reason` |
| 3. Unknown-tag Kiwi frames are counted | `kiwi.rs` `each_unknown_tag_and_text_frame_counts_one_malformed_packet`; the daemon test's `manta_input_malformed_packets_total` assertion |

The daemon test runs at production durations (about 35 to 40 s): a fake
KiwiSDR sends only unknown-tag frames on every connection, and the test
requires the `source kiwi lost: ... no usable samples` line no sooner than
28 s after `listening`, a second connection, a `/status` `kiwi` row with
`last_loss_reason` `no usable samples`, `manta_source_outages_total`
of at least 1 and a non-zero malformed counter. No live KiwiSDR or HPSDR
hardware was run; the SoapySDR tests are gated by CI's `test-soapy` job.
