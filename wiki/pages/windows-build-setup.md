---
id: windows-build-setup
title: How do I build and test manta on Windows?
kind: howto
status: current
maintainer: agent
sources:
  - .github/workflows/ci-full.yml
  - .github/workflows/release.yml
  - docs/superpowers/specs/2026-09-11-man212-windows-platform-support-design.md
verified:
  commit: 9b3d690
  date: 2026-10-10
links:
  - live-hardware-field-testing
---
# How do I build and test manta on Windows?

manta targets `x86_64-pc-windows-msvc` (not MinGW) -- this matches the
prebuilt release binaries (`.github/workflows/release.yml`) and is the only
Windows target CI (`.github/workflows/ci-full.yml`) exercises.

## Default build (no SoapySDR)

Requires the [Visual Studio Build Tools](https://visualstudio.microsoft.com/downloads/)
(the MSVC linker `link.exe`, not Visual Studio itself) and a `git`
executable on `PATH` (same git-dependency requirement as every platform --
see the main `README.md`'s Quickstart, `coppa` is a rev-pinned git
dependency). Then:

```powershell
cargo build --release -p manta-cli
cargo test --workspace
```

No extra system packages are needed for the default feature set --
`cpal`'s WASAPI backend, `ctrlc`'s native Windows console-event handler,
and the rest of the default dependency set are all pure-Rust or
Windows-native, unlike Linux's `libasound2-dev` requirement.

## `--features hpsdr`

No extra setup -- `hpsdr` has zero optional dependencies
(`crates/manta-input/Cargo.toml`'s `hpsdr = []`).

```powershell
cargo test -p manta-input -p manta-cli --features hpsdr
```

## `--features soapy`

Needs the SoapySDR C library, which has no Windows system-package manager
equivalent of Linux's `libsoapysdr-dev`. Two options:

1. **PothosSDR** (the prebuilt Windows SDR development environment from
   the SoapySDR maintainers) -- simplest for a real SDR-hardware setup:
   download and install from
   [pothosware/PothosSDR releases](https://github.com/pothosware/PothosSDR),
   which bundles SoapySDR core plus common driver modules (RTL-SDR,
   HackRF, etc; SDRplay's module needs the separate SDRplay API installer
   too -- SDRplay's own Windows API/driver, not part of PothosSDR). Set
   `SoapySDR_DIR` (or add PothosSDR's `bin`/`lib` dirs to `PATH`) so the
   `soapysdr-sys` crate's build script can find it -- see
   [kevinmehall/rust-soapysdr](https://github.com/kevinmehall/rust-soapysdr)'s
   own README for the exact env vars it checks.
2. **vcpkg**, for CI or a from-source setup without the GUI installer.
   MAN-212 tried this path in CI and it is not currently viable: `test-soapy`
   in `.github/workflows/ci-full.yml` runs only `[ubuntu-latest, macos-latest]` --
   `windows-latest` was dropped after real attempts, and the CI file itself
   no longer shows any trace of the attempt. In order, what was found:
   1. `lukka/run-vcpkg`'s pinned action SHA must resolve to a real commit on
      the `v11.5` tag -- a bad/typo'd SHA breaks Actions' upfront `uses:`
      resolution for the **whole job**, all OS legs, not just Windows.
   2. The `vcpkgGitCommitId` pin must be reasonably current -- a 2024-08 pin
      failed to configure `soapysdr:x64-windows` via Ninja against
      `windows-latest`'s VS2026/MSVC 14.44 toolset (failed in ~76ms, too
      fast to be a real compile attempt; a `CMP0174` CMake policy warning
      was also observed, consistent with vcpkg's own scripts predating the
      CMake/toolset generation on the runner). Bumping to a current vcpkg
      master tip fixed the `soapysdr` core build.
   3. Past that point, building `SoapyRTLSDR` from source (needed since it
      has no vcpkg port) failed with
      `error C1083: Cannot open include file: 'libusb.h'` -- `libusb`
      development headers aren't present on `windows-latest` and nothing
      installs them. This is the concrete next blocker for anyone picking
      this up: `vcpkg install libusb:x64-windows` and threading its
      include/lib paths into the `rtl-sdr` and `SoapyRTLSDR` CMake
      configure steps is the likely next step, untried.

   Full diagnostic detail (exact CMake invocations, error text, commit-by-
   commit narration) is in `git log` on the MAN-212 branch history rather
   than in the CI file -- look for the `MAN-212` commits with subjects
   `fix(manta): MAN-212 -- correct lukka/run-vcpkg action SHA pin (was
   unresolvable)`, `fix(manta): MAN-212 -- bump stale vcpkg baseline pin for
   windows soapy CI`, and `docs(manta): MAN-212 -- document windows soapy CI
   as a known gap, not attempted further`.

For an actual SDRplay RSP-series radio, the SDRplay Windows API/service
installer is separate from SoapySDR/PothosSDR entirely -- install it from
SDRplay's own site, then SoapySDRPlay3's module on top of that. This
repo's own live-hardware validation on Windows (RSP1B, SDRplay-service
reliability comparison against the macOS baseline in issues #166/#167/
#171) is tracked as a follow-up to MAN-212, not yet done as of this page's
`verified.commit` -- see that follow-up ticket for the up-to-date status
once it exists.

## Writing tests that pass on the Windows legs

- Three gotchas broke `test`/`test-hpsdr (windows-latest)` after the last
  green run (MAN-230): (1) a Unix-absolute literal such as `/x/v1.wav` has
  no drive prefix, so Windows treats it as relative and config path
  resolution joins it onto the config dir (`C:/x/v1.wav`) -- build
  absolute test paths from `std::env::temp_dir()` and write them as TOML
  literal strings (`'...'`), and split `path: msg` lines on `": "`, not
  `':'`; (2) the runners check text out with CRLF, and `include_str!`
  embeds those bytes, so normalise `\r\n` before matching `\n`-delimited
  text; (3) when a test file mixes cross-platform tests with a
  `#[cfg(unix)]` test, gate the unix-only imports and helpers per item
  (not `#![cfg(unix)]` on the file, which drops the cross-platform tests
  too), or `cargo clippy --all-targets -- -D warnings` fails on Windows
  before `cargo test` starts.

## Known gaps

- `--features soapy` is not covered by Windows CI. `test-soapy` runs only
  `ubuntu-latest` and `macos-latest`; the `windows-latest` leg was
  attempted (see the vcpkg subsection above) and dropped after hitting a
  concrete, unresolved blocker (`libusb.h` missing for `SoapyRTLSDR`'s
  from-source build), not left untried by default. In short: (1) the
  `lukka/run-vcpkg` action SHA had to be fixed to a real commit on the
  `v11.5` tag, since a bad pin broke the whole job's `uses:` resolution,
  not just Windows; (2) the `vcpkgGitCommitId` pin had to be bumped off a
  stale 2024-08 baseline, which failed configuring `soapysdr:x64-windows`
  via Ninja against `windows-latest`'s VS2026/MSVC 14.44 toolset; (3) past
  that, building `SoapyRTLSDR` from source hit `error C1083: Cannot open
  include file: 'libusb.h'` -- installing `libusb` via vcpkg and threading
  its paths into the CMake configure is the likely next step, untried. See
  the `--features soapy` section above for the full narrative, and
  [PR #184](https://github.com/HagaleTechnologies/manta/pull/184)'s
  Commits tab for the exact diffs and error text -- the PR persists after
  merge independent of this source branch's lifecycle, unlike raw `git
  log` on the branch itself.
- No Windows-native equivalent of `crates/manta-cli/tests/signal_shutdown.rs`
  exists -- that test is `#![cfg(unix)]`-gated and simply doesn't run on
  Windows. `ctrlc::set_handler`'s Windows console-event backend is used in
  production but only manually verified (Ctrl+C during a live `manta
  listen` session), not covered by an automated Windows CI test.
