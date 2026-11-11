# MAN-521: one test binary per golden pileup scene, each rendered once

`crates/manta-cli/tests/golden_v8_v8w.rs` was one test binary that took 626 to
696 s in CI. `scripts/ci-shards.py` pinned it alone to shard 0, and it was the
longest Ubuntu shard in every recent run, so sharding could not shorten the
Unix leg below it. MAN-521 splits it into one binary per scene, renders each
scene once per binary, and lets `assign()` place the binaries by weight like
any other.

The ticket's second acceptance line, "the longest shard finishes at least 4
minutes sooner than today", is **not met**, and no change confined to these
tests can meet it while CI uses eight shards. The floor table below shows why.

## What changed

- Three binaries replace `golden_v8_v8w`:
  - `golden_v8` (AWGN pileup): `v8_pileup_validates_at_least_45_of_50_with_no_bogus_calls`,
    `v8_pileup_validates_at_least_45_of_50_with_hsmm_engine`,
    `v8_with_prior_on_stays_at_zero_bogus`,
    `v8_with_repetition_fusion_on_stays_at_zero_bogus`.
  - `golden_v8w` (Watterson pileup): `v8w_pileup_fading_spots_no_bogus_callsigns`,
    `v8w_with_prior_on_stays_at_zero_bogus`,
    `v8w_with_repetition_fusion_on_stays_at_zero_bogus`, and the two ignored
    gates with their attributes unchanged:
    `v8w_pileup_fading_decodes_90pct_of_strong_signals_no_ghosts` (`#[ignore]`)
    and `v8w_pileup_fading_decodes_90pct_of_strong_signals_with_hsmm_engine`
    (`#[ignore = "stage-2 gate, un-ignored by Task 12 only if measured passing"]`).
  - `golden_vc3` (the V8w scene plus the pinned crash train):
    `vc3_pileup_fading_with_crashes_spots_no_bogus_callsigns`,
    `vc3_with_prior_on_stays_at_zero_bogus`,
    `vc3_with_repetition_fusion_on_stays_at_zero_bogus`.
- Name map. `v8_and_v8w_with_prior_on_stay_at_zero_bogus` became
  `v8_`, `v8w_` and `vc3_with_prior_on_stays_at_zero_bogus`.
  `v8_v8w_and_vc3_with_repetition_fusion_on_stay_at_zero_bogus` became `v8_`,
  `v8w_` and `vc3_with_repetition_fusion_on_stays_at_zero_bogus`. The other six
  names are unchanged.
- VC3 got its own binary because it renders the full V8w scene plus crashes and
  costs as much as V8w. Leaving it with V8w would keep one binary near 440 s on
  a 2-CPU container.
- Shared code is in `crates/manta-cli/tests/common/pileup.rs`. `per_track` comes
  from `common/decode.rs`; the old file's own copies of `decode_report` and
  `per_track` are gone.
- `common::pileup::fixture(scene)` renders the scene the first time a test in
  the binary asks for it (one `OnceLock` per scene) and hands every later test
  the same WAV and manifest. The directory is a `tempfile::TempDir` inside
  `CARGO_TARGET_TMPDIR` named `man521-golden-<scene>-*`, unique to the process,
  so concurrent runs and Windows are safe.
- The fixture never reuses a WAV from an earlier run. A stale WAV after a
  generator or coppa change would let a golden gate pass on old bytes. Statics
  are never dropped, so each binary leaves its directory behind; before
  rendering, the fixture removes `man521-golden-*` directories older than 24
  hours. No golden binary runs near that long, so a concurrent run's directory
  is never touched. `cargo clean` removes the rest.
- `prior.toml` and `fusion.toml` go into each test's own `tempfile::tempdir()`.
- `scripts/ci-shards.py` reserves no shard: `assign()` picks the least-loaded of
  all shards for every binary.
- `scripts/ci-test-weights.json` drops `golden_v8_v8w` (626) and adds
  `golden_v8w` 190, `golden_vc3` 173 and `golden_v8` 114. These are the
  research prototype's local times (229.35 s, 208.78 s, 137.75 s) scaled by the
  old binary's Ubuntu/local ratio 650/785.6, matching the file's Ubuntu basis.
  With these weights `assign()` puts the three binaries on three different
  shards; `scripts/tests/test_ci_shards.py` asserts that, and that every weight
  key names a real workspace target.

