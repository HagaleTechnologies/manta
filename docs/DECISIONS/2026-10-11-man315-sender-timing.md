# MAN-315: per-sender dot, dash and gap timing in the legacy decoder

Date: 2026-10-11. Ticket: MAN-315. Normative text: SPEC-decode-core.md
§4.1, §4.1a, §4.1b, §4.2, §5, §7.3 and §9.

## Decision

The `legacy` decoder (and `edge-legacy`, which shares its run path) learns
each track's sender timing instead of assuming 1 / 3 / 1 / 3 / 7:

1. **`SenderTiming`** (`crates/manta-decode/src/timing.rs`): five classes
   (dot, dash, element gap, char gap, word gap), each an EW mean and
   variance in dits of `dit_estimate_ms` with an observation count. One per
   `TrackDecoder`. Marks are labelled by the decoded character's pattern;
   gaps by their classification; flushed word gaps enter at their closed
   length, or at the flush length when the next mark absorbs a remainder
   shorter than the demod's debounce window; a word gap of
   `max(15, 4 × char gap)` dits or more is a pause and is not learned. A
   drift re-init resets it.
2. **Gap unit** (D3): every gap that meets `GapClassifier` (classify, the
   safety-net flush threshold, the Farnsworth flushed-gap folds) is measured
   in `dit_estimate_ms`, the symmetric mark/gap period, not `mu_dit_ms`.
   Element gaps replayed after tracker warm-up now also feed `mu_egap`.
3. **Sender word boundary** (D4): without Farnsworth, the char/word
   threshold is `sqrt(char_gap · word_gap)`, or `char_gap · sqrt(7/3)` before
   a word gap is learned, clamped to [3, 12] dits. Farnsworth still wins.
4. **Dash/dot ratio** (D5): `mu_ratio_bounds` upper end 4.5 → 6.0, and the
   re-anchor target is the sender's learned ratio (3.0 until both mark
   classes have 3 observations).
