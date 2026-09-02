# M3 Engine Wiring Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire `manta-spot`'s `Validator` into `manta-engine`'s batch (`decode_samples`/`decode_wav`) and streaming (`listen`) pipelines so real `Spot`s come out, replace the golden tests' text-substring validation stubs with the real thing, and update ROADMAP.md.

**Architecture:** `manta-spot` exposes its bundled `cty.dat`/`master.scp` data plus a `Validator::bundled(fs)` constructor. `manta-engine` takes a dependency on `manta-spot`, runs a `Validator` over its `DecoderEvent` stream in both the batch and streaming paths, and surfaces the resulting `Spot`s (new `DecodeReport.spots` field; new `listen()` `on_spot` callback). `manta-cli` prints spots in both subcommands. Golden tests swap their old substring/exact-match heuristics for real `Validator` output.

**Tech Stack:** Rust workspace, `manta-spot`/`manta-engine`/`manta-cli` crates, `cargo test`/`cargo bench` (criterion), `serde_json`.

## Global Constraints

- Deterministic decode path is a hard requirement: file input → byte-identical spot logs (SPEC §6). `Validator` is already proven deterministic; feeding it the same ordered event stream must keep producing the same spots.
- No `HashMap` on any output-order-affecting path (SPEC §6 rule 3) — not touched by this plan (no new `HashMap` introduced outside `manta-spot`, which already complies).
- `listen --json`'s new spot-line format (`{"spot": <Spot>}`) is explicitly provisional CLI-debugging output, not the ecosystem JSON contract — that's `manta-server`'s job (later M3 sub-project), and must be documented as such in a doc comment at the call site.
- Follow this repo's multi-agent hygiene (CLAUDE.md): branch/worktree isolation, `gh pr merge --auto --squash` armed immediately after opening the PR.
- `cargo test` runs at `opt-level = 1` (workspace root `Cargo.toml`) — DSP-heavy tests are slow otherwise; this is already configured, no action needed, but don't "fix" it.

---

### Task 1: `manta-spot` bundled-data constants + `Validator::bundled`

**Files:**
- Modify: `crates/manta-spot/src/lib.rs`
- Modify: `crates/manta-spot/src/validator.rs`

**Interfaces:**
- Produces: `manta_spot::CTY_DAT: &'static str`, `manta_spot::MASTER_SCP: &'static str`, `manta_spot::Validator::bundled(fs: f64) -> Validator`.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `crates/manta-spot/src/validator.rs` (alongside the existing `FS`/`CTY_FIXTURE` consts and `word_events`/`transmission_events`/`run` helpers already defined there):

```rust
    #[test]
    fn bundled_validator_spots_a_real_repeated_callsign() {
        let mut v = Validator::bundled(FS);
        let words = ["DE", "K5ARH", "K"];
        let mut spots = run(&transmission_events(1, &words, 0), &mut v);
        spots.extend(run(&transmission_events(1, &words, 100_000), &mut v));
        assert_eq!(spots.len(), 1);
        assert_eq!(spots[0].callsign, "K5ARH");
        assert_eq!(spots[0].spot_type, SpotType::De);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p manta-spot bundled_validator_spots_a_real_repeated_callsign`
Expected: FAIL to compile — `no function or associated item named 'bundled' found for struct 'Validator'`

- [ ] **Step 3: Add the bundled-data constants**

In `crates/manta-spot/src/lib.rs`, add after the existing `pub mod` declarations (before or after the existing `pub use` lines — keep the `pub use` lines grouped together):

```rust
/// AD1C's `cty.dat` country/prefix table, vendored under `data/` -- see
/// `data/SOURCES.md` for provenance and refresh instructions.
pub const CTY_DAT: &str = include_str!("../data/cty.dat");

/// The `MASTER.SCP` super-check-partial callsign list, vendored under
/// `data/` -- see `data/SOURCES.md` for provenance and refresh
/// instructions.
pub const MASTER_SCP: &str = include_str!("../data/master.scp");
```

- [ ] **Step 4: Add `Validator::bundled`**

In `crates/manta-spot/src/validator.rs`, add to `impl Validator` (immediately after the existing `pub fn new` method, before `pub fn ingest`):

```rust
    /// A production `Validator` backed by this crate's bundled `cty.dat`/
    /// `MASTER.SCP` snapshot (`crate::CTY_DAT`/`crate::MASTER_SCP`).
    pub fn bundled(fs: f64) -> Self {
        Self::new(fs, crate::CTY_DAT, Some(crate::MASTER_SCP))
    }
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p manta-spot bundled_validator_spots_a_real_repeated_callsign`
Expected: PASS

- [ ] **Step 6: Run the full `manta-spot` suite to check for regressions**

Run: `cargo test -p manta-spot`
Expected: all tests PASS (existing `validator.rs`/`cty.rs`/`scp.rs`/`gate.rs`/`dedupe.rs`/`grammar.rs`/`context.rs`/`confidence.rs` unit tests plus V11–V15 golden vectors, all unaffected by this addition)

- [ ] **Step 7: Commit**

```bash
git add crates/manta-spot/src/lib.rs crates/manta-spot/src/validator.rs
git commit -m "feat(spot): bundle cty.dat/master.scp as constants, add Validator::bundled"
```

---

### Task 2: `manta-engine` batch path — `decode_samples` emits `Spot`s

**Files:**
- Modify: `crates/manta-engine/Cargo.toml`
- Modify: `crates/manta-engine/src/lib.rs`
- Create: `crates/manta-engine/tests/spots.rs`

