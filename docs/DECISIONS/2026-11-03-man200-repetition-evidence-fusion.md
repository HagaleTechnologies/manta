# MAN-200: damaged repetitions of one self-ID can confirm the call

Date: 2026-11-03. Ticket: MAN-200.

## Problem

The repetition gate (`RepetitionGate`, `crates/manta-spot/src/gate.rs`) keys
its memory by `(frequency bucket, exact callsign)`. A call spots only when
enough separate decodes read it exactly, so at least one perfect copy, and
usually two, must exist in the 90 s window. Under correlated fading and QRM
the chance that every character of a call survives compounds across the
call, so "one perfect copy" is a low-recall requirement. Two copies `W1AV`
and `W1AQ` of the station W1AW never meet.

The gate also judged message-distinctness from word numbers and time alone.
Nothing compared the sample intervals two decodes came from, so the same
samples decoded twice could count twice.

Measured through `Validator::new(96_000.0, <K,W,N,AA,AB,AC cty fixture>,
None)` with `record_decisions(true)` at `212fa51`, characters at confidence
0.95:

- Pattern tiers: `W1AW`, `W1AV`, `W1AQ`, `K1ABC` are all `Common` (2 reps).
- **Ticket scenario 1.** `CQ W1AV K` then `CQ W1AQ K` 100 000 samples apart:
  `W1AV` withheld `Repetition` reps 1 of 2, `W1AQ` withheld `Repetition`
  reps 1 of 2, no spot.
- **Ticket scenario 2 failed.** `CQ W1AW K` with the W1AW characters at 100,
  200, 300, 350 and its boundary at 400, then a replay `CQ W1AW K` on the same
  track with the W1AW characters at 150, 250, 350, 400 and its boundary at
  420: `W1AW` withheld reps 1, then `W1AW` spotted `Confirmed` reps 2. The
  replay's word number is 3 past the first copy, so the word-gap rule counted
  it as a new message. With word numbers 0 and 1 (the research's probe) the
  word-gap rule collapses the pair, which is why the research reported this
  shape as holding.
- Gate only: `record(1, 14 MHz, "W1AW", 400, seq 1)` then `(…, 420, seq 2)`
  returns `1 1`; with `seq 4` it returns `1 2`.
- `CQ DE W1AV W1AQ K` twice: `W1AV` spots `Confirmed` at the second message;
  `W1AQ` gets no decision, because MAN-201's rule 8 makes a different call
  later in the same turn a mention. Different damage inside one message never
  reaches a gate, and this ticket does not change that.
- Length-changing damage `CQ W1A K`, `CQ W1AWE K`, `CQ W1AW K`: `W1A` withheld
  at reps 1 of 5 (`Unlisted`), `W1AWE` and `W1AW` at reps 1 of 2. No spot,
  even with one clean copy.
- A wrong repeated reading, `CQ W1AV K` twice: `W1AV` spots `Confirmed`.
  Fusion must not make this worse; it cannot fix it either.

### Is a character's confidence evidence of damage?

A release-profile probe (`#[ignore]`, 89 s, not committed) decoded the MAN-116
pileup (`series_signals`, trial 0, seed 1, 90 s, `PipelineConfig::default()`,
legacy engine) at 7 points. For decoded words of the same length as the
station's call (station chosen by track frequency within 150 Hz) with 1 or 2
wrong characters, it compared the confidence of the wrong characters with the
right ones:

| Point | Exact copies | 1-2 substitution copies | Right-char conf p10/p50/p90 | Wrong-char conf p10/p50/p90 |
|---|---|---|---|---|
| poor 25 WPM 6 dB | 35 | 6 | 0.300/0.300/0.536 | 0.300/0.300/0.414 |
| poor 25 WPM 9 dB | 47 | 13 | 0.300/0.460/0.716 | 0.263/0.300/0.502 |
| poor 25 WPM 12 dB | 66 | 7 | 0.352/0.499/0.748 | 0.252/0.529/0.607 |
| poor 15 WPM 9 dB | 20 | 8 | 0.300/0.556/0.750 | 0.300/0.423/0.719 |
| poor 15 WPM 15 dB | 38 | 9 | 0.527/0.761/1.000 | 0.300/0.683/0.872 |
| good 15 WPM 3 dB | 25 | 14 | 0.300/0.300/0.363 | 0.238/0.297/0.300 |
| good 15 WPM 6 dB | 42 | 8 | 0.300/0.300/0.473 | 0.295/0.300/0.300 |

Pooled, below a 0.5 threshold sit 56 of 73 wrong characters, 178 of 281 right
characters in damaged copies, and 750 of 1476 characters of exact copies.
Under the default engine a character's confidence barely tells a damaged
character from a correct one, and most copies are right even at confidence
0.300, so confidence is not a probability either (SPEC §4.6 calls it
uncalibrated). Votes therefore count copies (D4).

The ticket's literal pair damages the same position (`W` read as `V` and as
`Q`). From top-1 text alone `W1AW` cannot come out of `W1AV` plus `W1AQ`: no
copy read `W` there. Scenario 1 holds only when the copies carry `W` as a
per-character alternative, which the HSMM engine emits and the default legacy
engine does not (`crates/manta-decode/src/decoder.rs`). V60 encodes the
scenario that way; V61 covers the legacy shape.

