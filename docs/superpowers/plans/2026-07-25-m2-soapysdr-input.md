# M2 SoapySDR Input Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement ARCHITECTURE.md §3's SoapySDR `IqSource` (RTL-SDR/Airspy HF+/SDRplay via the `soapysdr` crate), generalize `skimmer_engine::listen`/`soak` to accept any `IqSource`, wire it into the CLI, and add CI coverage — the next M2 remaining sub-project after V8/V8w pileup validation.

**Architecture:** `skimmer-input` gains a `soapy` feature-gated module wrapping `soapysdr::Device`/`RxStream<Complex32>` directly (no format conversion needed — `Complex32` is SoapySDR's native `CF32`). `skimmer-engine`'s `listen`/`soak` change from a concrete `AudioIqSource` parameter to `Box<dyn IqSource>` (already dyn-compatible) so the CLI can select among file/audio/SoapySDR sources at runtime. `skimmer-cli` gets new `--soapy-*` flags, feature-forwarded.

**Tech Stack:** Rust, `soapysdr = "0.5"` (new optional dependency), existing `skimmer-input`/`skimmer-engine`/`skimmer-cli` crates.

## Global Constraints

- ROADMAP.md: "CI green on Linux + macOS (no SoapySDR dependency in default features)" — the `soapy` feature must be opt-in via `--features soapy`, never enabled by default, and the existing default `test` CI job must be untouched.
- ARCHITECTURE.md §3: `soapysdr` crate, feature-gated `soapy`. `IqSource` trait: `sample_rate()`, `center_freq_hz()`, `read(&mut [Complex32]) -> Result<usize>`.
- No RF hardware available anywhere in this environment. Two real, hardware-free error paths were confirmed via spike and must be covered by tests: (1) `driver=rtlsdr` with no device attached — `Device::new()` fails; (2) `type=null` — opens but `rx_stream()` fails with `NotSupported`. The actual `read()`/streaming path has no hardware-free coverage available — do not fake it with a mock; document the gap instead.
- `Complex32` (`num_complex::Complex<f32>`) is SoapySDR's native `CF32` stream format (confirmed: `unsafe impl StreamSample for Complex<f32>`) — no conversion layer.
- Real, pre-existing bug in scope for this plan: `crates/skimmer-engine/src/listen.rs` hardcodes `center_freq_hz = 0.0` in both `Channelizer::new(fs, 0.0)` and `TrackManager::new(.., 0.0, ..)` instead of reading `src.center_freq_hz()`. Fix this in Task 2 (the engine-generalization task) — it's harmless today (only `AudioIqSource`, always `0.0`, has ever fed `listen()`) but would silently produce wrong absolute spot frequencies once `SoapySdrIqSource` (real nonzero RF center frequency) can feed it.
- Full spec: `docs/superpowers/specs/2026-07-25-m2-soapysdr-input-design.md`.
- This repo's CI (`.github/workflows/ci.yml`) SHA-pins third-party actions (see existing `actions/checkout@34e114...`, `dtolnay/rust-toolchain@4cda84...`, `Swatinem/rust-cache@42dc69...`) — any new job reuses these exact same pinned actions, doesn't introduce new unpinned ones.

---

### Task 1: `SoapySdrIqSource`

**Files:**
- Modify: `Cargo.toml` (workspace root — add `soapysdr` to `[workspace.dependencies]`)
- Modify: `crates/skimmer-input/Cargo.toml` (add optional `soapysdr` dependency + `soapy` feature)
- Modify: `crates/skimmer-input/src/lib.rs` (add `#[cfg(feature = "soapy")] pub mod soapy;`)
- Create: `crates/skimmer-input/src/soapy.rs`

**Interfaces:**
- Consumes: `skimmer_input::IqSource` (existing trait, this crate).
- Produces: `pub struct SoapySdrIqSource` implementing `IqSource`, with `pub fn open(driver_args: &str, fs: f64, center_freq_hz: f64, gain_db: Option<f64>) -> anyhow::Result<Self>`. Used by Task 3 (CLI wiring).

- [ ] **Step 1: Add the dependency**

In `Cargo.toml` (workspace root), add to `[workspace.dependencies]`:

```toml
soapysdr = "0.5"
```

In `crates/skimmer-input/Cargo.toml`, change `[dependencies]`'s `hound = { workspace = true }` line area to add (keep alphabetical-ish ordering, next to the other deps) and add a `[features]` section:

```toml
[dependencies]
anyhow = { workspace = true }
coppa-audio = { workspace = true }
cpal = { workspace = true }
hound = { workspace = true }
num-complex = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
skimmer-dsp = { workspace = true }
soapysdr = { workspace = true, optional = true }

[features]
soapy = ["dep:soapysdr"]
```

- [ ] **Step 2: Write the failing tests**

Create `crates/skimmer-input/src/soapy.rs`:

```rust
//! SoapySDR IQ source (RTL-SDR, Airspy HF+, SDRplay). ARCHITECTURE §3.
//! Feature-gated `soapy` — the native SoapySDR C library is not required to
//! build without this feature (ROADMAP.md: "CI green on Linux + macOS (no
//! SoapySDR dependency in default features)").

use crate::IqSource;
use anyhow::Result;
use num_complex::Complex32;
use soapysdr::Direction::Rx;

/// A live SoapySDR device (RTL-SDR/Airspy HF+/SDRplay/...) as an `IqSource`.
/// ARCHITECTURE §3.
pub struct SoapySdrIqSource {
    stream: soapysdr::RxStream<Complex32>,
    fs: f64,
    center_freq_hz: f64,
}

/// Read timeout, microseconds. Short enough that a caller polling a stop
/// signal between `read()` calls (matching `AudioIqSource`/`ctrlc`'s pattern
/// in `skimmer-cli`) stays responsive; long enough not to busy-loop on an
/// idle stream.
const TIMEOUT_US: i64 = 100_000;

impl SoapySdrIqSource {
    /// Open `driver_args` (e.g. `"driver=rtlsdr"`), tune to `fs`/
    /// `center_freq_hz`, set `gain_db` (or enable AGC if `None` and the
    /// device supports gain mode), and activate an RX stream on channel 0.
    /// Every step's error (device not found, unsupported operation, etc.)
    /// propagates as a normal `Err`, never a panic.
    pub fn open(
        driver_args: &str,
        fs: f64,
        center_freq_hz: f64,
        gain_db: Option<f64>,
    ) -> Result<Self> {
        let device = soapysdr::Device::new(driver_args)?;
        device.set_sample_rate(Rx, 0, fs)?;
        device.set_frequency(Rx, 0, center_freq_hz, ())?;
        match gain_db {
            Some(db) => device.set_gain(Rx, 0, db)?,
            None => {
                if device.has_gain_mode(Rx, 0)? {
                    device.set_gain_mode(Rx, 0, true)?;
                }
            }
        }
        // Query back the actual negotiated values -- SDRs commonly round to
        // the nearest achievable rate/frequency; report truth, not the ask
        // (same convention as `Track::freq_hz`'s live-centroid reporting).
        let actual_fs = device.sample_rate(Rx, 0)?;
        let actual_freq = device.frequency(Rx, 0)?;
        let mut stream = device.rx_stream::<Complex32>(&[0])?;
        stream.activate(None)?;
        Ok(SoapySdrIqSource {
            stream,
            fs: actual_fs,
            center_freq_hz: actual_freq,
        })
    }
}

impl IqSource for SoapySdrIqSource {
    fn sample_rate(&self) -> f64 {
        self.fs
    }

    fn center_freq_hz(&self) -> f64 {
        self.center_freq_hz
    }

    fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
        let n = self.stream.read(&mut [buf], TIMEOUT_US)?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_surfaces_device_not_found_as_a_clean_error() {
        // No RTL-SDR hardware is attached in CI or this dev environment --
        // Device::new() itself must fail, not panic.
        let result = SoapySdrIqSource::open("driver=rtlsdr", 96_000.0, 14_025_000.0, None);
        assert!(
            result.is_err(),
            "expected an Err with no RTL-SDR hardware attached"
        );
    }

    #[test]
    fn open_surfaces_stream_not_supported_as_a_clean_error() {
        // SoapySDR's built-in `type=null` device opens successfully with no
        // hardware and no extra module install, but does not support RX
        // streaming -- open() must still return Err cleanly (confirms error
        // propagation past device construction, through to rx_stream()).
        let result = SoapySdrIqSource::open("type=null", 96_000.0, 14_025_000.0, None);
        assert!(
            result.is_err(),
            "expected an Err from type=null (no RX streaming support)"
        );
    }
}
```

- [ ] **Step 3: Wire the module into the crate**

In `crates/skimmer-input/src/lib.rs`, add after the existing `pub mod audio;`/`pub use audio::...` lines:

```rust
#[cfg(feature = "soapy")]
pub mod soapy;
#[cfg(feature = "soapy")]
pub use soapy::SoapySdrIqSource;
```

- [ ] **Step 4: Run the tests to verify they fail, then implement, then pass**

Run: `cargo test -p skimmer-input --features soapy soapy:: -- --nocapture`
Expected first (before Step 2's implementation code exists — if you're following strict TDD, write the tests against a stub `open()` that does `todo!()` first): FAIL to compile or `todo!()` panic.

After implementing per Step 2's full code: run the same command again.
Expected: both tests pass. Both are REAL runs against the real, installed SoapySDR library (confirmed installed on this machine via `brew install soapysdr soapyrtlsdr pkg-config`) — not mocked.

- [ ] **Step 5: Verify the default (non-soapy) build is untouched**

Run: `cargo build -p skimmer-input` (no `--features soapy`) and `cargo clippy -p skimmer-input --all-targets -- -D warnings` (no `--features soapy`).
Expected: both succeed with no reference to `soapysdr` at all — confirms the feature gate actually keeps the native dependency out of the default build path.

- [ ] **Step 6: Run clippy/fmt with the feature enabled**

Run: `cargo clippy -p skimmer-input --all-targets --features soapy -- -D warnings` and `cargo fmt -p skimmer-input --check`.
Expected: both clean.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml crates/skimmer-input/Cargo.toml crates/skimmer-input/src/lib.rs crates/skimmer-input/src/soapy.rs
git commit -m "feat(input): SoapySDR IqSource behind --features soapy"
```

---

### Task 2: Engine generalization (`Box<dyn IqSource>`) + `center_freq_hz` fix

**Files:**
- Modify: `crates/skimmer-engine/src/listen.rs`
- Modify: `crates/skimmer-engine/src/soak.rs`
- Modify: `crates/skimmer-engine/tests/listen_audio.rs` (existing call site)
- Modify: `crates/skimmer-cli/tests/soak_ci.rs` (existing call site)
- Modify: `crates/skimmer-cli/src/main.rs` (existing call sites — kept compiling; NEW `--soapy-*` flags are Task 3's job, not this one)

**Interfaces:**
- Consumes: `skimmer_input::IqSource` (dyn-compatible, confirmed: no generics, no `Self`-returning methods).
- Produces: `pub fn listen(mut src: Box<dyn IqSource>, cfg: &PipelineConfig, stop: Arc<AtomicBool>, on_event: impl FnMut(&DecoderEvent)) -> Result<()>`, `pub fn soak(src: Box<dyn IqSource>, cfg: &PipelineConfig, duration: Duration) -> Result<SoakReport>`. Used by Task 3.

- [ ] **Step 1: Write/update the failing tests**

In `crates/skimmer-engine/src/soak.rs`'s existing `#[cfg(test)] mod tests` block, add an explicit `use skimmer_input::AudioIqSource;` (the test needs it directly now — Step 3 below removes `soak.rs`'s top-level `use skimmer_input::AudioIqSource;`, since the non-test body no longer references the concrete type, only `use super::*;` won't bring it back):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use skimmer_input::AudioIqSource;

    #[test]
    fn soak_reports_no_panic_on_a_clean_short_signal() {
        let fs = skimmer_input::TARGET_RATE_HZ;
        let spec = skimmer_testkit::keyer::KeyerSpec::new(20.0);
        let (env, _) =
            skimmer_testkit::keyer::key_text_loop("CQ CQ DE W1AW W1AW K", &spec, fs as f64, 8.0)
                .unwrap();
        let mut real = vec![0.0f32; env.len()];
        let dphi = std::f64::consts::TAU * 700.0 / fs as f64;
        let mut phi = 0.0f64;
        for (i, r) in real.iter_mut().enumerate() {
            *r = env.get(i).copied().unwrap_or(0.0) * phi.cos() as f32;
            phi += dphi;
        }
        let src: Box<dyn skimmer_input::IqSource> = Box::new(
            AudioIqSource::new(Box::new(coppa_audio::WavSource::from_samples(real, fs))).unwrap(),
        );
        let report = soak(src, &PipelineConfig::default(), Duration::from_secs(1)).unwrap();
        assert!(!report.panicked);
        assert!(soak_passed(&report));
    }
}
```

This replaces `soak.rs`'s entire existing `#[cfg(test)] mod tests { ... }` block (it currently has exactly this one test and nothing else) — not an insertion alongside other content.

