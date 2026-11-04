# MAN-203: reversible input conditioning, first slices (DC removal and impulse suppression)

Date: 2026-11-03. Ticket: MAN-203, "manta should offer reversible correction for
DC offset, IQ imbalance, and short impulses instead of only diagnosing them".
Source: `docs/DECISIONS/2026-09-09-decoder-recall-research.md`, investigation 13.
Precedent: `docs/DECISIONS/2026-10-15-man234-bad-input-samples.md` (its
"MAN-203 hand-off" names this ticket and the channelizer choke point).

## What landed

- `manta_dsp::condition`: `StageMode::{Off, Observe, Apply}`,
  `ConditioningConfig`, the span log (`ConditioningLog`, `MarkedSpan`, a
  ring of 256 spans per (stage, mark) with a `spans_dropped` count and
  exact per-(stage, mark) counters) and `InputConditioner`.
- `manta_dsp::condition::impulse`: the short-impulse suppressor
  (`ImpulseSuppressor`), its originals ring (`Repair`) and its exact
  inverse `revert_repairs` (plan Phase 4).
- `manta_dsp::condition::dc`: the DC-removal stage (`DcBlocker`) and its
  exact inverse `revert_dc`.
- `Channelizer::with_conditioning(fs, center, &ConditioningConfig)` runs the
  chain on every input sample right after the MAN-234 sample guard (SPEC
  §1.3 step 0a). `Channelizer::new` delegates with the all-off config and is
  bit-identical to before: no conditioner is built.
- `PipelineConfig::conditioning` (default all off) reaches every decode
  channelizer: `decode_source` (`decode`, file replay) and the listen loops'
  `new_segment` (`run`, `soak`, `doctor`). `check` keeps `Channelizer::new`
  (D9).

Every default is off, so no golden vector, fixture, spot log or `decode`
output changes, and no operator surface changes in this slice.

## Decisions

- **D1. Placement.** The chain runs inside `Channelizer::process`, after
  `sample_guard::replace_unusable`, on the newly appended samples. That is
  the MAN-234 D1 choke point: every decode path goes through it, and raw
  recordings (taken upstream) stay raw, so an A/B comparison is a re-decode
  of the same file with a different config. The order is impulse, then DC
  (a 1e9 sample would otherwise kick the DC estimate), then IQ; the IQ
  stage is not built yet.
- **D2. Modes.** `Off` (the default; `InputConditioner::new` returns `None`,
  so an all-off channelizer does no per-sample work), `Observe` (measure
  and mark, never write a sample or delay a hop) and `Apply` (correct, and
  mark what it changed). The DC stage marks every corrected sample
  `Corrected` in `Apply` and nothing in `Observe`. The impulse stage marks
  repaired runs `Repaired`, runs it judged impulsive but left alone
  (`Observe`, too long, or paused) `Flagged`, and a block it paused itself
  in `Uncertain`.
- **D3. Annotation.** Spans and counters, not a per-sample bitmask. Span
  indices are channelizer-input indices, counting any lead-in padding the
  caller feeds. Each (stage, mark) has its own ring of 256 spans, so each
  ring is in stream order however the input is split; a span that starts
  where its ring's newest span ends extends it, so a DC stage in `Apply`
  keeps one span for the whole stream. `spans()` lists them stage by
  stage, then mark by mark.
- **D4. Reversibility.** `revert_dc(y, fs)` rebuilds the estimate from the
  corrected samples and returns `x[n] = y[n] + dc[n]`. The estimate's update
  reads only the f32 output, so the inverse recomputes every `dc[n]`
  bit-for-bit, and the restored input matches up to the f32 rounding of `y`
  (test `dc_blocker_reverts_exactly`, including a 1e9 impulse hitting the
  clamp). `revert_repairs(y, first_sample, log)` writes back the original
  value of every repaired sample from the log's originals ring (4096 runs,
  older ones dropped and counted in `repairs_dropped`), bit for bit (test
  `repairs_revert_bit_exactly`). The chain reverts in reverse order: DC
  first, then the repairs. The operational reversals are the mode switch
  and raw recordings.
