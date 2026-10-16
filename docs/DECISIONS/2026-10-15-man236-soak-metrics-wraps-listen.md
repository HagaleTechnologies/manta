# MAN-236: the soak harness measures the loop manta run executes

## Context

`soak_with_metrics`, used by `man19-soak` and `examples/close_counts.rs`,
maintained a separate calibration, channelizer, track manager, validator
and chunk loop. MAN-73 added discontinuity handling to the shipped loop,
and MAN-122 added stop checks during calibration. Neither reached the copy.
A pass from that copy therefore did not exercise the daemon's control flow.

## Decision

`soak_with_metrics` now wraps `listen_with_observers`, the same loop used by
`manta run`, while retaining its public signature, sample/report types,
watchdog, panic capture and RSS-growth acceptance rule. The two callers
need no changes. `peak_rss_bytes`, `WARMUP` and `RSS_GROWTH_LIMIT_BYTES`
have one home in `soak.rs`, shared with the metrics wrapper.

`ListenObservers::track_lifecycle` carries an optional
`TrackLifecycleObserver`. The loop writes relaxed atomics after batches,
never callbacks into another runtime and never per hop. Readers get
individually current fields, without an atomic cross-field snapshot.
The wrapper reads synchronously from `on_tracks`, after publication. A
fresh observer belongs to one run and has one writer. With `None`, the
loop skips all lifecycle counter reads and atomic stores.

The observer publishes the all-entry active-track gauge, its peak, eight
close-reason counts, gate-record total and steady-state chunk count.
Close counts sum completed segments plus the current segment; the
validator spans segments, so its gate total needs no extra accumulation.
The gauge is cleared at EOF and a steady-state read error. Events, spots
and emitted `TrackClosed` events are counted by the wrapper's existing
callbacks. Static crashes use the existing observer, already summed
across segments. The validator still receives raw events and applies
frequency correction once; the wrapper only counts the calibrated
callback events by type.

## Semantics retained

The all-entry gauge includes unconfirmed candidates; the daemon's
existing gauge counts tracks holding decoders. The peak remains MAN-19's
maximum after each steady-state chunk and just before an EOF or
outage flush. Padding and calibration do not independently update it.
`finish` clears the map, so the pre-flush reading must precede it.

Sampling occurs only when the steady-state chunk count advances and the
interval has elapsed. `on_tracks` calls after padding, calibration and
finish do not sample. `on_sample` stays on the decode thread and retains
its non-`Send` signature; this preserves the harness's `RefCell` and writer
captures and the same synchronous sampling work the copy performed.

## Behavior now inherited from the daemon

- A watchdog stop during calibration processes the samples read so far
  and flushes promptly, instead of filling the entire calibration buffer.
- A source discontinuity closes the old segment, starts a fresh segment
  and retains the validator, rather than splicing across an outage.
- A panic report retains the last published counters, including gate
  records and static crashes. It remains a partial report with `panicked`
  set, and fails the soak acceptance rule.

## Reproducible comparison

The CLI source loops forever and stops on wall time. A fixed seed fixes
its samples, but does not fix the processed prefix. The research phase's
two unchanged runs with seed 12345 produced byte-identical `scene.wav`
files but 15,754 versus 14,617 events, 171 versus 161 emitted closes, and
61 versus 57 gate records. Comparing those totals cannot prove parity.

The permanent harness test uses its own `pileup_signals`, `render_scene`
and `LoopingAudioIqSource`, with a wrapper that returns EOF at a fixed
sample count. It asserts repeatability and nonzero workload within the
same build. Exact counts below are evidence for this build and platform,
not permanent golden expectations: DSP and AWGN use platform math, and
SPEC section 6 requires cross-platform equality at the decoded-text level.
Wall time and RSS are excluded from count equality.

## Measurements

Measured 2026-10-15 in a Linux x86_64 container on an AMD Ryzen 9 5950X
with 32 logical CPUs. The before build used base
`5499511bc122183cbc504a4fcadb6a4741c37fde`. The after build used this
refactor with the same toolchain, flags and machine. No hardware SDR or
Pi4 was available; these runs do not close the live 24h or Pi4 gates.

Both scenes use the harness's `DEFAULT_SEED`. The small scene is 10 s
looped to 30 s of input; the default scene is 120 s looped to 300 s.

