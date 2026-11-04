# 2026-11-01 — MAN-202: an input gap of unknown length drops the character it cut instead of decoding through it

**Status:** Implemented (branch `MAN-202`). The `SourceBlock`/`EngineSession`
merge named in the ticket's Outcome is split out (D1). The hardware check (a
starved host with an RSP1B or RTL-SDR) is an owner-run step, not an
acceptance criterion: CI's SoapySDR leg drives the `type=null` device, which
cannot overflow.

**Ticket:** MAN-202. Source: investigation 12 of
`docs/DECISIONS/2026-09-09-decoder-recall-research.md` (rank 12 of 20). The
2026-10-31 scope note narrows it to overflow gaps plus the listen/decode
parity scenario, and allows the full merge to split. Related: MAN-180, MAN-194.

## Problem

A SoapySDR driver overflow drops samples of unknown count. Before this
change the SoapySDR adapter retried the read, counted the overflow (MAN-233)
and handed the engine the next samples as if they were contiguous. The
decoder then read the splice as Morse timing. A silently dropped 150 ms or
400 ms span turned a `K` into `M` or `T`, or `W1AW` into `T1AW`, with no
closure and no flag.

`IqSource` had two counted gap hooks. `take_discontinuity()` is a known-length
outage: the engine closes every track and rebuilds the segment.
`take_missing_samples()` is a known-length loss the source spliced over: the
engine only advances the clock. Neither can say "lost, count unknown".
Reporting `take_discontinuity(Some(0))` invents a count, which `GapScaler`,
`ReconnectingSource` and `GroupSource` then sum.

## What changed

- `crates/manta-input/src/lib.rs`: a third gap hook,
  `IqSource::take_unknown_gap() -> bool` (default `false`, returns `true` once
  per loss), and `manta_input::ReadContinuity`, which takes all three hooks
  in the engine's order (missing samples, discontinuity, unknown gap). Every
  production wrapper forwards the new hook: `DecimatingSource` (ORed across
  the inner reads of one outer read, filters not reset), `PacedSource`,
  `LoopingSource`, `RecordingSource` (forwards, does not record),
  `ReconnectingSource` (from the current inner source; a reopen's
  discontinuity covers a pending one from the old inner), `GroupSource`,
  `FixedCenterFreqSource` and doctor's `SampleBoundedSource`.
- `crates/manta-input/src/overflow.rs` (new, not feature-gated):
  `account_overflow` and its MAN-233 tests moved out of `soapy.rs`, plus
  `UnknownGapLatch`. Every `ErrorCode::Overflow` marks the latch, and
  `SoapySdrIqSource::take_unknown_gap` takes it. The latch tests run in the
  default build, so this container and the validate container run them.
- `crates/manta-decode`: `TrackDecoder::note_input_gap()` invalidates the
  decoder's continuity-dependent state (D3) and advances an internal
  `continuity_epoch`. `Demod::break_continuity()` drops the open run and
  restarts an init window.
- `crates/manta-engine/src/track.rs`: `first_hop_after_gap(pos, hop) =
  pos / hop + 1`, the first hop whose channelizer window holds a post-gap
  sample. `TrackManager::note_input_gap(first_hop)` queues the gap; when
  `process_hops` reaches that hop, every track drains its refiner backlog
  into `pending` (the MAN-194 order), resets the refiner, and queues a
  marker by `sample_ts`. The pending drains apply the marker in order, so the
  decoder sees the gap at the same hop whatever the read sizes.
- `crates/manta-engine/src/listen.rs` and `lib.rs`: `listen_with_observers`
  (calibration fill and main loop), `run_input_front` and `decode_source`
  read their per-read gap reports through `ReadContinuity::take` only. Each
  end-of-stream check takes both `take_discontinuity()` and
  `take_unknown_gap()`.
- Tests: `crates/manta-engine/tests/replay_live_parity.rs` (Scenario 2) and
  `crates/manta-engine/tests/unknown_input_gap.rs` (Scenario 1).

## Decisions

1. **The `SourceBlock`/`EngineSession` merge is split out.** The scope note
   says the full merge "may need a split". It touches nine adapters, three
   engine loops, the recording format and every wrapper. Scenario 2's
   observable claim already held across today's entry points (measured
   below). This ticket builds the two pieces any merge needs first: one
   continuity intake (`ReadContinuity`) and one gap mechanism inside
   `TrackManager`. Scenario 2's "the same `EngineSession`" is met here by the
   shared intake and the shared `TrackManager`, not by one state machine.
   The merge is follow-up 1 below.
