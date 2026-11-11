# MAN-528: release binaries build with lto = "fat" and codegen-units = 1, chosen by measurement; panic stays unwind

The workspace had no `[profile.release]`, so every shipped binary was built
with Cargo's defaults. This record measures the ticket's variant matrix on
the shipped `manta decode` binary, applies a decision rule fixed in advance,
and pins the winner in the root `Cargo.toml`: `lto = "fat"`,
`codegen-units = 1`, `panic = "unwind"`. On x86_64 the pin makes the binary
about 32% smaller with byte-identical decode output and no measurable speed
change. The Pi 4 and M-series Mac legs are not measured yet; section 9 gives
the commands.

## 1. Context

`Cargo.toml` held `[profile.dev]` and `[profile.dev.package."*"]` only, and
`.cargo/config.toml` holds only `[net]`. With no release profile, rustc got
`-C opt-level=3 -C strip=debuginfo` and nothing else: 16 codegen units, no
cross-crate LTO, `panic = "unwind"` (research, `cargo build --release -v`).
Every release route (`.github/workflows/release-publish.yml`'s `build`,
`build-soapy`, `build-soapy-arm64` and Docker jobs, the `Dockerfile`, and
`.github/workflows/corpus.yml`) runs plain `cargo build --release` or
`cross build --release`, so one root `[profile.release]` reaches all of them
with no workflow edit (decision D1). The required checks build the dev
profile, so the pin does not change their cost.

## 2. Method

**D3. Time the shipped binary on the harness's own scene.**
`scripts/release-profile-ab.py` renders `cpu_budget_scene()` from
`crates/manta-engine/tests/cpu_budget.rs` through its `#[ignore]`d
`write_cpu_budget_scene` test (300 looped 20 WPM signals at 15 dB across
14.000 MHz +/- 90 kHz, 192 kS/s, 60 s, seed `0x4350_555F_4250_5431`), then
times `manta decode <wav> --json` for each variant. Per-hop CPU is
CPU-seconds divided by (audio seconds x 375), with 375 Hz from
`manta_decode::FO_HZ`; 60 s is 22,500 hops. The `cpu_budget` test itself
was not used for two reasons. Its `sustained == 300` precondition fails at
286 of 300 before it prints any ratio (research at `de39a71`, and
`docs/DECISIONS/2026-11-01-man54-on-device-decode-feasibility.md`). And test
targets always unwind, so a test cannot measure `panic = "abort"`.

Scene and matrix commands:

```sh
MANTA_CPU_BUDGET_SCENE_DIR=/tmp/scene cargo test --release --locked -p manta-engine \
  --test cpu_budget write_cpu_budget_scene -- --ignored --exact --nocapture
python3 scripts/release-profile-ab.py --out /tmp/ab --threads 1,all --cpu 9 --reps 7
```

Each variant sets the three keys through `CARGO_PROFILE_RELEASE_*`, builds
from clean into its own `CARGO_TARGET_DIR` with
`cargo build --release --locked -p manta-cli`, and is timed with variants
interleaved in a rotating order. `base` sets Cargo's defaults explicitly, so
it still means "no profile" after this pin.

| variant | lto | codegen-units | panic |
|---|---|---|---|
| base | false | 16 | unwind |
| thin | thin | 16 | unwind |
| fat | fat | 16 | unwind |
| cu1 | false | 1 | unwind |
| thin_cu1 | thin | 1 | unwind |
| fat_cu1 | fat | 1 | unwind |
| abort | false | 16 | abort |
| fat_cu1_abort | fat | 1 | abort |

**D4. The decision rule, fixed in advance and implemented as `choose()`.**
The rule reads the single-thread run (`RAYON_NUM_THREADS=1`, plus `taskset`
on Linux), which matches the Pi 4 budget of one core and was the least noisy
in research. The multi-thread run is reported for information only.

