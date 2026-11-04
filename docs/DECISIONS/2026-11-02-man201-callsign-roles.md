# MAN-201: who sent a callsign, not only which callsign

Date: 2026-11-02. Ticket: MAN-201.

## Problem

`manta-spot` chose spots from a regex parse that returned at most one named
match per 16-word window and had no idea who was sending. A call copied
perfectly as the station called was still reported as the transmitter.

Measured through `Validator` at `d144e36` (`x2` is sent twice, 100 000 samples
apart, then closed):

| Case | Spots at `d144e36` |
|---|---|
| `DE W1AW DE K1ABC K` x2 | `W1AW:DE`; no K1ABC decision at all |
| `CQ W1AW DE K1ABC` x2 | `K1ABC:CQ` |
| `CQ CQ W5XYZ W5XYZ K 5NN 05 TU 73 GL OM DE K1ABC K1ABC K` x2 | `K1ABC:CQ`, a stale `CQ` typing a later turn |
| `K1ABC K1ABC T` x1 | `K1ABC:BEACON` with no repetition |
| `K5ARH T` x1, K5ARH allowlisted | `unknown` then `beacon` for one word |
| `W1AW DE K1ABC K` x1, W1AW allowlisted | `W1AW:unknown` |

`CQ DX W1AW` already spotted as `CQ`, so the ticket's second scenario is a
regression pin (D1).

## Decision

`manta_spot::exchange` gives every callsign occurrence one role: `self_id`,
`addressee`, `relayed_mention` or `uncertain`. `Validator` spots only a
settled `self_id` with a spot form, at the call's own word time.

- **D1.** `CQ DX <call>` is pinned by V54, not new behaviour.
- **D2. Role rules.** The rule table is in the `exchange` module docs. A call
  directly before `DE` is the addressee even if a `DE` also precedes it.
- **D3. Turns.** A turn ends after `K`, `KN`, `(`, `BK`, `AR` or `SK`, after an
  `AR`/`SK` prosign glyph, after 5 s with no decoded character, and at track
  close. 5 s is the longest character-free stretch at 8 WPM (4.35 s), rounded
  out, and is not configurable. Frames and mentions never cross a turn end.
- **D4. Stale CQ.** `CQ` and `TEST` frame a call only through a contiguous run
  of `CQ`, `TEST` and fillers leading straight into the call or its `DE`.
- **D5. Settle, then evaluate at the occurrence's own time.** Spots are never
  retracted, so an occurrence waits until another word follows its run, its
  turn ends or its track ends. Repetition, dedupe, the decision and the spot
  all use the word's own timestamp. A held occurrence older than a newer spot
  for the same call and frequency is withheld `dedupe`.
- **D6, D6a.** Each candidate binds to its exact word. A role latches at its
  first settle, so a framing word leaving the window cannot change it.
- **D7. The allowlist skips plausibility and repetition, not role.**
- **D8. Beacon evidence.** A `<call> T` is a `BEACON` candidate only for an
  allowlisted call. Another plausible, allocated call records
  `beaconNoEvidence` once per occurrence, unless the CQ/DE guard covers it. `V
  V V <call>` keeps its deferred path.
- **D9.** `exchange` is a pure pass over the bounded window, re-run at each
  boundary. `context::parse` is an adapter over it.
- **D10.** Only `beaconNoEvidence` is newly visible on an output.
- **D11.** A call is a call-shaped word that is not a keyword, Q-code or
  `grammar` convention.
- **D12.** An existing test changes only when its subject is a behaviour this
  ticket changes. See "Amended tests".
- **D13.** One PR, on `MAN-201`.

### Silence on the decoder's clock (D3, as built)

Reading silence as `TrackMeta.sample_ts - word end` dropped V8 with the prior
on to 40 of 50 (floor 45). `TrackMeta.sample_ts` runs ahead of a word the
MAN-317 prior still holds, so every word looked silent. `TrackMeta` now carries
`last_char_ts`, the decoder's newest character, held or released, and silence
is measured from it. It is `lastCharTs` in `decode --json` events, so a
replayed report decides as the live run did, and is on the shared clock in a
multi-input run. V8 is back at 48 of 50 at weights 0, 1 and 2.

### A stream that stops (Tony's decision, 2026-11-01)

A stalled stream delivers no hops, so no `TrackMeta` arrives and a tail call
would wait for the stream to return. `IqSource::take_stalled` reports it:
`read` returns `Ok(0)` with the flag set, which is not an end of stream, and
the engine closes that input's tracks with the outage teardown. Samples after
the stall start a fresh segment, as a discontinuity of length 0. A recording
keeps the stall as a zero-length `gap` record, so a replay resets the same
state. ka9q reports 5 s after a delivering stream stops. Other live sources report when
`ReconnectingSource` loses them, at their own stall bound; MAN-489 brings them
to 5 s. A read error closes tracks the same way.

### Repetition across a dead input

A no-survivor closure stays "conservatively active" in the gate. When a dead
input's track held the anchor, a live track's repeats deferred to it until the
60 s gap and `a_panicking_input_is_isolated` never spotted K5ARH. An occurrence
a full message past the track's own newest touch now counts, provided the other
track's message is no newer than that touch.