5. **Diagnostics** (D8): a `TimingUpdate` decoder event per track on the
   `TrackMeta` cadence when a class gained an observation, plus once at
   finish. A drift re-init that wipes timing already reported also
   triggers one (wiped classes `null`; Codex on PR #273), so a track that
   closes before relearning does not leave the old scale's update as its
   last; `manta decode --json` adds top-level `timing`; `manta decode
   --timing` prints a `timing (dits, mean ± sd): …` stderr line after the
   summary. It is opt-in so that plain `decode`'s stderr stays MAN-270's
   single summary line.
6. **Testkit and vectors** (D9, D10): `KeyerSpec`/`SignalSpec` gain
   `dit_units` and `element_gap_units` (1.0 everywhere before, V1 renders
   bit-identically: `existing_vectors_render_unchanged`); VW1–VW5 in
   `crates/manta-cli/tests/golden_vw.rs`; `manta gen vw1`…`vw5`.

The HSMM engine is unchanged and emits no `TimingUpdate`. `decode.engine`
stays `legacy`. No config key, spot, telnet or JSON-stream schema change.

## Mechanism and evidence

**Scenario 2 (long gaps).** The transmitter's rise time shortens every
mark and lengthens every gap by the same amount (MAN-103). In mark dits a
4.5-dit letter gap read above the fixed 5.0 word boundary. Envelope level
(`crates/manta-testkit/tests/roundtrip_envelope.rs`, 25 WPM, 5 ms rise):

| case | base | MAN-315 |
|---|---|---|
| `CQ TEST W1AW`, gaps 4.5 / 9 | `C Q T E S T W 1 A W` | `CQ TEST W1AW` |
| 15 / 25 / 35 WPM, ×2 | 25 WPM: 20 words | 6 words each |
| ×3, 5 % jitter (seed 1) | — | exactly 9 words |
| 35 WPM 2.5 / 5, 8 % jitter | ends `JA1ZZZ JA1ZZZ TEST` | unchanged |
| weight 4.0 / 4.5 / 5.0, clean | exact | exact |

**Scenario 1 (heavy dashes).** On VW1's noise seed a 13 ms noise fragment
seeds the dit cluster (clamped to 20 ms); real 48 ms dits join the dash
cluster, and the old 4.5 ceiling re-anchored `mu_dah = 3 · mu_dit` on every
mark, so the dit never healed. With the 6.0 ceiling the dash centroid can
climb far enough for real dits to fall below the boundary
(`noise_seeded_init_heals_within_a_callsign_for_heavy_dashes`: healed within
60 marks; `noise_seeded_scale_recovers_within_three_callsigns`).

**Scenario 3 (per-track timing).** Each `TrackDecoder` owns its
`SenderTiming`; VW5's two senders report their own estimates on their own
`track_id`s.

### VW1–VW5, base decoder vs MAN-315 (`manta decode --json`, legacy)

| vector | base CER | MAN-315 CER | learned timing (last `TimingUpdate`, dits) |
|---|---|---|---|
| VW1 heavy-dash | 0.5169 (dash/dot confusion past word 20) | 0.2034 (exact from word 9) | dot 0.95, dash 4.06 |
| VW2 long-gaps | 0.0492 | 0.0410 | char gap 4.53, word gap 8.87 |
| VW3 bug-key | 0.0349 | 0.0349 | dot 0.86, dash 4.80, element gap 1.18 |
| VW4 heavy hand-key | 0.0375 | 0.0375 | dot 1.19, dash 3.38, element gap 0.85 |
| VW5 A / B | 0.0193 / 0.0197 | 0.0193 / 0.0197 | A dash 2.96, char gap 3.00, word gap 7.00; B dash 4.13, char gap 4.50, word gap 9.14 |

Base measured on commit `5346c33` with only the testkit's VW vectors added
(a throwaway probe calling `decode_samples`); MAN-315 by `golden_vw.rs`.
VW3 and VW4 pass their gates on both decoders; they pin that the learned
non-ideal weighting does not regress them.

### Regression

`cargo test -p manta-decode -p manta-testkit -p manta-engine` and the
golden suites `golden_v1`, `golden_v2_v3`, `golden_v7_v9_v10`,
`golden_v8_v8w`, `golden_decimated_capture` and `golden_vr` keep their
pass set. Known-failing ignored tests, legacy engine (`-- --ignored --skip
hsmm`): see Implementation verification.

## Alternatives rejected

- **Element-gap unit** (gaps in learned element-gap dits): merged every
  word of a tight 2.5 / 5 sender at 35 WPM (planning probe: 1 word).
- **Scaling the flush threshold by the sender boundary**: moves the safety
  net's timing, and the word-boundary emission latency, for every sender.
- **A `timing` field on `TrackMeta`**: about 24 literal sites against about
  seven exhaustive matches for a new variant.
- **Learning the 2.0 element/character boundary**: MAN-111's scored
  boundaries own it, from these variances.
- **Degenerate-scale dash re-split (planned D6), dropped.** When the marks
  labelled as dashes were bimodal, the plan re-split the mark scale from
  them. It healed VW1 a callsign sooner (first exact `K1ABC` at word 7,
  CER 0.110), but it also fired on a fading V8w track and spotted a bogus
  call: `v8w_pileup_fading_spots_no_bogus_callsigns` failed with `AC3N`
  (1 of 29 spotted). Requiring the short side at ≥ 2 × `mu_dit` and the long
  side at ≥ 1.4 × `mu_dah` still spotted `AC3N`; with the rule disabled the
  test passes. It is left to MAN-113's WPM hypotheses, which the plan
  already expected to subsume it. VW1's CER bound is 0.25 accordingly
  (measured 0.2034 without the rule).

## Test fixtures

- `crates/manta-decode/src/timing.rs`: `sender_timing_*`,
  `sender_word_dits_*`, `pause_rule_scales_with_the_char_gap`,
  `ratio_ceiling_admits_heavy_dashes`, `ratio_hint_sets_reanchor_target`,
  `noise_seeded_init_heals_within_a_callsign_for_heavy_dashes`,
  `drift_reinit_bumps_the_regime`, `timing_estimates_display_line`.
- `crates/manta-decode/src/decoder.rs`: `sender_timing_reset_on_drift_reinit`,
  `noise_seeded_scale_recovers_within_three_callsigns`,
  `legacy_emits_timing_update_after_meta_tick`, `hsmm_emits_no_timing_update`,
  `flushed_word_gap_closed_by_the_next_mark_is_learned`,
  `timing_update_reports_a_drift_reset_with_no_new_observation`.
- `crates/manta-decode/src/events.rs`: `timing_update_json_shape`,
  `timing_update_roundtrips`.
- `crates/manta-testkit/tests/roundtrip_envelope.rs`: the table above.
- `crates/manta-testkit/src/keyer.rs`:
  `dit_and_element_gap_units_change_segment_lengths`;
  `crates/manta-testkit/src/vectors.rs`: `existing_vectors_render_unchanged`.
- `crates/manta-engine/src/lib.rs`: `decode_report_timing_is_primary_tracks_last`.
- `crates/manta-cli/tests/cli.rs`: `decode_prints_timing_line_for_primary_track`.
- `crates/manta-cli/tests/golden_vw.rs`: VW1–VW5.

## Implementation verification

Run in the implement phase, 2026-10-12:

- All of the above pass; the decode/testkit/engine suites and the
  non-ignored golden suites listed under Regression pass.
- VW5's tracks report frequencies 19.3 and 22.2 Hz above their signals on
  both the base and the MAN-315 decoder, so the test matches a track
  within half a channel (46.875 Hz) rather than the planned 15 Hz. Both
  decodes are exact; the offset is a frequency-estimate matter.
- Ignored legacy-engine gates (`-- --ignored --skip hsmm`), still failing
  as before and not worse: V2-CER 0.0244 (base 0.0244), V5 0.7604 (base
  0.750; bound 0.80), V8w ≥ 90 % 6/34 (base 6/34).
  `char_gap_dits_failure_sweep_envelope` (ignored) passes.
- Code review: the space a stream ends inside was learned as a word or
  letter gap at `finish` (V1 reported word gap 7.82 ± 1.89 for an ideal
  7-dit sender). `finish` now classifies that space without learning it
  (`trailing_space_at_finish_is_not_learned`).

## Follow-ups

- MAN-111: score the element/character and character/word boundaries
  from `SenderTiming`'s variances; per-class beam sigma.
- MAN-113: one `SenderTiming` per WPM hypothesis; revisit the dropped
  dash re-split there.
- VW5: the adjacent pair's ~20 Hz frequency bias.
- Detector/channelizer: senders two channels (187.5 Hz) apart produce
  crosstalk and track churn (planning probe: 17 tracks, mixed text).
- HSMM: fixed 1/3/1/3/7 segment nominals; feed it `SenderTiming` if HSMM is
  ever promoted.
- The first gap of a stream, before any class is learned, can still
  mis-split under ≥ 8 % jitter.
- `edge-legacy` under heavy Farnsworth (Codex review, PR #273):
  `check_flush_edge` has none of `check_flush`'s Farnsworth bookkeeping
  (`observe_flushed`, `confirm_rebuilt`, the one-character-word closed
  lengths for `reinit_from_flushed`; MAN-103 D8, MAN-264). At 18/5 to
  30/12 WPM `edge-legacy` decodes every character as its own word for the
  whole transmission, as it does on main, and at 20/8 to 30/12 WPM
  `SenderTiming` learns those flushed ~14.5-dit character gaps as word
  gaps, which is how the decoder classified them. Copying the bookkeeping
  into `check_flush_edge` fixes the decode at all four speeds, but it
  merges every later word in the cold-start inputs of
  `legacy_pauses_after_one_char_words_do_not_merge_later_words` and
  `legacy_one_char_words_after_a_false_rebuild_do_not_confirm_it` run
  under `edge-legacy`, which decode correctly without it. Traced:
  `edge-legacy`'s cold start classifies a 15-dit first gap, and with three
  censored 7-dit flushed gaps the long-gap pair becomes Farnsworth-shaped.
  That is no `reinit_from_flushed` rebuild, so the tail's 3-dit character
  gaps do not undo it. Both need fixing together.
