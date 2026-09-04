# M2 manual acceptance: Raspberry Pi 4 CPU-budget gate

Not part of CI (same reasoning as `benches/cpu_budget.rs`'s module doc:
GitHub-hosted runners aren't Pi4 hardware, and perf assertions on shared CI
are flaky). Run this yourself against real Pi4 hardware before flipping
ROADMAP.md's M2 Pi4 leg from "outstanding" to measured, and before deciding
MAN-30's fate.

## What you need

- A physical Raspberry Pi 4 (any RAM size; this is a CPU, not a memory,
  gate), running Raspberry Pi OS **Bookworm** (2023-10+) or another
  glibc >= 2.35 aarch64 Linux — same baseline the sibling `pancetta`
  repo's aarch64 target assumes (not present in a standalone manta
  checkout — see
  [pancetta's README](https://github.com/HagaleTechnologies/pancetta#readme),
  a public repo, for that baseline's own rationale).
- Nothing else competing for the Pi4's cores during the run (no desktop
  environment doing real work, no other soak/benchmark process).
- A Rust toolchain on the Pi4 itself, **1.85.0 or newer** (this
  workspace's root `Cargo.toml` sets `rust-version = "1.85.0"`; check with
  `rustc --version` before the native build below — a fresh Raspberry Pi
  OS install's distro-packaged `rustc` is commonly older than this and
  will fail partway through an otherwise lengthy build. Install/update via
  [rustup](https://rustup.rs) if so). Cross-compiling and copying a binary
  over works too (same version requirement on the build host), but native
  `cargo test` is simpler and removes a class of "did the cross build
  actually match" doubt.

## Steps

1. `git clone` (or `git pull`) this repo on the Pi4, or `scp`/`rsync` a
   built binary over if you cross-compiled instead — see "Cross-compiling"
   below if the Pi4 itself is too slow to build from source in reasonable
   time (it isn't, in practice; a from-scratch `manta-engine` release build
   takes a few minutes on Pi4, not hours). **Before building natively on
   the Pi4**, install the ALSA dev headers and `pkg-config` — a fresh
   Raspberry Pi OS install has neither, and `manta-engine` pulls in
   `manta-input` → `cpal` → `alsa-sys`, which needs both to build. This
   mirrors the repo's own Linux CI step (`.github/workflows/ci.yml`):
   ```
   sudo apt-get update && sudo apt-get install -y libasound2-dev pkg-config
   ```
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
   cpu_budget: 300 tracks sustained across most of the run (scene has 300 signals)
   cpu_budget: <N>s wall / 58.00s steady-state audio (60.00s scene minus 2.0s detector warmup) = <ratio>x realtime wall-clock (Mac budget: < 0.5x)
   cpu_budget: <N>s (user+sys) CPU / 58.00s steady-state audio = <ratio>x core-seconds (Pi4 budget: < 1.0x; Mac budget: < 0.5x)
   ```
   Both ratios divide by 58.0s (the 60s scene minus its 2s detector
   warmup window, during which no tracks are active yet), not the full
   60s -- dividing by the full scene duration understates the ratio by
   diluting the steady-state cost with warmup's share of near-free time (a
   bug in earlier versions of this bench, including the 0.360x number on
   record in `docs/DECISIONS/2026-07-24-m2-pileup-cpu-budget-pins.md`).
   Warmup's share was ~13% (2s of 15s) when the scene was still 15s long;
   the scene was later lengthened to 60s specifically to shrink that share
   to ~3.3% (2s of 60s) and empirically confirm the correction itself was
   accurate — both the 15s and 60s methodologies are historical at this
   point, only the current 58.0s-denominator behavior in
   `tests/cpu_budget.rs` is live; see
   `docs/DECISIONS/2026-09-02-man18-pi4-cpu-budget-gate.md` for the full
   history.
   - **The "tracks sustained" line must read exactly 300** (the test
     asserts `== 300`, not a tolerance band — see
     `docs/DECISIONS/2026-09-02-man18-pi4-cpu-budget-gate.md`'s
     rationale). It counts tracks whose decode events span most of the
     file, not just distinct `track_id`s seen anywhere — a detector that
     promotes fewer tracks at once, or one that churns through more than
     300 short-lived IDs without ever holding close to 300 concurrently,
     both show up as a low
     count here. It's a proxy for "held ~300 tracks concurrently", not an
     exact instantaneous count (`decode_samples`'s public event stream has
     no track-opened/closed events to count that directly — see the
     function's doc comment in `tests/cpu_budget.rs`). A materially lower
     count than 300 means the run below is benchmarking a cheaper workload
     than ROADMAP.md's criterion — don't trust the timing numbers if this
     failed.
   - **The `(user+sys) CPU / audio` line — not the wall-clock line — is
     the number to compare against the Pi4 budget (< 1.0x).** ROADMAP.md's
     criterion is a CPU-time budget ("< 1 full core"), and wall-clock only
     equals CPU time when the pipeline is single-core-bound. Measured
     2026-09-02: on Mac it isn't quite — `decode_samples` shows ~1.2x-1.25x
     parallelism (CPU-time ratio noticeably higher than wall-clock), which
     is why the test now asserts *both* ratios against the Mac 0.5x budget,
     not just wall-clock. Whether Pi4's 4 weaker, differently-scheduled
     cores show a similar, larger, or smaller parallelism factor is
     unconfirmed — this is exactly what running this test on real hardware
     answers. If the two ratios diverge noticeably, that divergence *is*
     part of the finding — record both, not just whichever one looks
     better.
   - The test's own `assert!`s check the Mac budget (< 0.5x, both ratios)
     and will `panic` on Pi4 even for a passing Pi4 CPU-time result (Pi4's
     budget is < 1.0x, a different number) — expected; judge Pi4 pass/fail
     from the printed `core-seconds` line yourself
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
   **How the runs decide pass/fail:** record every run's CPU-time ratio,
   not just the best or the last one. The gate is **PASS only if every
   non-throttled run's CPU-time ratio is < 1.0x**. A single non-throttled
   run at or above 1.0x is a **FAIL**, even if other runs on the same Pi
   came in lower — don't average across runs or report only the most
   favorable one; ROADMAP's criterion is about the pipeline actually
   staying under budget, not about it being possible to catch it under
   budget on a good run. If every run is throttled (per step 2/this
   step's `vcgencmd` checks), the gate is **INCONCLUSIVE**, not a pass —
   fix cooling/config and re-run rather than reporting a throttled number
   either way.
6. Record the result in this file's "Runs" section below: date, Pi4
   revision/RAM size, OS version, stock vs. overclocked config, throttling
   flags before/after, track count, both ratios, pass/fail against the
   1.0x CPU-time bar -- **and which exact code was measured**: `git
   rev-parse HEAD` and `git status --porcelain` (empty output = clean) on
   the checkout you built from, plus `rustc --version`. This result closes
   a performance gate for a specific pipeline implementation; without the
   commit and clean-status recorded, nobody reading it later can tell
   whether it measured the code currently on `main`, an older revision, or
   a local modification.

## Cross-compiling (only if native build is impractical)

Cross-compiling for aarch64 needs a linker and glibc sysroot for that
target, not just Rust's own target libraries — `rustup target add` alone
does not provide either, and this repo has no `.cargo/config.toml`
supplying one (checked 2026-09-02), so a bare `cargo build --target
aarch64-unknown-linux-gnu` will fail at the link step on a non-aarch64
host. Two ways to get a working cross toolchain:

`manta-engine` pulls in `manta-input` → `cpal` → `alsa-sys`, a native
dependency needing ALSA's dev headers for whichever target you're building
— not just a Rust-level cross toolchain. Two ways to get a working setup:

**Option A — `cross` (Docker-based, simplest):**
```
cargo install cross --git https://github.com/cross-rs/cross
cross test --release --target aarch64-unknown-linux-gnu -p manta-engine \
  --test cpu_budget --no-run -- --ignored --nocapture
```
`cross`'s stock `aarch64-unknown-linux-gnu` image bundles the
`aarch64-linux-gnu` toolchain and sysroot, but has not been confirmed here
to include the aarch64 ALSA dev headers `alsa-sys`'s build script needs —
verify this actually links before relying on it (unverified as of
2026-09-02: no Docker available in the environment this runbook was
written in); if it fails on the `alsa-sys` build script, a custom `cross`
image (`Cross.toml` + `RUN apt-get install -y libasound2-dev:arm64`, or
equivalent) or Option B is the fallback. Requires Docker (or Podman) on
the build host.

**Option B — native cross-linker package + explicit Cargo linker config:**
```
# Debian build host -- Debian's mirrors serve every release architecture,
# including arm64, from the same tree, so this is all it needs:
sudo dpkg --add-architecture arm64
sudo apt update
sudo apt install gcc-aarch64-linux-gnu libasound2-dev:arm64 pkg-config

# Ubuntu build host -- do THIS FIRST instead. Unlike Debian, Ubuntu's
# default mirrors (archive.ubuntu.com / security.ubuntu.com) carry only
# amd64/i386; arm64 lives on a separate mirror, ports.ubuntu.com. Without
# this block, `sudo apt update` FAILS (exit 100, "E: Failed to fetch
# .../binary-arm64/Packages 404 Not Found") as soon as arm64 is added, and
# never reaches the install/build steps below. Two things are needed: point
# apt at ports for arm64, AND restrict the existing default sources to
# amd64 so they stop being asked for an arm64 index they will never have.
sudo dpkg --add-architecture arm64
codename=$(. /etc/os-release && echo "$VERSION_CODENAME")
keyring=/usr/share/keyrings/ubuntu-archive-keyring.gpg
if [ -f /etc/apt/sources.list.d/ubuntu.sources ]; then
  # 24.04 (noble) and later: deb822 stanza format.
  grep -q '^Architectures:' /etc/apt/sources.list.d/ubuntu.sources ||
    sudo sed -i.bak '/^URIs:/a Architectures: amd64' \
      /etc/apt/sources.list.d/ubuntu.sources
  sudo tee /etc/apt/sources.list.d/ubuntu-ports-arm64.sources >/dev/null <<EOF
Types: deb
URIs: http://ports.ubuntu.com/ubuntu-ports
Suites: ${codename} ${codename}-updates ${codename}-security
Components: main
Architectures: arm64
Signed-By: ${keyring}
EOF
else
  # 22.04 (jammy) and earlier, and in-place upgrades that kept this format:
  # classic one-line format.
  sudo sed -i.bak -E 's|^(deb[[:space:]]+)([a-z0-9+.-]+:)|\1[arch=amd64] \2|' \
    /etc/apt/sources.list
  sudo tee /etc/apt/sources.list.d/ubuntu-ports-arm64.list >/dev/null <<EOF
deb [arch=arm64 signed-by=${keyring}] http://ports.ubuntu.com/ubuntu-ports ${codename} main
deb [arch=arm64 signed-by=${keyring}] http://ports.ubuntu.com/ubuntu-ports ${codename}-updates main
deb [arch=arm64 signed-by=${keyring}] http://ports.ubuntu.com/ubuntu-ports ${codename}-security main
EOF
fi
sudo apt update
sudo apt install gcc-aarch64-linux-gnu libasound2-dev:arm64 pkg-config

# Rust's own target standard library -- gcc-aarch64-linux-gnu above gives
# a linker, not this; skipping it fails before linking with a missing-
# target/`core` error:
rustup target add aarch64-unknown-linux-gnu

# Either set this for one invocation. PKG_CONFIG_ALLOW_CROSS=1 alone only
# permits a cross query -- it does NOT point pkg-config at the arm64
# .pc files; PKG_CONFIG_LIBDIR overrides its search path to Debian's
# multiarch arm64 location (empty PKG_CONFIG_PATH so the host's own
# x86_64 paths aren't also searched and matched by mistake):
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
  PKG_CONFIG_ALLOW_CROSS=1 \
  PKG_CONFIG_PATH= \
  PKG_CONFIG_LIBDIR=/usr/lib/aarch64-linux-gnu/pkgconfig \
  cargo test --release --target aarch64-unknown-linux-gnu -p manta-engine \
  --test cpu_budget --no-run -- --ignored --nocapture

# ...or add the linker line to .cargo/config.toml (not committed to this
# repo — local only, since the fleet's other build hosts aren't all
# cross-compiling for Pi4):
#   [target.aarch64-unknown-linux-gnu]
#   linker = "aarch64-linux-gnu-gcc"
```

Notes on the Ubuntu block above (verified 2026-09-04 against the live Ubuntu
mirrors from an x86_64 host, apt 3.0.3, using isolated apt roots rather than a
mutated host):

- Only `main` is listed for ports: `libasound2-dev` arm64 is in `main` on
  focal/jammy/noble and is *absent* from `universe` on all three, so the
  other components add index-fetch time and nothing else. Add them if some
  other arm64 `-dev` package you need turns out to live outside `main`.
- The branch keys on whether `/etc/apt/sources.list.d/ubuntu.sources` exists,
  not on the release number, so a 22.04-to-24.04 in-place upgrade that kept
  the classic format is still handled correctly.
- Both `sed`s are safe to run twice: the deb822 one is guarded by the `grep`,
  and the classic one only matches a `deb ` line whose next token is a URI
  scheme, so it skips `# deb-src` comments and lines that already carry
  `[arch=...]`. Both write a `.bak` beside the file they edit.
- `gcc-aarch64-linux-gnu` is an amd64 package from the host's own archive
  (`main`), so it installs fine even when the arm64 half is broken -- which
  is exactly how this failure hides itself if the `apt install` output is
  only skimmed.
- Unverified as of 2026-09-04: a full `apt install` + `cargo test --target
  aarch64-unknown-linux-gnu` run on a stock Ubuntu desktop/server host. What
  *was* verified is that apt resolves `libasound2-dev:arm64` to a real
  ports candidate in both sources formats and that `apt update` exits 0
  afterwards; the link step itself carries the same "verify before relying
  on it" status as Option A above.

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
native SDR library beyond `manta-input`'s ALSA dependency above (checked
2026-09-02) — this bench only needs `manta-testkit`'s synthetic scene
generator, no additional hardware-specific features to disable.

## Runs

(append entries here as you run this on real Pi4 hardware)

- 2026-09-02 — not run. No Raspberry Pi 4 (or any other Linux/aarch64 SBC)
  was reachable from the environment this MAN-18 session ran in — checked
  `~/.ssh/config` and known hosts across the fleet (aldebaran, rigel,
  sophon, vega, pandora); the only aarch64 hosts found are Apple Silicon
  Macs (M-series), which are the *other* leg of this gate, not a Pi4
  substitute. **That other leg's own pass/fail status is unresolved too**
  — a warmup-dilution bug found on this same PR invalidates the Mac
  numbers recorded before 2026-09-02 (including the 0.360x on record since
  2026-07-24); a clean rerun of the now-fixed test on a quiet, uncontended
  machine is needed before either leg of this M2 criterion can be called
  settled. See `docs/DECISIONS/2026-09-02-man18-pi4-cpu-budget-gate.md`
  for the full history, a cross-architecture Pi4 *estimate* (not a
  measurement), and why neither should be treated as satisfying this gate.