## Decision

Two parts. The same samples count once, always on. An evidence lattice per
self-ID episode, opt-in through `[spot] repetition_fusion` (default `false`),
lets damaged copies confirm the call a clear per-position majority reads.

- **D1. Two parts, one always on.** The span rule fixes double counting and
  can only lower a count, so it is always on. Fusion can create spots, so it
  is opt-in, following the MAN-317 `[decode] prior` precedent.
- **D2. A span** is the word's first character's `sample_ts` to its
  `WordBoundary` `sample_ts`, closed at both ends (MAN-207's D8 definition).
  Both ends are event times, which lag keying by the same filter support on
  every track of an input, and the start misses the first character's
  duration. Two decodes of the same keying still overlap. Two separate
  utterances never do: a word's boundary is its last character's time, and
  the next word's first character comes strictly later.
- **D3. Overlap is checked within one gate entry, one ledger entry or one
  lattice episode**, never across different calls. Two stations keying at
  once on one channel are different calls whose spans overlap, and both are
  real. A gate touch that overlaps a stored span is not pushed to the
  accepted list, its track's last-seen entry takes the overlapped span's
  anchor under the never-regress rule, and the tally is returned unchanged.
- **D4. Votes count copies.** At a position each copy votes 1 for its top
  glyph, 1 for "nothing here" when the alignment gives it a gap, and 1 for
  unknown when its character is the `?` placeholder. An alternative `(g, a)`
  adds `min(a / top_confidence, 1.0)` for `g` when the top confidence is
  finite and positive; prosign alternatives are ignored. Confidence is not
  used otherwise (measured above).
- **D5. A position is decided** when one candidate (a glyph or "nothing
  here") has the unique highest vote, more than half the copies
  (`vote > N / 2`), and a lead of at least `MIN_COLUMN_MARGIN = 0.5` over
  every other candidate, unknown included. "Nothing here" winning drops the
  position, which is how insertions go away. Any undecided position makes the
  explicit unknown hypothesis win, and nothing is confirmed. For top-1 copies
  the margin is an integer, so the rule reads "a strict majority and at least
  one more copy than any rival": 2 of 3, 3 of 5.
- **D6. Episodes.** Key: the input (`Validator::input_of`), a channel (within
  `gate::SAME_SIGNAL_MAX_HZ` of the episode's anchor frequency, the newest
  copy's) and the 90 s window. A copy joins an episode only if it is at most
  `MAX_JOIN_EDITS = 2` edits (`variant::MAX_EDIT_DISTANCE`) from every copy
  already in it. With several candidates: the smallest worst distance, then
  the newest activity, then the lowest episode id. Otherwise it opens a new
  episode. Copies shorter than 3 characters (the exchange grammar's call
  minimum) never enter.
- **D7. Independence.** A copy whose span overlaps a copy already in an
  eligible episode (same input, same channel, at most 2 edits apart) is a
  duplicate of it. It adds no observation and no vote, even if it reads
  better; the first arrival stands, as the gate freezes its verdicts.
  Message-distinctness reuses the gate's own `classify_new_occurrence` and
  `promote_orphaned_leader` over the episode's observations, so "one message
  counts once" means the same thing in both places.
- **D8. Confirmation bar.** Every position decided (D5); the winner at least
  3 characters; message-distinct observations within 2 edits of the winner
  at least `required_reps(tier(winner))`. Then the winner goes through the
  same checks as any candidate: blocklist, notch, grammar and cty (unless
  allowlisted), zero evidence, variant arbitration (rivals that are this
  episode's own copy texts are ignored), the dedupe watermark and assess,
  then the callsign lookup (`admit`).
- **D9. A copy that already spotted is a station, not damage.** If any
  observation that disagrees with the winner has a text that dedupe shows as
  spotted on this channel and input, the fusion is refused.
- **D10. When fusion runs.** Only on a candidate's first evaluation (never on
  a reclassification), only for a non-allowlisted, non-beacon candidate, and
  only when the exact path did not confirm it: after a `Grammar`, `Cty` or
  `Repetition` withhold. Copies enter the lattice before the operator and
  plausibility checks, because a damaged copy is often implausible. If the
  winner is the candidate's own text, the fused spot replaces that
  candidate's `Repetition` decision. Otherwise the candidate's own withhold
  is recorded first, then the winner's decision. A refused fusion records no
  decision for the hypothetical winner.
- **D11. The fused spot** uses the triggering copy's track, `end_ts`,
  frequency, SNR, speed, spot type and RST/QRL annotations. Its confidence is
  `c_call` over, per winner position, the mean top confidence of the copies
  whose top glyph is the winner (the largest alternative score when only
  alternatives named it), with `reps` = the fused count, then the usual SCP
  boost. Its `emission_state` is `Confirmed`.
- **D12. Marker.** `Decision` gains `confirmation: Option<Confirmation>`
  with `Confirmation::{ExactRepeat, Fused}`, set on `Spotted` decisions only:
  `Fused` for a lattice spot, `ExactRepeat` for any other `Confirmed` spot,
  `None` for a tentative one. A spot held for a callsign lookup keeps it.
  Live state serializes it as `confirmation`. `Spot` does not change, so
  telnet, the RBN uplink, `spots.v1` and CLI JSON spots are unchanged.
- **D13. Bounds and determinism.** At most `MAX_OBSERVATIONS = 16`
  observations per episode (oldest evicted) and `MAX_EPISODES = 4096` (the
  episode whose newest observation is oldest is evicted), each eviction
  counted in `LatticeCounts`. All maps are `BTreeMap`; observations are
  aligned in `(span start, track_id, word_seq)` order; votes are `f64`,
  summed in that order. The lattice is swept with the gate. With fusion off
  the validator holds no lattice and allocates nothing.
- **D14. MAN-207 coordination.** MAN-207 (#496) had not merged when this
  landed, so `Word::start_ts` and the span overlap rule are added here with
  MAN-207's D8 definition. Whichever of the two lands second reconciles them
  to one type.

### Decisions taken on the plan's review questions

- **Default off.** Fusion ships with `repetition_fusion = false`, as
  `[decode] prior` did: no real audio corpus is committed, and false spots
  de-list an RBN node. Turning it on by default needs the real-traffic gate
  under Follow-ups.
- **The marker is live-state only.** `confirmation` appears on the
  live-state `candidateDecision` record and the in-process `Decision`, not on
  CLI JSON spots, `spots.v1`, telnet or the uplink. Consumers of those surfaces
  see an ordinary confirmed spot.
- **The bar as built.** A strict majority at every position and a lead of at
  least half a copy, from as many separate messages as the tier needs. The
  calibration ladder below tightens it only if the synthetic grid finds a
  wrong fused call.

## Calibration on the synthetic pileup

The instrument is `crates/manta-engine/tests/man200_fusion_grid.rs`:

```sh
cargo test --release -p manta-engine --test man200_fusion_grid -- --ignored --nocapture
```

It decodes each point of the MAN-116 grid once (`Condition::WattersonGood`
and `WattersonPoor`; 15 and 25 WPM; 3, 6, 9, 12, 15 and 20 dB; legacy engine;
plus the HSMM engine at both conditions, 25 WPM, 6 and 12 dB), replays the
events through a fusion-off and a fusion-on validator, checks the off replay
reproduces the decode's own spots, and prints one `MAN200` row per point. A
wrong call is a spot whose call is none of the 10 station calls, or a station
call more than 150 Hz from that station. It asserts that fusion-on spots no
more wrong calls than fusion-off at any point.

The tightening ladder, applied in order only if a point fails:

1. Every supporting observation within 1 edit of the winner
   (`MAX_SUPPORT_EDITS = 1`; joining stays at 2).
2. A fusion decided by top-1 votes alone also needs one exact copy of the
   winner.
3. Stop tightening: fusion stays off and this record says how many wrong
   calls it produced.

Not measured yet. The implement round of 2026-11-03 ran out of time
before the instrument `crates/manta-engine/tests/man200_fusion_grid.rs` was
written, so no grid rows exist and no ladder step was applied. Fusion
therefore ships default off with no synthetic-grid evidence for or against
it; the instrument and its 28 rows are the first follow-up below, and
default-on stays gated on them and on the real-traffic gate.

### V8, V8w and VC3 with fusion on

`v8_v8w_and_vc3_with_repetition_fusion_on_stay_at_zero_bogus`
(`crates/manta-cli/tests/golden_v8_v8w.rs`) decodes each scene with and
without `[spot] repetition_fusion = true` and asserts 0 bogus calls and the
existing validated floors (45 for V8, `MIN_V8W_VALIDATED` for V8w and VC3).

The test asserts 0 bogus calls and the floors with fusion on and off on
every gate run. Its printed validated counts were not captured in this
round (cargo test hides a passing test's output); rerun it with
`-- --nocapture` to read them.

## Proposed addition to the `liveState.v1` fragment

The MAN-324 record's proposed dispensa fragment
([2026-10-12-man324-live-state-stream.md](2026-10-12-man324-live-state-stream.md#proposed-dispensa-livestatev1-schema-fragment))
gains one `candidateDecision` property, appended to `required` after
`spotId`:

```json
"confirmation": { "type": ["string", "null"], "enum": ["exactRepeat", "fused", null], "description": "How a spotted, confirmed call met its repetition requirement: exactRepeat when separate decodes read it exactly, fused when [spot] repetition_fusion combined damaged copies. Null when not spotted or tentative." }
```

## Accepted costs

- Under the default engine, two copies never fuse: with no alternatives, two
  copies that disagree tie. Fusion there needs three or more copies whose
  damage falls at different positions, or exact copies plus a damaged one.
- Two stations within one channel and 90 s whose calls differ by one
  character can fuse into a call that takes a character from each. D9 catches
  it only when one of them already spotted.
- A clean call after a damaged copy of itself inside one message is still a
  mention or uncertain (MAN-201), so it never reaches fusion.
- After a fused spot, a damaged form that later repeats exactly can still
  spot on the exact path; the lattice does not veto.

## Open claims

Fusion's precision and lift on real traffic are unmeasured: no audio corpus is
committed, and the calibration above is synthetic. HSMM alternative scores
are relative margins, not probabilities; D4's `min(a / top, 1)` is measured
only on the grid's HSMM points.

## Follow-ups

- The calibration instrument `crates/manta-engine/tests/man200_fusion_grid.rs`
  (MAN-116 pileup, 24 legacy points plus 4 HSMM points, fusion off and on,
  asserting wrong calls on <= off) and its rows here, with the tightening
  ladder applied if a point fails.
- Expose the legacy beam's survivors as `alternatives`.
- Element-level (dit and dah) alignment from decoder data.
- The lattice as a subtractive variant arbiter.
- Separating QSO partners on one channel.
- Pooling episodes across receivers.
- A fused-spot metric.
- A real-traffic gate (MAN-198's loss funnel on labelled windows) before
  default-on.

## Evidence and tests

- V59 to V65, `crates/manta-spot/tests/golden_v59_v65_repetition_evidence.rs`
  (SPEC §7.1). V59 runs with the default config; V60 to V65 run with fusion
  on and repeat with it off.
- `crates/manta-spot/src/gate.rs`, `support.rs` and `lattice.rs` unit tests:
  the overlap rule, alignment, votes, the unknown hypothesis, independence,
  joining, bounds and arrival-order independence.
- `v8_v8w_and_vc3_with_repetition_fusion_on_stay_at_zero_bogus`.
- Config: `repetition_fusion_defaults_off_and_reads_true`,
  `repetition_fusion_rejects_a_non_boolean`,
  `env_repetition_fusion_overrides_the_file`,
  `repetition_fusion_reaches_the_pipeline_config`,
  `repetition_fusion_needs_a_restart`, `check_summary_reports_repetition_fusion`.
