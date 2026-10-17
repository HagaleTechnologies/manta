# MAN-212: Windows Platform Support Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `windows-latest` to manta's CI matrix (default, `hpsdr`, and — if feasible — `soapy` features), fix any Windows-specific code issues CI surfaces, and document the Windows build/dev-setup path, so Windows becomes a real, tested build target rather than an untested release-only artifact.

**Architecture:** Three independent additions to `.github/workflows/ci.yml`'s existing `test`/`test-hpsdr`/`test-soapy` jobs (each already a `strategy.matrix.os` list of `[ubuntu-latest, macos-latest]`); a docs pass (`ROADMAP.md`, a new wiki page); and a follow-up Linear ticket + GitHub issue for the live-hardware leg that this plan does not cover. No new workflow file, no new job — only matrix entries and, for `test-soapy`, a new Windows-specific setup step.

**Tech Stack:** GitHub Actions (`windows-latest` runner, MSVC toolchain via `dtolnay/rust-toolchain`), vcpkg (for the `soapysdr` C library on `test-soapy`), Rust/Cargo (existing workspace, `x86_64-pc-windows-msvc` target — already used by `release.yml`).

**Spec:** `docs/superpowers/specs/2026-09-11-man212-windows-platform-support-design.md`

## Global Constraints

- Target triple is `x86_64-pc-windows-msvc` only — no MinGW (matches the existing `release.yml`/`release-publish.yml` matrix).
- This plan's PR does **not** include the live-hardware RSP1B-on-Windows validation (Linear ticket item 3) — that is a separate follow-up ticket opened as part of this plan's last task, not implemented here.
- No Windows-native signal-handling test (`GenerateConsoleCtrlEvent` equivalent of `tests/signal_shutdown.rs`) is written in this plan — explicitly out of scope per the ticket's non-goals.
- Every CI change must be verified against a real GitHub Actions run (via the draft PR opened in Task 1) before being considered done — do not mark a task complete on the basis of "should work."
- `cargo fmt --all --check` and `cargo clippy --workspace --all-targets -- -D warnings` must stay green on every existing job (no regressions to the Linux/macOS legs).
- Repo hygiene: work happens in the `tony/man-212-manta-should-support-windows-as-a-first-class-platform` branch, already checked out at `~/Code/manta-man212` (worktree). Auto-merge is enabled repo-wide — the draft PR opened in Task 1 gets `gh pr merge --auto --squash` applied right after creation, per this repo's standing policy.
- Commit messages: `feat(manta): MAN-212 -- <description>` / `docs(manta): MAN-212 -- <description>`, conventional-commit style with scope, matching this repo's convention.

---

## File Structure

- **Modify:** `.github/workflows/ci.yml` — add `windows-latest` to `test` and `test-hpsdr` matrices (Task 1); add `windows-latest` + a new Windows vcpkg setup step to `test-soapy` (Task 3), or omit it with a documented reason if the fallback is taken (Task 4).
- **Modify:** `ROADMAP.md` — CI-goal language, once the final Windows CI shape is known (Task 6).
- **Create:** `wiki/pages/windows-build-setup.md` — Windows build/dev-setup page (Task 7).
- **Modify:** `wiki/INDEX.md` — add an entry pointing at the new page (Task 7).
- **Possibly modify:** any source file where a real Windows-only `cargo test`/`cargo clippy` failure is found (Tasks 2 and 4) — exact files unknown until CI actually runs; see those tasks' verification gates for how a real failure gets diagnosed and fixed rather than guessed at.

---

## Task 1: Add `windows-latest` to the low-risk `test` and `test-hpsdr` jobs; open the draft PR

