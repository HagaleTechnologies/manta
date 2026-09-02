# M2 manual acceptance: Raspberry Pi 4 CPU-budget gate

Not part of CI (same reasoning as `benches/cpu_budget.rs`'s module doc:
GitHub-hosted runners aren't Pi4 hardware, and perf assertions on shared CI
are flaky). Run this yourself against real Pi4 hardware before flipping
ROADMAP.md's M2 Pi4 leg from "outstanding" to measured, and before deciding
MAN-30's fate.

## What you need

- A physical Raspberry Pi 4 (any RAM size; this is a CPU, not a memory,
  gate), running Raspberry Pi OS **Bookworm** (2023-10+) or another
  glibc >= 2.35 aarch64 Linux — same baseline `pancetta`'s aarch64 target
  already assumes (see `pancetta/README.md`).
- Nothing else competing for the Pi4's cores during the run (no desktop
  environment doing real work, no other soak/benchmark process).
- A Rust toolchain on the Pi4 itself (native build — cross-compiling and
  copying a binary over works too, but native `cargo test` is simpler and
  removes a class of "did the cross build actually match" doubt).

## Steps

1. `git clone` (or `git pull`) this repo on the Pi4, or `scp`/`rsync` a
   built binary over if you cross-compiled instead — see "Cross-compiling"
   below if the Pi4 itself is too slow to build from source in reasonable
   time (it isn't, in practice; a from-scratch `manta-engine` release build
   takes a few minutes on Pi4, not hours).
2. Record throttling/clock state *before* the first run, so a throttled
   or overclocked Pi doesn't get silently misread as a CPU-budget result:
   ```
   vcgencmd get_throttled
   vcgencmd measure_clock arm
   ```
   `get_throttled`'s bitmask: any of bits 0-3 set means it's throttled or
   under-voltage *right now*; bits 16-19 mean it happened at some point
   since boot. `0x0` is the clean baseline you want before you start.
   Record the raw hex value, and whether the board is running stock or
   overclocked config (`/boot/firmware/config.txt`'s `arm_freq`/`over_voltage`,
   if set).
3. `cargo test --release -p manta-engine --test cpu_budget -- --ignored --nocapture`
   **Must be `--release`.** Plain `cargo test` measures dev-profile speed
   (~1.45x slower per this workspace's `opt-level = 1` first-party dev
   profile — see `docs/DECISIONS/2026-07-24-m2-pileup-cpu-budget-pins.md`
   item 5) and will produce a misleadingly pessimistic number.
4. Read the printed output:
   ```
   cpu_budget: 300 unique tracks decoded (scene has 300 signals)
   cpu_budget: <N>s wall / 15.00s audio = <ratio>x realtime wall-clock (Mac budget: < 0.5x)
   cpu_budget: <N>s (user+sys) CPU / 15.00s audio = <ratio>x core-seconds (Pi4 budget: < 1.0x; Mac budget: < 0.5x)
   ```
   - **The track count line must read close to 300** (the test itself
     asserts >= 285, but read the real number). A materially lower count
     means the detector promoted fewer simultaneous tracks than the
     scene intends, and the run below it is benchmarking a cheaper
     workload than the ROADMAP.md criterion actually specifies — don't
     trust the timing numbers if this failed.
   - **The `(user+sys) CPU / audio` line — not the wall-clock line — is
     the number to compare against the Pi4 budget (< 1.0x).** ROADMAP.md's
     criterion is a CPU-time budget ("< 1 full core"), and wall-clock only
     equals CPU time when the pipeline is single-core-bound. That's true
     on Mac today (confirmed by direct measurement — see the pins doc and
     the 2026-09-02 decision doc) but has not been separately confirmed on
     Pi4's 4 weaker, differently-scheduled cores; the test now measures
     both so you don't have to assume they still match on Pi4. If the two
     ratios diverge noticeably (CPU-time ratio much higher than
     wall-clock), that divergence *is* the finding — record it, it means
     the pipeline used more aggregate core-time than the wall-clock number
     alone would suggest.
   - The test's own `assert!` only checks the Mac 0.5x wall-clock bar and
     will `panic` on Pi4 even for a passing Pi4 CPU-time result — expected;
     judge Pi4 pass/fail from the printed `core-seconds` line yourself
     rather than the test's exit code. (If this becomes a recurring manual
     step, consider adding a Pi4-specific `#[ignore]`d test with its own
     1.0x assertion on the CPU-time ratio instead of overloading this one
     — out of scope for this measurement-only change.)