1. Drop any variant whose decode output digest differs from base on any run.
2. Drop any variant with `panic = "abort"` (D2). It is still reported.
3. Let `margin = max(0.03, (base median - base min) / base median)`. A
   variant wins on speed when its median CPU is below
   `base median x (1 - margin)` and its best-of-N CPU is below
   `base min x (1 - margin)`. If any variant wins, pick the lowest median,
   with ties broken by smaller binary and then by variant order.
4. Otherwise, among the remaining variants (base included) whose clean-build
   CPU-seconds stay within +50% of base, pick the smallest binary, with ties
   broken by variant order. The +50% cap allows for noise: fat_cu1 measured
   +23%, and a cap near that figure would let one noisy run flip the
   verdict.

`scripts/tests/test_release_profile_ab.py` pins the rule, including
`test_research_single_thread_numbers_pick_fat_cu1`, which feeds it the
research run's numbers below.

## 3. Host

Two runs on the same kind of container, both at `de39a71` (the commit this
branch starts from), rustc 1.98.0:

- **Research run** (MAN-528 research phase, all eight variants, hand-run
  with the same method before the script existed): AMD Ryzen 9 5950X,
  32 threads, shared with other tenants, load average 11 to 23, 5 GiB memory
  cgroup. 5 reps multi-thread, 6 reps single-thread pinned with
  `taskset -c 9`.
- **Implement run** (this PR, `scripts/release-profile-ab.py`, all eight
  variants): the tool's host block is quoted verbatim in section 4.

## 4. Results

### Research run (de39a71)

Binary size and build cost (one clean build each, sequential; wall time
carries tens of seconds of host noise, CPU seconds are steadier):

| variant | size B | vs base | gzip -9 B | build wall s | build CPU s |
|---|---|---|---|---|---|
| base | 25,486,488 | 0% | 8,596,569 | 158.7 | 307.2 |
| thin | 25,678,744 | +0.8% | 8,706,630 | 173.1 | 335.0 |
| fat | 18,092,160 | -29.0% | 7,069,138 | 219.9 | 304.3 |
| cu1 | 20,543,496 | -19.4% | 7,694,394 | 187.7 | 306.0 |
| thin_cu1 | 20,700,592 | -18.8% | 7,806,378 | 185.5 | 326.2 |
| fat_cu1 | 17,229,944 | -32.4% | 6,693,325 | 264.4 | 379.2 |
| abort | 21,585,336 | -15.3% | 7,331,772 | 154.0 | 296.2 |
| fat_cu1_abort | 14,866,624 | -41.7% | 5,816,119 | 216.9 | 311.4 |

Decode of the 300-signal scene, single thread (`RAYON_NUM_THREADS=1
taskset -c 9`), 6 reps, CPU = user+sys:

| variant | CPU min s | CPU median s | median us/hop |
|---|---|---|---|
| base | 14.27 | 14.72 | 654 |
| thin | 14.68 | 15.13 | 673 |
| fat | 13.93 | 14.59 | 648 |
| cu1 | 14.20 | 15.80 | 702 |
| thin_cu1 | 14.08 | 15.18 | 675 |
| fat_cu1 | 14.28 | 14.92 | 663 |
| abort | 14.05 | 14.75 | 656 |
| fat_cu1_abort | 14.46 | 14.96 | 665 |

Same scene, default threads, 5 reps (informational):

| variant | CPU min s | CPU median s | median us/hop |
|---|---|---|---|
| base | 16.90 | 17.79 | 791 |
| thin | 16.11 | 16.84 | 749 |
| fat | 15.71 | 17.82 | 792 |
| cu1 | 16.01 | 16.63 | 739 |
| thin_cu1 | 15.94 | 17.88 | 795 |
| fat_cu1 | 15.63 | 16.71 | 743 |
| abort | 16.45 | 17.36 | 772 |
| fat_cu1_abort | 16.01 | 17.11 | 760 |

Within one variant, run-to-run spread reached 8% (multi-thread base) to 27%
(single-thread cu1). Every run of every variant produced the same `--json`
SHA-256 (`00ef6b13fd582a48...`, 40 multi-thread and 48 single-thread runs).