## The cost was the render, not the decode

The MAN-101 record attributed V8w's cost to the decode. It is the scene
synthesis: the Watterson route in `crates/manta-testkit/src/scene.rs` runs
coppa's Watterson channel (three 11.52 M-point FFTs per signal) and a 129-tap
Hilbert transform for each of 50 signals, one after another. Measured on a
2-CPU container: V8 render 11.3 s, V8w 170.9 to 184.3 s, VC3 176.8 s, each
legacy decode about 15 s. The old binary rendered a scene 10 times (once per
test, three times in each of the two loop tests). It now renders each scene
once per binary, which removes about 755 of the roughly 1,600 CPU-seconds.

## Gating is unchanged

Every threshold, assertion and message text is the old one, and the `MAN-317`
and `MAN-200` stderr lines print the same values. One difference: a V8 failure
in the prior or fusion check no longer stops the V8w and VC3 checks, because
each scene is now its own test. That is the MAN-101 rule
(`wiki/pages/golden-vector-freeze.md`) applied one level up; every check that
ran before still runs and fails with the same text.

## Local results

Measured in the implement session on a shared 6-CPU-quota container. To fit
the session, the old binary, the three new binaries and three mutation builds
ran at the same time, with load average 20 to 41. The wall times below are
therefore inflated and only show that everything finished. The research
phase's isolated 2-CPU timings are the fair comparison: old binary 785.61 s;
prototype split 229.35 s (V8w), 208.78 s (VC3), 137.75 s (V8).

| Binary | Result | Wall time, concurrent run (s) |
|---|---|---|
| `golden_v8_v8w` (before) | 6 passed; 0 failed; 2 ignored | 1194 |
| `golden_v8` | 4 passed; 0 failed; 0 ignored | 425 |
| `golden_v8w` | 3 passed; 0 failed; 2 ignored | 877 |
| `golden_vc3` | 3 passed; 0 failed; 0 ignored | 872 |

The sorted `MAN-317` and `MAN-200` stderr lines of the old binary and of the
three new ones are identical: 18 lines each (9 `MAN-317`, 6 `MAN-200 ...
repetition_fusion=`, 3 `MAN-200 <scene>: validated off=.. on=..`). Validated
counts: V8 48 in every mode; V8w 25 with the prior at weights 0, 1 and 2, 28
with fusion off and 30 with it on; VC3 24 with the prior, 27 with fusion off
and 29 with it on. Bogus is 0 everywhere.

Mutation check (temporary edits, not committed):

- `MIN_V8W_VALIDATED` set to 51. `golden_v8w`: 0 passed, 3 failed, with
  `V8w: only 28/50 genuine callsigns validated (floor 51); 0 bogus is vacuous
  if recall has collapsed`, `V8w prior 0: 25 < 51` and
  `V8w fusion false: 28 < 51`. `golden_vc3`: 0 passed, 3 failed, with
  `VC3: only 27/50 genuine callsigns validated (floor 51); ...`,
  `VC3 prior 0: 24 < 51` and `VC3 fusion false: 27 < 51`.
- In the same build, the V8 floor literal 45 in
  `v8_pileup_validates_at_least_45_of_50_with_no_bogus_calls` set to 51.
  `golden_v8`: 3 passed, 1 failed; only that test failed, with
  `V8 must validate >= 45/50 callsigns, got 48/50 (spotted: ...)`. The message
  text is the old one, which names 45.

After these runs `target/tmp` held one `man521-golden-<scene>-*` directory per
binary run (six), and the system temp directory nothing from these tests.

## The 4-minute criterion

**Method.** For each of five recent CI runs and each OS, sum the per-binary
`seconds` in the `shard-<os>-<n>` evidence. "Longest" is that run's longest
shard. "Target" is longest minus 240 s. Eight shards cannot finish faster than
total work divided by 8, or than the biggest single binary, whichever is
larger: that is the floor, the best any static packing could do. "Floor after
MAN-521" replaces `golden_v8_v8w` by 0.733 of its time (the prototype's
575.9 s of 785.6 s). "Floor, golden deleted" removes it entirely.