2. **A separate boolean hook, not `take_discontinuity(Some(0))`.** A zero
   count is indistinguishable from a real zero and gets summed by
   `GapScaler`, `ReconnectingSource` and `GroupSource`. MAN-233 Decision 1
   refuses to invent a count. `ReadContinuity` is the seed of the source
   block's "discontinuity status" field.
3. **Invalidate per-track run-level state and keep the track.** The sweep
   below is the evidence: 0 wrong characters against 6 to 38. At an unknown
   gap:
   - the `Demod` held run is processed as a normal run first: it was
     debounce-confirmed before the gap (the `finish_speed_only` reasoning);
   - the `Demod` open run and fade run are dropped: they span the gap, so
     their length is not the sender's;
   - a `Demod` init window restarts, so its replay never assembles runs
     across the gap;
   - the marks of the character in progress are emitted only if the open
     space had already reached a character gap before the gap, and dropped
     otherwise: the boundary must be observed, not inferred from the gap;
   - a word in progress with committed characters closes with a
     `WordBoundary` at the last pre-gap hop, so no word is spliced across
     the gap;
   - warm-up `pending` runs, `flushed_gap_ms` and `single_flush_ms` are
     cleared, so nothing is replayed or learned across the gap;
   - post-gap runs are discarded until a space of at least a character gap
     closes (resync), because the first post-gap mark may be truncated
     mid-element; before tracker readiness the later, whole post-gap marks
     set the unit that space is measured in and teach nothing else, or a
     fast sender's spaces would never reach the no-marks-yet 150 ms unit;
   - the refiner's backlog is drained into `pending`, then the refiner FIR,
     `clean_hist` and `phase_hist` are reset (the MAN-194 order), because
     their windows span the splice;
   - `SpeedTracker`, `GapClassifier`, `SenderTiming`, rails, `NoiseTracker`,
     SNR history, `hop_count`, track identity, lifecycle timers,
     `FloorBank` and the channelizer are kept. They are sender properties,
     not continuity, and silent cuts up to 60 ms left text intact, so the
     floor and the channelizer tolerate the splice.
4. **No clock advance and no invented count.** `ErrorCode::Overflow` carries
   no count. `RxStream::time_ns()` exists in soapysdr 0.5.1, but
   `SOAPY_SDR_HAS_TIME` is unreadable there and driver behaviour is
   unverified, so measuring the lost span is a follow-up. Spot timestamps
   after an overflow stay early by the lost time, exactly as before.
5. **No event-schema change.** Events serialize into `decode --json` and the
   live-state stream for dispensa, and the ticket says to stay manta-only
   unless the work becomes cross-repo. The epoch is an internal counter
   (`TrackDecoder::continuity_epoch`, `TrackManager::continuity_epoch`) read
   by tests.
6. **SoapySDR is the only new reporter.** The scope note names overflow
   gaps. Kiwi and ka9q know their loss counts, so they belong under
   `take_missing_samples`. HPSDR protocol 1's arrival-time gap can be jitter
   without loss. Audio's ring overrun is not exposed by `rtrb`. All four are
   follow-ups.
7. **Precedence when several reports arrive on one read.** A discontinuity
   wins: the segment is rebuilt and an unknown gap on the same read is
   subsumed. Missing samples and an unknown gap both apply: the clock moves
   by the count and the tracks are invalidated. An unknown gap pending at
   end of stream closes like a pending discontinuity
   (`finish_for_discontinuity`, the MAN-258 rule). A gap noted during
   `listen`'s calibration fill is applied after the fill, and discarded with
   the fill if a calibration discontinuity follows.
8. **The recorder forwards the gap and does not record it.** A new `Gap`
   record type would be a recording-format change, and older readers'
   handling of an unknown record type is untested. A replay of a recording
   made through an overflow therefore runs the samples together, as it
   already does for `take_missing_samples`. Follow-up 9.