## Measured before and after

`manta decode --json` once per scene, then the report replayed through
`replay_spots` at main (`3a6f3c9`) and at this branch. Spots with no bogus
calls on either side.

| Scene | Spots before | Spots after | Genuine distinct before | after | Bogus | `beacon` spots | Type changes |
|---|---|---|---|---|---|---|---|
| V1 | 1 | 1 | 1/1 | 1/1 | 0 | 0 | none |
| V7 | 2 | 2 | 2/2 | 2/2 | 0 | 0 | none |
| V9 | 1 | 1 | 1/1 | 1/1 | 0 | 0 | none |
| V10 | 1 | 1 | 1/1 | 1/1 | 0 | 0 | none |
| V8w | 33 | 42 | 27/57 | 25/57 | 0 | 0 | `De` 1 to 11, `Cq` 32 to 31 |

V8w is a faded pileup, and every change is a named decision:

- **More `De` spots (D4).** A faded `CQ` no longer reaches across the window, so
  a call with a damaged `CQ` is `De`. The extra spots are dedupe's existing
  type-change override.
- **VE3QHG, W5REB and N3NXI lost, N3FXO gained (D2, rule 8).** Every clean copy
  of the lost calls follows a damaged copy of the same call
  (`CQ CQ DE N<SK>NXI N3NXI K`). The damaged word takes the `DE`, and the clean
  word is an unframed `uncertain`, which is not a candidate. On main, the window
  scan framed it anyway.

`man120_beacon_fading` with K5ARH allowlisted spots 2 `beacon` and 1 `unknown`
under Watterson poor: a faded `T` reads `unknown`, and the type change re-spots
the later `beacon`. Unlisted, it spots no `beacon`.

The A/B grid (`man398_grid`) is byte-identical to main over 288 cases: nothing
here touches decoding.

## Accepted costs

- One word of latency, up to 5 s plus one `TrackMeta` at a transmission's end.
- A run of one bare call waits for a different word.
- Unterminated back-to-back `DE A DE B` pairs under 5 s apart spot neither call.
- A clean call right after a damaged copy of itself does not spot (V8w above).
- Non-allowlisted NCDXF beacons stop spotting unless they send `V V V`.

## Open claims

Real-traffic role accuracy and beacon false positives are unmeasured: the audio
corpus is not committed.

## Follow-ups

- MAN-489: 5 s stall reporting on Kiwi, SoapySDR, HPSDR and audio.
- Framing a clean call through a damaged copy of itself.
- Decoder-side power-step measurement as beacon evidence.
- Spots from unframed and `TU <call>` self-IDs.
- POTA, SOTA, CWT, SST and MST fillers.
- A role-aware `SupportLedger`.
- Simplifying the guard, now that only allowlisted calls reach it.
- Separating frequency-coincident turns.

## Amended tests

By D12 category. Tests that only gained the `last_char_ts: 0` field are not
listed.

- **A spot at settle, not at the call's boundary** (a trailing word or a close):
  `lists_offered_during_calibration_apply_to_the_calibration_buffer` (Tony's
  choice: its feed gained turn ends, as a bare run waits for a different word),
  V20 to V23, V25 to V29, V41, V45b, V50, V15, V32/V33's
  `v33_a_cq_after_qrl_query_is_flagged` and
  `v33_the_ticket_transmission_carries_both`, V17,
  `suppressed_spots_are_counted_by_reason`, `recording_decisions_never_changes_spots`,
  and in `validator.rs` the lookup, allowlist and calibration tests that added a
  trailing word.
- **The trailing-`T` exemption** (`V V V` fixture or allowlist): the deferred
  beacon tests in `validator.rs`, V11's `<call> T` row, V30,
  `power_step_guard_suppressions_are_counted_once_per_occurrence`,
  `a_previously_attempted_but_unspotted_word_still_counts_its_suppression`,
  `a_spotted_word_still_counts_its_guard_suppression_once`,
  `an_unsuppressed_power_step_beacon_counts_nothing`,
  `a_beacon_processed_before_the_guard_appeared_counts_no_suppression`,
  `a_glued_cq_de_substring_does_not_suppress_a_power_step_beacon`,
  `power_step_beacon_retains_every_unattempted_occurrence_across_the_metadata_gate`,
  `chained_merge_resolves_the_deferred_beacon_at_the_final_survivor`,
  `beacon_spots_exactly_once_under_watterson_poor` (one beacon is no longer the
  count, see above) and `candidate_decision_line_for_each_reason`.
- **A stale `CQ` promoting a call:**
  `unlisted_decisions_and_reclassification_keep_the_five_rep_requirement`.
- **First-match-wins hiding a further `self_id`:** `cq_de_call_call_is_cq_type`,
  `cq_test_call_is_cq_type`, `v_v_v_call_is_beacon_type`,
  `bare_test_after_a_sign_off_word_is_ambiguous`,
  `de_call_is_not_reclassified_to_cq_by_a_glued_cq_substring`,
  `sign_off_test_does_not_promote_the_next_callers_de`, and
  `de_call_range_covers_only_the_de_match_when_no_cq_present`, renamed
  `de_call_range_covers_the_de_and_the_call_when_no_cq_present`.
