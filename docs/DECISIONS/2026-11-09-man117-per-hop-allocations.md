# MAN-117: per-hop allocations in the channelizer, decoder and pool handoff

The decoder path made about 211 heap allocation events (allocations plus
reallocations) per 512-sample hop at steady state on the 300-signal
`cpu_budget` scene. This change removes the per-hop allocations at the three
sites the ticket names (the channelizer's call to coppa's
`FftProcessor::forward()`, `Demod::push` and `TrackDecoder::push_envelope` /
`push_hop`), plus the two sites on the same paths that cost the most: the
channelizer's scratch buffers and the regrowth of each track's `pending`
queue after `drain_pool` takes it. Decoded output is byte-identical.

Scope (the ticket's 2026-11-06 scope note): measure allocations per hop with
a counting allocator, and CPU time before and after on the same host in one
run. No Pi4, macOS or CPU-budget claim is made, and README.md and ROADMAP.md
are unchanged.

## Measured before

- Host: AMD Ryzen 9 5950X, 32 vCPU, a shared container (1-minute load
  average 11 to 22 during the runs), Linux, glibc 2.39 allocator,
  rustc 1.98.0, release profile.
- Baseline: `7020ede` (main), built in its own worktree and target
  directory, with the probe file copied in. The probe uses only APIs present
  at `7020ede`, so the baseline and branch binaries run the same code.
- Scene: the `cpu_budget` scene, 300 looped 20 WPM signals at 15 dB across
  14.000 MHz +/- 90 kHz, 192 kS/s, seed `0x4350_555F_4250_5431`, 60 s.
- Method: `crates/manta-engine/tests/man117_alloc_per_hop.rs`, an
  `#[ignore]`d probe with a process-global counting `#[global_allocator]`
  (relaxed atomics, so rayon workers are counted). It decodes the first 30 s
  and then all 60 s with `decode_samples`, and divides the difference of the
  two counts by the 11,250 extra hops, which cancels start-up cost. CPU is
  `getrusage(RUSAGE_SELF)` user+sys around the 60 s decode. The digest is
  FNV-1a 64 over `format!("{:?}|{:?}|{}", events, spots, text)`. The
  channelizer line times `Channelizer::process` alone over 10 s after a 2 s
  warm-up, in 4096-sample chunks.
- `cw_only` is the same scene with `detector.digital_window_filter = false`.

Run it with
`cargo test --release --locked -p manta-engine --test man117_alloc_per_hop -- --ignored --nocapture`.

## Design

- **D1: in-place forward FFT with coppa's plan.** The channelizer builds the
  forward plan `FftProcessor::new(N)` builds
  (`rustfft::FftPlanner::new().plan_fft_forward(N)`), once, and runs it with
  `process_with_scratch` on the hop's own output buffer with a scratch it
  owns. It folds into a reused `f64` buffer and rotates while narrowing to
  `f32`, the same values as rotating the `f32` copy. The arithmetic and its
  order are unchanged, so the output is bit-identical to
  `FftProcessor::forward()` (tests
  `process_is_bit_identical_to_the_fft_processor_path` and
  `in_place_forward_matches_fft_processor_forward` in
  `crates/manta-dsp/src/channelizer.rs`). This needs `rustfft` as a direct
  dependency of `manta-dsp`; 6.4.1 was already in `Cargo.lock` through
  coppa-dsp, and the lock gains one line. It is not a coppa change: a
  `forward_with_scratch` upstream would be cross-repo and would move the
  coppa pin that the golden fading vectors depend on. coppa-dsp stays a
  dependency for `carrier.rs`, `freqest.rs` and `condition/iq.rs`.
  `try_forward` was never an allocation-free path: at `f8a4d16d` it calls
  `forward`.
- **D2: append-only `Demod::push_into` and `TrackDecoder::push_hop_into`.**
  Both append to a caller-owned buffer and never read or clear it.
  `push_lead_in_hop_into` does the same. `push`, `push_hop`,
  `push_envelope` and `push_lead_in_hop` keep their signatures as thin
  wrappers. The `Legacy` engine reuses one `Vec<Run>` per decoder.
  `EdgeLegacy` and `Hsmm` only route through `push_hop_into`. The pool
  writes each hop's events straight into its task's output buffer.
- **D3: `pending` recycling with a 64-entry bound.** `drain_pool` drains
  each decoding track's `pending` queue in place instead of taking it, and
  `push_pending` keeps its capacity up to `PENDING_RETAIN_ENTRIES` (64). A
  queue no longer regrows from zero every batch, and a one-off larger
  backlog (a MAN-313 qualification wait, a large live read) is released.
  The retained capacity is at most 64 entries of 40 bytes per decoding
  track, about 0.8 MB at 300 tracks.

## After

Same host, same session, A = baseline `7020ede`, B = this branch, run
alternately in the foreground. Load is the 1-minute load average at the
start of each run. Events are allocations plus reallocations per steady
hop.

| Run | Build | Load | Channelizer events/hop (bytes) | Default events/hop | Default bytes/hop | Default CPU s (ratio) | cw_only events/hop | cw_only bytes/hop | cw_only CPU s (ratio) | Digest default / cw_only |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | A | 22.1 | 6.250 (106,720) | 211.15 | 218,603 | 20.78 (0.346x) | 219.46 | 221,201 | 24.11 (0.402x) | `3e944943d42a0617` / `def6d5ce3582c88e` |
| 2 | B | 15.9 | 2.250 (24,800) | 127.77 | 118,475 | 19.38 (0.323x) | 132.46 | 120,231 | 18.87 (0.314x) | `3e944943d42a0617` / `def6d5ce3582c88e` |
| 3 | A | 16.9 | 6.250 (106,720) | 211.17 | 218,607 | 18.62 (0.310x) | 219.47 | 221,202 | 16.98 (0.283x) | `3e944943d42a0617` / `def6d5ce3582c88e` |
| 4 | B | 15.5 | 2.250 (24,800) | 127.76 | 118,477 | 16.74 (0.279x) | 132.47 | 120,223 | 16.22 (0.270x) | `3e944943d42a0617` / `def6d5ce3582c88e` |
| 5 | A | 11.3 | 6.250 (106,720) | 211.16 | 218,607 | 20.33 (0.339x) | 219.47 | 221,211 | 19.45 (0.324x) | `3e944943d42a0617` / `def6d5ce3582c88e` |
| 6 | B | 12.9 | 2.250 (24,800) | 127.76 | 118,448 | 17.49 (0.291x) | 132.51 | 120,191 | 20.12 (0.335x) | `3e944943d42a0617` / `def6d5ce3582c88e` |

Every run decoded 62,270 events and 287 spots (default) and 65,118 events
and 300 spots (`cw_only`), and A and B digests are equal for both configs.
Runs 1 and 2, and the start of run 3, overlapped this session's own debug
test builds, which the load column includes.

Allocation reductions, B against A:

- Default config: 211.16 to 127.76 events per hop, 60.5% of the baseline
  (39.5% fewer). Bytes per hop 218,606 to 118,467, 54.2% (45.8% fewer).
- `cw_only`: 219.47 to 132.48 events per hop, 60.4% (39.6% fewer). Bytes
  per hop 221,205 to 120,215, 54.3%.
- Channelizer alone: 6.25 to 2.25 events per hop. What remains is each
  hop's own `x` and `power` and the returned `Vec<HopOutput>`. Bytes per hop
  106,720 to 24,800.

CPU: the median paired change (each A run against the B run after it) is
-1.88 s on the default config (-1.40, -1.88 and -2.84 s), and the three A
runs alone spread over 2.16 s (18.62 to 20.78 s), so the change does not
exceed the run-to-run spread on this host. On `cw_only` the paired changes
are -5.24, -0.76 and +0.67 s against a 7.13 s A-run spread. No CPU
improvement is claimed from these numbers.

## Not claimed

No Pi4, aarch64, macOS or CPU-budget claim. README.md and ROADMAP.md are
unchanged. The quiet-host Pi4 measurement stays with the M2 CPU-budget legs
in ROADMAP.md and `docs/RUNBOOKS/m2-pi4-cpu-budget.md`.

## Follow-ups

For a person to file as tickets:

1. The refine `fronts` `BTreeSet` node churn
   (`crates/manta-engine/src/refine.rs`), about 47 events per hop.
   Replacing it needs a keyed structure that keeps MAN-525's eviction order.
2. The beam search's path clones (`crates/manta-decode/src/beam.rs`), about
   36 events per hop, per decoded character.
3. `step_hop`'s detector-side buffers: the floor `median` copy, the two
   `Vec<bool>` from `Gate::update`, `CenterIndex::new`, `power_db_vals` and
   comod, about 19 events but about 57 KB per hop.
4. Pooling `HopOutput`'s `x` and `power`, a public type used in 8 source
   files.
5. A coppa `forward_with_scratch` upstream, so the channelizer can call
   coppa again without the per-call allocations.
6. The `cpu_budget` scene and gate test fix (MAN-524 PR follow-up 1): the
   gate test fails at 286 of 300 tracks before it prints a ratio.
