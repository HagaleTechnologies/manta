# M2 remaining sub-project 2: SoapySDR input

Status: approved
Date: 2026-07-25

## Purpose

M2's remaining sub-projects (ROADMAP.md) are: SoapySDR input, KiwiSDR input. This
spec covers the first: ARCHITECTURE.md §3's SoapySDR `IqSource` (RTL-SDR, Airspy
HF+, SDRplay via the `soapysdr` crate, feature-gated `soapy`), plus the engine/CLI
wiring needed to actually drive `skimmer listen`/`skimmer soak` from a real SDR.

## Environment finding (changes the risk profile from "blind code")

No RF hardware is available in this environment, but the native SoapySDR C
library *is* now installed here (`brew install soapysdr soapyrtlsdr pkg-config`),
confirmed via a real spike:
- `soapysdr = "0.5"` (crate) compiles and links against the installed library.
- `soapysdr::Device::new("driver=rtlsdr")` with no hardware attached returns a
  real, catchable error (`Other: No RTL-SDR devices found!`), not a panic or
  build failure.
- `num_complex::Complex<f32>` (skimmer's `Complex32`) directly implements the
  crate's `StreamSample` trait as `Format::CF32` — no sample-format conversion
  layer needed between SoapySDR and `IqSource`.

So this can be real, compile-and-unit-test-verified code (device-not-found error
handling is a genuine, testable path), not code written blind against docs. Real
over-the-air validation with actual hardware remains an outstanding manual step —
same pattern as the Pi4 CPU-budget leg and M1's still-outstanding W1AW live-copy
run.

## Scope

1. **`skimmer-input::soapy::SoapySdrIqSource`** — the crate-level `IqSource` impl.
2. **Engine generalization** — `skimmer_engine::listen`/`soak` are currently
   hard-coded to `AudioIqSource`; generalize both to accept `Box<dyn IqSource>`
   (the trait is already object-safe: no generics, no `Self`-returning methods)
   so the CLI can select among audio/file/SoapySDR sources at runtime.
3. **CLI wiring** — new `--soapy-driver`/`--soapy-freq`/`--soapy-rate`/
   `--soapy-gain` flags on `listen`/`soak`, so `skimmer listen --soapy-driver
   "driver=rtlsdr" --soapy-freq 14025000 --soapy-rate 96000` actually works
   end-to-end against real hardware (untested here, but the code path is real).
4. **CI** — a new, separate job building/testing `--features soapy` on both
   `ubuntu-latest` (`apt-get install libsoapysdr-dev`) and `macos-latest`
   (`brew install soapysdr`), isolated from the existing default job so the
   default job stays exactly as fast/dependency-free as ROADMAP requires
   ("CI green on Linux + macOS (no SoapySDR dependency in default features)").

KiwiSDR input is explicitly out of scope — separate sub-project, separate spec.

## `SoapySdrIqSource`

`crates/skimmer-input/src/soapy.rs`, gated `#[cfg(feature = "soapy")]` at the
module level in `lib.rs` (`#[cfg(feature = "soapy")] pub mod soapy;`).

```rust
pub struct SoapySdrIqSource {
    stream: soapysdr::RxStream<num_complex::Complex32>,
    fs: f64,
    center_freq_hz: f64,
}

impl SoapySdrIqSource {
    pub fn open(
        driver_args: &str,
        fs: f64,
        center_freq_hz: f64,
        gain_db: Option<f64>,
    ) -> anyhow::Result<Self> { ... }
}

impl IqSource for SoapySdrIqSource {
    fn sample_rate(&self) -> f64 { self.fs }
    fn center_freq_hz(&self) -> f64 { self.center_freq_hz }
    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> { ... }
}
```

`open()`:
1. `soapysdr::Device::new(driver_args)` — surfaces "no device" as a normal
   `Err`, not a panic (confirmed above).
2. `device.set_sample_rate(Rx, 0, fs)`, `device.set_frequency(Rx, 0,
   center_freq_hz, ())`.
3. Gain: `Some(db)` → `set_gain(Rx, 0, db)`; `None` → if
   `device.has_gain_mode(Rx, 0)?` is true, enable AGC via
   `set_gain_mode(Rx, 0, true)`; otherwise leave the driver's default.
4. Query back the *actual* negotiated `sample_rate`/`frequency` from the device
   (SDRs commonly round to the nearest achievable value) and store those, not
   the requested values — matches this codebase's existing honesty-in-metadata
   convention (`Track::freq_hz` reports live truth, not a requested/nominal
   value).
5. `device.rx_stream::<Complex32>(&[0])`, `stream.activate(None)`.

`read()` wraps `RxStream::read(&mut [buf], TIMEOUT_US)` (single-channel: one
one-element `buffers` slice), converting `soapysdr::Error` to `anyhow::Error`
via `?` (the crate's `Error` implements `std::error::Error`). `TIMEOUT_US =
100_000` (100 ms) — short enough that `listen`'s stop-signal check (polled
between `read()` calls, matching the existing `AudioIqSource`/`ctrlc` pattern
in `main.rs`) stays responsive, long enough not to busy-loop on an idle
stream.

No manual `Drop` needed — `RxStream`'s own `Drop` already deactivates and
closes the stream.

## Engine generalization

`crates/skimmer-engine/src/listen.rs` and `soak.rs`: change

```rust
pub fn listen(mut src: AudioIqSource, ...) -> Result<()>
pub fn soak(src: AudioIqSource, ...) -> Result<SoakReport>
```

to

```rust
pub fn listen(mut src: Box<dyn IqSource>, ...) -> Result<()>
pub fn soak(src: Box<dyn IqSource>, ...) -> Result<SoakReport>
```

`IqSource` (defined in `skimmer-input`) is already dyn-compatible; this is a
signature-only change, no behavioral change to either function's body beyond
`src.read(...)`/`src.sample_rate()` calls working identically through the trait
object.

## CLI wiring

`crates/skimmer-cli/src/main.rs`'s `Command::Listen`/`Command::Soak`: add, gated
`#[cfg(feature = "soapy")]` on the fields themselves (so `--help` on a
non-`soapy` build doesn't show flags that can't work):

```rust
#[cfg(feature = "soapy")]
#[arg(long, conflicts_with_all = ["device", "source"])]
soapy_driver: Option<String>,
#[cfg(feature = "soapy")]
#[arg(long, requires = "soapy_driver")]
soapy_freq: Option<f64>,
#[cfg(feature = "soapy")]
#[arg(long, requires = "soapy_driver")]
soapy_rate: Option<f64>,
#[cfg(feature = "soapy")]
#[arg(long, requires = "soapy_driver")]
soapy_gain: Option<f64>,
```

`soapy_freq`/`soapy_rate` are required (via clap validation, not `Option`
unwrapping) when `soapy_driver` is set — no sensible default center frequency
or sample rate exists the way it does for file/audio sources reading their own
metadata. Dispatch: box whichever source was selected as `Box<dyn IqSource>`
before calling the (now-generalized) `skimmer_engine::listen`/`soak`.

`skimmer-cli/Cargo.toml` gets its own `soapy` feature forwarding:
```toml
[features]
soapy = ["skimmer-input/soapy"]
```

## Testing

No hardware, so tests focus on what's genuinely verifiable:
- `SoapySdrIqSource::open("driver=rtlsdr", ...)` with no device attached
  returns `Err`, not a panic (real, confirmed-reproducible path on this
  machine).
- Any additional coverage the implementer finds achievable via
  `soapysdr::Device::null_device()` (a no-op software device the crate
  provides) — if `null_device()` supports enough of the `Device` API to open
  a stream and get zero-filled reads, that's real io-path coverage without
  hardware; if it doesn't support streaming, this is a documented limitation,
  not a gap to force-fix.
- Existing `IqSource` conformance is implicit (the trait itself has no
  separate conformance test suite today — matches `AudioIqSource`/
  `WavIqSource`'s existing pattern of just implementing + directly testing the
  concrete type).
- `cargo build`/`cargo test -p skimmer-input -p skimmer-cli --features soapy`
  must succeed locally (confirmed achievable — native lib now installed) and
  in the new CI job.

## CI

New job in `.github/workflows/ci.yml`, matrix `[ubuntu-latest, macos-latest]`,
separate from the existing `test` job:
- Linux: `sudo apt-get install -y libsoapysdr-dev`
- macOS: `brew install soapysdr`
- `cargo build -p skimmer-input -p skimmer-cli --features soapy`
- `cargo test -p skimmer-input -p skimmer-cli --features soapy`
- `cargo clippy -p skimmer-input -p skimmer-cli --features soapy --all-targets
  -- -D warnings`

Does not touch the existing default `test` job — ROADMAP's "no SoapySDR
dependency in default features" constraint is about the *default* build/CI
path, which this job doesn't alter.
