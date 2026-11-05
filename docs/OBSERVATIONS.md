# Observation record (`manta-observations.v1`)

Why each callsign candidate was or was not spotted, written by
`manta decode FILE --observations PATH` as JSON Lines. MAN-207; the design
is in
[DECISIONS/2026-11-03-man207-observation-record.md](DECISIONS/2026-11-03-man207-observation-record.md).

The file is local and opt-in. It is not a public contract like the
[JSON spot schema](SPOT-SCHEMA.md) or the [live-state stream](LIVE-STATE.md):
it may change, and an incompatible change bumps its `schema` string. Telnet,
the JSON stream, the RBN uplink, the live-state stream and `decode`'s own
stdout are the same with or without it.

## Producing it

```sh
manta decode k5arh.wav --observations k5arh.jsonl
manta decode k5arh.wav --observations k5arh.jsonl --observation-text
```

`PATH` must not exist; `decode` refuses an existing path before decoding,
and removes the file again if the decode fails. To investigate a miss on a
running node, record the session with `manta run --record-iq` and replay the
recording through `decode --observations`
([GUIDES/iq-recording.md](GUIDES/iq-recording.md)). The same input, config
and build give the same bytes.

## Lines

Line 1 is the header, the last line is the summary, and every line between
is one observation record, in `seq` order. Each line has a `kind`
(`observation` or `summary`), except the header.

### Header

| Key | Type | Meaning |
|---|---|---|
| `schema` | string | `manta-observations.v1`. |
| `detail` | string | `records`, or `text` with `--observation-text`. |
| `decoderVersion` | string | The build, as the JSON stream's `decoderVersion`. |
| `sampleRateHz` | number | The recording's sample rate; every `*SampleTs` counts samples at it. |

### Observation

| Key | Type | Meaning |
|---|---|---|
| `kind` | string | `observation`. |
| `seq` | integer | The record's own id: the log's append counter, from 0. |
| `observation` | string | The occurrence: `t{trackId}.w{wordSeq}`, unique in a run. Several records can share it. |
| `supersedes` | integer or null | The `seq` of the earlier record about the same occurrence this one replaces as the latest; `null` for the first. |
| `cause` | string | Why the record exists (below). |
| `recordedAt` | integer | The stream sample time the record was appended at. |
| `input` | integer | The `[[input]]` index the track came from (0 for one input). |
| `epoch` | integer | The gap-free segment of a recording, from 0; each recorded gap starts the next. |
| `trackId` | integer | The track. |
| `wordSeq` | integer | The word's position on its track, from 0. |
| `episode` | integer or null | The MAN-201 turn the word was heard in, from 0: the turn ends on its track before it (an `AR`/`SK` prosign, or 5 s of silence). `null` once the word left the window. |
| `startSampleTs` | integer or null | The word's first character; `null` once the word left the track's 16-word window. |
| `endSampleTs` | integer or null | The word's word boundary; `null` as `startSampleTs`. |
| `decidedSampleTs` | integer | When the decision was made; for a `notCandidate`, the word's boundary. |
| `frequency` | number | The calibrated track frequency in Hz when decided. |
| `frequencySpan` | array or null | `[low, high]` Hz: the track frequencies read while the word was decoded. A span, not an uncertainty. |
| `call` | string | The candidate callsign, or the role-skipped call. |
| `spotType` | string or null | The pattern that made it a candidate: `cq`, `de`, `beacon` or `unknown`; `null` for a `notCandidate`. |
| `role` | string or null | The MAN-201 role the exchange grammar gave the call (below); `null` when no role was latched on the word or it left the window. |
| `frame` | string or null | For a `self_id` role, the exchange form it was heard in (below); else `null`. |
| `basis` | string or null | The rule that decided the role (below). |
| `decision` | string | `spotted`, `withheld` or `deferred`, as `candidateDecision` in [LIVE-STATE.md](LIVE-STATE.md), or `notCandidate`: a settled occurrence of a plausible callsign that its role kept from becoming a candidate when it settled. |
| `reason` | string or null | Why it was withheld or deferred, with the labels of LIVE-STATE.md's "Decisions and reasons"; `null` on `spotted` and `notCandidate`. |
| `reps` | integer or null | The repetition gate's count of message-distinct copies; `null` where the gate was not reached. |
| `requiredReps` | integer or null | Copies the call's pattern tier needs to spot; `null` for a `notCandidate`. |
| `patternTier` | string or null | `common`, `listed` or `unlisted`; `null` for beacons, allowlisted calls and `notCandidate`. |
| `wordConfidence` | number or null | The decoder's geometric-mean character confidence for the word. |
| `callConfidence` | number or null | The calibrated call confidence, once validation computed one. |
| `rival` | string or null | For `variant`, the better-supported confusable call. |
| `evidence` | array | The occurrences of this call the repetition gate held near this frequency when it decided, each `observation`, `sampleTs` and `messageDistinct` (whether the gate counted it as a separate message; `reps` counts the `true` ones). Empty where the gate was not called. |
| `trace` | object | Only with `--observation-text`, while the word is in the window (below). |