Through D4: base median 14.72 s and min 14.27 s give a margin of 3.06%.
No variant has a median below 14.27 s, so none wins on speed. Every variant
fits the +50% build cap (460.8 CPU-s), and the smallest eligible binary is
fat_cu1 (17,229,944 B).

### Implement run (scripts/release-profile-ab.py)

Command (run at `de39a71` plus this branch's uncommitted changes, before the
pin; base's binary is 72 KB larger than research's because the build
identity string differs):

```sh
MANTA_CPU_BUDGET_SCENE_DIR=/tmp/man528/scene cargo test --locked -p manta-engine \
  --test cpu_budget write_cpu_budget_scene -- --ignored --exact --nocapture
python3 scripts/release-profile-ab.py --out /tmp/man528/ab --scene /tmp/man528/scene \
  --threads 1 --cpu 9 --reps 3
```

The scene was rendered once by the same writer under the dev profile; every
variant decodes that one file. The tool's output, verbatim:

```text
host: x86_64, Linux-6.8.0-147-generic-x86_64-with-glibc2.39, AMD Ryzen 9 5950X 16-Core Processor x32, rustc 1.98.0 (88d9e12ae 2026-08-18), commit de39a7150f4f60af9533a9bfb6fcacd7793762a9
load average: start 7.27/8.24/10.99, end 15.58/17.12/15.78
single-thread runs pinned with taskset -c 9
scene: 60.0 s of audio = 22,500 hops

| variant | lto | cgu | panic | size B | gzip -9 B | build wall s | build CPU s |
|---|---|---|---|---|---|---|---|
| base | false | 16 | unwind | 25558840 | 8600503 | 56.4 | 296.7 |
| thin | thin | 16 | unwind | 25775696 | 8720621 | 60.3 | 324.6 |
| fat | fat | 16 | unwind | 18069544 | 7059404 | 207.0 | 320.9 |
| cu1 | false | 1 | unwind | 20543496 | 7688540 | 176.7 | 325.6 |
| thin_cu1 | thin | 1 | unwind | 20700496 | 7803729 | 134.4 | 373.7 |
| fat_cu1 | fat | 1 | unwind | 17229944 | 6692676 | 193.0 | 327.2 |
| abort | false | 16 | abort | 21634576 | 7327724 | 69.8 | 319.0 |
| fat_cu1_abort | fat | 1 | abort | 14866624 | 5815520 | 178.3 | 299.5 |

| variant | mode | runs | CPU min s | CPU median s | spread | median us/hop | output sha256 |
|---|---|---|---|---|---|---|---|
| base | 1 thread | 3 | 16.07 | 18.42 | 65.8% | 818.5 | 00ef6b13fd582a48 |
| thin | 1 thread | 3 | 17.43 | 18.61 | 11.0% | 827.3 | 00ef6b13fd582a48 |
| fat | 1 thread | 3 | 16.96 | 18.02 | 52.9% | 800.8 | 00ef6b13fd582a48 |
| cu1 | 1 thread | 3 | 17.99 | 24.09 | 37.2% | 1070.7 | 00ef6b13fd582a48 |
| thin_cu1 | 1 thread | 3 | 19.11 | 19.13 | 0.8% | 850.4 | 00ef6b13fd582a48 |
| fat_cu1 | 1 thread | 3 | 20.36 | 20.72 | 2.7% | 921.0 | 00ef6b13fd582a48 |
| abort | 1 thread | 3 | 18.49 | 18.73 | 6.9% | 832.4 | 00ef6b13fd582a48 |
| fat_cu1_abort | 1 thread | 3 | 17.69 | 20.22 | 22.6% | 898.7 | 00ef6b13fd582a48 |
```

Read this run for size, build cost and output digests only. Its decode
timings ran while the base and pinned `decode_fingerprints` runs and the
pinned golden-test builds shared the host (load average 15 to 17 at the
end), so base's own spread is 66%. fat_cu1's median is 12.6% above base's,
well inside that spread, and the research run above, on a quieter host, puts
the two within 1.4% on median and 0.1% on best-of-N. Sizes match the
research run to within 0.3% (fat_cu1 and fat_cu1_abort to the byte), and
build CPU puts fat_cu1 at +10% over base here against +23% in research.
Every run of every variant produced the same `--json` SHA-256 prefix,
`00ef6b13fd582a48`, which is also research's digest.