**Interfaces:**
- Consumes: `manta_spot::Validator::bundled(fs: f64) -> Validator` (Task 1), `Validator::ingest(&mut self, event: &DecoderEvent) -> Vec<Spot>` (existing), `manta_spot::Spot` (existing, fields: `callsign: String`, `freq_hz: f64`, `snr_db: f32`, `wpm: f32`, `spot_type: SpotType`, `confidence: f32`, `track_id: u32`, `sample_ts: u64`).
- Produces: `manta_engine::Spot` (re-export of `manta_spot::Spot`), `DecodeReport.spots: Vec<Spot>`.

- [ ] **Step 1: Write the failing test**

Create `crates/manta-engine/tests/spots.rs`:

```rust
//! `decode_samples`'s new `spots` field: a real `manta-spot::Validator`
//! run over the full multi-track event stream. M3 engine-wiring sub-
//! project, docs/superpowers/specs/2026-07-26-m3-engine-wiring-design.md.

use manta_engine::{decode_samples, PipelineConfig};
use manta_engine::SpotType;
use manta_testkit::scene::{render_scene, SignalSpec};

#[test]
fn decode_samples_spots_a_repeated_valid_callsign() {
    let sig = SignalSpec {
        text: "CQ CQ DE K5ARH K5ARH K".into(),
        loop_text: true,
        wpm: 20.0,
        offset_hz: 12_340.0,
        snr_2500_db: 20.0,
        jitter: None,
        qsb: None,
        watterson: None,
        char_wpm: None,
    };
    let (iq, _texts) =
        render_scene(std::slice::from_ref(&sig), 96_000.0, 30.0, Some(1)).unwrap();
    let report =
        decode_samples(&iq, 96_000.0, 14_000_000.0, &PipelineConfig::default()).unwrap();

    assert!(
        report.spots.iter().any(|s| s.callsign == "K5ARH"),
        "expected a K5ARH spot, got spots: {:?}",
        report.spots
    );
    let spot = report.spots.iter().find(|s| s.callsign == "K5ARH").unwrap();
    assert_eq!(spot.spot_type, SpotType::Cq);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p manta-engine --test spots`
