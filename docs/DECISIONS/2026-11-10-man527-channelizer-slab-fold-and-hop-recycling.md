# MAN-527: slab-by-slab channelizer fold and recycled hop buffers

`Channelizer::process` now folds each hop slab by slab: the prototype's 8
slabs of N taps in the outer loop, the N channels in the inner loop, over
contiguous slices, with the SPEC §1.3 step-2 rotation applied as the fold's
write index. There is no per-sample `% N` and no rotation copy. A new
`Channelizer::process_into` recycles the previous call's hop buffers, and
the offline and live decode paths use it, so a steady-state hop allocates
nothing. Every hop's `x` and `power` are bit-identical to `de39a71`'s.

The ticket's "five allocations per hop" described the code before MAN-117
(#559). At `de39a71` the two fold buffers, the rotation copy and coppa's FFT
output were already gone (MAN-117 reused `fold` and ran rustfft in place).
The plan phase measured 2.125 allocations plus 0.125 reallocations per hop
at 192 kS/s at `de39a71`: each hop's `x` and `power`, and the returned
`Vec`. What was still true was the scalar fold with a per-sample modulo, the
per-element modulo in the rotation, and those per-hop allocations.

## Measured before

- Host: AMD Ryzen 9 5950X, 32 vCPU, a shared container (1-minute load
  average 18.8 to 20.5 during the A/B runs), Linux, glibc allocator,
  rustc 1.98.0, release profile, no `target-cpu` flag.
- Baseline: `de39a71`, built in its own worktree and target directory with
  the probe file copied in. The probe uses only APIs present at `de39a71`,
  so the baseline and branch binaries run the same code.
- Method: `crates/manta-dsp/tests/man527_channelizer_cost.rs` (an
  `#[ignore]`d probe: a hop digest over 2,001 hops of noise with a
  signed-zero tail, at 4,096- and 1,000-sample chunks, and the minimum and
  median time per hop over nine repetitions of 3,200 hops at 192 kS/s and
  6,400 hops at 96 kS/s), and
  `crates/manta-testkit/tests/man527_channel_power_digests.rs` (an FNV-1a
  digest of every channel power and every complex output over V1 to V10 and
  V8w). Allocations are a CI test: `crates/manta-dsp/tests/channelizer_alloc.rs`.

## Design

**D1, the slab fold.** `fold_rotated` loops slab p outer over
`window.chunks_exact(n).zip(taps_rev.chunks_exact(n))` and channel j inner
through `accumulate`, which does `acc[j] += h[j] * w[j]` in f64 over three
equal-length slices. Each choice keeps bit identity:

- The taps are stored reversed (`taps_rev[i] = h[LN-1-i]`), so slab p of the
  window meets `taps_rev[p*N..(p+1)*N]` in order. They stay f32: an f32
  widens exactly to f64, so the products are the same as before.
- Every `fold[j]` starts at +0.0 (`fill`) and adds its products in ascending
  p, which is the order the per-sample loop used. Assigning the first slab
  instead of adding it to +0.0 would differ for a -0.0 product, so the fill
  stays.
- Multiply and add are separate operations. Rust does not contract them into
  an FMA, and `mul_add` would change the bits (SPEC §6 rule 4).
- The rotation is the write index: slab positions `r..N` go to
  `fold[..N-r]` and positions `0..r` to `fold[N-r..]`. `r = (m * hop) mod N`
  is computed once per hop and is always below N, so the splits never panic.
  The f32 narrowing is then a straight map.

**D2, why output identity does not prove the summation order.** In the plan
phase, reversing the slab order (p descending) still passed
`process_is_bit_identical_to_the_fft_processor_path`: the f64 sums differ in
their last bits, but narrowing to f32 absorbed that on all 400 hops at both
rates. A test that compares the f64 fold itself against the old loop failed
under the same mutation, with 10,207 of 16,384 values different at 96 kS/s.
That test is now in the tree:
`fold_rotated_matches_the_spec_sum_bit_for_bit_in_f64` (all four rotation
values at both rates, plus all-(-0.0) and signed-zero-tail windows). It is
the guard for the ticket's "summation order per channel unchanged (p
ascending)".

**D3, `process_into` and the spare pool.** `process_into(iq, out)` first
moves `out`'s hops into a channelizer-owned `spare` pool, then builds each
new hop from `spare.pop()` (or allocates one with capacity N when the pool
is empty), clearing and refilling `x` and `power`. On return `out` holds
exactly this call's hops. After the hop loop the pool is truncated to
`SPARE_RETAIN_HOPS = 32` surplus hops: four times the largest steady batch on
the decode paths (8 hops per offline 4,096-sample chunk at 192 kS/s; a live
2,048-sample read is 4, and the lead-in padding call of `filter_len` zeros is
1), so a call somewhat larger than the last one still allocates nothing. The
pool holds at most 768 KB at N = 2048. The bound applies to surplus only: a call that
needs 72 hops recycles 72. `process` is a wrapper that passes a fresh `Vec`,
so its callers (checks, diagnostics, tests, testkit) see the same hops and
the same allocation profile as before.