Also add a new test to `crates/skimmer-engine/src/listen.rs`'s (currently nonexistent) `#[cfg(test)] mod tests` at the end of the file, proving the `center_freq_hz` fix with a minimal test-only `IqSource` that reports a nonzero center frequency:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal in-memory IqSource for testing, reporting a fixed,
    /// caller-chosen `center_freq_hz` (unlike `AudioIqSource`, which always
    /// reports 0.0) -- this is what proves `listen()` actually reads
    /// `src.center_freq_hz()` instead of hardcoding 0.0.
    struct FixedFreqSource {
        samples: Vec<Complex32>,
        cursor: usize,
        fs: f64,
        center_freq_hz: f64,
    }

    impl skimmer_input::IqSource for FixedFreqSource {
        fn sample_rate(&self) -> f64 {
            self.fs
        }
        fn center_freq_hz(&self) -> f64 {
            self.center_freq_hz
        }
        fn read(&mut self, buf: &mut [Complex32]) -> Result<usize> {
            let n = buf.len().min(self.samples.len() - self.cursor);
            buf[..n].copy_from_slice(&self.samples[self.cursor..self.cursor + n]);
            self.cursor += n;
            Ok(n)
        }
    }

    #[test]
    fn listen_uses_the_sources_center_freq_hz_not_a_hardcoded_zero() {
        // A real V1-style golden signal (clean +20 dB tone), but fed through
        // a source that reports a nonzero center_freq_hz -- if listen() were
        // still hardcoding 0.0, every TrackMeta.freq_hz would come back as
        // just the +12.34 kHz baseband offset, not centered on 14 MHz.
        let spec = skimmer_testkit::vectors::v1();
        let rendered = skimmer_testkit::vectors::render(&spec).unwrap();
        let src: Box<dyn skimmer_input::IqSource> = Box::new(FixedFreqSource {
            samples: rendered.samples,
            cursor: 0,
            fs: spec.fs,
            center_freq_hz: spec.center_freq_hz,
        });

        let stop = Arc::new(AtomicBool::new(false));
        let mut last_freq_hz = None;
        listen(src, &PipelineConfig::default(), stop, |ev| {
            if let DecoderEvent::TrackMeta { freq_hz, .. } = ev {
                last_freq_hz = Some(*freq_hz);
            }
        })
        .unwrap();

        let freq_hz = last_freq_hz.expect("expected at least one TrackMeta event");
        assert!(
            (freq_hz - (spec.center_freq_hz + 12_340.0)).abs() < 100.0,
            "freq_hz {freq_hz} should be near {} (center_freq_hz + V1's known offset), not near 12340 \
             (which is what a hardcoded center_freq_hz=0.0 would produce)",
            spec.center_freq_hz + 12_340.0
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p skimmer-engine --lib` and `cargo test -p skimmer-cli --test soak_ci` (this second one will fail to *compile* right now, since Step 1 already changed its source to a boxed type but `soak()`'s signature hasn't changed yet).
Expected: compile errors (signature mismatch) — confirms the tests are actually exercising the not-yet-changed signatures.

- [ ] **Step 3: Change the signatures and fix the `center_freq_hz` bug**

In `crates/skimmer-engine/src/listen.rs`, change the imports and signature:

```rust
use skimmer_input::IqSource;
```
(replace the existing `use skimmer_input::{AudioIqSource, IqSource};` — `AudioIqSource` is no longer referenced directly in this file)

```rust
pub fn listen(
    mut src: Box<dyn IqSource>,
    cfg: &PipelineConfig,
    stop: Arc<AtomicBool>,
    mut on_event: impl FnMut(&DecoderEvent),
) -> Result<()> {
    let fs = src.sample_rate();
    let center_freq_hz = src.center_freq_hz();
```

and change both constructor calls below it from the literal `0.0` to `center_freq_hz`:

```rust
    let mut ch =
        skimmer_dsp::channelizer::Channelizer::new(fs, center_freq_hz).map_err(|e| anyhow::anyhow!(e))?;
    let hop = ch.hop() as u64;
    let mut tm = crate::track::TrackManager::new(
        ch.n_channels(),
        fs,
        center_freq_hz,
        cfg.detector,
        cfg.decode.clone(),
    );
```

In `crates/skimmer-engine/src/soak.rs`, change the import and signature:

```rust
use skimmer_input::IqSource;
```
(replace `use skimmer_input::AudioIqSource;`)

```rust
pub fn soak(src: Box<dyn IqSource>, cfg: &PipelineConfig, duration: Duration) -> Result<SoakReport> {
```

(the body is otherwise unchanged — `listen(src, cfg, stop.clone(), ...)` inside `soak()` already just forwards `src`, which now flows through as `Box<dyn IqSource>` automatically).

- [ ] **Step 4: Fix the existing call sites so the workspace compiles**

In `crates/skimmer-engine/tests/listen_audio.rs`, change:
```rust
    let src =
        AudioIqSource::new(Box::new(coppa_audio::WavSource::from_samples(real, 48_000))).unwrap();
```
to:
```rust
    let src: Box<dyn skimmer_input::IqSource> = Box::new(
        AudioIqSource::new(Box::new(coppa_audio::WavSource::from_samples(real, 48_000))).unwrap(),
    );
```

In `crates/skimmer-cli/tests/soak_ci.rs`, change:
```rust
    let src = AudioIqSource::new(Box::new(coppa_audio::WavSource::from_samples(real, fs))).unwrap();
    let report = soak(src, &PipelineConfig::default(), Duration::from_secs(120)).unwrap();
```
to:
```rust
    let src: Box<dyn skimmer_input::IqSource> =
        Box::new(AudioIqSource::new(Box::new(coppa_audio::WavSource::from_samples(real, fs))).unwrap());
    let report = soak(src, &PipelineConfig::default(), Duration::from_secs(120)).unwrap();
```

In `crates/skimmer-cli/src/main.rs`, the two existing call sites need their already-constructed `src` boxed before the call (this is NOT adding the new `--soapy-*` flags — that's Task 3 — just keeping the existing `--device`/`--source` paths compiling against the new signature):

In `Command::Listen`'s handler, change:
```rust
            let src = match source {
                Some(path) => skimmer_input::AudioIqSource::from_wav_file(&path)?,
                None => skimmer_input::AudioIqSource::from_device(device.as_deref())?,
            };
```
to:
```rust
            let src: Box<dyn skimmer_input::IqSource> = match source {
                Some(path) => Box::new(skimmer_input::AudioIqSource::from_wav_file(&path)?),
                None => Box::new(skimmer_input::AudioIqSource::from_device(device.as_deref())?),
            };
```

In `Command::Soak`'s handler, change:
```rust
            let src = match source {
                Some(path) => skimmer_input::AudioIqSource::from_wav_file(&path)?,
                None => skimmer_input::AudioIqSource::from_device(device.as_deref())?,
            };
```
to the same pattern:
```rust
            let src: Box<dyn skimmer_input::IqSource> = match source {
                Some(path) => Box::new(skimmer_input::AudioIqSource::from_wav_file(&path)?),
                None => Box::new(skimmer_input::AudioIqSource::from_device(device.as_deref())?),
            };
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p skimmer-engine --lib` — expect `listen_uses_the_sources_center_freq_hz_not_a_hardcoded_zero` and the updated `soak_reports_no_panic_on_a_clean_short_signal` both pass.
Run: `cargo test -p skimmer-cli --test soak_ci` — expect pass.
Run: `cargo build -p skimmer-cli` — expect clean compile (confirms `main.rs`'s two updated call sites are correct).

- [ ] **Step 6: Run the full workspace test suite and clippy**

Run: `cargo test --workspace` (several minutes — real golden-vector decodes). Expect no regressions vs. the pre-existing baseline (V1/V3/V4/V7/V8/V9/V10 green, V2/V5/V6/V8w/`listen_decodes_a_clean_real_audio_signal`/CPU-budget-ignored-test all still `#[ignore]`d exactly as before — this task doesn't touch any of those).
Run: `cargo clippy --workspace --all-targets -- -D warnings`. Expect clean.

- [ ] **Step 7: Commit**

```bash
git add crates/skimmer-engine/src/listen.rs crates/skimmer-engine/src/soak.rs crates/skimmer-engine/tests/listen_audio.rs crates/skimmer-cli/tests/soak_ci.rs crates/skimmer-cli/src/main.rs
git commit -m "feat(engine): generalize listen()/soak() to Box<dyn IqSource>, fix hardcoded center_freq_hz=0.0"
```

---

### Task 3: CLI wiring

**Files:**
- Modify: `crates/skimmer-cli/Cargo.toml` (add `soapy` feature forwarding)
- Modify: `crates/skimmer-cli/src/main.rs` (new `--soapy-*` flags, dispatch)

**Interfaces:**
- Consumes: `skimmer_input::soapy::SoapySdrIqSource::open(driver_args: &str, fs: f64, center_freq_hz: f64, gain_db: Option<f64>) -> Result<Self>` (Task 1), `skimmer_engine::{listen, soak}` now taking `Box<dyn IqSource>` (Task 2).

- [ ] **Step 1: Add feature forwarding**

In `crates/skimmer-cli/Cargo.toml`, add:

```toml
[features]
soapy = ["skimmer-input/soapy"]
```

- [ ] **Step 2: Add the CLI flags**

In `crates/skimmer-cli/src/main.rs`, add to both `Command::Listen` and `Command::Soak` variants (after their existing `device`/`source` fields):

```rust
        /// SoapySDR driver args (e.g. "driver=rtlsdr"), feature `soapy`.
        /// Requires --soapy-freq and --soapy-rate.
        #[cfg(feature = "soapy")]
        #[arg(long, conflicts_with_all = ["device", "source"])]
        soapy_driver: Option<String>,
        /// RF center frequency in Hz. Required with --soapy-driver.
        #[cfg(feature = "soapy")]
        #[arg(long, requires = "soapy_driver")]
        soapy_freq: Option<f64>,
        /// Sample rate in Hz. Required with --soapy-driver.
        #[cfg(feature = "soapy")]
        #[arg(long, requires = "soapy_driver")]
        soapy_rate: Option<f64>,
        /// Gain in dB (omit for AGC, if the device supports it).
        #[cfg(feature = "soapy")]
        #[arg(long, requires = "soapy_driver")]
        soapy_gain: Option<f64>,
```

(`Command::Listen` gets these fields alongside its existing `device`/`source`/`json`; `Command::Soak` gets them alongside its existing `duration`/`device`/`source`.)

- [ ] **Step 3: Add the shared source-opening helper**

First, change `main.rs`'s existing `use anyhow::{bail, Result};` (top of file) to add `anyhow` the macro-like constructor function alongside the already-imported `bail`/`Result`:

```rust
use anyhow::{anyhow, bail, Result};
```

Then add, near the top of `main.rs` (after the imports, before `fn main()`):

```rust
use skimmer_input::IqSource;

#[cfg(feature = "soapy")]
fn open_source(
    device: Option<String>,
    source: Option<PathBuf>,
    soapy_driver: Option<String>,
    soapy_freq: Option<f64>,
    soapy_rate: Option<f64>,
    soapy_gain: Option<f64>,
) -> Result<Box<dyn IqSource>> {
    if let Some(driver) = soapy_driver {
        let freq = soapy_freq.ok_or_else(|| anyhow!("--soapy-freq is required with --soapy-driver"))?;
        let rate = soapy_rate.ok_or_else(|| anyhow!("--soapy-rate is required with --soapy-driver"))?;
        return Ok(Box::new(skimmer_input::soapy::SoapySdrIqSource::open(
            &driver, rate, freq, soapy_gain,
        )?));
    }
    open_audio_source(device, source)
}

#[cfg(not(feature = "soapy"))]
fn open_source(device: Option<String>, source: Option<PathBuf>) -> Result<Box<dyn IqSource>> {
    open_audio_source(device, source)
}

fn open_audio_source(device: Option<String>, source: Option<PathBuf>) -> Result<Box<dyn IqSource>> {
    Ok(match source {
        Some(path) => Box::new(skimmer_input::AudioIqSource::from_wav_file(&path)?),
        None => Box::new(skimmer_input::AudioIqSource::from_device(device.as_deref())?),
    })
}
```

- [ ] **Step 4: Wire the dispatch**

In `Command::Listen`'s handler, replace the (Task 2's Step 4) `let src: Box<dyn skimmer_input::IqSource> = match source { ... };` block with:

```rust
            #[cfg(feature = "soapy")]
            let src = open_source(device, source, soapy_driver, soapy_freq, soapy_rate, soapy_gain)?;
            #[cfg(not(feature = "soapy"))]
            let src = open_source(device, source)?;
```

Do the identical replacement in `Command::Soak`'s handler (same two lines, same pattern — that arm also destructures `device`/`source` plus, under `soapy`, the four new fields).

Also update the two `match Cli::parse().command` arm patterns themselves to destructure the new fields under `#[cfg(feature = "soapy")]`:

```rust
        Command::Listen {
            device,
            source,
            json,
            #[cfg(feature = "soapy")]
            soapy_driver,
            #[cfg(feature = "soapy")]
            soapy_freq,
            #[cfg(feature = "soapy")]
            soapy_rate,
            #[cfg(feature = "soapy")]
            soapy_gain,
        } => {
```

and analogously for `Command::Soak { duration, device, source, #[cfg(feature = "soapy")] soapy_driver, #[cfg(feature = "soapy")] soapy_freq, #[cfg(feature = "soapy")] soapy_rate, #[cfg(feature = "soapy")] soapy_gain } => { ... }`.

- [ ] **Step 5: Build both ways**

Run: `cargo build -p skimmer-cli` (no feature) — expect clean, `--help` shows no `--soapy-*` flags (spot-check: `./target/debug/skimmer listen --help` should not mention `soapy`).
Run: `cargo build -p skimmer-cli --features soapy` — expect clean, `--help` on this build DOES show the new flags.

- [ ] **Step 6: Write a CLI-level test for the validation error**

Add to `crates/skimmer-cli/tests/cli.rs` (read the existing file first to match its conventions — likely uses `assert_cmd`-style `Command::new(env!("CARGO_BIN_EXE_skimmer"))` per this repo's established pattern seen in the golden tests):

```rust
#[test]
#[cfg(feature = "soapy")]
fn soapy_driver_without_freq_and_rate_is_a_clean_error() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_skimmer"))
        .args(["listen", "--soapy-driver", "driver=rtlsdr"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "expected a clean failure without --soapy-freq/--soapy-rate"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("soapy-freq") || stderr.contains("soapy-rate") || stderr.contains("required"),
        "expected an explanatory error, got: {stderr}"
    );
}
```

Run: `cargo test -p skimmer-cli --features soapy soapy_driver_without` — expect pass. (This test doesn't exist/compile without the feature, matching the same `#[cfg(feature = "soapy")]` gating as the CLI flags themselves.)

- [ ] **Step 7: Full check**

Run:
```
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p skimmer-input -p skimmer-cli --all-targets --features soapy -- -D warnings
cargo test --workspace
cargo test -p skimmer-input -p skimmer-cli --features soapy
```
All must be clean.

- [ ] **Step 8: Commit**

```bash
git add crates/skimmer-cli/Cargo.toml crates/skimmer-cli/src/main.rs crates/skimmer-cli/tests/cli.rs
git commit -m "feat(cli): wire --soapy-driver/--soapy-freq/--soapy-rate/--soapy-gain into listen/soak"
```

---

### Task 4: CI job for the `soapy` feature

**Files:**
- Modify: `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: nothing new — exercises Tasks 1-3's `--features soapy` build/test/clippy paths.

- [ ] **Step 1: Add the job**

In `.github/workflows/ci.yml`, add a new job after the existing `test` job (same file, top-level under `jobs:`), reusing the exact same pinned action versions as the existing job:

```yaml
  test-soapy:
    strategy:
      fail-fast: false
      matrix:
        os: [ubuntu-latest, macos-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@34e114876b0b11c390a56381ad16ebd13914f8d5 # v4.3.1
      - if: runner.os == 'Linux'
        run: sudo apt-get update && sudo apt-get install -y libasound2-dev libsoapysdr-dev
      - if: runner.os == 'macOS'
        run: brew install soapysdr
      - uses: dtolnay/rust-toolchain@4cda84d5c5c54efe2404f9d843567869ab1699d4 # stable
        with:
          components: rustfmt, clippy
      - uses: Swatinem/rust-cache@42dc69e1aa15d09112580998cf2ef0119e2e91ae # v2.9.1
      - run: cargo clippy -p skimmer-input -p skimmer-cli --all-targets --features soapy -- -D warnings
      - run: cargo test -p skimmer-input -p skimmer-cli --features soapy
```

(`libasound2-dev` is repeated from the existing Linux step because `skimmer-input`/`skimmer-cli` still pull in `cpal`/`coppa-audio` regardless of the `soapy` feature — this job needs the same audio build deps as the default job, plus `libsoapysdr-dev`.)

- [ ] **Step 2: Validate the job's commands locally**

This machine already has `soapysdr`/`pkg-config` installed (from this plan's brainstorming phase) — run the exact commands the new job runs, to catch any issue before it only surfaces in CI:

```
cargo clippy -p skimmer-input -p skimmer-cli --all-targets --features soapy -- -D warnings
cargo test -p skimmer-input -p skimmer-cli --features soapy
```

Both must be clean/passing (they should already be, from Tasks 1 and 3's own verification — this step is a final confirmation of the exact CI command line, not new work).

- [ ] **Step 3: Validate YAML syntax**

Run: `cd /Users/thagale/Code/skimmer/.claude/worktrees/m2-soapysdr-input && python3 -c "import yaml; yaml.safe_load(open('.github/workflows/ci.yml'))" ` (or any available YAML linter) to catch indentation errors before pushing — GitHub Actions failures from bad YAML are otherwise only visible after a push.

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: add soapy-feature build/test job (ubuntu + macos)"
```

---

### Task 5: Final integration

**Files:**
- Modify: `ROADMAP.md`
- Modify: `CLAUDE.md`
- Create: `docs/DECISIONS/2026-07-25-m2-soapysdr-input-pins.md`

**Interfaces:**
- Consumes: the real state of Tasks 1-4 (all should be green; there is no "real hardware" empirical outcome to report the way V8w/CPU-budget had one — this sub-project's honest limitation is "compiles, links, unit-tests the error paths; streaming/decode-accuracy-over-real-RF is untested," and that must be stated plainly, not glossed over).

- [ ] **Step 1: Write the DECISIONS pin doc**

Create `docs/DECISIONS/2026-07-25-m2-soapysdr-input-pins.md`, matching the style of `docs/DECISIONS/2026-07-24-m2-pileup-cpu-budget-pins.md` (numbered pinned decisions). Must include:

1. `soapysdr = "0.5"` chosen (the standard high-level Rust wrapper crate matching ARCHITECTURE.md's reference); `Complex32` maps directly to its `CF32` stream format, no conversion layer.
2. No RF hardware available; real testing is limited to two confirmed hardware-free error paths (device-not-found via `driver=rtlsdr`, stream-not-supported via `type=null`) — the actual streaming/read()/decode-accuracy path is genuinely untested, flagged as an outstanding manual step (same pattern as Pi4/W1AW), not silently assumed to work.
3. The `listen()` `center_freq_hz` hardcoded-`0.0` bug found and fixed as part of this sub-project (cite the real test that proves it: `listen_uses_the_sources_center_freq_hz_not_a_hardcoded_zero`).
4. `listen`/`soak` signature change (`AudioIqSource` → `Box<dyn IqSource>`) — a real, if small, engine-level API change; note it as a deviation from the original design's crate-only framing (Tony explicitly asked for the CLI/engine scope to be included, cf. the brainstorming session).
5. The new `test-soapy` CI job's scope and why it's a separate job (keeps the default job free of the native SoapySDR dependency, per ROADMAP.md).

- [ ] **Step 2: Update ROADMAP.md**

In the M2 section, following the existing "M2 sub-project N ... is complete" pattern, add a sentence noting SoapySDR input is implemented (crate-level `IqSource` + CLI wiring + CI), with real hardware validation flagged outstanding. Update "Remaining M2 sub-projects" to drop SoapySDR input, leaving only KiwiSDR input.

- [ ] **Step 3: Update CLAUDE.md Status section**

Mirror the existing paragraph's style; keep the whole file under ~100 lines (check with `wc -l CLAUDE.md` before and after, same hard constraint as the previous M2 sub-project's close-out).

- [ ] **Step 4: Full workspace verification**

Run, in order:
```
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p skimmer-input -p skimmer-cli --all-targets --features soapy -- -D warnings
cargo test --workspace
cargo test -p skimmer-input -p skimmer-cli --features soapy
```
All five must be clean.

- [ ] **Step 5: Commit**

```bash
git add ROADMAP.md CLAUDE.md docs/DECISIONS/2026-07-25-m2-soapysdr-input-pins.md
git commit -m "docs: M2 SoapySDR input close-out, pin real findings"
```

- [ ] **Step 6: Push and open the PR**

```bash
git push -u origin feat/m2-soapysdr-input
gh pr create --title "feat(input,engine,cli): M2 SoapySDR input (RTL-SDR/Airspy HF+/SDRplay)" --body "$(cat <<'EOF'
## Summary

- ARCHITECTURE.md §3 SoapySDR IqSource (crates/skimmer-input/src/soapy.rs), feature-gated `soapy`.
- skimmer_engine::listen()/soak() generalized from AudioIqSource to Box<dyn IqSource>.
- Fixed a real pre-existing bug: listen() hardcoded center_freq_hz=0.0 instead of reading it from the source.
- CLI --soapy-driver/--soapy-freq/--soapy-rate/--soapy-gain flags on listen/soak.
- New CI job building/testing --features soapy on ubuntu-latest + macos-latest.

See docs/superpowers/specs/2026-07-25-m2-soapysdr-input-design.md for the full design.

## Real limitation, stated plainly

No RF hardware was available anywhere in this environment. Two real, hardware-free error paths are unit-tested (device-not-found, stream-not-supported); the actual streaming/read()/decode-accuracy-over-real-RF path is untested and flagged as an outstanding manual step in docs/DECISIONS/2026-07-25-m2-soapysdr-input-pins.md — same pattern as the Pi4 CPU-budget leg and M1's W1AW live-copy run.

## Test plan

- [x] cargo fmt --all --check
- [x] cargo clippy --workspace --all-targets -- -D warnings
- [x] cargo clippy -p skimmer-input -p skimmer-cli --all-targets --features soapy -- -D warnings
- [x] cargo test --workspace
- [x] cargo test -p skimmer-input -p skimmer-cli --features soapy

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Report the PR URL.
