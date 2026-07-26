# M3 sub-project 2 — wire `skimmer-spot` into `skimmer-engine`: Design

Design for the second of M3's independent sub-projects (ROADMAP.md "M3 —
Spots: validation + servers + RBN parity benchmark"). `skimmer-spot`
(ARCHITECTURE §6's validation pipeline: context parse, grammar, cty.dat,
SCP, confidence, repetition gate, dedupe) landed as a standalone crate in
PR #34, deliberately scoped to stop short of engine wiring — see
`docs/superpowers/specs/2026-07-25-m3-skimmer-spot-design.md` §1. This
sub-project wires it in: real `Spot`s come out of `skimmer-engine`'s batch
and streaming pipelines instead of raw decoder text, and the golden tests
that previously approximated validation with text-substring heuristics
swap onto the real thing.

## 1. Scope

In scope:

- `skimmer-spot` exposes its bundled `cty.dat`/`master.scp` data as public
  constants plus a `Validator::bundled(fs)` convenience constructor.
- `skimmer-engine` takes a dependency on `skimmer-spot`; `decode_samples`/
  `decode_wav` run a `Validator` over the full multi-track event stream and
  return the resulting `Spot`s as a new `DecodeReport` field.
- `skimmer-engine::listen` gains a second callback, `on_spot`, invoked for
  every `Spot` a live `Validator` emits as events stream through.
- `skimmer-cli`'s `decode`/`listen` subcommands surface spots (JSON and
  human-readable).
- `golden_v8_v8w.rs` and `golden_v2_v3.rs` (V5) swap their
  exact-match-against-fixture/substring-count validation stubs for the real
  `Validator`'s output.
- ROADMAP.md M3 status update.

Explicitly out of scope (later M3 sub-projects, per the skimmer-spot design
doc's §1):

- `skimmer-server` (telnet cluster + JSON Lines/WebSocket transport), TOML
  config loading, metrics endpoint. `listen --json`'s spot-line format
  introduced here is explicitly provisional CLI-debugging output, not the
  ecosystem JSON contract skimmer-server will define.
- The RBN parity benchmark and its golden IQ corpus (data dependency still
  unresolved).
- Any change to `skimmer-spot`'s validation logic itself (context/grammar/
  cty/scp/confidence/gate/dedupe) — this sub-project only wires the
  already-complete, already-tested `Validator` in.

## 2. Components

### `skimmer-spot`: bundled-data ownership

`crates/skimmer-spot/src/lib.rs` gains:

```rust
pub const CTY_DAT: &str = include_str!("../data/cty.dat");
pub const MASTER_SCP: &str = include_str!("../data/master.scp");
```

and `validator.rs` gains:

```rust
impl Validator {
    pub fn bundled(fs: f64) -> Self {
        Self::new(fs, CTY_DAT, Some(MASTER_SCP))
    }
}
```

Callers never reference the data files directly — `skimmer-spot` is the
sole owner of "what data backs a production validator," matching its
existing ownership of `data/SOURCES.md`.

### `skimmer-engine`: batch path

`Cargo.toml` gains `skimmer-spot = { workspace = true }`. In
`decode_samples`, after the existing chunked channelizer/track-manager loop
produces `events: Vec<DecoderEvent>` (already globally ordered — the
existing `this_track` filter already relies on this), construct
`skimmer_spot::Validator::bundled(fs)` and feed it every event in order,
collecting the returned `Spot`s. Validation runs over the **full**
multi-track stream, not the `this_track` filter used for `report.text`/
`freq_hz`/`wpm` — V8/V8w's pileup scenes need spots from all tracks.

`DecodeReport` gains:

```rust
pub spots: Vec<skimmer_spot::Spot>,
```

`skimmer_engine::lib.rs` re-exports `pub use skimmer_spot::Spot;` so
downstream crates (`skimmer-cli`, tests) don't need a direct `skimmer-spot`
dependency just to name the type.

`decode_wav` is unchanged beyond inheriting `decode_samples`'s new
behavior — no signature change.