- **D5. DC blocker.** First order, corner `DC_CORNER_HZ = 5.0`, fast start
  `a_n = max(2π·5/fs, 1/(n+1))`, f64 state, `y[n] = x[n] - dc[n]`, then
  `dc[n+1] = dc[n] + a_n·clamp(y[n])` with the update's magnitude clamped to
  `DC_UPDATE_CLAMP = 1.0` (full scale). A stray impulse moves the estimate
  by at most `a_n` (test `dc_update_clamp_bounds_an_impulse_kick`).
  Exact-zero samples before the first nonzero one leave the state untouched,
  so the engine's lead-in padding neither uses up the fast start nor shifts
  the inverse (test `leading_zeros_keep_the_fast_start_and_the_inverse`).
  Known limit, for the follow-up: a sample the MAN-234 guard replaced with
  silence mid-stream is treated as input, so with `Apply` it leaves as
  `-dc` and nudges the estimate; keeping it silent and the inverse exact
  needs the guard's replacement positions in the log. The
  corner is a constant: 2, 5 and 20 Hz were indistinguishable in the
  research, and a station within about 40 Hz of the centre is undecodable
  in bypass anyway. 60 Hz loses under 0.05 dB
  (`dc_corner_attenuates_60_hz_by_under_0_05_db`).
- **D6. Impulse suppressor.** A sample is flagged when its power exceeds
  `IMPULSE_RATIO² = 144` times the reference power of the
  `IMPULSE_WINDOW = 33` samples centred on it (fewer at the stream start).
  The reference is the window's mean power once its `IMPULSE_TRIM = 8`
  strongest powers (two repairable runs) are left out; a zero reference
  never flags, so lead-in padding is left alone. A flagged run of at most
  `IMPULSE_MAX_RUN = 4` samples is repaired by linear interpolation between
  its unflagged neighbours; a longer run is counted in `impulse_long_runs`,
  marked `Flagged` and never written. A burst of 9 samples or more is not
  flagged at all, because more of it than the trim stays in the reference;
  it is never touched either (test
  `runs_longer_than_four_samples_are_never_touched`). A keyed carrier's
  edge is never flagged, because at least 9 of the window's 25 kept
  samples are on the carrier side of the step (test
  `a_keyed_carrier_step_is_never_flagged`).
  **Deviation from the plan, measured.** The plan's reference was the
  window's median. On the clean 45 dB `series_scene`, trials 1–4, the
  median gate repaired 15, 98, 474 and 737 samples, paused trials 3 and 4
  (288000 and 480000 samples marked uncertain), and cost trial 4 one
  station (10 to 9). Two strong stations whose beat is near `fs/2` leave
  every other sample near zero, so the median sits in the nulls. The
  trimmed mean touches 0, 0, 0 and 5 samples there, never pauses, keeps
  every call, and still recovers every impulse row below. Pause
  rule: when more than `IMPULSE_PAUSE_FRACTION = 1e-3` of a one-second block
  (`round(fs)` samples) is flagged, the next block repairs nothing and is
  marked `Uncertain` (test
  `dense_flags_pause_the_next_block_and_mark_it_uncertain`). Deciding a
  sample needs `IMPULSE_LOOKAHEAD = 16 + 4 + 1 = 21` later samples, so an
  applying suppressor releases each sample exactly 21 samples late; the
  channelizer forms windows only from released samples (`Channelizer`'s
  `ready` watermark), so sample indices do not move and at most the
  stream's final `ceil(21 / hop)` hops are never formed: one hop at every
  rate from 12 kS/s up, where `hop >= 32` (test
  `impulse_apply_loses_at_most_the_final_hop`, at 96 kS/s). The last 16
  samples of a stream, or of a decode segment, are never judged, and a
  run still open there is neither repaired nor logged. A run with any
  sample in a paused block is not repaired (test
  `a_run_reaching_into_a_paused_block_is_not_repaired`). In `Observe` nothing is
  held back and hops are bit-identical to `Off`
  (`impulse_observe_never_changes_a_hop`). Chunk invariance holds for the
  hops and the whole log (`impulse_apply_is_chunk_invariant`, chunk sizes
  1, 13 and 4096).
- **D9. `check` and `calibrate` stay raw.** `check` reports the receiver's
  own level and floor, so it keeps `Channelizer::new`. `calibrate` has no
  channelizer.
- **D13. Fixtures avoid known confounds.** One near-centre station per DC
  scene (stations 90 Hz apart interfere with each other), trials 1 and up
  (trial 0 has five A-prefix calls that never spot), and every gate is a
  measured count with a bypass control in the same test.
- **D14. Cost.** Off adds one `Option` check per `process` call. The DC
  stage is a few f64 operations per sample (about 9 ns/sample in the
  research). The impulse stage keeps a sorted window of 33 powers (a
  binary search, an insert and a remove per sample) and sums its 25 kept
  powers per sample. The research measured a median gate at about
  65–73 ns/sample; the trimmed mean was not timed here. The Pi 4 budget is
  a profile, not a CI gate, so this is recorded, not gated.

## Measured: DC removal beats bypass (Gherkin scenario 1)