| OS | Run | Longest (s) | Target (s) | Total work (s) | Floor after MAN-521 (s) | Floor, golden deleted (s) |
|---|---|---|---|---|---|---|
| ubuntu | 42754116234 | 652 | 412 | 3614 | 430 | 370 |
| ubuntu | 42756123638 | 655 | 415 | 3270 | 387 | 327 |
| ubuntu | 42763780021 | 652 | 412 | 3702 | 441 | 381 |
| ubuntu | 42765682586 | 571 | 331 | 3390 | 406 | 357 |
| ubuntu | 42767566704 | 650 | 410 | 3503 | 416 | 357 |
| macos | 42754116234 | 579 | 339 | 3329 | 398 | 397 |
| macos | 42756123638 | 627 | 387 | 3528 | 442 | 442 |
| macos | 42763780021 | 557 | 317 | 3155 | 376 | 325 |
| macos | 42765682586 | 493 | 253 | 3289 | 397 | 357 |
| macos | 42767566704 | 601 | 361 | 3204 | 384 | 341 |

Even with perfect packing, the target is reachable in 1 of 5 Ubuntu runs and 0
of 5 macOS runs. With the golden tests deleted outright, macOS still reaches it
in only 1 of 5, because `manta_engine`'s unit-test binary alone takes 237 to
442 s there.

**Expected saving.** Each run re-packed with the pin removed, the golden binary
replaced by three binaries at the prototype's share of that run's golden time,
and static weights. Savings are that run's longest shard minus the simulated
longest, in seconds.

| Variant | Ubuntu, 5 runs | Ubuntu mean | macOS, 5 runs | macOS mean |
|---|---|---|---|---|
| Split + render once, weights 190/173/114 (shipped) | 138, 191, 118, 64, 141 | 130 s (2.2 min) | 29, 30, 127, 14, 136 | 67 s (1.1 min) |
| Split + render once, weights 170/155/100 | 78, 110, 90, 72, 115 | 93 s (1.5 min) | 97, 111, 130, 37, 130 | 101 s (1.7 min) |
| Split + render once, each run's own times as weights (best static packing) | | 157 s | | 122 s |
| Also a 2-worker parallel Watterson render (not shipped) | | 154 s | | 85 s |

The two weight sets are within run-to-run noise of each other. Per-binary times
vary by up to 160 s between runs (`manta_engine` on Ubuntu: 187 to 347 s), so a
realistic packing trails the best static one.

## Follow-ups that could reach 4 minutes

Each is its own ticket:

- Split work out of `manta_engine`'s lib unit-test binary. It sets the macOS
  floor (397 to 442 s in two of five runs).
- Raise the shard count (this ticket fixed it at eight).
- A byte-identical parallel `manta_testkit::scene::render_scene`. Summing the
  per-signal contributions in signal order gives the same bytes: on a 2-CPU
  container the full V8w scene took 170.86 s sequentially and 103.94 s on 2
  workers, digest `0xfbbb57f77251c55f` both ways, peak RSS 1.68 GB. It needs a
  process-wide limit on concurrent signal renders, because several tests in one
  binary can render at once and each worker peaks near 0.75 GB. The simulation
  gives only about 24 s more on Ubuntu and 18 s on macOS for these binaries, but
  it also speeds the other binaries that render V8w (`rtty`, `rtty_detector`,
  `non_cw`, `minimizer`, `observations` in `manta-engine`). It changes a shared
  generator under the golden-vector freeze, so it was left out here.

## How to measure the hosted result

Download the `shard-<os>-<n>` artifacts of a run (`gh run download <id> -p
'shard-*'`), sum `results[].seconds` per shard, and compare each OS's longest
shard with the five runs above. The PR's description carries its own run's
numbers; a committed file cannot quote the CI run of the commit that holds it.
Once a few post-merge runs exist, the three weights can be replaced by measured
means. The simulation shows the placement is insensitive to that within noise.

## How to run the gates now

Name the scene's binary where old commands named `golden_v8_v8w`:

```sh
cargo test -p manta-cli --locked --test golden_v8
cargo test -p manta-cli --locked --test golden_v8w
cargo test -p manta-cli --locked --test golden_vc3
# The ignored CER gates (legacy and hsmm):
cargo test -p manta-cli --locked --test golden_v8w -- --ignored
```