9. **Non-legacy engines.** `decode.engine` stays `legacy`. For
   `edge_legacy` and `hsmm`, `note_input_gap` rebuilds the engine's buffered
   front-end state (`Evidence`, `EdgeDemod`, and `HsmmDecoder` for `hsmm`)
   from the config and forces nothing. `edge_legacy` then gets the same
   run-level steps as Legacy; `hsmm` gets only the rebuild. A post-gap
   resync for `hsmm` is follow-up 7, beside MAN-169's existing caveat.
10. **The cost, stated plainly.** About one dropped character per unknown
    gap, even for a 5 ms overflow that a silent splice would have survived.
    A deletion loses at most one spot opportunity; a substitution makes a
    busted spot. Frequent overflows already trigger MAN-233's warning,
    because the host is not keeping up.

**MAN-233 Decision 5 still holds.** An overflow still neither advances the
sample clock nor churns track segments; `take_discontinuity` is untouched
on the SoapySDR path.

**Deviation from investigation 12.** Investigation 12 asks for "no identity
spanning an unknown discontinuity"
(`docs/DECISIONS/2026-09-09-decoder-recall-research.md:266`). Tracks here
keep their identity across an unknown gap. The ticket's own Scenario 1 asks
for invalidation, not closure, and closure measured worst (the `Disc0` rows
below: a fresh track starts mid-element with an untrained speed tracker, the
MAN-398 failure). The validator's dedupe and repetition state already spans
gaps (`the_dedupe_window_spans_a_gap`).

**CHANGELOG placement.** The decoder output rule (`CHANGELOG.md`) defines
decoder output as the spots produced from an input *file*. No file source
reports an unknown gap, so golden vectors and file spot logs do not move,
and the entries go under `### Fixed` and `### Added`, not
`### Decoder output`.

## Measurements

These numbers are the plan's prototype measurements, taken at `d144e36` on
the V1 scene (`manta_testkit::vectors::v1()` at `duration_s = 30.0`, 96 kS/s,
20 WPM, "CQ CQ DE W1AW W1AW K" looped). The probe removed a span at a chosen
sample and either reported nothing (`Silent`, the SoapySDR overflow before
this change), reported `take_discontinuity(Some(0))` (`Disc0`), or reported
the unknown gap (`Unknown`). Baseline text: `"FQ CQ DE W1AW W1AW K CQ CQ DE
W1AW W1AW K CQ CQ"` (the leading `F` is the decoder's own start-up error,
present in every run).

Single runs before the change:

```
cut 150ms@12s SILENT changed=true closes=1 "FQ CQ DE W1AW W1AW M CQ CQ DE W1AW W1AW K CQ CQ"
cut 400ms@12s SILENT changed=true closes=1 "FQ CQ DE W1AW W1AW T CQ CQ DE W1AW W1AW K CQ CQ"
cut 400ms@18s SILENT changed=true closes=1 "FQ CQ DE W1AW W1AW K CQ CQ DE T1AW W1AW K CQ CQ"
cut 20ms@16s  DISC0  changed=true closes=2 "FQ CQ DE W1AW W1AW K CQ CA DE W1AW W1AW K CQ CQ"
```

Sweep over six cut positions (10, 12, 14, 16, 20, 24 s). "Wrong" is
substitutions plus insertions against the baseline, "missing" is
deletions, spaces ignored; closures include the six end-of-stream closes.

| Mode | Cut | Runs changed | Wrong chars | Missing chars | Closures |
|---|---|---|---|---|---|
| Silent (before) | 5 / 20 / 60 ms | 0/6 each | 0 | 0 | 6 |
| Silent (before) | 150 ms | 6/6 | 6 | 0 | 6 |
| Silent (before) | 400 ms | 6/6 | 6 | 2 | 6 |
| Disc0 (before) | 5 ms | 6/6 | 6 | 0 | 12 |
| Disc0 (before) | 20 ms | 6/6 | 38 | 0 | 12 |
| Disc0 (before) | 60 ms | 6/6 | 6 | 0 | 12 |
| Disc0 (before) | 150 ms | 6/6 | 5 | 2 | 12 |
| Disc0 (before) | 400 ms | 6/6 | 2 | 6 | 12 |
| Unknown (this change) | 5 / 20 / 60 / 150 ms | 6/6 each | 0 | 6 each (one per cut) | 6 |
| Unknown (this change) | 400 ms | 6/6 | 0 | 8 (at most 2 per cut) | 6 |

