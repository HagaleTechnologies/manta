# MAN-54: on-device decode on a Red Pitaya is a no-go; the CPU, not the link, is the limit

Date: 2026-11-01. Ticket: MAN-54.

**Status:** accepted (investigation only, no implementation in this record or its PR).

**Verdict: NO-GO**

This no-go covers running manta's decode pipeline (manta-dsp, manta-decode,
manta-engine and manta-spot) on the Zynq-7000 ARM cores of any Red Pitaya or
QMTech Zynq board. Two Cortex-A9 cores at 667 MHz cannot carry manta's front
end: one 192 kS/s DDC is predicted to need 0.9 to 3.4 of the board's 2.0
cores, and 8 DDCs need at least 4.7 cores even in the most favourable case.
The problem it would solve does not bind on this board either, because every
Red Pitaya has Gigabit Ethernet and its gateware stops at 8 DDCs (78.2 Mbps).
The record does not cover a channelizer in the Zynq's programmable logic, or
any other SoC. The Hermes-Lite 2 has no onboard general-purpose compute and is
out of scope.

## Scope and method: no hardware

The 2026-10-31 scope note limits MAN-54 to datasheet and cross-compile
findings; running on a board is out of scope. What ran, all in a cloud
container on 2026-11-01 at commit `4581725`:

- Primary vendor sources read by web fetch: Red Pitaya's hardware pages,
  Pavel Demin's project notes and server source, the AMD Zynq-7000 overview
  (DS190) and Alpine's armv7 package indexes. See "Sources".
- manta's per-DDC CPU cost measured natively with a scratch harness (source in
  the appendix; it was deleted from the tree after the run).
- The full `manta` binary cross-compiled for `armv7-unknown-linux-musleabihf`,
  inspected with `readelf` and `llvm-objdump`, and run with four unit-test
  suites under `qemu-arm-static -cpu cortex-a9` against the Alpine 3.22 armv7
  userland.

What did not run, and why it matters:

- Nothing ran on a Cortex-A9. Emulator wall time is not A9 speed, and this
  record draws no speed conclusion from it. The A9 cost below is a scaled
  estimate with stated assumptions and a sensitivity table, not a measurement.
- No Red Pitaya or QMTech board was reachable.
- The native cost was measured on a shared 32-thread host, an AMD Ryzen 9
  5950X (load average 21 to 31 during the runs), pinned to one core with
  `taskset`. SMT contention may inflate the native numbers; the optimistic
  corner below accounts for that.

The plan phase re-ran the two `cargo check` cross-compile rows and the
`cpu_budget` gate on `4581725`, with the same results as the research phase.
The full sysroot link, the `qemu-arm` runs and the native cost matrix were not
re-run; their numbers are the research phase's, on the same commit.

## Decision

Do not start any work to run manta's decode pipeline on a Red Pitaya's ARM
cores, and do not let MAN-13's design assume it. The reasons, in order of
weight:

1. The CPU budget misses by a wide margin under every assumption tried,
   including the strictest lower bound. No scaling factor in the plausible
   range makes 8 DDCs fit, and one 192 kS/s DDC alone uses most of the SoC at
   the central estimate (see "Scaling to the Zynq").