| Count | Small before | Small after | Default before | Default after |
|---|---:|---:|---:|---:|
| `events_emitted` | 1366 | 1366 | 19209 | 19209 |
| `spots_emitted` | 4 | 4 | 6 | 6 |
| `peak_active_tracks` | 25 | 25 | 28 | 28 |
| `track_closed_events` | 27 | 27 | 278 | 278 |
| `gate_records_total` | 15 | 15 | 211 | 211 |
| `final_static_crashes` | 0 | 0 | 0 | 0 |
| `panicked` | False | False | False | False |
| closes: `unconfirmed` | 1745 | 1745 | 17055 | 17055 |
| closes: `hang_expired` | 0 | 0 | 18 | 18 |
| closes: `silent` | 0 | 0 | 0 | 0 |
| closes: `merged` | 139 | 139 | 1562 | 1562 |
| closes: `evicted` | 0 | 0 | 0 | 0 |
| closes: `rtty` | 0 | 0 | 0 | 0 |
| closes: `not_cw` | 8 | 8 | 41 | 41 |
| closes: `unkeyed` | 0 | 0 | 0 | 0 |

The complete small-report lines match byte for byte:

Before:
```json
{"events_emitted":1366,"final_close_counts":{"evicted":0,"hang_expired":0,"merged":139,"not_cw":8,"rtty":0,"silent":0,"unconfirmed":1745,"unkeyed":0},"final_static_crashes":0,"gate_records_total":15,"panicked":false,"peak_active_tracks":25,"spots_emitted":4,"track_closed_events":27}
```

After:
```json
{"events_emitted":1366,"final_close_counts":{"evicted":0,"hang_expired":0,"merged":139,"not_cw":8,"rtty":0,"silent":0,"unconfirmed":1745,"unkeyed":0},"final_static_crashes":0,"gate_records_total":15,"panicked":false,"peak_active_tracks":25,"spots_emitted":4,"track_closed_events":27}
```

Commands:

```sh
cargo test -p manta-soak-harness --locked a_bounded_fixed_seed_run -- --nocapture
cargo test --release -p manta-soak-harness --locked bounded_default_scene_timing -- --ignored --nocapture
cargo test --release --locked -p manta-engine --test cpu_budget -- --ignored --nocapture
```

The timing test is an ignored, opt-in measurement tool, like `cpu_budget`.
After building both versions, their release test executables were copied
aside and run alternately, five times each, with no other build or test
started by this session during those pairs. Processing time excludes
scene generation and watchdog join. The count line matched in all ten
runs.

| Processing time, 300 s input | Before | After |
|---|---|---|
| Median (range), five alternating runs | 11.303 s (10.183–12.916 s) | 10.825 s (9.501–11.901 s) |

Individual before seconds: 10.436, 11.303, 12.916, 11.602, 10.183.
Individual after seconds: 10.825, 11.720, 11.901, 10.638, 9.501.

The CPU-budget test reported:

Before:
```text
cpu_budget: 300 tracks sustained across most of the run (scene has 300 signals)
cpu_budget: 49.77s wall / 59.98s steady-state audio (60.00s scene minus 0.0s detector warmup) = 0.830x realtime wall-clock (Mac budget: < 0.5x)
cpu_budget: 49.16s (user+sys) CPU / 59.98s steady-state audio = 0.820x core-seconds (Pi4 budget: < 1.0x; Mac budget: < 0.5x)
```

After:
```text
cpu_budget: 300 tracks sustained across most of the run (scene has 300 signals)
cpu_budget: 53.95s wall / 59.98s steady-state audio (60.00s scene minus 0.0s detector warmup) = 0.899x realtime wall-clock (Mac budget: < 0.5x)
cpu_budget: 55.38s (user+sys) CPU / 59.98s steady-state audio = 0.923x core-seconds (Pi4 budget: < 1.0x; Mac budget: < 0.5x)
```

Both CPU-budget invocations exit 101 on the existing desktop `< 0.5x`
assertion. They sustain 300 tracks. This test calls `decode_samples`,
which the refactor does not change; the streaming timing above measures
the affected path. These shared-host timings are comparisons, not a
hardware acceptance result.

An independent re-check created a detached worktree at the base, copied
only the harness file with its added test module, and ran the bounded
test there. Its report exactly matched the finished branch. The harness
file's production code is byte-identical to the base.

The CLI smoke command below passed before and after, with `overall_pass`
and every `man19_criteria` value true. Each run emitted 10 JSONL samples;
report and sample keys are unchanged. A 10.8 s smoke run deliberately
reports `is_full_24h_run: false` and `synthetic_soak_passed: false`.

```sh
cargo run --locked -p manta-soak-harness --bin man19-soak -- --duration-hours 0.003 --scene-seconds 10 --sample-interval-secs 1
```

## Reuse and rollback

MAN-118 or another exporter can poll `TrackLifecycleObserver::snapshot`
without adding another decoder observer or traversing the track map.
No Prometheus metric is added here. Reverting this change restores the
copy and its previous counters.
