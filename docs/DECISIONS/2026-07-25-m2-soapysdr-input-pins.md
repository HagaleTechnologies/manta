# M2 SoapySDR input implementation pins

This is the M2 "SoapySDR input" sub-project's
(`docs/superpowers/plans/2026-07-25-m2-soapysdr-input.md`, design:
`docs/superpowers/specs/2026-07-25-m2-soapysdr-input-design.md`)
implementation's pinned-decision record. Treat every numbered item below as
decided; SPEC and docs/ still win on anything not listed here.

## Deviations and pinned decisions

1. **`soapysdr = "0.5"` chosen** — the standard high-level Rust wrapper crate
   for SoapySDR, matching ARCHITECTURE.md §3's reference. `Complex32` (this
   workspace's IQ sample type, from `coppa-dsp`) maps directly onto
   SoapySDR's native `CF32` stream format; `SoapySdrIqSource`
   (`crates/manta-input/src/soapy.rs`, feature-gated `soapy`) reads
   straight into `Complex32` buffers with no intermediate conversion layer.

2. **No RF hardware was available anywhere in this environment.** Real
   testing is limited to two confirmed hardware-free error paths against the
   real native SoapySDR library (installed via `brew install soapysdr
   soapyrtlsdr pkg-config`), both in `crates/manta-input/src/soapy.rs`'s
   `tests` module:
   - `open_surfaces_device_not_found_as_a_clean_error` — `open("driver=rtlsdr",
     ...)` with no RTL-SDR hardware attached fails cleanly at `Device::new()`.
   - `open_succeeds_against_the_null_device` — `open("type=null", ...)`,
     SoapySDR's built-in null device, succeeds all the way through the
     entire `open()` sequence (device open, set rate/freq, gain-mode check,
     query-back reads, `rx_stream()`, `activate()`). This was **not** the
     original assumption: an earlier, simpler spike (without the
     gain-mode/query-back calls) got a `NotSupported` error at `rx_stream()`
     instead. The design spec was corrected twice during implementation to
     reflect the real, empirically-confirmed behavior — not guessed — worth
     recording as a concrete instance of this project's "measure, don't
     assume" discipline.
   - `read_surfaces_not_supported_as_a_clean_error_on_the_null_device` —
     calling `.read(&mut buf)` on that opened `type=null` source fails with
     a real `NotSupported` error: genuine, hardware-free coverage of
     `IqSource::read()`'s error path, which the original design believed
     was untestable without real hardware.

   The actual streaming/`read()`-returns-real-samples/decode-accuracy path
   against a live RF source is genuinely **untested** and is an outstanding
   manual step — same pattern as the M2 CPU-budget bench's Raspberry Pi 4
   leg and M1's still-outstanding W1AW live-copy run. Do not treat this
   sub-project as validating that manta can decode real over-the-air CW
   via SoapySDR; it only validates that the crate compiles, links against
   the real native library, and handles its two confirmed error paths
   cleanly.

3. **Found and fixed a real, pre-existing bug in `manta_engine::listen()`:
   `center_freq_hz` was hardcoded to `0.0`** when constructing the
   channelizer/track-manager instead of reading `src.center_freq_hz()` from
   the actual `IqSource`. Harmless while only `AudioIqSource` (always
   reports `0.0`) fed `listen()`, but would have silently produced wrong
   absolute spot frequencies once `SoapySdrIqSource` (a real nonzero RF
   center frequency) could feed it. Independently confirmed and fixed;
   proven by a new regression test,
   `listen_uses_the_sources_center_freq_hz_not_a_hardcoded_zero`
   (`crates/manta-engine/src/listen.rs`), using a minimal test-only
   `IqSource` that reports a nonzero center frequency and asserting that
   emitted `TrackMeta.freq_hz` events land near the true RF frequency, not
   near a baseband-only offset.

