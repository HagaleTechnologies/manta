# MAN-207: a bounded, local record of why each callsign candidate was or was not spotted

Date: 2026-11-03. Ticket: MAN-207.

**Status:** accepted, first slice. Builds on the
[MAN-324 decision log](2026-10-12-man324-live-state-stream.md) and the
[MAN-118 decode-quality metrics](2026-10-12-man118-decode-quality-metrics.md).
Source: investigation 18 of
[the decoder recall research](2026-09-09-decoder-recall-research.md).

## Context

When a station is not spotted, manta kept nothing that explained why once
the track closed. MAN-324's decision log is opt-in, runs only while a
live-state client is connected, and hands each decision to that client.
Every "why didn't this spot?" investigation (MAN-100, MAN-106, the
live-hardware detection-gap threads) rebuilt the context by hand.

## Decision

`manta_spot::observation` defines private domain types (no serde, no wire
format) and an append-only, bounded `ObservationLog` that every `Validator`
owns:

- **D1. Wrap `Decision`, do not change it.** A record's `Draft` holds the
  MAN-324 `Decision` (call, spot type, verdict, reason, reps, required
  reps, pattern tier, rival). No `Decision` field and no `Reason` variant
  was added, so the live-state record and its pins are unchanged.
- **D2. Two identities.** The observation ID is
  `ObservationId { track_id, word_seq }`, shown `t{track}.w{seq}`: one
  decoded occurrence, unique in a run (track ids are never reused, word
  seqs strictly increase per track). The record ID is the log's append
  counter `seq`. Both are deterministic for a given input and config.
- **D3. Append-only revisions.** A record about an occurrence that already
  has a retained record gets `supersedes = Some(<that record's seq>)`.
  `Cause` says why it exists: `Decided`, `Reclassified` (the same word as
  a new spot type), `Settled` (a pending beacon or lookup decided later, or
  a beacon overflow or discard), `Refined` (`ObservationLog::revise`). No
  stored record is ever changed. If the predecessor has expired,
  `supersedes` is `None` and `LossCounts::orphaned_revisions` counts it.
- **D4. Refinement without a scheduler.** No re-decode scheduler exists
  yet. `ObservationLog::revise(id, draft)` appends a `Refined` record for a
  retained occurrence, refuses (and counts) one that is not retained, and
  appends nothing for a draft equal to the latest record. It never touches
  the repetition gate: a refinement adds no repetition by itself.
- **D5. Retention.** `Detail::Summary` (the default) counts every decision
  and appends nothing. `Records` and `Text` keep at most 20,000 records
  (the MAN-62 LRU size) and expire records more than 900 s of stream time
  old, inside the validator's existing 10 s sweep. Every expiry, eviction
  and record dropped before `take_appended` returned it is counted. No
  wall clock is read.
- **D6. The boundary.** `Validator::decide` is the one place a decision is
  made; the two pending-beacon overflow paths that pushed onto the log
  directly now pass through it too. It always increments one array slot.
  It builds the `Decision` only when the decision log is on or the record
  keeps records.
- **D7. Source.** `input` is `Validator::input_of(track_id)`; `epoch`
  counts `ObservationLog::note_discontinuity()` calls. Only
  `manta_engine::decode_source_observed` calls it, at the event index where
  each MAN-309 segment after the first begins. MAN-202 unknown-length gaps
  do not advance it, and the daemon writes no records.
- **D8. Spans, not modeled uncertainty.** `SampleSpan` runs from the
  word's first character to its `WordBoundary`. `FreqSpan` is the lowest
  and highest calibrated track frequency read while the word was decoded.
  The decoder has no uncertainty model and this ticket invents none.
- **D9. Text is opt-in.** At `Records` a record carries the candidate call
  and numbers only. `Text` adds `TextTrace`: the word (at most 64
  characters), its confidences, `'?'` erasure positions and the decoder's
  per-position alternatives, with `truncated` set when the cap cut it.
  Transcripts can hold personal traffic.
