# MAN-203: reversible input conditioning, first slice (the DC-removal stage)

Date: 2026-11-03. Ticket: MAN-203, "manta should offer reversible correction for
DC offset, IQ imbalance, and short impulses instead of only diagnosing them".
Source: `docs/DECISIONS/2026-09-09-decoder-recall-research.md`, investigation 13.
Precedent: `docs/DECISIONS/2026-10-15-man234-bad-input-samples.md` (its
"MAN-203 hand-off" names this ticket and the channelizer choke point).

## What landed

- `manta_dsp::condition`: `StageMode::{Off, Observe, Apply}`,
  `ConditioningConfig`, the span log (`ConditioningLog`, `MarkedSpan`, a
  ring of 256 spans with a `spans_dropped` count and exact per-(stage, mark)
  counters) and `InputConditioner`.
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
  of the same file with a different config. The planned order is impulse,
  then DC, then IQ; this slice has the DC stage only.
- **D2. Modes.** `Off` (the default; `InputConditioner::new` returns `None`,
  so an all-off channelizer does no per-sample work), `Observe` (measure,
  never write a sample, mark nothing) and `Apply` (correct, and mark every
  corrected sample `Corrected`).
- **D3. Annotation.** Spans and counters, not a per-sample bitmask. Span
  indices are channelizer-input indices, counting any lead-in padding the
  caller feeds. Adjacent same-(stage, mark) spans coalesce, so a DC stage in
  `Apply` keeps one span for the whole stream.
- **D4. Reversibility.** `revert_dc(y, fs)` rebuilds the estimate from the
  corrected samples and returns `x[n] = y[n] + dc[n]`. The estimate's update
  reads only the f32 output, so the inverse recomputes every `dc[n]`
  bit-for-bit, and the restored input matches up to the f32 rounding of `y`
  (test `dc_blocker_reverts_exactly`, including a 1e9 impulse hitting the
  clamp). The operational reversals are the mode switch and raw recordings.
- **D5. DC blocker.** First order, corner `DC_CORNER_HZ = 5.0`, fast start
  `a_n = max(2π·5/fs, 1/(n+1))`, f64 state, `y[n] = x[n] - dc[n]`, then
  `dc[n+1] = dc[n] + a_n·clamp(y[n])` with the update's magnitude clamped to
  `DC_UPDATE_CLAMP = 1.0` (full scale). A stray impulse moves the estimate
  by at most `a_n` (test `dc_update_clamp_bounds_an_impulse_kick`). The
  corner is a constant: 2, 5 and 20 Hz were indistinguishable in the
  research, and a station within about 40 Hz of the centre is undecodable
  in bypass anyway. 60 Hz loses under 0.05 dB
  (`dc_corner_attenuates_60_hz_by_under_0_05_db`).
- **D9. `check` and `calibrate` stay raw.** `check` reports the receiver's
  own level and floor, so it keeps `Channelizer::new`. `calibrate` has no
  channelizer.
- **D13. Fixtures avoid known confounds.** One near-centre station per DC
  scene (stations 90 Hz apart interfere with each other), trials 1 and up
  (trial 0 has five A-prefix calls that never spot), and every gate is a
  measured count with a bypass control in the same test.
- **D14. Cost.** Off adds one `Option` check per `process` call. The DC
  stage is a few f64 operations per sample (about 9 ns/sample in the
  research). The Pi 4 budget is a profile, not a CI gate.

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
| +150 Hz, DC 0.005: false tracks within 100 Hz of the centre | **5** | **0** |
| ten 12 dB stations plus one 1e8 impulse at 5 s | 0 of 10 | 0 of 10 |

The last row is the impulse stage's baseline: DC removal alone neither
helps nor hurts there (its update clamp keeps the impulse from moving the
estimate). The impulse stage is a follow-up (below).

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

1. The impulse suppressor (median-power gate, window 33, K = 12, runs of at
   most 4 samples repaired, a pause rule, an originals ring for the inverse).
2. IQ-imbalance correction from a tone-measured calibration, the blind
   mirror-image monitor, and the `uncertain` mark (Gherkin scenario 2: mirror
   energy alone never turns the correction on).
3. `manta doctor` observing every stage, its `input:` lines,
   `--iq-tone-hz` and the `inputConditioning` JSON object.
4. The six `[input]` keys, the `decode --json` field and the
   `manta_input_conditioned_samples_total` metric.
5. The operator docs (CONFIG, CLI-JSON, METRICS, the runbook, CHANGELOG).

Noise whitening stays deferred (plan D12: a coloured-noise probe found no
loss tied to noise colour). Nothing here blanks long intervals, notches a
keyed station or applies a time-varying gain.