5. Run it 3+ times back to back — SBC thermal throttling under sustained
   load is a real and common Pi4 failure mode. Re-check `vcgencmd
   get_throttled` and `measure_clock arm` after the last run, not just
   before the first: a Pi already throttled at the start, or throttled
   uniformly across all three runs, produces *flat* ratios and would look
   unthrottled by drift alone — the throttling flags are the direct
   signal, an upward drift across runs is only a secondary corroborating
   one.
6. Record the result (date, Pi4 revision/RAM size, OS version, stock vs.
   overclocked config, throttling flags before/after, track count, both
   ratios, pass/fail against the 1.0x CPU-time bar) in this file's "Runs"
   section below.

## Cross-compiling (only if native build is impractical)

Cross-compiling for aarch64 needs a linker and glibc sysroot for that
target, not just Rust's own target libraries — `rustup target add` alone
does not provide either, and this repo has no `.cargo/config.toml`
supplying one (checked 2026-09-02), so a bare `cargo build --target
aarch64-unknown-linux-gnu` will fail at the link step on a non-aarch64
host. Two ways to get a working cross toolchain:

**Option A — `cross` (Docker-based, simplest):**
```
cargo install cross --git https://github.com/cross-rs/cross
cross test --release --target aarch64-unknown-linux-gnu -p manta-engine \
  --test cpu_budget --no-run -- --ignored --nocapture
```
`cross` runs the build inside a container that already has the
`aarch64-linux-gnu` toolchain and a matching sysroot configured — no
manual linker setup needed. Requires Docker (or Podman) on the build host.

**Option B — native cross-linker package + explicit Cargo linker config:**
```
# Debian/Ubuntu build host:
sudo apt install gcc-aarch64-linux-gnu

# Either set this for one invocation:
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
  cargo test --release --target aarch64-unknown-linux-gnu -p manta-engine \
  --test cpu_budget --no-run -- --ignored --nocapture

# ...or add to .cargo/config.toml (not committed to this repo — local only,
# since the fleet's other build hosts aren't all cross-compiling for Pi4):
#   [target.aarch64-unknown-linux-gnu]
#   linker = "aarch64-linux-gnu-gcc"
```

Either way, `--no-run` only *builds* the test binary — it does not embed
`--ignored --nocapture` into it (those are libtest flags interpreted at
*run* time, not compile time, and `--no-run` skips the run entirely). Copy
the resulting binary over and invoke it directly with those same flags on
the Pi4 itself, or the `#[ignore]`d test silently no-ops:

```
scp target/aarch64-unknown-linux-gnu/release/deps/cpu_budget-<hash> pi4:~/cpu_budget
ssh pi4 './cpu_budget --ignored --nocapture'
```

Nothing in `manta-engine`'s own `Cargo.toml` pulls in SoapySDR or another
native SDR library (checked 2026-09-02) — this bench only needs
`manta-testkit`'s synthetic scene generator, no hardware-specific features
to disable.

## Runs

(append entries here as you run this on real Pi4 hardware)

- 2026-09-02 — not run. No Raspberry Pi 4 (or any other Linux/aarch64 SBC)
  was reachable from the environment this MAN-18 session ran in — checked
  `~/.ssh/config` and known hosts across the fleet (aldebaran, rigel,
  sophon, vega, pandora); the only aarch64 hosts found are Apple Silicon
  Macs (M-series), which are the *other*, already-passing leg of this
  gate, not a Pi4 substitute. See
  `docs/DECISIONS/2026-09-02-man18-pi4-cpu-budget-gate.md` for a
  cross-architecture *estimate* (not a measurement) and why it should not
  be treated as satisfying this gate.
