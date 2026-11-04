# MAN-489: every live input reports a stalled stream inside the 5 s bound

Date: 2026-11-04
Ticket: MAN-489 (follows D3/D5 of `2026-11-02-man201-callsign-roles.md`)
Status: decided and built; proved with fake sources, no hardware run (the ticket's scope note).

## Context

MAN-201 made the engine settle a track's last held call when its input
reports a stall: `Ok(0)` plus one consumable `IqSource::take_stalled` flag.
Only ka9q reported inside D3's 5 s bound. Kiwi errored after ~10 s of *no
messages at all* — and a peer that kept pinging while sending no `SND`
reset that counter forever, so it never reported at any latency. Both HPSDR
protocols errored at ~10 s. Grouped HPSDR had no `take_stalled` at all and
bypasses `ReconnectingSource` (`multi_run.rs`), so its held call settled
only when a replacement generation delivered samples. Soapy and audio
reached the engine as `Err`-bridged stall reports at ~2 s and ~5.0–5.3 s —
inside the bound, but only through losing the stream: same-stream resume
was impossible, and audio's report could land past 5 s wall-clock.

## Decision

Every live source reports its own soft stall, ka9q-style, before its
hard-failure bound:

- **Shared episode state** (`manta-input`'s private `stall` module):
  a one-shot latch armed only by *sample delivery* (all-zero samples are
  delivery), fired once per outage at a soft deadline, re-armed by the
  next delivery. Transport keepalives, status traffic and control frames
  never re-arm it. `hard > soft` guards the report, so a test (or config)
  with the hard bound at or below the soft one keeps the hard failure as
  the outage's only outcome.
- **4.5 s soft deadline** for Kiwi, both HPSDR protocols, audio and ka9q
  (D3's 5 s minus the 250 ms receive-poll margin; ka9q's own 5 s `SETTLE_AFTER`
  moved to the same value, its report contract unchanged). **SoapySDR uses
  1 s**, ahead of its ~2 s retry limit.
- **Each source's clock follows usable samples only.** Kiwi stamps the
  last non-empty `SND` payload (ping/MSG/unknown frames cannot hide the
  outage). HPSDR protocol 1 stamps the last valid Metis packet on the
  shared `Inner`; protocol 2 keeps its `last_valid_iq.max(entered)` clock,
  so a parked consumer still cannot look stalled. Audio's wait episode
  (`waiting_since`) survives the soft report, so its 5 s device-loss bound
  cannot restart mid-outage; Soapy's retry counters likewise persist
  across the soft return.
- **Hard limits stay**: ~10 s Kiwi/HPSDR (protocol 1's bound is now wall
  clock since the last valid packet — the same budget the old 40 x 250 ms
  consecutive-timeout count expressed — and the malformed-datagram bound
  moved onto `Inner` so bounded pump windows cannot reset it), 5 s audio,
  ~2 s Soapy timeouts. A soft report never resets a failure budget.
- **Protocol 1's pump is bounded.** `pump_for(wait)` returns to its caller
  after one receive window, and `read` releases the shared mutex between
  windows, parking a few milliseconds (`PUMP_HANDOFF`) so sibling DDC
  readers on one socket each observe their own deadlines — an immediate
  re-lock would let one member monopolize an unfair mutex through the
  whole hard-stall period (the race this ticket's first concurrent test
  caught). Shared outage evidence (`last_valid_packet`) is never cleared
  by one member taking its flag, so every productive member reports the
  same outage independently.
- **`GroupSource` forwards the report and keeps the generation.** An inner
  flagged zero is forwarded once (per-member latch armed by samples the
  wrapper actually returned), the generation is not retired or reopened,
  and the wrapper keeps reading the same stream for the resume — a stream
  that stopped sending is not a radio that needs replacing. A productive
  member's real failure (or a sibling retiring the generation) is likewise
  reported once *before* the member parks in `group.wait`, so the engine
  settles what it holds while the replacement opens. Stop keeps ordinary
  EOF precedence, and the outage boundary arithmetic (`resumed`/gap) is
  unchanged by the report.
- The engine, validator, recording and replay paths are untouched: the
  settle-at-track-close contract (`listen.rs`/`validator.rs`) already
  consumes `take_stalled` generically, pinned by the existing stall ≡
  recorded-gap test.

## The ticket premise, corrected

"Audio may never report" was not the code's behaviour: audio reported
device loss at its 5 s `DEVICE_STALL_TIMEOUT` through `ReconnectingSource`
(MAN-231). What audio could not do was report *inside* the bound on the
same stream and resume it; that is what the source-level report adds. The
never-reporting input was a ping-fed Kiwi.

## Evidence (fake sources; no hardware)

Short-duration episode tests cover two successive outages per source;
production constants are asserted below their hard limits
(`stall::tests`). Per source: Kiwi ping-fed outage reports once and
resumes on the same socket (`kiwi.rs` `a_ping_fed_iq_outage_reports_a_stall_once_and_resumes`);
protocol 1's two concurrent DDC readers each report promptly, re-arm, and
still hit the hard failure on a permanent outage (`hpsdr.rs`
`concurrent_ddcs_each_report_a_stalled_shared_stream_and_re_arm`);
protocol 2 reports with status streaming, re-arms, fails hard
(`proto2/mod.rs` `a_stopped_radio_reports_a_soft_stall_once_re_arms_and_still_fails_hard`);
audio reports before hard failure and resumes on the same ring, zero
samples counting as delivery (`audio.rs`); Soapy's production retry loop,
driven by scripted timed reads, reports at 1 s before its retry limit and
preserves the budget (`soapy.rs`). `hpsdr_group.rs` proves one-shot
forwarding without retire/reopen, sibling continuity, pre-wait failure
reports and no duplicate after loss; `reconnect.rs` proves the wrapper
forwards a source stall without opening a replacement. Timing note: the
5 s promise assumes an actively read source and normal OS scheduling;
this program is not a hard real-time system. Soapy's loop could not be
compiled or run in the implementing container (no native `libsoapysdr`);
its code and tests are written against the pinned crate and await the
`test-soapy` CI job.

Deferred to a follow-up (declared in the run's deviations): the
engine-level held-call emission tests (`listen.rs` gate), the child-process
protocol-2/Kiwi acceptance latency measurements, and the
production-duration timing smoke tests per source family; the shared
5 s bound they would measure is otherwise pinned by the unit suites above.