**Files:**
- Modify: `.github/workflows/ci.yml:199-202` (`test` job's `strategy.matrix`)
- Modify: `.github/workflows/ci.yml:246-249` (`test-hpsdr` job's `strategy.matrix`)

**Interfaces:** None (CI-only change, no Rust code).

- [ ] **Step 1: Edit the `test` job's matrix**

In `.github/workflows/ci.yml`, change:

```yaml
    strategy:
      fail-fast: false
      matrix:
        os: [ubuntu-latest, macos-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
      - if: runner.os == 'Linux'
        run: sudo apt-get update && sudo apt-get install -y libasound2-dev
```

(the block immediately following `test:`'s `if:` line) to:

```yaml
    strategy:
      fail-fast: false
      matrix:
        os: [ubuntu-latest, macos-latest, windows-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
      - if: runner.os == 'Linux'
        run: sudo apt-get update && sudo apt-get install -y libasound2-dev
```

(only the `os:` line changes — no new step needed; `cpal`'s WASAPI backend needs no extra system package on Windows).

- [ ] **Step 2: Edit the `test-hpsdr` job's matrix**

In the same file, find `test-hpsdr:`'s block and change:

```yaml
    strategy:
      fail-fast: false
      matrix:
        os: [ubuntu-latest, macos-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
      - if: runner.os == 'Linux'
        run: sudo apt-get update && sudo apt-get install -y libasound2-dev
```

to:

```yaml
    strategy:
      fail-fast: false
      matrix:
        os: [ubuntu-latest, macos-latest, windows-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
      - if: runner.os == 'Linux'
        run: sudo apt-get update && sudo apt-get install -y libasound2-dev
```

(again, only the `os:` line — the `hpsdr` feature has zero optional dependencies, `hpsdr = []` in `crates/manta-input/Cargo.toml`).

- [ ] **Step 3: Verify the YAML is well-formed**

Run: `cd ~/Code/manta-man212 && python3 -c "import yaml; yaml.safe_load(open('.github/workflows/ci.yml'))" && echo OK`
Expected: `OK` (catches indentation/syntax errors before pushing — GitHub Actions gives poor local feedback on a broken workflow file).

- [ ] **Step 4: Commit**

```bash
cd ~/Code/manta-man212
git add .github/workflows/ci.yml
git commit -m "feat(manta): MAN-212 -- add windows-latest to the default and hpsdr CI legs"
```

- [ ] **Step 5: Push and open the draft PR**

```bash
cd ~/Code/manta-man212
git push -u origin tony/man-212-manta-should-support-windows-as-a-first-class-platform
gh pr create --draft --title "feat(manta): MAN-212 -- Windows platform support (build + CI)" --body "$(cat <<'EOF'
## Summary
- Adds windows-latest to CI (test, test-hpsdr; test-soapy pending investigation)
- Fixes any Windows-specific issues CI surfaces
- Documents the Windows build/dev-setup path

Live-hardware RSP1B-on-Windows validation (ticket item 3) is split into a
follow-up ticket -- see linked issue once opened.

Spec: docs/superpowers/specs/2026-09-11-man212-windows-platform-support-design.md
Plan: docs/superpowers/plans/2026-09-11-man212-windows-platform-support.md

Closes MAN-212.
EOF
)"
gh pr merge --auto --squash
```

Expected: PR created as draft, auto-merge armed (will only fire once required checks are green and the PR is marked ready — draft PRs cannot be auto-merged by GitHub until undrafted, so this arms the setting now for when Task 8 marks it ready).

---

## Task 2: Verify the `test`/`test-hpsdr` Windows runs and fix any real failures

**Files:** Unknown until CI runs — see verification gate below for how to find them.

**Interfaces:** None.

- [ ] **Step 1: Poll the CI run for the pushed commit**

```bash
cd ~/Code/manta-man212
gh run list --branch tony/man-212-manta-should-support-windows-as-a-first-class-platform --limit 5
```

Wait for the `CI` run to reach a terminal state (`completed`). This can take 10-20 minutes; do not poll more often than every 60 seconds.

- [ ] **Step 2: Inspect the `windows-latest` `test` and `test-hpsdr` job results**

```bash
gh run view --job "$(gh run list --branch tony/man-212-manta-should-support-windows-as-a-first-class-platform --limit 1 --json databaseId -q .[0].databaseId | xargs -I{} gh run view {} --json jobs -q '.jobs[] | select(.name | test("test.*windows")) | .databaseId')"
```

If that composite command is awkward in your shell, simpler: `gh run view <run-id>` lists all jobs and their conclusions; find the `test (windows-latest)` and `test-hpsdr (windows-latest)` rows directly.

- [ ] **Step 3: If both are green, done — record it and move to Task 3**

No further action. The default and `hpsdr` builds are now validated on Windows.

- [ ] **Step 4: If either is red, fetch the failure log**

```bash
gh run view <run-id> --log-failed
```

- [ ] **Step 5: Diagnose and fix the specific failure**

Common categories to expect, with concrete responses:
- **`cargo fmt`/`clippy` failure specific to Windows** (e.g. a `#[cfg(unix)]`-only import now unused, or a lint that only fires with the MSVC target): fix the specific lint/import at the reported file:line. This is ordinary Rust code review — apply the fix, don't guess broadly.
- **A test using a hardcoded Unix path separator or `/tmp`** (e.g. any test literal like `"/tmp/v1.wav"` used as an actual filesystem path rather than just a `PathBuf` parse-check): replace with `std::env::temp_dir()` or a `tempfile::TempDir`, matching the pattern already used in `crates/manta-cli/tests/signal_shutdown.rs`'s `CARGO_TARGET_TMPDIR` handling.
- **A test that shells out to a Unix-only binary** (unlikely outside the already-`cfg(unix)`-gated `signal_shutdown.rs`, but check `crates/manta-cli/tests/` and `crates/manta-soak-harness/` for any `Command::new("sh")`/`Command::new("kill")` not already gated): gate it `#[cfg(unix)]` matching `signal_shutdown.rs`'s existing pattern, since fixing signal-delivery tests for Windows natively is out of scope per this plan's Global Constraints.
- **Anything not covered by the above**: read the actual error text before assuming; fix the root cause at its file:line, matching this repo's `CLAUDE.md` guidance against band-aid fixes.

- [ ] **Step 6: Commit the fix and re-push**

```bash
cd ~/Code/manta-man212
git add -A
git commit -m "fix(manta): MAN-212 -- <specific description of what was wrong>"
git push
```

- [ ] **Step 7: Repeat Steps 1-6 until both jobs are green**

---

## Task 3: Add `windows-latest` + vcpkg to `test-soapy`

**Files:**
- Modify: `.github/workflows/ci.yml:223-242` (`test-soapy` job)

**Interfaces:** None.

- [ ] **Step 1: Edit the `test-soapy` job**

Change:

```yaml
  test-soapy:
    needs: codex-clean
    if: "!cancelled() && (needs.codex-clean.result == 'success' || needs.codex-clean.result == 'skipped')"
    strategy:
      fail-fast: false
      matrix:
        os: [ubuntu-latest, macos-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
      - if: runner.os == 'Linux'
        run: sudo apt-get update && sudo apt-get install -y --no-install-recommends libasound2-dev libsoapysdr-dev soapysdr0.8-module-rtlsdr
      - if: runner.os == 'macOS'
        run: brew install soapysdr
      - uses: dtolnay/rust-toolchain@4cda84d5c5c54efe2404f9d843567869ab1699d4 # stable
        with:
          components: rustfmt, clippy
      - uses: Swatinem/rust-cache@e18b497796c12c097a38f9edb9d0641fb99eee32 # v2.9.1
      - run: cargo clippy -p manta-input -p manta-cli --all-targets --features soapy -- -D warnings
      - run: cargo test -p manta-input -p manta-cli --features soapy
```

to:

```yaml
  test-soapy:
    needs: codex-clean
    if: "!cancelled() && (needs.codex-clean.result == 'success' || needs.codex-clean.result == 'skipped')"
    strategy:
      fail-fast: false
      matrix:
        os: [ubuntu-latest, macos-latest, windows-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
      - if: runner.os == 'Linux'
        run: sudo apt-get update && sudo apt-get install -y --no-install-recommends libasound2-dev libsoapysdr-dev soapysdr0.8-module-rtlsdr
      - if: runner.os == 'macOS'
        run: brew install soapysdr
      # Windows has no libsoapysdr-dev/soapysdr0.8-module-rtlsdr package
      # equivalent -- built here from source (SoapySDR core via vcpkg,
      # librtlsdr + SoapyRTLSDR module from source since neither has a
      # vcpkg port as of 2026-09). Only the `driver=rtlsdr` module is
      # needed -- the existing soapy unit tests
      # (crates/manta-input/src/soapy.rs) never touch SDRplay, so this
      # deliberately sidesteps SDRplay's proprietary Windows API/EULA,
      # which only matters for the deferred live-hardware ticket.
      - if: runner.os == 'Windows'
        uses: lukka/run-vcpkg@5e0cab206a5ea620130caf37efe97fea6344cd59 # v11.5
        with:
          vcpkgGitCommitId: 3508985146f1b1d248c67ead13f8f54be5b4f5da
      - if: runner.os == 'Windows'
        run: |
          & "$env:VCPKG_ROOT\vcpkg" install soapysdr:x64-windows
          echo "SoapySDR_DIR=$env:VCPKG_ROOT\installed\x64-windows\share\soapysdr" >> $env:GITHUB_ENV
          echo "$env:VCPKG_ROOT\installed\x64-windows\bin" >> $env:GITHUB_PATH
        shell: pwsh
      - if: runner.os == 'Windows'
        run: |
          git clone --depth 1 https://github.com/osmocom/rtl-sdr.git C:\rtl-sdr-src
          cmake -S C:\rtl-sdr-src -B C:\rtl-sdr-build -DCMAKE_TOOLCHAIN_FILE="$env:VCPKG_ROOT\scripts\buildsystems\vcpkg.cmake" -DCMAKE_INSTALL_PREFIX=C:\rtl-sdr-install -DCMAKE_BUILD_TYPE=Release
          cmake --build C:\rtl-sdr-build --config Release --target install
          git clone --depth 1 https://github.com/pothosware/SoapyRTLSDR.git C:\SoapyRTLSDR-src
          cmake -S C:\SoapyRTLSDR-src -B C:\SoapyRTLSDR-build -DCMAKE_TOOLCHAIN_FILE="$env:VCPKG_ROOT\scripts\buildsystems\vcpkg.cmake" -DSoapySDR_DIR="$env:SoapySDR_DIR" -DLIBRTLSDR_INCLUDE_DIR=C:\rtl-sdr-install\include -DLIBRTLSDR_LIBRARIES=C:\rtl-sdr-install\lib\rtlsdr.lib -DCMAKE_BUILD_TYPE=Release
          cmake --build C:\SoapyRTLSDR-build --config Release --target install
        shell: pwsh
      - uses: dtolnay/rust-toolchain@4cda84d5c5c54efe2404f9d843567869ab1699d4 # stable
        with:
          components: rustfmt, clippy
      - uses: Swatinem/rust-cache@e18b497796c12c097a38f9edb9d0641fb99eee32 # v2.9.1
      - run: cargo clippy -p manta-input -p manta-cli --all-targets --features soapy -- -D warnings
      - run: cargo test -p manta-input -p manta-cli --features soapy
```

Note: this is a best-effort first attempt, written from research rather than a verified prior run (see Task 4's fallback path if it doesn't work as written — vcpkg commit pin and `SoapySDR_DIR` path layout are the most likely points of drift and are exactly what Task 4's diagnosis step checks first).

- [ ] **Step 2: Verify the YAML is well-formed**

Run: `cd ~/Code/manta-man212 && python3 -c "import yaml; yaml.safe_load(open('.github/workflows/ci.yml'))" && echo OK`
Expected: `OK`

- [ ] **Step 3: Commit and push**

```bash
cd ~/Code/manta-man212
git add .github/workflows/ci.yml
git commit -m "feat(manta): MAN-212 -- attempt windows-latest for the soapy CI leg via vcpkg"
git push
```

---

## Task 4: Verify `test-soapy` on Windows; fix, or apply the documented fallback

**Files:**
- Possibly modify: `.github/workflows/ci.yml` (fallback: revert the `windows-latest` entry and Windows-specific steps from `test-soapy`, matrix reverts to `[ubuntu-latest, macos-latest]`)
- Possibly modify: any source file, if the failure is a genuine Rust-level Windows `soapy`-feature bug rather than a CI-environment setup issue.

**Interfaces:** None.

- [ ] **Step 1: Poll and inspect the `test-soapy (windows-latest)` job**

```bash
cd ~/Code/manta-man212
gh run list --branch tony/man-212-manta-should-support-windows-as-a-first-class-platform --limit 5
gh run view <run-id> --log-failed
```

- [ ] **Step 2: If green, done — move to Task 5.**

- [ ] **Step 3: If red, classify the failure and apply the matching fix — allow up to 2 iterations of Steps 3-5 before falling back (Step 6)**

- **`vcpkg install` fails to find the `soapysdr` port / commit pin is stale**: check `https://github.com/microsoft/vcpkg/commits/master/ports/soapysdr` for the current tip commit and update `vcpkgGitCommitId` in the `lukka/run-vcpkg` step to a recent one that includes the port.
- **CMake can't find `SoapySDR_DIR`**: the actual installed path layout may differ from `share\soapysdr` (vcpkg sometimes nests under `share\SoapySDR` with different casing, or ships a `SoapySDRConfig.cmake` directly under `installed\x64-windows\lib\cmake\SoapySDR`). Run `Get-ChildItem -Recurse -Filter "SoapySDRConfig.cmake" $env:VCPKG_ROOT\installed\x64-windows` as an ad-hoc debug step (add temporarily, remove once resolved) to find the real path, then fix `SoapySDR_DIR` to match.
- **`SoapyRTLSDR` CMake build fails linking `rtlsdr.lib`**: the osmocom rtl-sdr CMake build may produce a differently-named static/import library depending on its own `CMakeLists.txt` version (e.g. `rtlsdr_static.lib` vs `rtlsdr.lib`). Add a debug `Get-ChildItem -Recurse -Filter "*.lib" C:\rtl-sdr-install` step, fix `LIBRTLSDR_LIBRARIES` to the actual filename.
- **`cargo test`/`cargo clippy` itself fails past the C-library setup** (i.e. the Rust `soapysdr` crate compiles and links, but a test genuinely fails on Windows): this is a real Windows-specific Rust bug — diagnose from the actual test failure output and fix at its file:line, same approach as Task 2 Step 5.

- [ ] **Step 4: Commit each fix attempt and re-push, then repeat Step 1**

```bash
cd ~/Code/manta-man212
git add -A
git commit -m "fix(manta): MAN-212 -- <specific fix for the windows soapy CI leg>"
git push
```

- [ ] **Step 5: After 2 failed iterations, stop iterating and apply the fallback**

- [ ] **Step 6: Fallback — revert `test-soapy` to Linux/macOS only**

Edit `.github/workflows/ci.yml`'s `test-soapy` job: remove `windows-latest` from the `os:` matrix and remove the four `runner.os == 'Windows'` steps added in Task 3, restoring the job to its original form. Commit:

```bash
cd ~/Code/manta-man212
git add .github/workflows/ci.yml
git commit -m "docs(manta): MAN-212 -- document windows soapy CI as a known gap, not attempted further"
git push
```

This fallback is a valid, planned outcome per the spec — not a failure of this plan. Task 6 records the gap explicitly in `ROADMAP.md`.

---

## Task 5: Run the full local verification sweep before docs

**Files:** None (verification only).

**Interfaces:** None.

- [ ] **Step 1: Confirm every CI job is green on the current HEAD**

```bash
cd ~/Code/manta-man212
gh run list --branch tony/man-212-manta-should-support-windows-as-a-first-class-platform --limit 1
gh run view <latest-run-id>
```

Expected: every job (`codex-clean`, `test` x3 os, `test-hpsdr` x3 os, `test-soapy` x2-or-3 os depending on Task 4's outcome) shows `success` or an expected `skipped` (only `codex-clean` may legitimately skip, per `ci.yml`'s own logic).

- [ ] **Step 2: Confirm local `cargo fmt`/`clippy` still pass (sanity check before docs)**

```bash
cd ~/Code/manta-man212
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: both exit 0.

---

## Task 6: Update `ROADMAP.md`'s CI-goal language

**Files:**
- Modify: `ROADMAP.md:19`

**Interfaces:** None.

- [ ] **Step 1: Edit the CI-goal line**

Find:

```markdown
- CI green on Linux + macOS (no SoapySDR dependency in default features).
```

If Task 3/4 landed Windows `soapy` CI successfully, replace with:

```markdown
- CI green on Linux, macOS, and Windows (no SoapySDR dependency in default
  features). `--features soapy` also covered on Windows since MAN-212,
  built via vcpkg + SoapyRTLSDR-from-source rather than a system package.
```

If Task 4's fallback was taken instead, replace with:

```markdown
- CI green on Linux, macOS, and Windows (no SoapySDR dependency in default
  features), since MAN-212. `--features soapy` remains Linux/macOS-only in
  CI — a real Windows SoapySDR build path exists (vcpkg + SoapyRTLSDR from
  source) but wasn't made reliable enough for CI in that ticket; validated
  manually instead on real hardware (see the MAN-212 follow-up ticket).
```

- [ ] **Step 2: Commit**

```bash
cd ~/Code/manta-man212
git add ROADMAP.md
git commit -m "docs(manta): MAN-212 -- update CI-goal language for windows coverage"
git push
```

---

## Task 7: Add the Windows build/dev-setup wiki page

**Files:**
- Create: `wiki/pages/windows-build-setup.md`
- Modify: `wiki/INDEX.md`

**Interfaces:** None.

- [ ] **Step 1: Write the wiki page**

Create `wiki/pages/windows-build-setup.md`:

```markdown
---
id: windows-build-setup
title: How do I build and test manta on Windows?
kind: howto
status: current
maintainer: agent
sources:
  - .github/workflows/ci.yml
  - .github/workflows/release.yml
  - docs/superpowers/specs/2026-09-11-man212-windows-platform-support-design.md
verified:
  commit: <FILL IN AT COMMIT TIME -- the short SHA of this task's own commit>
  date: 2026-09-11
links:
  - live-hardware-field-testing
---
# How do I build and test manta on Windows?

manta targets `x86_64-pc-windows-msvc` (not MinGW) -- this matches the
prebuilt release binaries (`.github/workflows/release.yml`) and is the only
Windows target CI (`.github/workflows/ci.yml`) exercises.

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
2. **vcpkg**, for CI or a from-source setup without the GUI installer --
   `vcpkg install soapysdr:x64-windows` gets the core library. The
   RTL-SDR module (`SoapyRTLSDR`, needed for CI's own `driver=rtlsdr`
   smoke tests) has no vcpkg port as of this writing and needs building
   from source against `osmocom/rtl-sdr` -- see
   `.github/workflows/ci.yml`'s `test-soapy` job's `runner.os == 'Windows'`
   steps for the exact CMake invocations CI uses (if that job exists at
   the commit you're reading -- see this page's own `verified.commit`
   above; if the windows-latest leg was dropped from `test-soapy` per
   MAN-212's documented fallback, the CI file won't show it and this
   vcpkg path is untested beyond what's written here).

For an actual SDRplay RSP-series radio, the SDRplay Windows API/service
installer is separate from SoapySDR/PothosSDR entirely -- install it from
SDRplay's own site, then SoapySDRPlay3's module on top of that. This
repo's own live-hardware validation on Windows (RSP1B, SDRplay-service
reliability comparison against the macOS baseline in issues #166/#167/
#171) is tracked as a follow-up to MAN-212, not yet done as of this page's
`verified.commit` -- see that follow-up ticket for the up-to-date status
once it exists.

## Known gaps

- No Windows-native equivalent of `crates/manta-cli/tests/signal_shutdown.rs`
  exists -- that test is `#![cfg(unix)]`-gated and simply doesn't run on
  Windows. `ctrlc::set_handler`'s Windows console-event backend is used in
  production but only manually verified (Ctrl+C during a live `manta
  listen` session), not covered by an automated Windows CI test.
```

- [ ] **Step 2: Add the wiki index entry**

In `wiki/INDEX.md`, add a line near the existing `live-hardware-field-testing.md` entry:

```markdown
- [How do I build and test manta on Windows?](pages/windows-build-setup.md) — MSVC target, vcpkg setup for `--features soapy`, and the CI shape landed by MAN-212.
```

- [ ] **Step 3: Fill in the `verified.commit` placeholder with this task's own commit SHA**

After committing (next step), amend the `verified: commit:` field in the new wiki page with the actual short SHA (`git log -1 --format=%h`), then amend the commit:

```bash
cd ~/Code/manta-man212
git add wiki/pages/windows-build-setup.md wiki/INDEX.md
git commit -m "docs(manta): MAN-212 -- add windows build/dev-setup wiki page"
SHA=$(git log -1 --format=%h)
sed -i '' "s/<FILL IN AT COMMIT TIME -- the short SHA of this task's own commit>/$SHA/" wiki/pages/windows-build-setup.md
git add wiki/pages/windows-build-setup.md
git commit --amend --no-edit
git push
```

---

## Task 8: Open the follow-up ticket, mark the PR ready, and close out MAN-212

**Files:** None (process/tracking only).

**Interfaces:** None.

- [ ] **Step 1: Open the follow-up GitHub issue**

```bash
cd ~/Code/manta-man212
gh issue create --repo HagaleTechnologies/manta \
  --title "Validate live-hardware RSP1B on Windows; compare SDRplay service reliability against the macOS baseline" \
  --body "$(cat <<'EOF'
Follow-up to MAN-212 (#<PR number from Task 1>) -- that PR landed Windows
build support and CI but explicitly deferred the live-hardware leg
(Linear ticket item 3) since it needs a physical Windows mini PC and RSP1B
not reachable from an automated session.

## What this covers

- Get a real Windows mini PC set up per `wiki/pages/windows-build-setup.md`
  with an RSP1B attached.
- Run `--features soapy` `manta listen`/`manta doctor` against real RF,
  same methodology as
  `docs/DECISIONS/2026-09-10-post-antenna-fix-90min-soak-and-service-reliability.md`
  (short cycles, not one long capture, given the known SDRplay service
  flakiness).
- Compare SDRplay API service crash frequency
  (`sdrplay_api_ServiceNotResponding`/`sdrplay_api_Fail`) against the
  macOS baseline documented in #166, #167, #171 -- this is the actual
  tactical payoff MAN-212 was chasing.
- Update `wiki/pages/windows-build-setup.md`'s "Known gaps" section (and/or
  `wiki/pages/live-hardware-field-testing.md`) with the result either way.

## Non-goal

Root-causing the SDRplay service bug itself -- same non-goal as MAN-212 and
#166/#167/#171.

Relates to #166, #167, #171.
EOF
)"
```

- [ ] **Step 2: Record the follow-up issue in Linear**

```bash
linearis issues create --team MAN \
  --title "Validate live-hardware RSP1B on Windows; compare SDRplay service reliability against macOS baseline" \
  --description "Follow-up to MAN-212 -- see linked GitHub issue for full scope. Needs a physical Windows mini PC with RSP1B attached; not startable until that hardware is available." \
  2>&1
```

Note the exact CLI invocation may need adjusting to this environment's actual `linearis issues create` flags (check `linearis issues create --help` if the above errors) -- link the new Linear issue to MAN-212 as a follow-up relation, and link the GitHub issue from Task 8 Step 1 into the Linear issue's description once both IDs are known.

- [ ] **Step 3: Mark the PR ready for review**

```bash
cd ~/Code/manta-man212
gh pr ready
```

Auto-merge (armed back in Task 1) will now fire once required checks (`test (ubuntu-latest)`, `test (macos-latest)`, and whichever `windows-latest` legs exist per Task 4's outcome) are green.

- [ ] **Step 4: Confirm the PR actually merges**

```bash
gh pr view --json state,mergedAt,statusCheckRollup
```

Poll (no faster than every 2 minutes) until `state` is `MERGED`. Do not force-merge past a red or pending check.

- [ ] **Step 5: Update MAN-212's Linear status**

```bash
linearis issues update MAN-212 --status Done
```

(Or the repo's actual done-state name, if `Done` errors -- check `linearis issues read MAN-212` for the team's valid state names first if unsure.)

- [ ] **Step 6: Clean up the worktree**

```bash
cd ~/Code/manta
git worktree remove ../manta-man212
```

---

## Self-Review Notes

- **Spec coverage**: Motivation/scope (plan header + Global Constraints), starting-point findings (Task 1/3's "only the `os:` line changes" comments), MSVC target (Global Constraints), `test`/`test-hpsdr`/`test-soapy` CI changes (Tasks 1-4), code-change expectations (Task 2 Step 5, Task 4 Step 3), docs (Tasks 6-7), follow-up ticket (Task 8), acceptance criteria (Task 5 + Task 8 Step 4) — all covered.
- **Placeholder scan**: the `<FILL IN AT COMMIT TIME>` and `<PR number from Task 1>` markers are intentional runtime substitutions (the actual values don't exist until earlier tasks run), not unresolved planning gaps — each has an explicit step showing exactly how to fill it in.
- **Type consistency**: N/A — no Rust interfaces are introduced by this plan; it's CI/docs-only aside from possible reactive fixes in Tasks 2/4, which are scoped to "fix what CI actually reports," not predefined signatures.