**D4, engine wiring.** `decode_source_observed` keeps one hop buffer for
the whole decode and passes it to `process_into`; a new segment's
channelizer recycles the previous segment's hops, which is safe because
`process_into` overwrites them. `TrackManager::process_hops` only borrows the
batch and `step_hop` copies what it keeps (`prev_hop.clone_from`). In
`listen.rs`, `Segment::channelize` is replaced by `Segment::track(iq,
seg_base)`, which channelizes into the segment's own `hops` buffer, feeds
the spectrum tap, and runs `process_hops`. Its seven callers each made
exactly that pair of calls.

## After

Cost probe, interleaved A (baseline) and B (branch), minimum µs per hop over
nine repetitions:

| Round | Load | A 192 kS/s | B 192 kS/s | A 96 kS/s | B 96 kS/s |
|---|---|---|---|---|---|
| 1 | 20.5 / 19.8 | 42.37 | 12.82 | 20.31 | 6.45 |
| 2 | 19.8 | 40.36 | 13.52 | 21.10 | 6.66 |
| 3 | 18.8 | 45.86 | 13.54 | 21.00 | 6.63 |

The branch takes 30% to 34% of the baseline's channelizer time per hop at
192 kS/s and 31% to 32% at 96 kS/s on this host. The whole-pipeline CPU
change is not claimed: the channelizer was a few percent of the pipeline in
MAN-117's 300-signal measurement, and no whole-pipeline run was made here.

Hop digests, identical on A and B in every round, at both chunk sizes:
`fs=192000 hops=2001 fnv=8da5f3742498d91f` and
`fs=96000 hops=2001 fnv=e5dfbb8a44607197`.

Channel-power digests on the branch (4,096-sample chunks, 96 kS/s), equal
line for line to the plan phase's baseline run at `de39a71`. `manta-dsp` and
`manta-testkit` did not change between that run and this branch's base.

```
v1   hops=44969 power_fnv=1be3af34bfa49edf x_fnv=ad5306d69d350e3b
v2   hops=33719 power_fnv=7663083dfed02612 x_fnv=5424b59586982963
v3   hops=44969 power_fnv=e2d6fa315d721506 x_fnv=785c218d30112c3e
v4   hops=44969 power_fnv=1dbd914362428657 x_fnv=28314f1ccff79e07
v5   hops=44969 power_fnv=43afcc496bed5ec6 x_fnv=33e73f982d57c849
v6   hops=44969 power_fnv=564171bf0ba98a7f x_fnv=ba99fc76c6177238
v7   hops=44969 power_fnv=0a26383a3bd12e88 x_fnv=ba0d9ce8bcb63846
v8   hops=44969 power_fnv=453d32e3c4f1315d x_fnv=b508ff7839410726
v8w  hops=44969 power_fnv=691f9b8d53624ab2 x_fnv=c8828ddf8121ccbc
v9   hops=44969 power_fnv=fa976cfc1aa08fc3 x_fnv=cc5b5db3b4c6c69e
v10  hops=44969 power_fnv=8495229661d68086 x_fnv=013c4b0e42cb298e
```

Allocations: `process_into_allocates_nothing_after_warm_up` asserts 0
allocation events over at least 1,000 hops after a 2 s warm-up, at 192 and
96 kS/s, for 4,096-, 2,048- and 1,000-sample chunks. The existing
`a_steady_state_process_call_allocates_only_its_outputs` bound (`2H + 2`
events per `process` call) still holds.

End to end, the determinism suites (`chunking_determinism`,
`channelizer_chunking_determinism`, `pipeline`, `replay_live_parity`) and
the listen suites (`listen_spots`, `listen_inputs`, `listen_audio`) pass on
the branch. In the plan phase, a prototype of this exact change produced
`manta decode --json` output byte-identical to `de39a71` for V1 to V10 and
V8w on both engines (22 fingerprints). The `decode_fingerprints` and
`man117_alloc_per_hop` probes were not re-run against a same-session
baseline for this record.

## Not claimed

No Pi4, aarch64 (NEON vectorisation was not checked), macOS or CPU-budget
claim. README.md and ROADMAP.md are unchanged. No coppa pin bump and no
`Cargo.toml` or `Cargo.lock` change: MAN-117 already uses a local rustfft
planner.

## Follow-ups

For a person to file as tickets:

1. The detector-side per-hop vectors, after PR #537 (MAN-526) merges, since
   it rewrites the same functions: `Gate::update`/`hold` `rise`/`drop`
   (`crates/manta-dsp/src/floor.rs:457-458` and `:496-497`),
   `power_db_vals` (`crates/manta-engine/src/track.rs:3233`), `ids`
   (`track.rs:3342`), the three `center_index()` builds per hop
   (`track.rs:3344`, `:3977`, `:4435`) and the floor `median` copy
   (`floor.rs:127-131`), line numbers as of 2026-11-10.
2. Check on a Pi4 or another aarch64 host that the slab loop vectorises
   (NEON) and measure it, as part of the M2 Pi4 CPU leg
   (`docs/RUNBOOKS/m2-pi4-cpu-budget.md`).
3. `Channelizer::with_conditioning` accepts `fs = 93.75` and `187.5`, where
   `hop = N/4 = 0` and `process` would never advance `read`. Found by
   reading, not run.