**CPU-budget bench impact (accepted, per design discussion):**
`crates/skimmer-engine/benches/cpu_budget.rs` and
`tests/cpu_budget.rs::cpu_budget_mac_under_half_core` call `decode_samples`
directly, so the Mac-leg 0.36x/0.5x ROADMAP M2 measurement now includes
cty.dat/master.scp parsing and per-word regex validation across the
scene's active tracks. This is intentional: the "full pipeline" now
includes M3 validation. The Mac leg is re-measured as part of this
sub-project's verification and the new ratio is recorded in ROADMAP.md; if
it regresses past 0.5x that's a real finding to surface, not silently
absorbed.

### `skimmer-engine`: streaming path

`listen`'s signature changes from:

```rust
pub fn listen(
    mut src: Box<dyn IqSource>,
    cfg: &PipelineConfig,
    stop: Arc<AtomicBool>,
    mut on_event: impl FnMut(&DecoderEvent),
) -> Result<()>
```

to:

```rust
pub fn listen(
    mut src: Box<dyn IqSource>,
    cfg: &PipelineConfig,
    stop: Arc<AtomicBool>,
    mut on_event: impl FnMut(&DecoderEvent),
    mut on_spot: impl FnMut(&Spot),
) -> Result<()>
```

A `Validator::bundled(fs)` is constructed once, alongside the
`Channelizer`/`TrackManager`, using the same `fs` already read from `src`.
Every place `listen` currently calls `on_event(&ev)`, it now also calls
`validator.ingest(&ev)` and `on_spot(&spot)` for each returned spot,
**after** `on_event` for that same `ev` — event-then-spot ordering per
event, matching `Validator::ingest`'s own contract (a spot only comes out
on the event that completes a passing candidate's word).

Call sites updated (all three current callers of `listen`, confirmed by
grep — no others exist):

- `soak.rs::soak`: `|_spot| {}` — `SoakReport` spot-counting is not in
  scope for this sub-project.
- `crates/skimmer-engine/tests/listen_audio.rs`: `|_spot| {}` — test is
  already `#[ignore]`d (pre-existing Hilbert near-DC issue, issue #21,
  unrelated to this change); just needs to keep compiling.
- `skimmer-cli`'s `Listen` command: real handling, below.

### `skimmer-cli`

**`decode` subcommand:**
- `--json`: `DecodeReport`'s new `spots` field serializes automatically
  (already `#[derive(serde::Serialize)]` end to end).
- Plain mode: one added stderr line, `spots: {n}`.

**`listen` subcommand:**
- `--json`: event lines are unchanged (bare `DecoderEvent` JSON, one per
  line, as today). Spot lines print as `{"spot": <Spot JSON>}` — wrapped so
  a consumer can distinguish the two without ambiguity, and explicitly
  called out (doc comment on the `on_spot` closure in `main.rs`) as
  provisional CLI-debugging output, not the ecosystem contract.
- Plain mode: one stderr line per spot,
  `SPOT: {callsign} ({spot_type:?}) {freq_hz:.1} Hz {snr_db:.0} dB {wpm:.0} wpm conf={confidence:.2}`.

## 3. Data flow

Batch (`decode_samples`):

```
IQ samples → channelizer/TrackManager (unchanged) → events: Vec<DecoderEvent>
  → Validator::bundled(fs).ingest(event) for event in events, in order
  → spots: Vec<Spot>
  → DecodeReport { freq_hz, wpm, text, events, spots }
```

Streaming (`listen`):

```
IQ chunk → channelizer/TrackManager (unchanged) → DecoderEvent
  → on_event(&ev)
  → validator.ingest(&ev) → 0+ Spot
  → on_spot(&spot) for each
```

## 4. Golden test rewrites

### `golden_v8_v8w.rs`

Replace substring-counting helpers with real-validator-based checks:

