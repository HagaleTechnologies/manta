# M2 KiwiSDR Input Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement ARCHITECTURE.md §3's KiwiSDR websocket IQ client (`skimmer-input::kiwi::KiwiIqSource`), generalize `skimmer_engine::listen`/`soak` to accept any `IqSource`, wire it into the CLI — the last of M2's remaining sub-projects.

**Architecture:** `skimmer-input` gains an unconditional (no feature gate — pure Rust, no native library) `kiwi` module: a synchronous `tungstenite` WebSocket client implementing the real, live-verified KiwiSDR wire protocol (handshake, binary `MSG`/`SND` frame parsing), feeding a `rubato`-based rational resampler (the real device rate, e.g. ~11999 Hz, is not a clean fraction of any SPEC §1.1 table rate and needs genuine resampling, not simple decimation) to produce 96 kHz `Complex32` samples. `skimmer_engine::listen`/`soak` change from a concrete `AudioIqSource` parameter to `Box<dyn IqSource>` (same generalization as the — separate, unmerged — SoapySDR PR #30, redone independently here per this repo's "always branch fresh from origin/main" hygiene).

**Tech Stack:** Rust, `tungstenite` + `rubato` (new dependencies), existing `skimmer-input`/`skimmer-engine`/`skimmer-cli` crates.

## Global Constraints

- `docs/SPEC-decode-core.md` §1.1: non-power-of-two input rates (KiwiSDR's ~12 kHz IQ) must be rational-resampled in `skimmer-input` to the nearest SPEC table rate (96000/192000/384000/768000 Hz) before reaching the channelizer — use 96000 (the nearest to 12 kHz's natural ~8x ratio).
- The real KiwiSDR protocol, verified via a live connection during brainstorming (`docs/superpowers/specs/2026-07-25-m2-kiwisdr-input-design.md` — read this in full before starting, it has the complete real findings):
  - WS URL: `ws://<host>:<port>/<timestamp>/SND`.
  - Handshake (WebSocket **text** frames): `SET auth t=kiwi p=<password>`, `SET mod=iq low_cut=-5000 high_cut=5000 freq=<khz>`, `SET agc=1 hang=0 thresh=-100 slope=6 decay=1000 manGain=50`, `SET compression=0`.
  - **`SET keepalive` must be sent at ~1 Hz for the connection's entire lifetime** (not just once at setup) — confirmed required: without it, `SND` frames never arrive.
  - Server responses arrive as WebSocket **binary** frames (not text — a real, live-confirmed correction of an initial wrong assumption), tagged by a 3-byte ASCII prefix: `"MSG"` (text key=value parameters) or `"SND"` (binary IQ payload).
  - The real IQ sample rate is reported via `MSG sample_rate=<float>` (e.g. `11998.937786` — NOT exactly 12000, device-specific) and must be read at connect time, never hardcoded.
  - `SND` frame layout (live-captured, internally consistent across 8 real frames, residual risk noted in the design spec — Step 1 of Task 1 re-verifies this): 3-byte `"SND"` tag, 1-byte flags, 4-byte little-endian seq, 2-byte big-endian S-meter, 10-byte GPS block, then big-endian int16 I/Q sample pairs (512 pairs per real captured frame). The `0x80` flag bit (`SND_FLAG_LITTLE_ENDIAN`) should be honored at runtime for the I/Q byte order, not hardcoded, even though it was unset (big-endian) in every captured frame.
- `rubato = "4.0"`'s `Fft::new` takes `usize` sample rates (round the real fractional rate to the nearest Hz) and needs its own re-exported `rubato::audioadapter_buffers::direct::InterleavedSlice` buffer-wrapper type for `process_into_buffer` — no separate `audioadapter`/`audioadapter-buffers` Cargo.toml entries needed, `rubato` re-exports both.
- Real, live-confirmed finding: feeding the resampler in KiwiSDR's native 512-sample SND-frame chunks produces a 48,000-output-sample (0.5s) delay with **zero** output after 6 consecutive chunks. The resampler's chunk size must be decoupled from the raw per-SND-frame sample count via a separate raw-sample accumulation buffer — Task 1 tunes the actual `RESAMPLER_CHUNK` value empirically.
- No feature gate needed (unlike `soapy`) — `tungstenite`/`rubato` are pure Rust with no system library dependency, so no new CI job is needed either; the existing default `test` job already covers this.
- Real, pre-existing bug (same one fixed independently in the unmerged SoapySDR PR #30 — this branch doesn't have that fix, redo it here): `crates/skimmer-engine/src/listen.rs` hardcodes `center_freq_hz = 0.0` in both `Channelizer::new(fs, 0.0)` and `TrackManager::new(.., 0.0, ..)` instead of reading `src.center_freq_hz()`. Fix in Task 2.
- Full design spec: `docs/superpowers/specs/2026-07-25-m2-kiwisdr-input-design.md`.
- A real public KiwiSDR receiver is available for genuine integration testing (unlike SoapySDR, which had no real hardware at all) — use it, `#[ignore]`d by default (network-dependent, third-party infrastructure, not for default `cargo test`/CI).

---

### Task 1: `KiwiIqSource`

**Files:**
- Modify: `Cargo.toml` (workspace root — add `tungstenite` and `rubato` to `[workspace.dependencies]`)
- Modify: `crates/skimmer-input/Cargo.toml` (add both as regular, unconditional dependencies — no feature gate)
- Modify: `crates/skimmer-input/src/lib.rs` (add `pub mod kiwi;`, no `#[cfg(...)]`)
- Create: `crates/skimmer-input/src/kiwi.rs`

**Interfaces:**
- Consumes: `skimmer_input::IqSource` (existing trait, this crate).
- Produces: `pub struct KiwiIqSource` implementing `IqSource`, with `pub fn connect(host: &str, port: u16, center_freq_hz: f64, password: &str) -> anyhow::Result<Self>`. Used by Task 3 (CLI wiring).

- [ ] **Step 1: Re-verify the SND frame byte layout with a fresh live capture**

Before writing the parser, confirm the byte layout independently (don't just trust the design spec's inferred layout — it was itself a correction of an earlier wrong assumption, and its own residual-risk note asks for this). Write a small throwaway Rust program (in a scratch directory, e.g. `/tmp` or your session's scratchpad — NOT committed to the repo) using `tungstenite` that:
1. Connects to a real public KiwiSDR receiver (try `greatlakesreceiver.hopto.me:8073` first — public nodes come and go, if that one's down, search for a current one, e.g. via `http://kiwisdr.com/public/` or a websearch for "kiwisdr public receiver list").
2. Sends the handshake exactly as documented in the Global Constraints above (including keepalive).
3. Captures several real `SND` frames and prints their exact byte length and a hex dump of the first ~30 bytes.
4. Confirms: total frame length, whether it's constant across frames, and where the actual I/Q sample data starts (cross-check against the hypothesis: tag(3)+flags(1)+seq(4)+smeter(2)+gps(10)=20 header bytes, then N×4-byte big-endian I/Q pairs).

Report your actual findings — if they match the design spec's hypothesis, proceed with Step 2 below. If they differ, adjust Step 2's implementation to match what you actually observed (this is expected to be possible, not a sign something's wrong with the plan — protocol reverse-engineering from indirect sources always carries this kind of residual risk, and direct verification is the correct way to resolve it).

- [ ] **Step 2: Add dependencies**

In `Cargo.toml` (workspace root), add to `[workspace.dependencies]`:

```toml
tungstenite = "0.24"
rubato = "4.0"
```

In `crates/skimmer-input/Cargo.toml`, add to `[dependencies]` (no feature gate — these are pure Rust, no system library):

```toml
tungstenite = { workspace = true }
rubato = { workspace = true }
```

- [ ] **Step 3: Implement the connection/handshake**

Create `crates/skimmer-input/src/kiwi.rs`. Implement `KiwiIqSource::connect()`:
1. `std::net::TcpStream::connect((host, port))`, then `.set_read_timeout(Some(Duration::from_millis(...)))` — pick a value that keeps `read()` responsive (matches `SoapySdrIqSource`'s `TIMEOUT_US` reasoning in `crates/skimmer-input/src/soapy.rs` from the — unmerged — SoapySDR PR; read that file if it's not present on this branch, the reasoning still applies, redo the analogous constant here).
2. Build the URL `format!("ws://{host}:{port}/{timestamp}/SND", timestamp = /* any process-unique value, e.g. current unix time in ms via std::time */)`.
3. `tungstenite::client(url, tcp_stream)` to perform the WS handshake.
4. Send the SET commands from Global Constraints, in order, as `Message::Text`.
5. Read frames (binary, 3-byte tag) until a `MSG` frame containing `sample_rate=` is seen; parse the float, round to nearest `usize` Hz.
6. Construct the `rubato::Fft::<f32>::new(rounded_rate_in, 96_000, RESAMPLER_CHUNK, 2, rubato::FixedSync::Input)` resampler — pick a `RESAMPLER_CHUNK` value larger than 512 (start with something like 4096 or 8192 — accumulate that many raw samples from multiple SND frames before the first resample call) and empirically check `resampler.output_delay()` prints a reasonable value (log it, don't just trust an assumption) and that real output starts flowing within a reasonable number of accumulated chunks (test this in Step 6, adjust `RESAMPLER_CHUNK` if the delay is unreasonably large — there's a real tradeoff here between latency and resampling quality/efficiency that this plan doesn't pre-resolve, use your judgment against real measured behavior).
7. Return `Ok(Self { socket, fs: 96_000.0, center_freq_hz, resampler, raw: Vec::new(), pending: VecDeque::new(), last_keepalive: Instant::now() })`.

Every fallible step propagates via `?` (or `.map_err(anyhow::Error::from)` / `anyhow::Context` as appropriate) — no `.unwrap()`/`.expect()` outside test code, matching this codebase's established error-handling convention (see `SoapySdrIqSource::open` for the pattern, if present on this branch, or the general principle: every real failure mode becomes a clean `Err`, never a panic).

- [ ] **Step 4: Implement SND frame parsing and the resampling pipeline**

Implement a private helper (e.g. `fn parse_snd_frame(body: &[u8]) -> Vec<Complex32>`) that takes an SND frame's bytes (after the 3-byte tag) and returns the raw (un-resampled) I/Q samples as `Complex32`, per Step 1's confirmed byte layout — honor the `0x80` (little-endian) flag bit at runtime for I/Q byte order rather than hardcoding big-endian.

Implement the resampling pipeline: push newly-parsed raw samples into `self.raw` (as interleaved `f32`: `[I0, Q0, I1, Q1, ...]`, matching `rubato`'s interleaved-channel convention with `channels=2`); once `self.raw` holds at least `RESAMPLER_CHUNK` interleaved sample-pairs, call `resampler.process_into_buffer` (via `rubato::audioadapter_buffers::direct::InterleavedSlice::new`/`new_mut` wrapping the input/output slices, per the design spec's confirmed API) and push the resulting resampled `Complex32` samples onto `self.pending`, draining the consumed portion of `self.raw`.

- [ ] **Step 5: Implement `IqSource` for `KiwiIqSource`**

```rust
impl IqSource for KiwiIqSource {
    fn sample_rate(&self) -> f64 {
        self.fs
    }

    fn center_freq_hz(&self) -> f64 {
        self.center_freq_hz
    }

    fn read(&mut self, buf: &mut [Complex32]) -> anyhow::Result<usize> {
        // Loop: send keepalive if ~1s has passed since the last one; if
        // `self.pending` already has enough samples, drain into `buf` and
        // return immediately; otherwise read the next WebSocket frame,
        // dispatch on its 3-byte tag (SND -> parse_snd_frame + resample per
        // Step 4; MSG -> ignore/log; Ping/Close -> normal WebSocket
        // housekeeping), and loop again. Bound the retry/wait behavior the
        // same way SoapySdrIqSource::read() bounds its timeout retries (if
        // present on this branch) -- a stalled connection should surface a
        // real Err eventually, not hang `listen()`'s Ctrl-C responsiveness
        // forever, but a single transient stall (e.g. one slow frame)
        // shouldn't be fatal either.
    }
}
```

Write the actual implementation following this shape — the exact retry/bound parameters are your call, informed by what you observe in Step 6's real testing.

- [ ] **Step 6: Write and run the real, live integration test**

```rust
#[test]
#[ignore]
fn connects_to_a_real_public_receiver_and_streams_iq() {
    let mut src = KiwiIqSource::connect("greatlakesreceiver.hopto.me", 8073, 14_025_000.0, "")
        .expect("connect to a real public KiwiSDR receiver");
    assert!(
        (src.sample_rate() - 96_000.0).abs() < 1.0,
        "expected resampled rate ~96000, got {}",
        src.sample_rate()
    );
    let mut buf = vec![Complex32::new(0.0, 0.0); 4096];
    let n = src.read(&mut buf).expect("read real IQ samples");
    assert!(n > 0, "expected real samples from a live receiver");
    // Sanity: real RF noise/signal should not be all-zero.
    assert!(
        buf[..n].iter().any(|s| s.norm() > 0.0),
        "expected non-silent real samples"
    );
}
```

If the receiver used in Step 1 isn't reachable when you run this, pick a different currently-live public node and use that instead (update both this test and Step 1's findings consistently).

Run: `cargo test -p skimmer-input kiwi:: -- --ignored --nocapture`
Expected: PASS, with real evidence (actual sample rate achieved, actual `n` samples read) in your report — this is a real network test, don't assume, run it and report the true output.

- [ ] **Step 7: Write hardware/network-independent tests**

```rust
#[test]
fn connect_refused_is_a_clean_error() {
    // Nothing listens on port 1 -- a fast, reliable, always-available
    // "connection refused" path, no real network dependency.
    let result = KiwiIqSource::connect("127.0.0.1", 1, 14_025_000.0, "");
    assert!(result.is_err(), "expected a clean Err, not a panic");
}
```

Add a focused unit test for the resampling math alone (no network): construct a `rubato::Fft` resampler directly with known parameters, feed it synthetic known-frequency input, and confirm the output sample rate/count relationship matches what's expected (e.g. `written / read ≈ rate_out / rate_in` once the resampler's internal delay has been primed with enough chunks — base this on what you actually observe, per Step 3's `RESAMPLER_CHUNK` tuning).

Run: `cargo test -p skimmer-input kiwi::` (no `--ignored` — these two should run by default)
Expected: both pass.

- [ ] **Step 8: Full check**

Run:
```
cargo build -p skimmer-input
cargo clippy -p skimmer-input --all-targets -- -D warnings
cargo fmt -p skimmer-input --check
cargo test -p skimmer-input kiwi::
```
All clean. (The `--ignored` live test from Step 6 doesn't need to be part of this default run, but should have already passed once for real per Step 6's own instructions.)

- [ ] **Step 9: Commit**

```bash
git add Cargo.toml crates/skimmer-input/Cargo.toml crates/skimmer-input/src/lib.rs crates/skimmer-input/src/kiwi.rs
git commit -m "feat(input): KiwiSDR websocket IqSource"
```

---

### Task 2: Engine generalization (`Box<dyn IqSource>`) + `center_freq_hz` fix

**Files:**
- Modify: `crates/skimmer-engine/src/listen.rs`
- Modify: `crates/skimmer-engine/src/soak.rs`
- Modify: `crates/skimmer-engine/tests/listen_audio.rs`
- Modify: `crates/skimmer-cli/tests/soak_ci.rs`
- Modify: `crates/skimmer-cli/src/main.rs`

**Interfaces:**
- Consumes: `skimmer_input::IqSource` (dyn-compatible: no generics, no `Self`-returning methods).
- Produces: `pub fn listen(mut src: Box<dyn IqSource>, cfg: &PipelineConfig, stop: Arc<AtomicBool>, on_event: impl FnMut(&DecoderEvent)) -> Result<()>`, `pub fn soak(src: Box<dyn IqSource>, cfg: &PipelineConfig, duration: Duration) -> Result<SoakReport>`. Used by Task 3.

This task is IDENTICAL in shape to a task already completed once on a separate, unmerged branch (`feat/m2-soapysdr-input`, PR #30) — same signature change, same bug, same fix. Follow this exact recipe (already implemented and code-reviewed once; this is a known-good pattern, not new design work):

- [ ] **Step 1: Write/update the failing tests**

In `crates/skimmer-engine/src/soak.rs`'s existing `#[cfg(test)] mod tests` block, box the source and add an explicit import (the non-test body will no longer import `AudioIqSource` directly after Step 3):

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

This replaces `soak.rs`'s entire existing `#[cfg(test)] mod tests { ... }` block (it currently has exactly this one test).

Also add a new test to `crates/skimmer-engine/src/listen.rs`'s (currently nonexistent) `#[cfg(test)] mod tests` at the end of the file, proving the `center_freq_hz` fix:

```rust
#[cfg(test)]
mod tests {
    use super::*;

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
            "freq_hz {freq_hz} should be near {} (center_freq_hz + V1's known offset), not near 12340",
            spec.center_freq_hz + 12_340.0
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p skimmer-engine --lib` and `cargo test -p skimmer-cli --test soak_ci`.
Expected: compile errors (signature mismatch) — confirms the tests exercise the not-yet-changed signatures.

- [ ] **Step 3: Change the signatures and fix the bug**

In `crates/skimmer-engine/src/listen.rs`: change `use skimmer_input::{AudioIqSource, IqSource};` to `use skimmer_input::IqSource;`. Change the signature to `pub fn listen(mut src: Box<dyn IqSource>, cfg: &PipelineConfig, stop: Arc<AtomicBool>, mut on_event: impl FnMut(&DecoderEvent)) -> Result<()> {`. Read `center_freq_hz` alongside `fs`:

```rust
    let fs = src.sample_rate();
    let center_freq_hz = src.center_freq_hz();
```

and use `center_freq_hz` (not the literal `0.0`) in both `Channelizer::new(fs, center_freq_hz)` and `TrackManager::new(ch.n_channels(), fs, center_freq_hz, cfg.detector, cfg.decode.clone())`.

In `crates/skimmer-engine/src/soak.rs`: change `use skimmer_input::AudioIqSource;` to `use skimmer_input::IqSource;`. Change the signature to `pub fn soak(src: Box<dyn IqSource>, cfg: &PipelineConfig, duration: Duration) -> Result<SoakReport> {` (body unchanged — `listen(src, cfg, stop.clone(), ...)` already just forwards `src`).

- [ ] **Step 4: Fix the existing call sites**

In `crates/skimmer-engine/tests/listen_audio.rs`, box the source:
```rust
    let src: Box<dyn skimmer_input::IqSource> = Box::new(
        AudioIqSource::new(Box::new(coppa_audio::WavSource::from_samples(real, 48_000))).unwrap(),
    );
```

In `crates/skimmer-cli/tests/soak_ci.rs`, box the source:
```rust
    let src: Box<dyn skimmer_input::IqSource> =
        Box::new(AudioIqSource::new(Box::new(coppa_audio::WavSource::from_samples(real, fs))).unwrap());
    let report = soak(src, &PipelineConfig::default(), Duration::from_secs(120)).unwrap();
```

In `crates/skimmer-cli/src/main.rs`, box the existing `AudioIqSource` construction in BOTH `Command::Listen` and `Command::Soak`'s handlers (do NOT add any new CLI flags here — that's Task 3):
```rust
            let src: Box<dyn skimmer_input::IqSource> = match source {
                Some(path) => Box::new(skimmer_input::AudioIqSource::from_wav_file(&path)?),
                None => Box::new(skimmer_input::AudioIqSource::from_device(device.as_deref())?),
            };
```
(same replacement in both handlers, matching what's already there structurally).

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p skimmer-engine --lib` — expect `listen_uses_the_sources_center_freq_hz_not_a_hardcoded_zero` and `soak_reports_no_panic_on_a_clean_short_signal` both pass.
Run: `cargo test -p skimmer-cli --test soak_ci` — expect pass.
Run: `cargo build -p skimmer-cli` — expect clean compile.

- [ ] **Step 6: Run the full workspace test suite and clippy**

Run: `cargo test --workspace` (several minutes — real golden-vector decodes, use direct output redirection to a file, not a pipe through `tail`, so you get a trustworthy exit code: `cargo test --workspace > /tmp/full.log 2>&1; echo "EXIT:$?"`). Expect no regressions.
Run: `cargo clippy --workspace --all-targets -- -D warnings`. Expect clean.

- [ ] **Step 7: Commit**

```bash
git add crates/skimmer-engine/src/listen.rs crates/skimmer-engine/src/soak.rs crates/skimmer-engine/tests/listen_audio.rs crates/skimmer-cli/tests/soak_ci.rs crates/skimmer-cli/src/main.rs
git commit -m "feat(engine): generalize listen()/soak() to Box<dyn IqSource>, fix hardcoded center_freq_hz=0.0"
```

---

### Task 3: CLI wiring

**Files:**
- Modify: `crates/skimmer-cli/src/main.rs`

**Interfaces:**
- Consumes: `skimmer_input::kiwi::KiwiIqSource::connect(host: &str, port: u16, center_freq_hz: f64, password: &str) -> anyhow::Result<Self>` (Task 1), `skimmer_engine::{listen, soak}` now taking `Box<dyn IqSource>` (Task 2).

- [ ] **Step 1: Check the real current state of `main.rs` first**

Read `crates/skimmer-cli/src/main.rs` as it exists after Task 2's changes on THIS branch. Unlike the SoapySDR sub-project, this branch has no `--soapy-*` flags (they exist only on the separate, unmerged `feat/m2-soapysdr-input` branch) — don't assume they're present; your CLI additions here are independent.

- [ ] **Step 2: Add the CLI flags**

Add to both `Command::Listen` and `Command::Soak` variants (after their existing `device`/`source` fields):

```rust
        /// KiwiSDR receiver hostname. Requires --kiwi-freq.
        #[arg(long, conflicts_with_all = ["device", "source"], requires = "kiwi_freq")]
        kiwi_host: Option<String>,
        /// KiwiSDR receiver port (default 8073, the standard KiwiSDR port).
        #[arg(long, default_value = "8073", requires = "kiwi_host")]
        kiwi_port: u16,
        /// RF center frequency in Hz. Required with --kiwi-host.
        #[arg(long, requires = "kiwi_host")]
        kiwi_freq: Option<f64>,
        /// KiwiSDR password (empty for anonymous/no-password receivers, the common case for public nodes).
        #[arg(long, requires = "kiwi_host", default_value = "")]
        kiwi_password: String,
```

(`Command::Soak` gets these alongside its existing `duration`/`device`/`source`, no `json` field on that variant — don't add one.)

- [ ] **Step 3: Extend (or create) the shared source-opening helper**

If Task 2 (or a prior task, if you're implementing after the SoapySDR PR has merged and this pattern already exists) left an `open_source`/`open_audio_source`-style helper in `main.rs`, extend it; otherwise add one following this shape:

```rust
fn open_source(
    device: Option<String>,
    source: Option<PathBuf>,
    kiwi_host: Option<String>,
    kiwi_port: u16,
    kiwi_freq: Option<f64>,
    kiwi_password: String,
) -> Result<Box<dyn skimmer_input::IqSource>> {
    if let Some(host) = kiwi_host {
        let freq = kiwi_freq.ok_or_else(|| anyhow!("--kiwi-freq is required with --kiwi-host"))?;
        return Ok(Box::new(skimmer_input::kiwi::KiwiIqSource::connect(
            &host,
            kiwi_port,
            freq,
            &kiwi_password,
        )?));
    }
    Ok(match source {
        Some(path) => Box::new(skimmer_input::AudioIqSource::from_wav_file(&path)?),
        None => Box::new(skimmer_input::AudioIqSource::from_device(device.as_deref())?),
    })
}
```

(Note: `kiwi_freq` is already enforced as required via clap's `requires = "kiwi_freq"` on `kiwi_host` in Step 2 — clap's declarative validation actually covers this direction correctly, unlike the SoapySDR case where the reverse direction needed a manual runtime check. Still worth the `ok_or_else` as a defensive belt-and-suspenders check, since `Option<f64>` is still the field's type regardless of what clap enforces.)

- [ ] **Step 4: Wire the dispatch**

In both `Command::Listen` and `Command::Soak`'s handlers, replace the existing source-construction block with a call to `open_source(...)`, passing through whichever fields exist on this branch (just `device`/`source`/`kiwi_*` — no `soapy_*` fields exist here). Update the `match Cli::parse().command` arm patterns to destructure the new fields.

- [ ] **Step 5: Build and test**

Run: `cargo build -p skimmer-cli` — clean.
Run: `./target/debug/skimmer listen --help` — confirm the new `--kiwi-*` flags appear with sensible descriptions.

- [ ] **Step 6: Write a CLI-level validation test**

Add to `crates/skimmer-cli/tests/cli.rs` (read the existing file first to match its conventions):

```rust
#[test]
fn kiwi_host_without_freq_is_a_clean_error() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_skimmer"))
        .args(["listen", "--kiwi-host", "example.com"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "expected a clean failure without --kiwi-freq"
    );
}
```

Run: `cargo test -p skimmer-cli --test cli kiwi_host_without_freq` — expect pass.

- [ ] **Step 7: Full check**

Run:
```
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
All clean (use direct output redirection for `cargo test --workspace`, not a pipe through `tail`, to get a trustworthy exit code).

- [ ] **Step 8: Commit**

```bash
git add crates/skimmer-cli/src/main.rs crates/skimmer-cli/tests/cli.rs
git commit -m "feat(cli): wire --kiwi-host/--kiwi-port/--kiwi-freq/--kiwi-password into listen/soak"
```

---

### Task 4: Final integration

**Files:**
- Modify: `ROADMAP.md`
- Modify: `CLAUDE.md`
- Create: `docs/DECISIONS/2026-07-25-m2-kiwisdr-input-pins.md`

**Interfaces:**
- Consumes: the real state of Tasks 1-3 (all should be green; report the REAL live-test findings from Task 1, not a summary of the plan's predictions).

- [ ] **Step 1: Check the real merge status of the other M2 sub-project PRs**

Before touching ROADMAP.md/CLAUDE.md, run `gh pr list --repo HagaleTechnologies/skimmer --state all --search "M2"` (or similar) to check whether PR #29 (V8/V8w pileup + CPU-budget) and PR #30 (SoapySDR input) have been merged since this branch started. M2's "Remaining M2 sub-projects" list and its accept-criteria checklist must reflect REAL current state, not an assumption — this exact kind of check caught a real staleness bug during the SoapySDR sub-project's own close-out (its Task 5 had to correct a brief that assumed PR #29 would already be merged; it wasn't). Do the analogous check here.

- [ ] **Step 2: Write the DECISIONS pin doc**

Create `docs/DECISIONS/2026-07-25-m2-kiwisdr-input-pins.md`, matching the style of `docs/DECISIONS/2026-07-24-m2-pileup-cpu-budget-pins.md` (numbered pinned decisions). Must include, using REAL findings from Tasks 1-3 (not the plan's predictions — if Task 1's live capture in its Step 1 confirmed or changed the byte layout, report what actually happened):

1. Protocol details: WS URL format, handshake sequence, real confirmed SND byte layout (or however it was actually found to differ), the keepalive requirement.
2. The real device sample rate observed during Task 1's live testing (not necessarily the same node/value used during brainstorming — report what Task 1 actually saw).
3. The resampler chunk-size/latency tuning: what `RESAMPLER_CHUNK` value Task 1 landed on and why, and the real `output_delay()` value observed.
4. The `center_freq_hz` bug fix (same bug independently found and fixed once already in PR #30 — note that this is the second independent fix of the same root cause on two different branches, and whichever PR merges second will need to resolve that as a trivial merge conflict, not a real disagreement).
5. Real live-network integration test coverage achieved (the `#[ignore]`d test connecting to a real public receiver) — note this is qualitatively different from SoapySDR's situation (no real hardware ever reachable) — KiwiSDR got genuine, real integration testing, not just error-path coverage.

- [ ] **Step 3: Update ROADMAP.md**

Using Step 1's real findings: update the M2 section, following the existing "M2 sub-project N ... is complete" pattern. Update "Remaining M2 sub-projects" accurately — if this is the last one, M2's accept-criteria checklist may now be fully satisfiable (check the actual criteria list against real current state); if V8/V8w or SoapySDR are still unmerged, keep them listed, matching the accuracy discipline from the SoapySDR sub-project's own close-out.

- [ ] **Step 4: Update CLAUDE.md Status section**

Mirror the existing paragraph's style; keep the whole file under ~100 lines (`wc -l CLAUDE.md` before and after — hard constraint).

- [ ] **Step 5: Full workspace verification**

Run, in order, with direct output redirection (not a pipe through `tail`) so exit codes are trustworthy:
```
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
All three clean.

- [ ] **Step 6: Commit**

```bash
git add ROADMAP.md CLAUDE.md docs/DECISIONS/2026-07-25-m2-kiwisdr-input-pins.md
git commit -m "docs: M2 KiwiSDR input close-out, pin real findings"
```

- [ ] **Step 7: Push and open the PR**

```bash
git push -u origin feat/m2-kiwisdr-input
gh pr create --title "feat(input,engine,cli): M2 KiwiSDR input (websocket IQ client)" --body "$(cat <<'EOF'
## Summary

- ARCHITECTURE.md §3 KiwiSDR websocket IqSource (crates/skimmer-input/src/kiwi.rs), no feature gate (pure Rust).
- skimmer_engine::listen()/soak() generalized from AudioIqSource to Box<dyn IqSource> (redone independently from the separate, unmerged SoapySDR PR #30).
- Fixed the same real pre-existing bug independently found on PR #30: listen() hardcoded center_freq_hz=0.0.
- CLI --kiwi-host/--kiwi-port/--kiwi-freq/--kiwi-password flags on listen/soak.
- Real, live integration test against a public KiwiSDR receiver (not just error-path coverage — genuine network testing was possible here, unlike SoapySDR's no-hardware situation).

See docs/superpowers/specs/2026-07-25-m2-kiwisdr-input-design.md for the full design and docs/DECISIONS/2026-07-25-m2-kiwisdr-input-pins.md for real implementation findings.

## Test plan

- [x] cargo fmt --all --check
- [x] cargo clippy --workspace --all-targets -- -D warnings
- [x] cargo test --workspace
- [x] Real live connection test against a public KiwiSDR receiver (--ignored, run manually)

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Report the PR URL.