Example outputs with the unknown gap: `5ms@10s "… W1AW W AW K …"` (the `1`
is dropped and a word boundary separates `W` from `AW`, so nothing splices
into a fabricated word); `400ms@20s "… DE W1 W1AW K …"`.

Parity between the file path and the live path on the V1 scene: the
skeleton (`TrackPromoted@ts`, `CharDecoded@ts:glyph`, `WordBoundary@ts`,
`TrackClosed:closure`) of `decode_source` (4096-sample reads) equals
`listen` (2048-sample reads), and `listen` 2048 equals `listen` with the
irregular schedule `[97, 1500, 4000, 333, 2048, 1, 1999, 777]`. With an
unknown gap, `listen` 2048 and the irregular schedule gave equal skeletons
at 150 ms@12 s, 400 ms@18 s and 20 ms@16 s. The full `Debug` streams can
differ by a pre-existing reorder: a `CharDecoded` placed on the other side
of a batch boundary from a later-stamped `TrackMeta`/`TimingUpdate`. It is
not a gap effect (follow-up 8).

The implementation's integration test
`crates/manta-engine/tests/unknown_input_gap.rs` re-checks the bounds the
sweep sets: for 5, 20, 60, 150 and 400 ms cuts at 10, 12 and 20 s through
`listen`, zero wrong characters, no closures beyond the uncut run's, and at
most 2 deletions per cut. It also checks that the same 150 ms cut with
nothing reported still produces a wrong character, so the report is what
fixes it. `crates/manta-engine/tests/replay_live_parity.rs` checks Scenario
2 on two scenes, with and without an unknown gap.

Channelizer geometry: at 96 kS/s, N = 1024, hop = 256 and `filter_len` =
8192, so a gap straddles 32 hops (85 ms at the 375 Hz hop rate).
`filter_len / hop = 4·L` is 32 at every rate.

## Follow-ups to file

These are for a person to file. They are not acceptance criteria.

1. `SourceBlock`/`EngineSession`: one engine state machine for file and live input, with source ID, epoch, first sample index and optional hardware time on a source block, and an epoch carried on events (the split part of MAN-202, D1).
2. Validator: discard the word an unknown gap broke instead of completing it at the gap's `WordBoundary`. This needs an event-level epoch (item 1).
3. Kiwi and ka9q-radio: report their known packet-loss counts through `take_missing_samples`, and make `decode_source` and the recording format honour `take_missing_samples`.
4. Spliced known-length losses (HPSDR protocol 2 under 0.5 s): measure whether `note_input_gap` above a length threshold beats the bare splice, since a silent splice measured clean up to 60 ms and damaged at 150 ms.
5. SoapySDR: `RxStream::time_ns()` and the `SOAPY_SDR_END_ABRUPT`/`HAS_TIME` flags, to measure the lost span and correct the clock (needs a soapysdr release that exposes `flags`).
6. Audio (cpal/`rtrb` ring overrun) and HPSDR protocol 1 (outer sequence numbers instead of arrival timing) as unknown-gap reporters.
7. `hsmm` engine: a post-gap resync like the Legacy one (D9), alongside MAN-169's `finish_speed_only` caveat.
8. The `CharDecoded`/`TrackMeta` batch-placement reorder between read schedules (investigation 12's "order events by source/epoch/sample/type/track").
9. Recording: an unknown-gap record in `PATH.meta.jsonl`, after testing how older readers treat an unknown record type.

**MAN-208.** MAN-208 is blocked by this ticket. Its needs were not visible
in the repository or the thoughts pool when this was implemented, so
nothing here was shaped for it beyond the contract above.

## Verification

- Container-runnable: `cargo test --workspace --locked` with no golden
  vector, recorded fixture or `expected` file edited; `cargo test -p
  manta-input --locked --lib overflow`; `cargo test -p manta-engine
  --locked --test unknown_input_gap --test replay_live_parity`.
- CI-only: the `test-soapy` leg compiles, lints and tests the soapy-gated
  lines (`SoapySdrIqSource::take_unknown_gap` and the latch field).
- Hardware (owner-run): on a starved host with an RSP1B or RTL-SDR,
  `manta_input_gaps_detected_total{source="soapy"}` rises and the decoded
  text shows dropped characters, not substituted ones.

## Rollback

Revert the PR. The hook defaults to `false`, so reverting only the SoapySDR
reporter restores the earlier live behaviour exactly.