## 5. Verdict

The implement run's lines, verbatim (rule 3's margin is wide because base's
own spread was wide in that loaded run):

```text
ineligible: abort, fat_cu1_abort (panic = "abort", decision D2)
verdict: fat_cu1 (no speed winner: no variant beat base by more than 12.7% on both median and best-of-N; smallest binary within +50% build CPU)
```

The research run through the same rule (section 4, and
`test_research_single_thread_numbers_pick_fat_cu1`):

```text
ineligible: abort, fat_cu1_abort (panic = "abort", decision D2)
verdict: fat_cu1 (no speed winner: no variant beat base by more than 3.1% on both median and best-of-N; smallest binary within +50% build CPU)
```

Both runs pick fat_cu1. The pin is justified by size, and by giving any
Pi 4 gain from cross-crate inlining a chance; it is not justified by a
measured x86_64 speedup, because there is none outside the noise.

## 6. D2: panic stays unwind

`panic = "abort"`'s only measured gain is size: -15% alone, and -42%
instead of -32% on top of fat_cu1. It would change what a panic does in code
that relies on unwinding today:

- `crates/manta-engine/src/multi.rs:396,430` and
  `crates/manta-cli/src/hpsdr_group.rs:188` catch a panicking input so the
  other inputs keep decoding (`ARCHITECTURE.md:1259`: an "input that fails
  or panics ends only its own thread"). Under abort one input's panic ends
  the whole daemon.
- `crates/manta-cli/src/main.rs:2502-2514` (`spawn_tracked_listener`) logs
  "listener task panicked" and clears `listener_up`, which `/healthz` and
  `manta_listener_up` read. Under abort a listener panic ends the process.
- `crates/manta-engine/src/soak_metrics.rs:115`,
  `crates/manta-engine/src/soak.rs:112` and
  `crates/manta-soak-harness/src/main.rs:517,588,634` turn a panic into
  `panicked: true`, a written `report.json` and exit 1. Under abort
  `report.json` is never written and `panicked` could only ever be false.
- `crates/manta-cli/src/reconnect.rs:464` re-raises a reopen-thread panic
  with `resume_unwind`.
- `Drop` cleanup, such as HPSDR stop datagrams and recording finalisation,
  does not run under abort.

Cargo cannot exempt the soak harness: a scratch crate with
`[profile.release.package.p] panic = "unwind"` under a release
`panic = "abort"` fails to parse with
`` `panic` may not be specified in a `package` profile `` (plan session). In
the same scratch crate, `cargo test --release` with `panic = "abort"` built
the `CARGO_BIN_EXE_*` binary with abort (a panic ended it with status 134)
while in-process tests still unwound, which is why only the shipped binary
can measure abort.

The double-SIGINT path (`crates/manta-cli/src/main.rs:1871-1895`) uses only
atomics and `signal_hook::low_level::exit` (`_exit`), so it does not depend
on the panic strategy. The pin states `panic = "unwind"` explicitly so a
later edit has to remove it on purpose.

## 7. Build cost

- **Clean build** (`cargo build --release --locked -p manta-cli`): research
  run base 158.7 s wall and 307.2 CPU-s against fat_cu1 264.4 s and
  379.2 CPU-s (+23% CPU); the implement run's figures are in section 4.
- **Edit-rebuild** (after `touch crates/manta-cli/src/main.rs`, plan
  session at `d0f42f1`): base 81.6 s wall, 152.3 CPU-s, peak child RSS
  894 MiB; fat_cu1 157.6 s wall, 182.5 CPU-s, 1,453 MiB. This PR's pinned
  build, after the same touch and under concurrent load: 151.1 s wall,
  129.2 CPU-s.
- **`cargo test --release`** (`cargo test --release --locked -p manta-cli
  --test decode_fingerprints --no-run` from clean, plan session): base
  177.4 s wall and 342.3 CPU-s against fat_cu1 231.2 s and 343.6 CPU-s.
- **Peak memory**: the final fat-LTO link peaks at about 1.5 GB (1,453 MiB
  on x86_64) against 0.9 GB (894 MiB). This matters for native builds on 1
  and 2 GB boards (`docs/GUIDES/raspberry-pi.md` gives the override) and for
  the 32-bit `linux/arm/v7` image build under QEMU.
- **Release CI baselines** (`gh run view 37330523145 --json jobs`, the
  2026-10-05 dispatch, read in the plan session): `docker-build-dispatch`
  17.6 min (15:15:33 to 15:33:07); `build (aarch64-apple-darwin, macos-14)`
  2.6 min; cross aarch64 Linux 2.8 min; Windows 4.1 min; macOS x86_64
  5.9 min. With +23% CPU and a serial LTO step, native jobs grow by up to
  about 2 minutes each, at tag time only. `release-publish.yml` sets no
  `timeout-minutes`, so the 6-hour default applies.
- The QEMU image build and the macOS, Windows and soapy builds under the pin
  are measured by the owner's `publish=false` dispatch before approval:
  `gh workflow run release-publish.yml --repo HagaleTechnologies/manta --ref MAN-528 -f publish=false`.
  Add each job's duration beside the baseline above.

## 8. Determinism on the pinned build

All on the pinned profile (no `CARGO_PROFILE_RELEASE_*` overrides in the
environment), at `de39a71` plus this branch, in
`CARGO_TARGET_DIR=/tmp/man528/fp/pinned`. The pinned `manta` binary was
built with `-C opt-level=3 -C lto=fat -C codegen-units=1 -C strip=debuginfo`
and no `-C panic` flag (`cargo build --release --locked -p manta-cli -v`
after `touch crates/manta-cli/src/main.rs`), and is 17,229,944 bytes, the
same as the A/B tool's fat_cu1.

**Decode fingerprints, Cargo defaults against the pin.** The base set was
captured before pinning, with the defaults forced through the environment:

```sh
CARGO_TARGET_DIR=/tmp/man528/fp/base \
CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 CARGO_PROFILE_RELEASE_PANIC=unwind \
MANTA_FINGERPRINT_JOBS=4 cargo test --release --locked -p manta-cli --test decode_fingerprints -- --ignored --nocapture \
  | grep ^FINGERPRINT > base.txt
CARGO_TARGET_DIR=/tmp/man528/fp/pinned MANTA_FINGERPRINT_JOBS=4 \
  cargo test --release --locked -p manta-cli --test decode_fingerprints -- --ignored --nocapture \
  | grep ^FINGERPRINT > pinned.txt
diff base.txt pinned.txt && echo IDENTICAL
```

Result: `IDENTICAL`, 46 lines in each file (23 vectors x 2 engines), both
runs `test result: ok. 1 passed`. The SHA-256 of each line set starts
`bd7336e2f8852a6f`, the same value the plan session got at `d0f42f1`. The
first line is
`FINGERPRINT v1 legacy 0 80518 5fe7c66d25ee8252401fa3016232d5e638d8a574c5909c2e4241ffd9430e614d`.

**Golden vectors and shutdown codes on the pinned build:**

```sh
cargo test --release --locked -p manta-cli -j 4 \
  --test golden_v1 --test golden_v2_v3 --test golden_v7_v9_v10 --test golden_v8_v8w \
  --test golden_vr --test golden_vw --test golden_close_pairs \
  --test golden_decimated_capture --test golden_v34_fade_reacquire \
  --test signal_shutdown -- --test-threads=4
```

| test binary | passed | failed | ignored |
|---|---|---|---|
| golden_close_pairs | 10 | 0 | 0 |
| golden_decimated_capture | 2 | 0 | 0 |
| golden_v1 | 1 | 0 | 1 |
| golden_v2_v3 | 8 | 0 | 4 |
| golden_v34_fade_reacquire | 2 | 0 | 0 |
| golden_v7_v9_v10 | 7 | 0 | 1 |
| golden_v8_v8w | 6 | 0 | 2 |
| golden_vr | 7 | 0 | 4 |
| golden_vw | 5 | 0 | 0 |
| signal_shutdown | 17 | 0 | 0 |

The ignored tests are the known-failing stage-2 and WPM gates their
`#[ignore]` reasons name. The fingerprint diff covers their inputs byte for
byte. `signal_shutdown` passing shows the 143 and 130 exit codes hold under
the pin. `--test-threads=4` bounds memory and does not affect output.

**Engine determinism tests on the pinned build:**

```sh
cargo test --release --locked -p manta-engine -j 4 \
  --test chunking_determinism --test channelizer_chunking_determinism \
  --test replay_live_parity --test man398_grid -- --test-threads=4
```

| test binary | passed | failed | ignored |
|---|---|---|---|
| channelizer_chunking_determinism | 1 | 0 | 0 |
| chunking_determinism | 1 | 0 | 0 |
| man398_grid | 1 | 0 | 1 |
| replay_live_parity | 2 | 0 | 0 |

`man398_grid`'s ignored test is its hand-run cross-build grid; the
fingerprint diff above is the cross-build comparison here.

**The pinned binary on the MAN-49 scene.** `manta decode
/tmp/man528/scene/cpu_budget.wav --json` with the pinned binary exited 0
with SHA-256 prefix `00ef6b13fd582a48`, the base digest. The scene writer
also passes on the pinned profile:
`MANTA_CPU_BUDGET_SCENE_DIR=<dir> cargo test --release --locked -p manta-engine --test cpu_budget write_cpu_budget_scene -- --ignored --exact --nocapture`
wrote a 60.0 s, 192,000 Hz `cpu_budget.wav` and a `cpu_budget.json` with
`"center_freq_hz": 14000000.0`.

**Across all eight variants.** Every timed and warm-up decode in both A/B
runs (section 4) produced the same `--json` SHA-256 prefix on the MAN-49
scene, `00ef6b13fd582a48`, including both abort variants.

This is x86_64 evidence only. aarch64 and macOS may differ in codegen, FMA
contraction and libm, so the Pi 4 and Mac legs in section 9 should repeat
the fingerprint diff.

## 9. Not measured

| host or build | status | command |
|---|---|---|
| Raspberry Pi 4 (Cortex-A72) | not measured; no Pi 4 is reachable from the container | `python3 scripts/release-profile-ab.py --out /tmp/ab --threads 1,all --cpu 3 --reps 7` |
| M-series Mac | not measured; no Mac is reachable from the container | `python3 scripts/release-profile-ab.py --out /tmp/ab --threads 1,all --reps 7` (no `--cpu`: macOS has no `taskset`) |
| QEMU `linux/arm64` and `linux/arm/v7` image builds | not measured; `docker-build-dispatch` needs the owner's `ghcr-test-publish` approval | `gh workflow run release-publish.yml --repo HagaleTechnologies/manta --ref MAN-528 -f publish=false` |
| Windows and macOS release build times | not measured | the same `publish=false` dispatch |

The Pi 4 runbook (`docs/RUNBOOKS/m2-pi4-cpu-budget.md`, "Release-profile
A/B (MAN-528)") carries the hardware commands. A Pi 4 has a smaller cache
and a different core than the Zen 3 host, so its verdict may differ. A later
run whose verdict differs from this pin goes in a new decision record that
supersedes this one and re-pins `Cargo.toml` in the same PR (D6).

## 10. Local iteration

`CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16`
restores Cargo's defaults for one command, for example
`CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 cargo build --release -p manta-cli`.
The binary is larger and decodes byte-identically (section 8). The
`cpu_budget` harness and the determinism tests run under
`cargo test --release`, which now builds the pinned profile; the override
speeds those builds up too, at the cost of not measuring the shipped
codegen.

## 11. Not varied

- `strip`: a full strip took the default binary from 25,486,488 to
  19,711,128 bytes in research, but drops the symbols backtraces use.
- `opt-level`, `target-cpu` and PGO.