2. The problem on-device decode would solve does not exist on this hardware.
   The Red Pitaya has a Gigabit port and a gateware cap of 8 DDCs (see "The
   bandwidth premise").
3. The subset in the ticket's wording does not exist. `manta-input` and
   `manta-spot` are the cheap ends of the pipeline. The cost sits in
   `manta-dsp` (channelizer, detector) and `manta-decode` (per-track
   decoders), which cannot be left behind if the goal is to ship spots instead
   of IQ.

**Effect on MAN-13.** The ticket's invariant says no on-device implementation
work starts without these findings. These are the findings. MAN-13
([2026-10-15-man13-multi-input-daemon.md](2026-10-15-man13-multi-input-daemon.md))
proceeds as multi-source orchestration on a central host and records on-device
decode as evaluated and rejected, with "What would change the answer" below as
the reopen condition.

**What to do instead for high DDC counts.** Both need no ARM compute and are
already in the repo or the FPGA:

- Use the Pitaya's own Gigabit port. 8 DDCs at 192 kS/s is 78.2 Mbps on the
  wire by `on_wire_mbps` (`crates/manta-input/src/hpsdr.rs:164-170`), and a
  second board has its own port.
- Lower the DDC rate in the FPGA with the HPSDR rate command (48, 96 or
  192 kS/s; Pavel's server maps `frame[1] & 3` to 1000, 500 or 250
  decimation in `sdr-receiver-hpsdr.c`). At 96 kS/s, 8 DDCs is 39.1 Mbps.
  `validate_ddc_config` (`crates/manta-input/src/hpsdr.rs:180`) already
  allows more DDCs at lower rates
  (`validate_ddc_config_allows_more_ddcs_at_lower_rates`,
  `crates/manta-input/src/hpsdr.rs:1674`).

## Compute budget

All values are from Red Pitaya's own documentation pages, fetched 2026-11-01,
unless noted.

| Board | SoC | CPU | RAM | Ethernet |
|---|---|---|---|---|
| STEMlab 125-10 | Zynq 7010 | dual Cortex-A9 | 256 MB | 1 Gbit |
| STEMlab 125-14 (and LN, ext-clock variants) | Zynq 7010 | dual Cortex-A9 @ 667 MHz | 512 MB (DDR3) | 1 Gbit |
| STEMlab 125-14 Z7020 LN | Zynq 7020 | dual Cortex-A9 | 512 MB | 1 Gbit |
| STEMlab 125-14 4-Input | Zynq 7020 | dual Cortex-A9 | 512 MB | 1 Gbit |
| SDRlab 122-16 | Zynq 7020 | dual Cortex-A9 @ 667 MHz | 512 MB | 1 Gbit |
| SIGNALlab 250-12 | Zynq 7020 | dual Cortex-A9 | 1 GB | 1 Gbit |

Sources for the table:

- 125-14 row (667 MHz, 512 MB DDR3, 1 Gbit):
  <https://redpitaya.readthedocs.io/en/latest/developerGuide/hardware/ORIG_GEN/125-14/top.html>
- 122-16 row (667 MHz, 512 MB, 1 Gbit):
  <https://redpitaya.readthedocs.io/en/latest/developerGuide/hardware/ORIG_GEN/122-16/top.html>
- All six rows (SoC, CPU, RAM, Ethernet):
  <https://redpitaya.readthedocs.io/en/latest/developerGuide/hardware/compares/vs.html>

The 667 MHz figure is stated on the 125-14 and 122-16 pages ("Dual-core ARM
Cortex-A9 @ 667 MHz"). The comparison page gives no CPU clock; the "125 MHz" it
lists is the FPGA core clock. The 122-16 is the model the market review names
for the RBN operator.

**Core details** come from the AMD/Xilinx Zynq-7000 overview, DS190 (text
extracted from a PDF mirror, because the AMD page itself has no table text):
dual-core ARM Cortex-A9 MPCore, "NEON & Single / Double Precision Floating
Point for each processor", "2.5 DMIPS/MHz per CPU", 32 KB I and 32 KB D L1
per processor, 512 KB shared L2, 256 KB on-chip memory, and DDR3, DDR3L,
DDR2 or LPDDR2 external memory. The 667 MHz (-1 grade) and 766 MHz (-2 grade)
figures appear in the maximum-frequency row, but the extraction scrambled its
column alignment, so this record relies on Red Pitaya's own 667 MHz statement
for these boards.

**DMIPS estimate.** One core is about 2.5 x 667 = 1,668 DMIPS, so two cores
are about 3,340 DMIPS. This is derived from a vendor figure, not measured.
Measured Dhrystone results on A9 parts are often lower (an Arm forum thread
reports about 1.4 DMIPS/MHz on a 1.2 GHz part), so treat it as an upper
estimate.

**Operating system.** Pavel Demin's HPSDR images use Alpine Linux 3.22 for
armv7 (`red-pitaya-alpine-3.22-armv7-20251012.zip`). Alpine is musl-based. The
image is installed by copying it to a micro SD card; packages install with
`apk` and changes persist with `lbu commit -d`. Red Pitaya's own stock OS is
not named in its spec table and was not researched, because the HPSDR
receiver image replaces it. RAM is not the constraint: `manta decode` peaks at
about 20 MB (see "Measured cost").

**QMTech XC7Z020 board** (the third device in MAN-10). Same Zynq-7020 family,
so the same dual Cortex-A9 core. Its speed grade and DRAM size are not stated
on Pavel's QMTech notes page, and no QMTech datasheet was found. A -1 part is
rated 667 MHz and a -2 part 766 MHz, a 15% spread that does not change the
verdict.

## What the ARM does today

The 8-DDC `sdr_receiver_hpsdr` image does all channelization in the FPGA. The
ARM program is a small UDP server, read in full at
`projects/sdr_receiver_hpsdr/server/sdr-receiver-hpsdr.c` (331 lines,
pavel-demin/red-pitaya-notes `master`):

- `main` maps the FPGA's config, status and data windows from `/dev/mem`
  (physical 0x40000000, 0x41000000, 0x42000000) and answers Metis discovery
  and control on UDP 1024.
- `handler_ep6` runs in one thread. It polls the FIFO fill counter with
  `usleep(1000)`, `memcpy`s the DDC samples out of the data window, builds
  1032-byte Metis packets, and sends up to 12 at a time with `sendmmsg`. It
  does no arithmetic on the samples.
- The receiver count is capped at 8 (`receivers = ((frame[4] >> 3) & 7) + 1`),
  and the rate bits select 48, 96 or 192 kS/s.
- The server is built on the board with
  `gcc -O3 -march=armv7-a -mtune=cortex-a9 -mfpu=neon -mfloat-abi=hard`
  (`projects/sdr_receiver_hpsdr/server/Makefile`) and started by `start.sh`,
  which loads the bitstream through `/dev/xdevcfg` and runs
  `sdr-receiver-hpsdr 1 1 1 1 1 1 1 1 &`.

That Makefile is the best evidence for the right compiler target: ARMv7-A,
Cortex-A9, NEON, hard-float. Its work (a memcpy and a UDP send) leaves the ARM
mostly idle today. That is inferred from reading the code; its load was not
measured.

## Which part of manta would run there

The ticket names `manta-input` and `manta-spot`. They are not where the cost
is.

| Crate | Role | Cost driver | On-device value |
|---|---|---|---|
| `manta-input` | IQ ingest: HPSDR UDP client, file, Kiwi, audio, ka9q. `IqSource` trait at `crates/manta-input/src/lib.rs:133` | per sample, but a decode and copy (24-bit to f32) | An on-device build could read the local server over loopback UDP with the existing HPSDR client, so no new source is needed to try it. |
| `manta-dsp` | polyphase channelizer, FFT (rustfft through coppa-dsp), detector | per input sample; scales with sample rate | the main cost (see "Measured cost") |
| `manta-decode` | per-track envelope, keying, character decode | per track at about 375 Hz | grows with signals |
| `manta-engine` | pipeline, decoder pool (rayon), track manager | glue | needed |
| `manta-spot` | callsign validation, cty/SCP, message scan | per decoded word | the cheap end: its 299 unit tests finish in 1.9 s even under emulation |

A subset that leaves out the channelizer cannot turn IQ into spots. Shipping
spots instead of IQ means running the whole chain on the ARM, and the cost
sits in manta-dsp, not in manta-input or manta-spot.

## Measured cost

**Method.** The scratch harness (appendix) renders N keyed CW signals at 15 dB
SNR into a 20 s complex scene and calls `manta_engine::decode_samples`
(`crates/manta-engine/src/lib.rs:327`), the same entry point the repo's gate
uses (`crates/manta-engine/tests/cpu_budget.rs`). It reads `getrusage`
user plus system CPU time around the call and divides by the audio duration
minus the 2 s detector warmup, the repo's own convention. It ran with
`RAYON_NUM_THREADS=1 taskset -c 5`, so each figure is single-core work, twice.
Host: AMD Ryzen 9 5950X (16 cores, 32 threads, 5085 MHz max), shared, load
average 21 to 31, commit `4581725`, 2026-11-01.

```
fs=192000 signals=  1  cpu/audio=0.131x / 0.137x    (two runs)
fs=192000 signals= 10  cpu/audio=0.157x / 0.143x
fs=192000 signals= 30  cpu/audio=0.154x / 0.169x
fs=192000 signals=100  cpu/audio=0.240x / 0.266x
fs=192000 signals=300  cpu/audio=0.844x / 0.835x
fs= 96000 signals=  1  cpu/audio=0.064x / 0.066x
fs= 96000 signals= 30  cpu/audio=0.079x / 0.085x
fs= 48000 signals=  1  cpu/audio=0.030x / 0.028x
fs= 48000 signals= 10  cpu/audio=0.038x / 0.033x
```

Peak RSS across the whole matrix was 51 to 65 MB, which includes the 31 MB
rendered input buffer, so the pipeline itself is tens of MB.

**Cross-check on the real binary.** `manta decode` on the 60 s, 192 kS/s `v1`
recording used 5.35 CPU-seconds with a 20.5 MB peak RSS: 0.089 core-seconds
per audio-second, multi-threaded, on the same loaded host. That is the low end
of the table above, so 0.089 is the "cheapest floor" in the optimistic corner.

What the numbers say:

- The cost is a front-end floor that scales linearly with sample rate (0.13,
  0.065 and 0.03 at 192, 96 and 48 kS/s) and barely moves until about 100
  signals. It is the channelizer and detector, not the decoders. The marginal
  cost is about 0.0013 core-s/s per signal from 30 to 100 signals and 0.0029
  from 100 to 300.
- The budget table in `ARCHITECTURE.md:336-346` (under 100 MFLOP/s, "≪ 1
  core") does not predict this. On a core far faster than a Pi 4 core, the
  single-signal floor is already 0.09 to 0.13 of a core. Where the time goes
  was not profiled. This record notes the disagreement and does not edit
  `ARCHITECTURE.md`.

**The repo's `cpu_budget` gate fails its precondition on `4581725`.**
`cargo test --release --locked -p manta-engine --test cpu_budget -- --ignored --nocapture`
fails at the `assert_eq!(sustained, 300, ...)` in
`crates/manta-engine/tests/cpu_budget.rs:255` with "only 286 of 300 scene
signals produced a track" (3 of 3 runs in the research phase, 1 more in the
plan phase). The scene is deterministic and the tree was clean. The assert runs
before the ratio lines print, so that gate prints no CPU ratio on this commit.
It was not bisected. It does not affect this verdict, because the harness
measures the same call without that assertion.

## Scaling to the Zynq

There is no A9 measurement, so the A9 cost is the native number times a factor
`k` (A9 core time per host core time). None of the three values is a
measurement:

- **k = 6.6, a lower bound from the clock ratio alone.** The host ran at about
  4,420 MHz (87% of 5,085 MHz, per `lscpu`) against 667 MHz. IPC and SIMD only
  add to this, since the A9 is a 2-issue core and the host's rustfft path uses
  runtime-detected AVX while the A9 gets none (see "Cross-compile findings").
  This is the strictest defensible bound and is optimistic for the A9.
- **k = 15, a central assumption.** The clock ratio times an IPC and SIMD gap
  of about 2.3. This is a judgement, not a sourced figure.
- **k = 25, a pessimistic assumption.**

A9 cores needed per second of audio, against 2.0 available, with the board
also running Linux and the HPSDR server thread:

| Configuration | Native (5950X) | k = 6.6 (bound) | k = 15 (assumed) | k = 25 (assumed) |
|---|---|---|---|---|
| 1 DDC at 192 kS/s, 1 signal | 0.13 | 0.9 | 2.0 | 3.4 |
| 1 DDC at 192 kS/s, 30 signals | 0.16 | 1.1 | 2.4 | 4.0 |
| 4 DDC at 192 kS/s, 30 signals | 0.65 | 4.3 | 9.7 | 16.2 |
| 8 DDC at 192 kS/s, 30 signals | 1.29 | 8.5 | 19.4 | 32.3 |
| 8 DDC at 96 kS/s, 30 signals | 0.66 | 4.3 | 9.8 | 16.4 |
| 8 DDC at 48 kS/s, 10 signals | 0.28 | 1.9 | 4.3 | 7.1 |
| 2 DDC at 48 kS/s, 10 signals | 0.07 | 0.5 | 1.1 | 1.8 |
| 1 DDC at 48 kS/s, 10 signals | 0.04 | 0.2 | 0.5 | 0.9 |

**Optimistic corner for 8 DDCs** (cheapest measured floor 0.089, k = 6.6):
4.7 cores at 192 kS/s, 2.4 at 96 kS/s and 1.2 at 48 kS/s. Only 8 DDCs at
48 kS/s fits, and only under the strictest bound, with nothing left for Linux.
Eight DDCs at 48 kS/s each cover 48 kHz, so that fit would not skim a band like
20 m (14.000 to 14.070 MHz) at full width. The 300-signal gate scene (0.84
native) would need 5.5 to 21 cores.

**Cross-check with no x86 involved.** The repo's Pi 4 target is "one 192 kS/s
passband in one Raspberry Pi 4 core"
([multiple-receivers.md](../GUIDES/multiple-receivers.md), line 310, stated as
a target, not measured), and MAN-18
([2026-09-02-man18-pi4-cpu-budget-gate.md](2026-09-02-man18-pi4-cpu-budget-gate.md))
estimates that the Pi 4 misses its 300-track budget by about 5.3 times (an
estimate, not a measurement). A Pi 4 core is a 1.5 GHz Cortex-A72. By DMIPS
(A9: 2.5 per MHz x 667 MHz; A72: 4.72 per MHz x 1,500 MHz, a figure sourced
only second-hand, with 7.3 also quoted), the two A9 cores together equal about
0.47 of a Pi 4 core, or 0.30 with the 7.3 figure. Both are estimates from
vendor and second-hand figures. On either, the Zynq cannot reach the repo's
own target of one core per 192 kS/s passband.

**A prediction a hardware run could falsify** (see "Needs real hardware"):
`manta decode` of the 60 s `v1` recording would need about 35 to 134
CPU-seconds on the A9, or 0.6 to 2.2 core-seconds per audio-second. This is
the 0.089 floor scaled by k = 6.6 to 25, not a measurement.

## Cross-compile findings

**Target choice.** Alpine armv7 is musl and hard-float, so the matching Rust
target is `armv7-unknown-linux-musleabihf`, not the glibc target. The repo's
release pipeline does not build it: it ships `x86_64` and `aarch64` gnu
tarballs (through `Cross.toml`) and a Docker image for `linux/arm/v7`
(`.github/workflows/release-publish.yml:431,523`), which is Debian-based glibc
and does not run on Alpine. The README already says that image's "decode CPU
budget is unmeasured" (`README.md:307-309`). Rust's stock target does not
enable NEON; the build used `-C target-cpu=cortex-a9 -C target-feature=+neon,+vfp3`.

**Toolchain.** Rust 1.98.0 (the pin) with the target added to a private
`RUSTUP_HOME`, because the container's rustup directory is read-only. The
linker and C compiler was `zig cc -target arm-linux-musleabihf` (ziglang
0.17.0 from pip), because `ring` (through ureq) needs a C compiler for the
target. A wrapper dropped the `--target=armv7-...` flag that `cc-rs` passes,
since zig cannot parse "armv7". The sysroot was the Alpine 3.22 armv7 packages
`alsa-lib`, `alsa-lib-dev`, `musl`, `musl-dev` and `libgcc`, unpacked into a
directory and handed to `pkg-config`.

**What built and ran.**

| Step | Result |
|---|---|
| `cargo check --target armv7-unknown-linux-musleabihf -p manta-decode -p manta-dsp -p manta-spot` | passes, 22 s (19 s in the plan phase's re-run) |
| `cargo check ... -p manta-engine` with no sysroot | fails: `alsa-sys` build script panics with "pkg-config has not been configured to support cross-compilation" (reproduced in the plan phase, exit 101) |
| `cargo build --release --locked --target armv7-unknown-linux-musleabihf -p manta-cli` (with the Alpine sysroot) | passes, 2m06s; `manta` is 12,766,816 bytes unstripped (the native x86-64 release binary is 21,858,712) |
| `readelf` on the binary | ELF32, ARM, EABI5, hard-float; `Tag_CPU_name: "cortex-a9"`, `Tag_CPU_arch: v7`, `Tag_FP_arch: VFPv3`, `Tag_Advanced_SIMD_arch: NEONv1`, Thumb-2; interpreter `/lib/ld-musl-armhf.so.1`; NEEDED `libasound.so.2` |
| `manta --version` and `--help` under `qemu-arm-static -cpu cortex-a9 -L <alpine sysroot>` | `manta 0.1.0 (git 4581725f426d; features: none)`, full command list |
| `manta gen v1`, `v2`, `vr1` on ARM under qemu versus native x86-64 | all 9 output files (WAV, JSON, manifest) byte-identical per `cmp` |
| `manta decode v1.wav` on ARM under qemu versus native | outputs byte-identical (195 bytes, same copy, "frequency: 14012.3 kHz speed: 20 WPM spots: 1") |
| `cargo test --release --target ...` for `manta-spot --lib` | 299 passed, 0 failed |
| same for `manta-dsp --lib` | 275 passed, 0 failed |
| same for `manta-decode --lib` | 154 passed, 0 failed, 1 ignored |
| same for `manta-input --features hpsdr --lib hpsdr` | 161 passed, 0 failed, 3 ignored (includes the loopback-device and `validate_ddc_config_*` tests) |

The determinism constraint in `CLAUDE.md` (file input gives byte-identical
spot logs) held across x86-64 and 32-bit ARM for the cases run: the `gen` and
`decode` outputs above were byte-identical. That is the one place an ARM build
could have surprised, since pointer-width and floating-point differences would
show as a diff.

**Findings for anyone who tries again.**

1. **`cpal` is an unconditional dependency of `manta-input`**
   (`crates/manta-input/Cargo.toml:17`: `cpal = { workspace = true }`, not
   optional). Any build of `manta-engine` or `manta-cli` therefore needs ALSA
   headers and a target `libasound` at build and run time, even though a board
   with no sound card never opens it. `Cross.toml` and
   [raspberry-pi.md](../GUIDES/raspberry-pi.md) ("ALSA headers are required
   even for a network-only receiver") already record this. Alpine has
   `alsa-lib`, so it is solvable without a code change. A feature gate would be
   a code change and is out of scope here.
2. **No native build on the board.** Alpine 3.22 armv7 packages Rust 1.87.0
   (`rust` and `cargo` in `main`), below manta's 1.98.0 pin, and rustup offers
   no musl-armv7 host toolchain. The board would need a cross-built binary.
3. **No NEON from the compiler.** `llvm-objdump` on the binary (2,349,186
   lines) shows 0 packed single-precision NEON arithmetic instructions on Q
   registers, against 21,471 scalar VFP f32 arithmetic instructions. LLVM does
   not auto-vectorize f32 math to NEON on ARMv7, because NEON is not fully
   IEEE-754 compliant. The DSP therefore runs as scalar VFPv3 on the A9. (A
   first attempt with the host `objdump`, which cannot disassemble ARM and
   printed nothing, was discarded.)
4. **rustfft has no 32-bit ARM SIMD.** `rustfft` 6.4.1 compiles its NEON
   module only under `all(target_arch = "aarch64", feature = "neon")`
   (`src/lib.rs:441-447` in the registry copy), and uses AVX or SSE only on
   x86_64. coppa-dsp's FFT, the channelizer's core, takes the scalar path on a
   Zynq. It takes SIMD paths on the x86 host and on a 64-bit Pi, which is
   another reason `k` is not smaller than its clock-ratio bound.
5. **The `libc.so` quirk comes from zig, not manta.** The zig-linked binary
   lists `libc.so` as NEEDED, which Alpine would not resolve by that name. A
   real musl cross toolchain would not produce it. It is noted here so nobody
   reads it as a manta defect.

## The bandwidth premise

MAN-10
([2026-09-02-hpsdr-hermes-protocol-spike.md](2026-09-02-hpsdr-hermes-protocol-spike.md),
"Bandwidth math") framed the constraint as about 8 simultaneous 192 kHz DDCs
per 100 Mbps link; in code it is `LINK_BUDGET_MBPS = 78.4`
(`crates/manta-input/src/hpsdr.rs:164`). Against the primary sources:

- **Red Pitaya Ethernet is 1 Gbit** on every model (see "Compute budget"). The
  100 Mbps figure describes the Hermes-Lite 2 and the operator's LAN, not the
  Pitaya's port. The spike says so in passing ("or one unit on a faster
  link"), but it also describes a second Pitaya as "a second 100 Mbps link".
- **The 8-DDC cap is gateware.** `sdr_receiver_hpsdr` has eight identical
  DDCs; its project page says it "emulates a Hermes module with eight
  receivers", and the server clamps the count at 8. A single board therefore
  cannot exceed 8 DDCs whatever the link does, and 8 DDCs at 192 kS/s is
  78.2 Mbps on the wire (`on_wire_mbps`, 6% overhead), which fits even a
  100 Mbps link.
- **Where the ceiling does bite** is the HL2 skimmer gateware (9 to 10 DDCs,
  88 to 98 Mbps), which has no onboard compute, and two or more Pitayas behind
  a single 100 Mbps uplink. On-device decode on a Pitaya helps neither.
- **The real scaling limit for high DDC counts is host CPU**, not the link.
  [multiple-receivers.md](../GUIDES/multiple-receivers.md) already says a
  192 kS/s passband is about one Pi 4 core, and MAN-307
  ([2026-10-19-man307-hpsdr-multiple-receivers.md](2026-10-19-man307-hpsdr-multiple-receivers.md))
  runs one decode front end per receiver. On-device decode would move that
  load onto a core about a quarter as fast.

**Notes for MAN-10's next editor** (not changed here):

- The MAN-10 record cites `https://pavel-demin.github.io/red-pitaya-notes/sdr-receiver/`
  for `sdr_receiver_hpsdr`. That URL is a different project (the TCP "SDR
  SMEM" receiver, tunable to 490 MHz). The HPSDR project page is
  `https://pavel-demin.github.io/red-pitaya-notes/sdr-receiver-hpsdr/`, which
  states a 0 to 61.44 MHz range and 48, 96 and 192 kSPS.
- The MAN-10 device table lists `sdr_receiver_hpsdr` as "0-490 MHz" (its line
  70), which matches the other project, not this one.

These page readings came from a fetch tool's page summaries, so check the pages
directly before editing MAN-10.

## What would change the answer

Reopen only if all three hold:

1. A real Cortex-A9 measurement exists (the first external step below), not an
   estimate.
2. The front-end cost per 192 kS/s stream falls about fivefold from today's
   0.134 core-s/s on the reference host. For example, 8 DDCs at 96 kS/s in 1.5
   usable A9 cores at k = 15 needs 0.0125 native core-s/s per 96 kS/s DDC, or
   0.025 per 192 kS/s equivalent, against 0.134 now.
3. The deployment needs fewer, narrower streams than the 8-DDC case, since the
   only configurations in the table that come close to fitting are 1 or 2 DDCs
   at 48 kS/s.

A move to a faster SoC removes the premise instead of reopening this, since a
Pi-class host on the same switch is already the supported shape.

## Needs real hardware (external, not acceptance)

These are steps only Tony can do. They are not part of MAN-54's acceptance:

1. Copy the armv7 binary and Alpine's `libasound.so.2` to a Red Pitaya running
   the Alpine image, and time `manta decode` on the 60 s `v1` recording. The
   prediction above is 35 to 134 CPU-seconds.
2. Measure the existing `sdr-receiver-hpsdr` thread's CPU load at 8 DDCs and
   192 kS/s with `top`. This record infers it is low from reading the code
   only.
3. Confirm the QMTech board's speed grade and DRAM size.

## Sources

- Red Pitaya STEMlab 125-14: <https://redpitaya.readthedocs.io/en/latest/developerGuide/hardware/ORIG_GEN/125-14/top.html>
- Red Pitaya SDRlab 122-16: <https://redpitaya.readthedocs.io/en/latest/developerGuide/hardware/ORIG_GEN/122-16/top.html>
- Red Pitaya board comparison: <https://redpitaya.readthedocs.io/en/latest/developerGuide/hardware/compares/vs.html>
- AMD Zynq-7000 overview, DS190 (PDF mirror): <https://ranger.uta.edu/~jlosh/Xilinx/Zynq-7000_SoC_Overview.pdf>
- Pavel Demin, sdr-receiver-hpsdr: <https://pavel-demin.github.io/red-pitaya-notes/sdr-receiver-hpsdr/>
- Pavel Demin, Alpine image: <https://pavel-demin.github.io/red-pitaya-notes/alpine/>
- Pavel Demin, server source and Makefile: `projects/sdr_receiver_hpsdr/server/` in <https://github.com/pavel-demin/red-pitaya-notes>
- Pavel Demin, QMTech notes: <https://pavel-demin.github.io/qmtech-xc7z020-notes/>
- Alpine 3.22 armv7 package indexes: <https://dl-cdn.alpinelinux.org/alpine/v3.22/main/armv7/> and <https://dl-cdn.alpinelinux.org/alpine/v3.22/community/armv7/>
- Thoughts pool (not in this repo, cited by name): the MAN-54 research
  `2026-11-01-MAN-54-can-manta-decode-on-a-red-pitayas-onboard-arm-cores-instead.md`,
  the MAN-13 research
  `2026-10-15-MAN-13-one-manta-daemon-combining-multiple-sdrs-or-multiple.md`,
  and the market review `2026-09-05-manta-review-lens-4-market.md`.

## Non-outcomes

- No implementation: no code, no `cpal` feature gate, no armv7 release target,
  no change to `Cross.toml` or the release workflow.
- Nothing was run on a Zynq or any Red Pitaya.
- No edit to the MAN-10 record; its two inaccuracies are listed above for its
  next editor.
- No edit to `ARCHITECTURE.md:336-346` or `README.md:307-309`; the
  disagreement with the measurement is noted above.
- No fix or bisect of the `cpu_budget` gate failure; it is recorded above as
  an observed fact.

## Reproduction

```sh
# private toolchain (the container's rustup dir is read-only)
export RUSTUP_HOME=/tmp/man54-rustup
rustup toolchain install 1.98.0 --profile minimal \
  -t armv7-unknown-linux-musleabihf -t aarch64-unknown-linux-gnu
# Alpine 3.22 armv7 sysroot: alsa-lib, alsa-lib-dev, musl, musl-dev, libgcc (dl-cdn.alpinelinux.org/alpine/v3.22/main/armv7)
# linker/C compiler: pip install --target /tmp/man54-zig ziglang   (wrapper script: zig cc -target arm-linux-musleabihf, drop --target=*)
export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER=/tmp/man54-bin/zigcc
export CC_armv7_unknown_linux_musleabihf=/tmp/man54-bin/zigcc AR_armv7_unknown_linux_musleabihf=/tmp/man54-bin/zigar
export PKG_CONFIG_ALLOW_CROSS=1 PKG_CONFIG_SYSROOT_DIR=<sysroot> PKG_CONFIG_PATH=<sysroot>/usr/lib/pkgconfig
export RUSTFLAGS="-C target-feature=-crt-static -C link-self-contained=no -C target-cpu=cortex-a9 -C target-feature=+neon,+vfp3"
cargo build --release --locked --target armv7-unknown-linux-musleabihf -p manta-cli
# run: qemu-arm-static -cpu cortex-a9 -L <sysroot> target/armv7-unknown-linux-musleabihf/release/manta decode v1.wav
# qemu-arm-static 7.2: extracted from Debian's qemu-user-static_7.2+dfsg-7+deb12u18+b3_amd64.deb with dpkg-deb -x
```

The native cost matrix is the appendix harness run as
`RAYON_NUM_THREADS=1 taskset -c <core> <test-binary> --ignored --nocapture`,
built with `cargo test --release --locked -p manta-engine --test man54_scratch`
after saving the harness as `crates/manta-engine/tests/man54_scratch.rs`.

## Appendix: scratch measurement harness

Saved from `crates/manta-engine/tests/man54_scratch.rs`, which was deleted
after the run; this appendix is the only in-repo copy. It needs `libc`,
`num-complex` and `manta-testkit`, all already dependencies or
dev-dependencies of `manta-engine`.

```rust
use manta_engine::PipelineConfig;
use manta_testkit::scene::{render_scene, SignalSpec};

fn cpu_seconds() -> f64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) }, 0);
    let s = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 / 1e6;
    s(usage.ru_utime) + s(usage.ru_stime)
}

fn scene(fs: f64, n: usize, dur: f64) -> Vec<num_complex::Complex32> {
    let span = 0.9 * fs; // spread over 90% of the passband
    let signals: Vec<SignalSpec> = (0..n).map(|i| {
        let offset_hz = if n == 1 { fs * 0.1 } else { -span / 2.0 + i as f64 * (span / (n - 1) as f64) };
        SignalSpec {
            dit_units: 1.0, element_gap_units: 1.0, text: "CQ CQ DE K1BNC K1BNC K".into(),
            loop_text: true, wpm: 20.0, offset_hz, snr_2500_db: 15.0, jitter: None, qsb: None,
            watterson: None, char_wpm: None, weight: 3.0, char_gap_units: 3.0, word_gap_units: 7.0,
            rise_ms: 5.0,
        }
    }).collect();
    render_scene(&signals, fs, dur, Some(0x4D41_4E35_3400_0001)).unwrap().0
}

#[test]
#[ignore]
fn man54_matrix() {
    let cfg = PipelineConfig::default();
    let (dur, warm) = (20.0, 2.0);
    for &(fs, n) in &[(192_000.0, 1), (192_000.0, 10), (192_000.0, 30), (192_000.0, 100),
                      (192_000.0, 300), (96_000.0, 1), (96_000.0, 30), (48_000.0, 1), (48_000.0, 10)] {
        let iq = scene(fs, n, dur);
        let c0 = cpu_seconds();
        let r = manta_engine::decode_samples(&iq, fs, 14_000_000.0, &cfg);
        let cpu = cpu_seconds() - c0;
        println!("MAN54 fs={fs:>6.0} signals={n:>3} cpu/audio={:.3}x ok={}", cpu / (dur - warm), r.is_ok());
    }
}
```

`decode_samples` returns an error ("no signal found") for input with no keyed
signal, so the floor case uses one signal at +0.1 x fs, not silence. A signal
at exactly 0 Hz is also rejected.