- **"validated" (V8's ≥45/50 gate):** a known call is validated iff it
  appears as `spots[i].callsign` anywhere in `report["spots"]` — no
  per-track frequency matching needed, since `Validator`'s own repetition
  gate already enforces the ≥2-distinct-decode requirement the old
  substring-count(≥2) heuristic was approximating.
- **"bogus" (V8/V8w's 0-bogus gate):** any `spots[i].callsign` not in
  `known_calls`. Expected to be *easier* to satisfy than the old heuristic,
  since a real spot additionally requires a cty.dat-allocated prefix and
  CQ/DE/beacon context match — corrupted-decode noise that merely looked
  callsign-shaped no longer produces a false spot.
- **"ghost decode" (V8w's 0-cross-channel-ghost gate, stays `#[ignore]`d):**
  a known call is a ghost iff its spots span more than one distinct
  `track_id` — computed from `report["spots"]`'s `track_id` field, real
  per-track attribution instead of substring-presence-per-matched-track.

`per_track`/`match_tracks_by_freq`/`cer` stay as-is for V8w's CER
measurement (unrelated to validation — measures raw per-track decode
quality under fading). `bogus_calls`/`call_from_keyed_text`'s substring
logic is removed entirely; `call_from_keyed_text` (extracting the known
call from a fixture's keyed text) is kept, since `known_calls` still needs
to be built from the manifest.

### `golden_v2_v3.rs` (V5 only)

V5's "callsign validated within 90 s" check swaps its running-decoded-text
substring scan for: find the first `Spot` in `report["spots"]` with
`callsign == "ZL2XYZ"`, assert its `sample_ts <= 90.0 * manifest.fs`. Test
stays `#[ignore]`d — V5's underlying CER-under-fading failure (tracked
separately, M1 pins doc) is unchanged and unrelated to this sub-project.

## 5. Error handling

No new fallible surface: `Validator::bundled`/`Validator::ingest` are
infallible (matches `skimmer-spot`'s existing API — no `Result` anywhere in
`validator.rs`). `decode_samples`/`decode_wav`/`listen`'s existing
`Result<...>` return types are unchanged.

## 6. Testing

- New `skimmer-engine` integration test (`tests/spots.rs` or added to
  existing `pipeline.rs`): a synthetic "CQ CQ DE K5ARH K5ARH K" fixture,
  looped so the repetition gate is satisfied, decoded via `decode_samples`,
  asserting `report.spots` contains exactly one `Spot` with
  `callsign == "K5ARH"` and `spot_type == SpotType::Cq`.
- New `listen()` test alongside `listen_audio.rs`'s existing pattern (or a
  new file), asserting `on_spot` fires for the same kind of fixture via the
  streaming path — using the same clean-signal-avoiding-DC-leakage
  precautions `listen_audio.rs` documents, or a direct `decode_samples`-
  style complex-IQ source that bypasses `AudioIqSource`'s Hilbert
  transformer (per that file's own note on issue #21) so this new test
  isn't blocked by an unrelated, already-tracked bug.
- Full existing suite (V1–V10, V8/V8w, V2–V6, `skimmer-spot`'s own
  V11–V15) re-run to confirm no regressions from the golden-test rewrite.
- CPU-budget Mac leg (`cpu_budget_mac_under_half_core`, `--release`,
  currently `#[ignore]`d — run explicitly) re-measured; new ratio recorded.

## 7. Determinism

No new determinism risk: `Validator` is already proven deterministic
(`skimmer-spot`'s own SPEC §6 compliance — `BTreeMap`-backed gate/dedupe,
`sample_ts`-based timing, no RNG). Feeding it the same already-deterministic
`events` stream in the same order produces the same `spots` every time.
`decode_wav`'s byte-identical-output requirement (SPEC §6) extends
naturally to the new `spots` field with no additional work.

## 8. ROADMAP.md update

M3's status paragraph updates to note the engine-wiring sub-project
complete, alongside `skimmer-spot`'s validation crate. Remaining M3 work:
`skimmer-server` (telnet + JSON/WebSocket, TOML config, metrics) and the
RBN parity benchmark (still blocked on the recorded-IQ data dependency).
