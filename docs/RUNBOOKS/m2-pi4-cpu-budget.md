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
2. `cargo test --release -p manta-engine --test cpu_budget -- --ignored --nocapture`
   **Must be `--release`.** Plain `cargo test` measures dev-profile speed
   (~1.45x slower per this workspace's `opt-level = 1` first-party dev
   profile — see `docs/DECISIONS/2026-07-24-m2-pileup-cpu-budget-pins.md`
   item 5) and will produce a misleadingly pessimistic number.
3. Read the printed ratio:
   ```
   cpu_budget: <N>s wall / 15.00s audio = <ratio>x realtime (Mac budget: < 0.5x)
   ```
   The printed "Mac budget" label is a leftover from the Mac-only assertion
   in this same test file — the Pi4 accept threshold per ROADMAP.md M2 is
   **< 1.0x realtime** (< 1 full core), not < 0.5x. The test's own
   `assert!` only checks the 0.5x Mac bar and will `panic` on Pi4 even for
   a passing Pi4 result — that's expected; read the printed ratio yourself
   rather than trusting the test's pass/fail exit code on Pi4. (If this
   becomes a recurring manual step, consider adding a Pi4-specific
   `#[ignore]`d test with its own 1.0x assertion instead of overloading
   this one — out of scope for a measurement-only change.)
4. Also capture real vs. user CPU time, not just wall clock, so a future
   reader can tell whether the pipeline stayed close to single-core-bound
   on Pi4 the way it does on Mac (see pins doc item 7 — the wall-clock and
   CPU-time budgets are only known to be equivalent on Mac-class hardware;
   confirm the same holds on Pi4 rather than assuming it):
   ```
   /usr/bin/time -v cargo test --release -p manta-engine --test cpu_budget -- --ignored --nocapture
   ```
   (`-v` is the GNU coreutils `time` verbose flag, standard on Raspberry Pi
   OS; note this is *not* the same flag as macOS's BSD `/usr/bin/time -l`.)
   Record wall, user, and sys seconds.
5. Run it 3+ times back to back — SBC thermal throttling under sustained
   load is a real and common Pi4 failure mode that a single short run can
   miss. Watch for the ratio drifting upward across runs (a sign of
   throttling) rather than staying flat.
6. Record the result (date, Pi4 revision/RAM size, OS version, ratio,
   whether it throttled, pass/fail against the 1.0x bar) in this file's
   "Runs" section below.

## Cross-compiling (only if native build is impractical)

```
rustup target add aarch64-unknown-linux-gnu
cargo test --release --target aarch64-unknown-linux-gnu -p manta-engine \
  --test cpu_budget --no-run -- --ignored --nocapture
# copy the resulting target/aarch64-unknown-linux-gnu/release/deps/cpu_budget-<hash>
# binary to the Pi4 and run it directly there.
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