- **D10. Evidence links.** At `Records` and above the repetition gate's
  `record_traced` returns the occurrences it holds for the call, each with
  whether it counted as a separate message, and the record links each by
  observation ID. A reclassification makes no new gate call, so
  its evidence is on the record it supersedes. A pending beacon or lookup
  is decided about its origin occurrence, even after a merge migrated it
  to a survivor track.
- **D11. The inspection command is `decode`, command-line only.**
  `manta decode FILE --observations PATH [--observation-text]` writes the
  record as JSON Lines, schema `manta-observations.v1`
  ([OBSERVATIONS.md](../OBSERVATIONS.md)). PATH is created with
  `create_new` before decoding, so an existing path fails fast and is never
  truncated; a failed decode removes it. The private types are mapped to
  wire structs in `manta-cli` (`observations.rs`), as MAN-324 split the
  live-state record, so `manta-spot` carries no file format. Live misses
  are investigated by replaying a `run --record-iq` recording (MAN-309).
- **D12. A counter on `/metrics`.** `manta_candidate_decisions_total{decision,reason}`
  renders every valid pair (16 series, `reason="none"` on a spot) from the
  first decode batch, stored once per batch like
  `manta_candidate_repetitions`.
- **D13. The public contract is unchanged.** Telnet, the JSON spot stream,
  the RBN uplink, the live-state stream, `decode --json` and `decode`'s
  stdout are byte-identical. No dispensa contract changes.
- **D14. Ticket citations.** "SPEC v2 §18" is investigation 18 of the
  recall research (SPEC v2 ends at §10). Dispensa Q-0028 is about
  unknown-DXCC representation, not observations; nothing here relies on it.

## Field to source

| Ticket outcome item | Field |
|---|---|
| source/epoch | `Draft::source` (`input`, `epoch`) |
| stable observation ID | `Draft::id` (`ObservationId`), `Observation::seq` |
| sample/RF interval and uncertainty | `Draft::span`, `Draft::freq` (spans, D8) |
| track association | `ObservationId::track_id` |
| text alternatives/erasures | `TextTrace::alternatives`, `TextTrace::erasures` |
| scores | `Draft::scores` (`word_confidence`, `call_confidence`), `Decision::reps`/`required_reps` |
| rejection reason | `Decision::verdict`, `Decision::reason`, `Decision::rival` |
| evidence links | `Draft::evidence` (`EvidenceLink`) |
| revision/supersession | `Observation::supersedes`, `Draft::cause` |
| bounded retention | `ObservationPolicy`, `LossCounts` |

## Measured cost

`manta decode` (release build, 32-core container) on V8 (120 s, 50
signals, 49 spots), three runs each, wall clock:

| Run | Times (s) | Median (s) | Records | File size |
|---|---|---|---|---|
| no flag | 9.51, 9.17, 8.63 | 9.17 | none | none |
| `--observations` | 8.46, 10.50, 10.20 | 10.20 | 908 | 1,096,497 bytes |
| `--observations --observation-text` | 11.10, 9.29, 8.32 | 9.29 | 908 | 1,228,551 bytes |

The spread inside each row is wider than the gap between rows, so the
record costs no time this measurement can separate from noise. stdout was
byte-identical across the three modes. Every key in the text-detail file
is documented in OBSERVATIONS.md, and its tables name no key the file
lacks.

## Follow-ups

This record is the first slice. Moved to a follow-up ticket:

- MAN-201 roles: role evidence on each record, `episode` as the MAN-201
  turn index, `NotCandidate` records for role-skipped call occurrences and
  `manta_call_occurrences_not_candidate_total{role}`. MAN-201 is on main
  but not on this branch's base.
- A daemon records sink with a retention and privacy policy; per-spot
  delivery state; epoch for MAN-202 unknown gaps and for the daemon;
  `ReadingsHeld` readings in the trace; links to a variant rival's own
  occurrences; the rank-15 re-decode scheduler calling `revise`.