`cargo test -p manta-engine --locked --test man203_conditioners -- --nocapture`,
2026-11-03. One station (`series_calls(trial)[0]`, 25 WPM) per scene at
6 dB (2500 Hz reference), noise seed 203, 30 s, 96 kS/s at 14 MHz, DC offset
`(d, 0.7 d)`, trials 1–4. A hit is a spot of the right call within 150 Hz.

| scene | bypass | `dc = Apply` |
|---|---|---|
| +60 and +150 Hz, DC 0.01 | **0 of 8** | **8 of 8** |
| +60 and +150 Hz, no DC (control) | 8 of 8 | 8 of 8, same calls |
| +900 Hz, DC 0.01 | (not run) | 4 of 4 |
| +150 Hz, DC 0.003: false tracks within 100 Hz of the centre | **4** | **0** |
| ten 12 dB stations plus one 1e8 impulse at 5 s | 0 of 10 | 0 of 10 |

The false-track row first used DC 0.005 (bypass 5, apply 0). After main
gained MAN-112's close-pair association, DC 0.005 grows no false track in
bypass, so the row moved to DC 0.003, where bypass still grows one in all
four trials.

The last row is the impulse stage's baseline: DC removal alone neither
helps nor hurts there (its update clamp keeps the impulse from moving the
estimate).

## Measured: impulse suppression beats bypass

Same command and date. Ten series stations (`series_scene`, 25 WPM) at
12 dB, noise seed 203, 30 s; a hit is the right call within 150 Hz.

| scene | clean | bypass | `impulse = Apply` |
|---|---|---|---|
| one 1e8 impulse at 5 s, trials 1 and 2 | 10, 10 | **0, 0** | **10, 10**, the clean calls; 1 sample repaired |
| one 1e9 impulse at 5 s, trials 1 and 2 | 10, 10 | **0, 0** | **10, 10**, the clean calls; 1 sample repaired |
| 290 impulses of amplitude 30 (10 a second), trial 1 | 10 | **1** | **10**, the clean calls; 290 repaired |
| no impulse, 12 dB, trial 1 (control) | 10 | 10 | 10, same calls |
| no impulse, 45 dB, trials 1–4 (control) | 10 each | 10 each | 10 each, same calls |
| no impulse, 12 dB and 45 dB, trials 1–4: samples touched | — | — | 0 at 12 dB; 0, 0, 0, 5 at 45 dB; no pause |

Amplitude 10 at 10 a second left bypass at 10 of 10, so the train uses 30
(600 times the noise rms). The plan's clean-scene bound is 16 touched
samples of 2.88 million, with no pause.

Tests: `impulse_suppression_recovers_a_single_huge_impulse`,
`impulse_suppression_recovers_an_impulse_train` and
`impulse_suppression_leaves_clean_scenes_alone` in
`crates/manta-engine/tests/man203_conditioners.rs`; the `condition::impulse`
unit tests; and the channelizer's `impulse_apply_is_chunk_invariant`,
`impulse_apply_loses_at_most_the_final_hop` and
`impulse_observe_never_changes_a_hop`.

Tests: `bypass_loses_a_station_near_a_dc_offset`,
`bypass_grows_a_false_dc_track_at_a_small_offset`,
`bypass_is_silenced_by_one_huge_impulse`,
`dc_removal_recovers_a_station_near_a_dc_offset`,
`dc_removal_does_not_regress_the_clean_control`,
`dc_removal_removes_the_false_dc_track` and
`dc_apply_alone_is_no_worse_than_bypass_on_a_huge_impulse` in
`crates/manta-engine/tests/man203_conditioners.rs`; the channelizer's
`all_off_conditioning_is_bit_identical_to_new`, `observe_never_changes_a_hop`,
`dc_apply_removes_the_centre_channel_offset` and
`conditioned_process_is_chunk_invariant`; and the `condition` module's span
log tests.

## What is NOT in this slice

The MAN-203 plan's later phases are follow-up work, in this order:

1. IQ-imbalance correction from a tone-measured calibration, the blind
   mirror-image monitor, and the `uncertain` mark (Gherkin scenario 2: mirror
   energy alone never turns the correction on).
2. `manta doctor` observing every stage, its `input:` lines,
   `--iq-tone-hz` and the `inputConditioning` JSON object.
3. The six `[input]` keys, the `decode --json` field and the
   `manta_input_conditioned_samples_total` metric.
4. The operator docs (CONFIG, CLI-JSON, METRICS, the runbook, CHANGELOG).

Noise whitening stays deferred (plan D12: a coloured-noise probe found no
loss tied to noise colour). Nothing here blanks long intervals, notches a
keyed station or applies a time-varying gain.