4. **`manta_engine::listen()`/`soak()` signature changed from a concrete
   `AudioIqSource` parameter to `Box<dyn IqSource>`** — the trait is fully
   dyn-compatible. This is a real, if small, engine-level API change. It is
   a deviation from the original design brainstorm, which initially scoped
   this sub-project as crate-level-only (`manta-input::soapy`, deferring
   CLI/engine wiring to a later sub-project). Tony explicitly chose to
   include the engine generalization and CLI wiring in this same plan when
   asked during brainstorming — this is why this PR's diff spans three
   crates (`manta-input`, `manta-engine`, `manta-cli`), not just
   `manta-input`.

5. **New CLI flags `--soapy-driver`/`--soapy-freq`/`--soapy-rate`/
   `--soapy-gain`** on `manta listen`/`manta soak`, entirely absent
   (don't even appear in `--help`) from a non-`soapy` build via
   `#[cfg(feature = "soapy")]`. Unlike file/audio sources, which read their
   own metadata, no sensible default center frequency or sample rate exists
   for an arbitrary SoapySDR driver, so `--soapy-freq`/`--soapy-rate` are
   required together with `--soapy-driver` — enforced in
   `crates/manta-cli`'s option-resolution code via a clean `Err` with an
   informative message (`"--soapy-freq is required with --soapy-driver"` /
   the `--soapy-rate` equivalent), not a panic or a silent default.

6. **New, separate `test-soapy` CI job** (`.github/workflows/ci.yml`,
   matrix: `ubuntu-latest`, `macos-latest`) installs the native library
   (`apt-get install libsoapysdr-dev` on Linux, `brew install soapysdr` on
   macOS) and runs `cargo clippy -p manta-input -p manta-cli --all-targets
   --features soapy -- -D warnings` and `cargo test -p manta-input -p
   manta-cli --features soapy`. It is deliberately a separate job, not
   folded into the existing default `test` job: ROADMAP.md requires the
   default build to have zero SoapySDR footprint, and the default `test` job
   is untouched — no native SoapySDR dependency, no `soapy` feature flag,
   verified via `cargo build`/`cargo clippy` with no feature flag showing no
   reference to the `soapysdr` crate at all.

7. **`test-soapy (ubuntu-latest)` crashed with `Hash collision!!! Fatal
   error!!`, root-caused from the live CI log (no local repro needed).**
   Two compounding causes, both fixed:
   - `apt-get install libsoapysdr-dev` without `--no-install-recommends`
     transitively installs `soapysdr0.8-module-all` — every SoapySDR
     hardware/network module (audio/RtAudio, remote/avahi, uhd, bladerf,
     hackrf, lms7, mirisdr, osmosdr, redpitaya, rfspace, airspy), not just
     the `rtlsdr` module this crate needs. Confirmed directly in the apt
     log's package list.
   - SoapySDR lazily initializes its plugin registry (loading and running
     every installed module's registration code) on the *first*
     `Device::new()` call in a process. `cargo test`'s default thread-per-
     test concurrency (no `--test-threads=1` set anywhere in this repo)
     lets `manta-input::soapy::tests`' three tests race each other into
     that one-time init from separate threads; the fatal message is the
     signature of a hash table corrupted by concurrent unsynchronized
     writes, not a real collision, and not manta's own code. It fired
     specifically on the third `Device::new()` call in the log, consistent
     with a race needing enough concurrent traffic to trip.
   - Fix: `.github/workflows/ci.yml`'s `test-soapy` Linux step now installs
     `--no-install-recommends libasound2-dev libsoapysdr-dev
     soapysdr0.8-module-rtlsdr` (removes the unneeded/racy modules
     entirely). Independently, `crates/manta-input/src/soapy.rs`'s three
     `Device::new()`-calling tests now serialize through a
     `static SOAPY_TEST_LOCK: Mutex<()>` — cheap insurance against the same
     class of bug if module packaging changes again, since nothing
     guarantees SoapySDR's registry init is thread-safe even for modules we
     do want.