`cause` values:

- `decided`: the occurrence's first decision, or its `notCandidate` record.
- `reclassified`: the same word evaluated again as a new spot type (for
  example an allowlisted `CQ K5ARH` spotted as `cq`, then a trailing `T`
  re-typing it as `beacon`), or a `notCandidate` word that became a
  candidate later (an allowlist reload). It makes no new gate call, so its
  `evidence` is on the record it supersedes.
- `settled`: a pending beacon or callsign lookup decided later, or a pending
  beacon overflowed or discarded. A beacon migrated to a merge survivor
  keeps its origin track's `observation`.
- `refined`: a re-decode of the same evidence. Nothing produces it yet; a
  refinement never adds a repetition.

`role` values (MAN-201, `docs/DECISIONS/2026-11-02-man201-callsign-roles.md`):

- `self_id`: the sending station identifying itself (`DE <call>`, `CQ <call>`,
  `TU <call>`).
- `addressee`: the station being called (`<call> DE <other>`, `<call> 5NN`).
  Never a candidate.
- `relayed_mention`: another station named inside a station's own turn.
  Never a candidate.
- `uncertain`: no rule decides it (`<call> T`, a bare or corrected call). A
  candidate only when allowlisted.

`frame` values: `cq`, `de`, `up`, `beacon`, and `signOff` (`TU <call>`, a
self-ID with no spot form: a candidate only when allowlisted).

`basis` values: `afterDe`, `afterCq`, `afterTest`, `beaconPreamble`,
`beforeUp`, `afterSignOff`, `beforeDe`, `beforeReport`, `beforeQuery`,
`testSignOff`, `corrected`, `mentionInTurn`, `noFrame`.

### Trace (`--observation-text`)

| Key | Type | Meaning |
|---|---|---|
| `text` | string | The word as decoded, at most 64 characters. |
| `confidences` | array | One confidence per character. |
| `erasures` | array | Positions of `?` erasure characters. |
| `alternatives` | array | `[]` when the decoder offered none; else one list per character of `[glyph, mass]` pairs. The `legacy` engine offers none. |
| `context` | array | The words that decided the role, in order, while they are in the window, at most 128 characters with spaces; `[]` when no role was latched. |
| `truncated` | boolean | Whether the 64-character cap cut the word or the 128-character cap cut `context`. |

Transcripts can hold personal traffic. The trace is never written without
`--observation-text`.

### Summary

| Key | Type | Meaning |
|---|---|---|
| `kind` | string | `summary`. |
| `records` | integer | Observation lines written. |
| `decisions` | array | Every decision of the run with a non-zero count, each `decision`, `reason` and `count`, in a fixed order. |
| `notCandidates` | array | Every role with a non-zero count of `notCandidate` occurrences, each `role` and `count`, in the order `addressee`, `relayed_mention`, `self_id`, `uncertain`. |
| `expired` | integer | Records dropped after 900 s of stream time. |
| `evicted` | integer | Records dropped by the 20,000-record cap. |
| `orphanedRevisions` | integer | Later records whose earlier record was no longer kept, so `supersedes` is `null`. |
| `unsent` | integer | Records dropped before they were written; 0 for `decode`. |

## Revisions

No record is ever rewritten. A later decision about the same occurrence is
a new line with its own `seq`, the same `observation`, and `supersedes` set
to the latest earlier record's `seq`. Read an occurrence's history with
`jq -c 'select(.observation=="t1.w2")' k5arh.jsonl`.

### Live delayed revisions

MAN-208 can produce `cause: "refined"` from the single-input listen loop. These
records use the existing `manta-observations.v1` schema. `supersedes` names the
prior record for the same observation id; start and end samples keep the original
span. `decided_sample_ts` records replay completion, so readers can distinguish
RF time from decision availability. The repetition gate moves the occurrence's
support rather than adding a repetition. See
[the measurement limits](DECISIONS/2026-11-05-man208-delayed-refinement.md) before
comparing this delayed output with causal output.