Expected: FAIL to compile — `no field 'spots' on type 'DecodeReport'` (and `manta_engine::SpotType` doesn't exist yet)

- [ ] **Step 3: Add the `manta-spot` dependency**

In `crates/manta-engine/Cargo.toml`, add to the `[dependencies]` table (alphabetical, matching the existing ordering):

```toml
manta-spot = { workspace = true }
```

- [ ] **Step 4: Wire the `Validator` into `decode_samples` and re-export `Spot`/`SpotType`**

In `crates/manta-engine/src/lib.rs`:

Add re-exports near the top (after the existing `pub use soak::{...}` line):

```rust
pub use manta_spot::{Spot, SpotType};
```

Add `pub spots: Vec<Spot>,` to the `DecodeReport` struct, after the existing `pub events: Vec<DecoderEvent>,` field:

```rust
    /// The full decoder event stream, for JSON output.
    pub events: Vec<DecoderEvent>,
    /// Validated spots (`manta-spot::Validator`, ARCHITECTURE §6), run
    /// over the full multi-track event stream above.
    pub spots: Vec<Spot>,
```

In `decode_samples`, immediately before the final `Ok(DecodeReport { ... })` (i.e. right after the existing `let text = events_to_text(&this_track);` line), add:

```rust
    let mut validator = manta_spot::Validator::bundled(fs);
    let mut spots = Vec::new();
    for ev in &events {
        spots.extend(validator.ingest(ev));
    }
```

Then update the `Ok(DecodeReport { ... })` literal to include the new field:

```rust
    Ok(DecodeReport {
        freq_hz,
        wpm,
        text,
        events,
        spots,
    })
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p manta-engine --test spots`
Expected: PASS

- [ ] **Step 6: Run the full `manta-engine` suite to check for regressions**

Run: `cargo test -p manta-engine`
Expected: all non-ignored tests PASS (`pipeline.rs`, `chunking_determinism.rs`, `channelizer_chunking_determinism.rs`, `regression_char_gap_high_wpm.rs`, `roundtrip_iq.rs`, `track.rs` unit tests, `soak.rs` unit test — none reference `DecodeReport`'s field list positionally, so the new field doesn't break them)

- [ ] **Step 7: Commit**

```bash
git add crates/manta-engine/Cargo.toml crates/manta-engine/src/lib.rs crates/manta-engine/tests/spots.rs
git commit -m "feat(engine): wire manta-spot::Validator into decode_samples"
```

---

### Task 3: `manta-engine` streaming path — `listen` gains `on_spot`

**Files:**
- Modify: `crates/manta-engine/src/listen.rs`
- Modify: `crates/manta-engine/src/soak.rs`
- Modify: `crates/manta-engine/tests/listen_audio.rs`
- Create: `crates/manta-engine/tests/listen_spots.rs`

**Interfaces:**
- Consumes: `manta_spot::Validator::bundled(fs: f64)` (Task 1), `manta_engine::Spot` (Task 2).
- Produces: `manta_engine::listen(src, cfg, stop, on_event: impl FnMut(&DecoderEvent), on_spot: impl FnMut(&Spot)) -> Result<()>` (signature change — every caller updates in this task).

- [ ] **Step 1: Write the failing test**

Create `crates/manta-engine/tests/listen_spots.rs`:

```rust
//! `listen()`'s new `on_spot` callback: a real `manta-spot::Validator`
//! run over the streamed event sequence. Uses a raw-complex-IQ in-memory
//! source (not `AudioIqSource`) to avoid the pre-existing near-DC Hilbert
//! leakage tracked as issue #21 (see `listen_audio.rs`'s doc comment) --
//! unrelated to this task, and this test doesn't need real audio hardware
//! semantics to exercise `on_spot`.

use num_complex::Complex32;
use manta_engine::{listen, PipelineConfig, Spot};
use manta_testkit::scene::{render_scene, SignalSpec};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

struct FixedFreqSource {
    samples: Vec<Complex32>,
    cursor: usize,
    fs: f64,
    center_freq_hz: f64,
}

impl manta_input::IqSource for FixedFreqSource {
    fn sample_rate(&self) -> f64 {
        self.fs
    }
    fn center_freq_hz(&self) -> f64 {
        self.center_freq_hz
    }
    fn read(&mut self, buf: &mut [Complex32]) -> anyhow::Result<usize> {
        let n = buf.len().min(self.samples.len() - self.cursor);
        buf[..n].copy_from_slice(&self.samples[self.cursor..self.cursor + n]);
        self.cursor += n;
        Ok(n)
    }
}

#[test]
fn listen_emits_a_spot_via_on_spot() {
    let sig = SignalSpec {
        text: "CQ CQ DE K5ARH K5ARH K".into(),
        loop_text: true,
        wpm: 20.0,
        offset_hz: 12_340.0,
        snr_2500_db: 20.0,
        jitter: None,
        qsb: None,
        watterson: None,
        char_wpm: None,
    };
    let (samples, _texts) =
        render_scene(std::slice::from_ref(&sig), 96_000.0, 30.0, Some(1)).unwrap();
    let src: Box<dyn manta_input::IqSource> = Box::new(FixedFreqSource {
        samples,
        cursor: 0,
        fs: 96_000.0,
        center_freq_hz: 14_000_000.0,
    });

    let stop = Arc::new(AtomicBool::new(false));
    let mut spots: Vec<Spot> = Vec::new();
    listen(
        src,
        &PipelineConfig::default(),
        stop,
        |_ev| {},
        |spot| spots.push(spot.clone()),
    )
    .unwrap();

    assert!(
        spots.iter().any(|s| s.callsign == "K5ARH"),
        "expected a K5ARH spot from on_spot, got: {spots:?}"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p manta-engine --test listen_spots`
Expected: FAIL to compile — `this function takes 4 arguments but 5 arguments were supplied` (or similar arity mismatch against the current `listen` signature)

- [ ] **Step 3: Change `listen`'s signature and wire the `Validator`**

In `crates/manta-engine/src/listen.rs`, change the function signature:

```rust
pub fn listen(
    mut src: Box<dyn IqSource>,
    cfg: &PipelineConfig,
    stop: Arc<AtomicBool>,
    mut on_event: impl FnMut(&DecoderEvent),
    mut on_spot: impl FnMut(&crate::Spot),
) -> Result<()> {
```

Add the import at the top of the file (alongside the existing `use` block):

```rust
use manta_spot::Validator;
```

After the existing `let mut tm = crate::track::TrackManager::new(...)` block (which already has `fs` in scope), construct the validator:

```rust
    let mut validator = Validator::bundled(fs);
```

Replace each of the three `for ev in tm.process_hops(...) { on_event(&ev); }`-shaped loops (the two startup ones over `padding`/`calib`, and the main streaming loop) so every `on_event(&ev)` call is immediately followed by validation. Concretely, change this pattern (appearing 3 times: padding, calib, and the main `loop { ... }` body):

```rust
        for ev in tm.process_hops(&ch.process(&padding), |m| m.saturating_sub(pad_hops) * hop) {
            on_event(&ev);
        }
```

to:

```rust
        for ev in tm.process_hops(&ch.process(&padding), |m| m.saturating_sub(pad_hops) * hop) {
            on_event(&ev);
            for spot in validator.ingest(&ev) {
                on_spot(&spot);
            }
        }
```

Apply the same `on_event(&ev); for spot in validator.ingest(&ev) { on_spot(&spot); }` body to the `calib` loop and to the main `while`/`loop`'s `for ev in tm.process_hops(&ch.process(&chunk[..n]), ...) { ... }` block. Also update the final drain:

```rust
    for ev in tm.finish() {
        on_event(&ev);
    }
```

to:

```rust
    for ev in tm.finish() {
        on_event(&ev);
        for spot in validator.ingest(&ev) {
            on_spot(&spot);
        }
    }
```

- [ ] **Step 4: Update `listen`'s own test module**

In `crates/manta-engine/src/listen.rs`'s `#[cfg(test)] mod tests`, the existing `listen_uses_the_sources_center_freq_hz_not_a_hardcoded_zero` test calls `listen(src, &PipelineConfig::default(), stop, |ev| { ... })` with one closure. Update that call site to pass a no-op second closure:

```rust
        listen(src, &PipelineConfig::default(), stop, |ev| {
            if let DecoderEvent::TrackMeta { freq_hz, .. } = ev {
                last_freq_hz = Some(*freq_hz);
            }
        }, |_spot| {})
        .unwrap();
```

- [ ] **Step 5: Update `soak.rs`'s call site**

In `crates/manta-engine/src/soak.rs`, change:

```rust
        listen(src, cfg, stop.clone(), |_ev| {
            event_count += 1;
            if start.elapsed() >= WARMUP {
                let rss = peak_rss_bytes();
                worst_growth = worst_growth.max(rss.saturating_sub(baseline_rss));
            }
        })
```

to:

```rust
        listen(src, cfg, stop.clone(), |_ev| {
            event_count += 1;
            if start.elapsed() >= WARMUP {
                let rss = peak_rss_bytes();
                worst_growth = worst_growth.max(rss.saturating_sub(baseline_rss));
            }
        }, |_spot| {})
```

- [ ] **Step 6: Update `listen_audio.rs`'s call site**

`crates/manta-engine/tests/listen_audio.rs`'s `listen_decodes_a_clean_real_audio_signal` test (already `#[ignore]`d, issue #21) has this call site:

```rust
    listen(src, &PipelineConfig::default(), stop, move |ev| {
        if let manta_decode::events::DecoderEvent::CharDecoded { glyph, .. } = ev {
            if let Some(c) = glyph.text_char() {
                text_clone.lock().unwrap().push(c);
            }
        }
        if matches!(
            ev,
            manta_decode::events::DecoderEvent::WordBoundary { .. }
        ) {
            text_clone.lock().unwrap().push(' ');
        }
    })
    .unwrap();
```

Change the closing `})` / `.unwrap();` lines to add a second, no-op closure:

```rust
    listen(src, &PipelineConfig::default(), stop, move |ev| {
        if let manta_decode::events::DecoderEvent::CharDecoded { glyph, .. } = ev {
            if let Some(c) = glyph.text_char() {
                text_clone.lock().unwrap().push(c);
            }
        }
        if matches!(
            ev,
            manta_decode::events::DecoderEvent::WordBoundary { .. }
        ) {
            text_clone.lock().unwrap().push(' ');
        }
    }, |_spot| {})
    .unwrap();
```

Do not change anything else in this file — it must keep compiling but its `#[ignore]` and existing doc comment (issue #21) are unrelated to this task and stay as-is.

- [ ] **Step 7: Run test to verify it passes**

Run: `cargo test -p manta-engine --test listen_spots`
Expected: PASS

- [ ] **Step 8: Run the full `manta-engine` suite (including ignored tests, to confirm they still compile) to check for regressions**

Run: `cargo test -p manta-engine && cargo test -p manta-engine -- --ignored --list`
Expected: first command's non-ignored tests all PASS; second command lists the ignored tests without a compile error (confirms `listen_audio.rs` still builds)

- [ ] **Step 9: Commit**

```bash
git add crates/manta-engine/src/listen.rs crates/manta-engine/src/soak.rs crates/manta-engine/tests/listen_audio.rs crates/manta-engine/tests/listen_spots.rs
git commit -m "feat(engine): listen() gains on_spot callback, wired to manta-spot::Validator"
```

---

### Task 4: `manta-cli` — surface spots from `decode` and `listen`

**Files:**
- Modify: `crates/manta-cli/src/main.rs`
- Modify: `crates/manta-cli/tests/cli.rs`

**Interfaces:**
- Consumes: `DecodeReport.spots: Vec<Spot>` (Task 2), `manta_engine::listen(..., on_event, on_spot)` (Task 3), `Spot` fields (`callsign`, `freq_hz`, `snr_db`, `wpm`, `spot_type`, `confidence`).

- [ ] **Step 1: Write the failing test**

Add to `crates/manta-cli/tests/cli.rs` (new test, alongside the existing ones — check the file's existing `use`/helper patterns before inserting; it already has `Command::new(env!("CARGO_BIN_EXE_manta"))`-style invocations for the `listen --kiwi-host`/`--soapy-driver` arg-parsing tests):

```rust
#[test]
fn decode_json_includes_spots_field() {
    let dir = tempfile::tempdir().unwrap();
    let spec = manta_testkit::vectors::v1();
    manta_testkit::vectors::write_fixture_set(&spec, dir.path()).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_manta"))
        .args(["decode", "--json"])
        .arg(dir.path().join(format!("{}.wav", spec.name)))
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        report.get("spots").is_some_and(|s| s.is_array()),
        "expected a 'spots' array field in decode --json output, got: {report}"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p manta-cli --test cli decode_json_includes_spots_field`
Expected: PASS already, actually — `DecodeReport` derives `serde::Serialize` on all fields including the new `spots: Vec<Spot>` from Task 2, so this assertion should already hold once Task 2 landed. Run it anyway to confirm; if it unexpectedly fails, that means Task 2's field wasn't wired correctly — stop and re-check Task 2 rather than proceeding.

- [ ] **Step 3: Add human-readable spot output to `decode`**

In `crates/manta-cli/src/main.rs`, in the `Command::Decode { path, json }` arm, change:

```rust
        Command::Decode { path, json } => {
            let report = decode_wav(&path, &PipelineConfig::default())?;
            if json {
                println!("{}", serde_json::to_string(&report)?);
            } else {
                println!("{}", report.text);
                eprintln!("freq_hz: {:.1}  wpm: {:?}", report.freq_hz, report.wpm);
            }
        }
```

to:

```rust
        Command::Decode { path, json } => {
            let report = decode_wav(&path, &PipelineConfig::default())?;
            if json {
                println!("{}", serde_json::to_string(&report)?);
            } else {
                println!("{}", report.text);
                eprintln!("freq_hz: {:.1}  wpm: {:?}", report.freq_hz, report.wpm);
                eprintln!("spots: {}", report.spots.len());
            }
        }
```

- [ ] **Step 4: Add spot output to `listen`**

In `crates/manta-cli/src/main.rs`, in the `Command::Listen { ... }` arm, change the `manta_engine::listen(src, &PipelineConfig::default(), stop, |ev| { ... })?;` call. First, the current body:

```rust
            manta_engine::listen(src, &PipelineConfig::default(), stop, |ev| {
                if json {
                    println!("{}", serde_json::to_string(ev).unwrap());
                    return;
                }
                use manta_decode::events::DecoderEvent;
                use std::io::Write as _;
                match ev {
                    DecoderEvent::CharDecoded { glyph, .. } => {
                        if let Some(c) = glyph.text_char() {
                            print!("{c}");
                            let _ = std::io::stdout().flush();
                        }
                    }
                    DecoderEvent::WordBoundary { .. } => {
                        print!(" ");
                        let _ = std::io::stdout().flush();
                    }
                    _ => {}
                }
            })?;
```

becomes:

```rust
            manta_engine::listen(
                src,
                &PipelineConfig::default(),
                stop,
                |ev| {
                    if json {
                        println!("{}", serde_json::to_string(ev).unwrap());
                        return;
                    }
                    use manta_decode::events::DecoderEvent;
                    use std::io::Write as _;
                    match ev {
                        DecoderEvent::CharDecoded { glyph, .. } => {
                            if let Some(c) = glyph.text_char() {
                                print!("{c}");
                                let _ = std::io::stdout().flush();
                            }
                        }
                        DecoderEvent::WordBoundary { .. } => {
                            print!(" ");
                            let _ = std::io::stdout().flush();
                        }
                        _ => {}
                    }
                },
                // Provisional CLI-debugging spot output only -- NOT the
                // ecosystem JSON contract. manta-server (a later M3
                // sub-project) defines the real spot wire format.
                |spot| {
                    if json {
                        println!(
                            "{}",
                            serde_json::json!({ "spot": spot }).to_string()
                        );
                        return;
                    }
                    eprintln!(
                        "SPOT: {} ({:?}) {:.1} Hz {:.0} dB {:.0} wpm conf={:.2}",
                        spot.callsign,
                        spot.spot_type,
                        spot.freq_hz,
                        spot.snr_db,
                        spot.wpm,
                        spot.confidence
                    );
                },
            )?;
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p manta-cli --test cli decode_json_includes_spots_field`
Expected: PASS

- [ ] **Step 6: Run the full `manta-cli` `cli.rs` suite to check for regressions**

Run: `cargo test -p manta-cli --test cli`
Expected: all tests PASS, including the pre-existing `listen --kiwi-host`/`--soapy-driver` arg-parsing tests (unaffected — they never reach the `on_spot` closure since those tests fail before opening a source)

- [ ] **Step 7: Commit**

```bash
git add crates/manta-cli/src/main.rs crates/manta-cli/tests/cli.rs
git commit -m "feat(cli): surface validated spots from decode/listen"
```

---

### Task 5: Rewrite `golden_v8_v8w.rs` onto real `Spot` output

**Files:**
- Modify: `crates/manta-cli/tests/golden_v8_v8w.rs`

**Interfaces:**
- Consumes: `report["spots"]` JSON array (Task 4), each element having `callsign: String`, `track_id: u64` (serialized `u32` fields deserialize as JSON numbers, read via `.as_u64()`/`.as_str()` like the existing `per_track` helper already does for `events`).

- [ ] **Step 1: Update the module doc comment**

Replace the file's top doc comment:

```rust
//! SPEC §7 V8/V8w pileup golden gates. "Callsign validated"/"bogus
//! callsign"/"ghost decode" approximate the future manta-spot validator
//! (M3) the same way V5/V6 approximate "callsign validated" today -- see
//! docs/superpowers/specs/2026-07-24-m2-pileup-cpu-budget-design.md.
```

with:

```rust
//! SPEC §7 V8/V8w pileup golden gates. "Callsign validated"/"bogus
//! callsign"/"ghost decode" are measured against the real
//! `manta-spot::Validator`'s output (`report["spots"]`), wired into
//! `decode_samples` in M3's engine-wiring sub-project -- see
//! docs/superpowers/specs/2026-07-26-m3-engine-wiring-design.md. Previously
//! approximated with text-substring heuristics against raw decoder text,
//! the same way V5/V6 approximated "callsign validated" before this landed
//! -- see docs/superpowers/specs/2026-07-24-m2-pileup-cpu-budget-design.md.
```

- [ ] **Step 2: Remove the now-unused `bogus_calls` helper**

Delete the `bogus_calls` function entirely:

```rust
/// Callsign-shaped, >=2-rep tokens in `decoded_text` that are not in
/// `known_calls` -- SPEC §7 V8/V8w's "0 bogus callsigns spotted".
fn bogus_calls(decoded_text: &str, known_calls: &HashSet<&str>) -> Vec<String> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for word in decoded_text.split_whitespace() {
        if (3..=7).contains(&word.len())
            && word.chars().all(|c| c.is_ascii_alphanumeric())
            && word.chars().any(|c| c.is_ascii_digit())
            && word.chars().any(|c| c.is_ascii_alphabetic())
        {
            *counts.entry(word).or_insert(0) += 1;
        }
    }
    counts
        .into_iter()
        .filter(|&(word, n)| n >= 2 && !known_calls.contains(word))
        .map(|(word, _)| word.to_string())
        .collect()
}
```

Update the `use std::collections::{BTreeMap, HashMap, HashSet};` line at the top of the file to drop `HashMap` (no longer used anywhere in the file after this task):

```rust
use std::collections::{BTreeMap, HashSet};
```

- [ ] **Step 3: Add a `spotted_calls` helper**

Add this helper function, placed right after `match_tracks_by_freq` (before `bogus_calls` used to be):

```rust
/// `(callsign, track_id)` pairs from `report["spots"]` -- the real
/// `manta-spot::Validator`'s output, not a text-heuristic approximation.
fn spotted_calls(report: &serde_json::Value) -> Vec<(String, u64)> {
    report["spots"]
        .as_array()
        .expect("decode --json output must include a 'spots' array")
        .iter()
        .map(|s| {
            (
                s["callsign"].as_str().unwrap().to_string(),
                s["track_id"].as_u64().unwrap(),
            )
        })
        .collect()
}
```

- [ ] **Step 4: Rewrite `v8_pileup_validates_at_least_45_of_50_with_no_bogus_calls`**

Replace the test body:

```rust
#[test]
fn v8_pileup_validates_at_least_45_of_50_with_no_bogus_calls() {
    let spec = manta_testkit::vectors::v8();
    let (report, manifest) = decode_report(&spec);
    let tracks = per_track(&report);
    let known_calls: HashSet<&str> = manifest
        .keyed_texts
        .iter()
        .map(|t| call_from_keyed_text(t))
        .collect();
    assert_eq!(
        known_calls.len(),
        50,
        "V8 fixture must have 50 unique callsigns"
    );

    let matched = match_tracks_by_freq(&manifest, &tracks);
    let mut validated = 0;
    for (i, keyed_text) in manifest.keyed_texts.iter().enumerate() {
        let call = call_from_keyed_text(keyed_text);
        let (decoded_text, _freq) = matched[i];
        if decoded_text.matches(call).count() >= 2 {
            validated += 1;
        }
    }
    assert!(
        validated >= 45,
        "V8 must validate >= 45/50 callsigns, got {validated}/50"
    );

    let mut bogus = Vec::new();
    for (decoded_text, _freq) in tracks.values() {
        bogus.extend(bogus_calls(decoded_text, &known_calls));
    }
    assert!(
        bogus.is_empty(),
        "V8 must spot 0 bogus callsigns, got {bogus:?}"
    );
}
```

with:

```rust
#[test]
fn v8_pileup_validates_at_least_45_of_50_with_no_bogus_calls() {
    let spec = manta_testkit::vectors::v8();
    let (report, manifest) = decode_report(&spec);
    let known_calls: HashSet<&str> = manifest
        .keyed_texts
        .iter()
        .map(|t| call_from_keyed_text(t))
        .collect();
    assert_eq!(
        known_calls.len(),
        50,
        "V8 fixture must have 50 unique callsigns"
    );

    let spots = spotted_calls(&report);
    let spotted: HashSet<&str> = spots.iter().map(|(c, _)| c.as_str()).collect();

    let validated = known_calls.iter().filter(|c| spotted.contains(**c)).count();
    assert!(
        validated >= 45,
        "V8 must validate >= 45/50 callsigns, got {validated}/50 (spotted: {spotted:?})"
    );

    let bogus: Vec<&str> = spotted
        .iter()
        .filter(|c| !known_calls.contains(**c))
        .copied()
        .collect();
    assert!(
        bogus.is_empty(),
        "V8 must spot 0 bogus callsigns, got {bogus:?}"
    );
}
```

Note: `per_track`/`match_tracks_by_freq` are still used by the `v8w_...` test below (Step 5), so keep both functions defined in the file — do not delete them in this step.

- [ ] **Step 5: Rewrite `v8w_pileup_fading_decodes_90pct_of_strong_signals_no_ghosts`'s bogus/ghost checks**

This test stays `#[ignore]`d (issue #28, unrelated to this task) and keeps its CER measurement (`per_track`/`match_tracks_by_freq`/`manta_testkit::cer::cer`) unchanged. Only its trailing bogus-calls and ghost-decode blocks change. Replace:

```rust
    let mut bogus = Vec::new();
    for (decoded_text, _freq) in tracks.values() {
        bogus.extend(bogus_calls(decoded_text, &known_calls));
    }
    assert!(
        bogus.is_empty(),
        "V8w must spot 0 bogus callsigns, got {bogus:?}"
    );

    // 0 cross-channel ghost decodes: no fixture call's >=2-rep substring
    // appears in more than one distinct track.
    for call in &known_calls {
        let hits = tracks
            .values()
            .filter(|(text, _)| text.matches(call).count() >= 2)
            .count();
        assert!(
            hits <= 1,
            "callsign {call} decoded (>=2 reps) in {hits} distinct tracks, expected <= 1 (ghost decode)"
        );
    }
}
```

with:

```rust
    let spots = spotted_calls(&report);
    let spotted: HashSet<&str> = spots.iter().map(|(c, _)| c.as_str()).collect();
    let bogus: Vec<&str> = spotted
        .iter()
        .filter(|c| !known_calls.contains(**c))
        .copied()
        .collect();
    assert!(
        bogus.is_empty(),
        "V8w must spot 0 bogus callsigns, got {bogus:?}"
    );

    // 0 cross-channel ghost decodes: no known call's spots span more than
    // one distinct track_id.
    for call in &known_calls {
        let track_ids: HashSet<u64> = spots
            .iter()
            .filter(|(c, _)| c == call)
            .map(|(_, tid)| *tid)
            .collect();
        assert!(
            track_ids.len() <= 1,
            "callsign {call} spotted from {} distinct tracks, expected <= 1 (ghost decode)",
            track_ids.len()
        );
    }
}
```

- [ ] **Step 6: Run the non-ignored test to verify it passes**

Run: `cargo test -p manta-cli --test golden_v8_v8w v8_pileup_validates_at_least_45_of_50_with_no_bogus_calls`
Expected: PASS — report the actual `validated`/50 count from the test output (via `--nocapture` if needed: `cargo test -p manta-cli --test golden_v8_v8w v8_pileup_validates_at_least_45_of_50_with_no_bogus_calls -- --nocapture`) so it can be compared against the pre-change baseline

- [ ] **Step 7: Confirm the ignored test still compiles**

Run: `cargo test -p manta-cli --test golden_v8_v8w -- --ignored --list`
Expected: lists `v8w_pileup_fading_decodes_90pct_of_strong_signals_no_ghosts` with no compile error

- [ ] **Step 8: Commit**

```bash
git add crates/manta-cli/tests/golden_v8_v8w.rs
git commit -m "test(spot): swap V8/V8w golden gates onto real Validator output"
```

---

### Task 6: Rewrite `golden_v2_v3.rs`'s V5 validation stub onto real `Spot` output

**Files:**
- Modify: `crates/manta-cli/tests/golden_v2_v3.rs`

**Interfaces:**
- Consumes: `report["spots"]` JSON array (Task 4), each element having `callsign: String`, `sample_ts: u64`.

- [ ] **Step 1: Rewrite `v5_passes_end_to_end_from_wav`'s validation block**

The test stays `#[ignore]`d (V5's CER-under-fading failure is unchanged and unrelated). Its doc comment currently reads:

```rust
/// Ignored: WattersonPreset::Poor at V5's 3 dB SNR produces near-continuous
/// fading with essentially no calm stretches (coherence time ~0.32s vs a
/// 22 WPM dit's ~54ms -- multiple dits per fade cycle). An exhaustive
/// 60-seed sweep of WattersonFade.seed found zero candidates meeting the
/// SPEC §7 CER <= 0.20 threshold (best of 60 was 0.38, roughly 2x over).
/// Pure-AWGN decode at the same 3 dB SNR (no fading) is CER=0, ruling out
/// an SNR-headroom bug -- this is a genuine classical-decoder fading-
/// robustness gap, consistent with this project's stated design (CLAUDE.md:
/// "Classical decoder first; ML fusion ... only at M4, gated on beating the
/// classical baseline under simulated fading"). Tracked in the M1 pinned-
/// decisions doc; revisit once manta-decode gains real fading resilience
/// (M4) or a different mitigation is found.
```

Leave that doc comment as-is (it explains the CER failure, unrelated to this task) and append one paragraph directly below it, still inside the same doc comment block, immediately before the `#[test]`/`#[ignore]` lines:

```rust
///
/// Its "callsign validated within 90 s" check below now uses the real
/// `manta-spot::Validator` (`report["spots"]`, M3 engine-wiring
/// sub-project) instead of the earlier running-decoded-text substring scan
/// -- see docs/superpowers/specs/2026-07-26-m3-engine-wiring-design.md.
```

Then replace the test's trailing validation block:

```rust
    // Callsign validated within 90 s: find the sample_ts at which "ZL2XYZ"
    // first appears as a contiguous substring of the running decoded text.
    // M1 doesn't have manta-spot's callsign validation yet, so this
    // approximates ROADMAP's "callsign validated within 90 s" gate.
    // sample_ts is in raw input samples at manifest.fs (SPEC §1.1).
    let events = report["events"].as_array().unwrap();
    let mut running = String::new();
    let mut validated_ts: Option<f64> = None;
    for ev in events {
        if ev["event"].as_str() == Some("CharDecoded") {
            if let Some(c) = ev["glyph"]["Char"].as_str() {
                running.push_str(c);
            }
            if validated_ts.is_none() && running.contains("ZL2XYZ") {
                validated_ts = ev["sample_ts"].as_u64().map(|ts| ts as f64);
            }
        }
    }
    let validated_ts = validated_ts.expect("ZL2XYZ never appeared in decoded output");
    assert!(
        validated_ts <= 90.0 * manifest.fs,
        "ZL2XYZ validated at {:.1} s, expected <= 90 s",
        validated_ts / manifest.fs
    );
}
```

with:

```rust
    // Callsign validated within 90 s: the real Validator's first ZL2XYZ
    // spot's sample_ts. sample_ts is in raw input samples at manifest.fs
    // (SPEC §1.1).
    let validated_ts = report["spots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["callsign"].as_str() == Some("ZL2XYZ"))
        .map(|s| s["sample_ts"].as_u64().unwrap() as f64)
        .expect("ZL2XYZ never validated as a spot");
    assert!(
        validated_ts <= 90.0 * manifest.fs,
        "ZL2XYZ validated at {:.1} s, expected <= 90 s",
        validated_ts / manifest.fs
    );
}
```

- [ ] **Step 2: Confirm the file still compiles and the ignored test lists correctly**

Run: `cargo test -p manta-cli --test golden_v2_v3 -- --ignored --list`
Expected: lists `v2_passes_end_to_end_from_wav`, `v5_passes_end_to_end_from_wav`, `v6_passes_end_to_end_from_wav` with no compile error

- [ ] **Step 3: Run the non-ignored tests in this file to check for regressions**

Run: `cargo test -p manta-cli --test golden_v2_v3`
Expected: `v3_passes_end_to_end_from_wav` and `v4_passes_end_to_end_from_wav` PASS (unaffected by this task's changes)

- [ ] **Step 4: Commit**

```bash
git add crates/manta-cli/tests/golden_v2_v3.rs
git commit -m "test(spot): swap V5's validation stub onto real Validator output"
```

---

### Task 7: Re-measure CPU-budget Mac leg, update ROADMAP.md

**Files:**
- Modify: `ROADMAP.md`

**Interfaces:**
- None (documentation-only task; no new code interfaces).

- [ ] **Step 1: Re-run the CPU-budget Mac-leg test in release mode**

Run: `cargo test -p manta-engine --release --test cpu_budget -- --ignored --nocapture`
Expected: the test prints a line like `cpu_budget: {elapsed}s wall / {audio_duration}s audio = {ratio}x realtime (Mac budget: < 0.5x)`, and the assertion `ratio < 0.5` PASSes. Record the exact `{ratio}` value for the ROADMAP update in the next step. If it does NOT pass (i.e., the ratio regressed to >= 0.5x after wiring `Validator` into `decode_samples`), stop and report this — do not silently widen the budget or skip the assertion; this is a real finding that needs a decision, not a plan step to route around.

- [ ] **Step 2: Update ROADMAP.md's M2 section with the re-measured ratio**

In `ROADMAP.md`, find the M2 "Accept when" bullet:

```markdown
- Criterion bench: full pipeline at 192 kS/s with 300 active tracks uses < 50 %
  of one core on an M-series Mac AND < 1 core on a Raspberry Pi 4. Mac leg
  passes (0.36x realtime, `crates/manta-engine/benches/cpu_budget.rs`);
  **Pi4 leg outstanding** — needs real Raspberry Pi 4 hardware.
```

Replace `0.36x realtime` with the ratio measured in Step 1, and add a clause noting the measurement now includes M3 validation cost:

```markdown
- Criterion bench: full pipeline at 192 kS/s with 300 active tracks uses < 50 %
  of one core on an M-series Mac AND < 1 core on a Raspberry Pi 4. Mac leg
  passes ({RATIO}x realtime, now including `manta-spot::Validator` cost
  per M3's engine-wiring sub-project — see
  `crates/manta-engine/benches/cpu_budget.rs`); **Pi4 leg outstanding** —
  needs real Raspberry Pi 4 hardware.
```

(Replace `{RATIO}` with the actual measured number, e.g. `0.38`.)

- [ ] **Step 3: Update ROADMAP.md's M3 section**

Replace:

```markdown
`manta-spot` (callsign/CQ-DE validation, cty.dat/SCP cross-check,
repetition gate, dedupe) is complete as a standalone crate -- see
`docs/superpowers/specs/2026-07-25-m3-manta-spot-design.md` and SPEC
-decode-core.md §7.1 (V11-V15). Remaining M3 sub-projects: wiring
`manta-spot` into `manta-engine`'s live pipeline, `manta-server`
(telnet + JSON/WebSocket output, TOML config, metrics), and the RBN parity
benchmark (needs ≥ 2 h of recorded contest-weekend IQ with RBN reference
spots -- a data dependency not yet resolved).
```

with:

```markdown
`manta-spot` (callsign/CQ-DE validation, cty.dat/SCP cross-check,
repetition gate, dedupe) is complete as a standalone crate -- see
`docs/superpowers/specs/2026-07-25-m3-manta-spot-design.md` and SPEC
-decode-core.md §7.1 (V11-V15). It is now wired into `manta-engine`'s
batch (`decode_samples`/`decode_wav`) and streaming (`listen`) pipelines,
both emitting real `Spot`s -- see
`docs/superpowers/specs/2026-07-26-m3-engine-wiring-design.md`. Remaining
M3 sub-projects: `manta-server` (telnet + JSON/WebSocket output, TOML
config, metrics), and the RBN parity benchmark (needs ≥ 2 h of recorded
contest-weekend IQ with RBN reference spots -- a data dependency not yet
resolved).
```

- [ ] **Step 4: Commit**

```bash
git add ROADMAP.md
git commit -m "docs(m3): engine-wiring sub-project complete, re-measured CPU-budget Mac leg"
```

---

### Task 8: Full workspace verification and PR

**Files:**
- None (verification-only task).

- [ ] **Step 1: Run the full workspace test suite**

Run: `cargo test --workspace`
Expected: all non-ignored tests PASS across every crate (`manta-decode`, `manta-dsp`, `manta-input`, `manta-testkit`, `manta-engine`, `manta-spot`, `manta-cli`)

- [ ] **Step 2: Run clippy across the workspace**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: no warnings (this repo treats clippy warnings as errors per its established CI gate — confirm by checking `.github/workflows/*.yml` if unsure of the exact invocation used in CI, and match it)

- [ ] **Step 3: Confirm formatting**

Run: `cargo fmt --all -- --check`
Expected: no diff

- [ ] **Step 4: Push and open a PR**

```bash
git push -u origin HEAD
gh pr create --title "feat(m3): wire manta-spot::Validator into manta-engine" --body "$(cat <<'EOF'
## Summary
- Wires the M3 `manta-spot` `Validator` (PR #34) into `manta-engine`'s batch (`decode_samples`/`decode_wav`) and streaming (`listen`) pipelines -- both now emit real `Spot`s instead of raw decoder text.
- `manta-cli`'s `decode`/`listen` subcommands surface spots (JSON and human-readable; `listen --json`'s spot format is explicitly provisional, not the ecosystem contract).
- `golden_v8_v8w.rs`/`golden_v2_v3.rs` (V5) swap their text-substring validation stubs for the real `Validator`'s output.
- ROADMAP.md M3 status updated; CPU-budget Mac leg re-measured with validation cost included.

See docs/superpowers/specs/2026-07-26-m3-engine-wiring-design.md and docs/superpowers/plans/2026-07-26-m3-engine-wiring.md.

## Test plan
- [x] `cargo test --workspace`
- [x] `cargo clippy --workspace --all-targets -- -D warnings`
- [x] `cargo fmt --all -- --check`
- [x] `cargo test -p manta-engine --release --test cpu_budget -- --ignored --nocapture` (Mac CPU-budget leg re-measured)
EOF
)"
gh pr merge --auto --squash
```

- [ ] **Step 5: Confirm auto-merge is armed**

Run: `gh pr view --json autoMergeRequest`
Expected: `autoMergeRequest` is non-null, confirming squash auto-merge is armed per this repo's PR policy (docs/DECISIONS/2026-07-25-pr-auto-merge-policy.md)
