# MAN-212: Windows as a first-class build/CI platform

## Motivation

Two independent reasons, both in the Linear ticket:

- **Strategic**: manta's target operators (RBN CW-skimmer users) predominantly
  run Windows already, since the incumbent it replaces (CW Skimmer) is
  Windows-only. CI today covers Linux + macOS only; Windows has never had a
  `cargo test` run against it.
- **Tactical**: a 2026-09-10/11 live-hardware debugging session (GitHub
  issues #166, #167, #171; `docs/DECISIONS/2026-09-10-*.md`) hit a real,
  reproducible SDRplay API service crash
  (`sdrplay_api_ServiceNotResponding`/`sdrplay_api_Fail`) repeatedly during
  sustained streaming on macOS — a documented upstream deadlock
  (`fventuri/gr-sdrplay3#14`). SDRplay is historically Windows-first;
  whether their Windows build sidesteps this in practice is an open,
  answerable question.

## Scope of this ticket / this PR

Per discussion with Tony: this PR delivers **build support + CI +
Windows-specific code fixes only**. The live-hardware RSP1B-on-Windows
validation (ticket item 3 — the step that actually answers the tactical
motivation) is **split into a follow-up ticket**, since it requires a
physical Windows mini PC and RSP1B that aren't reachable from this
environment. This PR is not blocked on that hardware step.

Non-goals (from the Linear ticket, unchanged): root-causing the SDRplay
service bug itself; full Windows feature-parity test coverage in this first
pass.

## Starting point (already true before this PR)

Windows is not a green-field target:

- `.github/workflows/release.yml` and `release-publish.yml` already build
  `x86_64-pc-windows-msvc` (default + `hpsdr` features) on every PR
  touching `crates/**`, `Cargo.toml`/`.lock`, or `rust-toolchain.toml`, and
  have been green through today's PRs.
- `README.md` already documents Windows as a supported release platform
  (mentions the VC++ Redistributable requirement, MAN-65).
- No POSIX-only production code was found. `ctrlc` (workspace dep, with the
  `termination` feature) has a native Windows console-event backend and
  needs no code change. All path handling in `manta-cli`/`manta-input` uses
  `PathBuf`, not raw string joins. The one `libc`/POSIX-signal-specific file
  (`crates/manta-cli/tests/signal_shutdown.rs`) is already
  `#![cfg(unix)]`-gated and simply excluded on Windows today — no compile
  break.

What's actually missing: `cargo test` has never run on Windows (the release
workflow only *builds*, never runs the test suite), and `--features soapy`
has never been attempted on Windows at all.

## Target

`x86_64-pc-windows-msvc`. Not MinGW — the existing release matrix already
committed to MSVC, and there's no reason to introduce a second
Windows toolchain/ABI story alongside the vcpkg-based C-library work below.

## CI changes (`.github/workflows/ci.yml`)

Three existing jobs each get a `windows-latest` entry added to their `os`
matrix:

1. **`test`** (default features, no `soapy`/`hpsdr`): add `windows-latest`.
   Every dependency in the default feature set (`cpal` via WASAPI, `ctrlc`,
   `tokio`, `tungstenite`, `hound`, `rubato`) is a cross-platform Rust
   crate with no known Windows-specific system-package requirement (unlike
   Linux's `libasound2-dev` need). Treated as low-risk; any concrete gap
   found once the job actually runs gets fixed as part of this PR.
2. **`test-hpsdr`**: add `windows-latest`. The `hpsdr` feature in
   `manta-input`/`manta-cli` has zero optional dependencies
   (`hpsdr = []`) — pure Rust, should need no new CI setup beyond the
   toolchain step already shared with the other jobs.
3. **`test-soapy`**: add `windows-latest`, installed via vcpkg. Confirmed a
   `soapysdr` vcpkg port exists (core library, v0.8.1). Critically, the
   existing soapy unit tests (`crates/manta-input/src/soapy.rs`) only
   exercise `driver=rtlsdr` — **no SDRplay module is needed for CI**, which
   sidesteps SDRplay's proprietary-API/EULA question entirely for this
   leg (that only matters for the deferred hardware-validation ticket).
   Needs a Windows build of the SoapyRTLSDR module alongside the vcpkg
   `soapysdr` core; use a vcpkg port if one exists, else build it from
   source via CMake as an explicit CI step (small dependency surface).

   **Fallback if this proves unreliable**: if the vcpkg/CMake path for
   SoapyRTLSDR can't be made to pass cleanly and reproducibly within
   reasonable CI-setup effort, `test-soapy` stays Linux/macOS-only and
   Windows CI covers default + `hpsdr` only. That gap gets stated
   explicitly in `ROADMAP.md`/the wiki (below) as a deliberate, known
   limitation — not silently dropped. This is an implementation-time call,
   made once the vcpkg route has actually been attempted.

No new job is added; existing `codex-clean` gating and matrix structure are
unchanged apart from the added `os` entries.

## Code changes

Expected to be small, discovered empirically once `cargo test --workspace`
actually runs on `windows-latest` for the first time:

- No shutdown-handling code change anticipated — `ctrlc::set_handler` is
  already cross-platform. Verified only by the CI job passing (and, later,
  by the deferred hardware step's manual Ctrl+C check).
- A Windows-native equivalent of `tests/signal_shutdown.rs` (e.g. via
  `GenerateConsoleCtrlEvent`) is explicitly **out of scope** for this PR —
  full Windows test-parity is a stated non-goal in the Linear ticket. Note
  it as a candidate follow-up in the wiki page below rather than building
  it now.
- Any real compile/test failure `windows-latest` surfaces (path-separator
  assumption in a test fixture, a Linux/macOS-only dev-dependency, etc.)
  gets fixed inline as part of this PR — this is exactly the kind of gotcha
  the ticket asks this work to find.

## Docs

- `ROADMAP.md`: update the CI-goal language to reflect three-OS coverage
  (or, if the `test-soapy` fallback above is taken, state the Linux/macOS-
  only soapy gap explicitly).
- `wiki/`: add a Windows build/dev-setup page (or a section in an existing
  build-oriented page) covering what this PR actually establishes — MSVC
  toolchain, vcpkg setup for `--features soapy`, any gotchas found while
  landing the CI legs. Matches `live-hardware-field-testing.md`'s level of
  detail for the build/setup side only; the live-RF side is the follow-up
  ticket's to document, once it exists.

## Follow-up ticket (opened alongside this PR, not blocking it)

A new Linear ticket + linked GitHub issue for the live-hardware leg:
RSP1B on a real Windows mini PC, running the same synchronized-capture /
soak methodology already used on macOS
(`docs/DECISIONS/2026-09-10-post-antenna-fix-90min-soak-and-service-reliability.md`),
comparing SDRplay-service crash frequency against the macOS baseline in
issues #167/#171. Links back to MAN-212 and to #166/#167/#171.

## Testing / acceptance for this PR

- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D
  warnings`, `cargo test --workspace` green on `windows-latest`.
- `cargo clippy -p manta-input -p manta-cli --all-targets --features hpsdr
  -- -D warnings` and `cargo test -p manta-input -p manta-cli --features
  hpsdr` green on `windows-latest`.
- `--features soapy` on `windows-latest`: green if the vcpkg/CMake route
  works out; otherwise explicitly documented as not covered, per the
  fallback above.
- Existing Linux/macOS jobs stay green (no regressions).
- Draft PR opened early per repo hygiene; auto-merge enabled once CI is
  green.
