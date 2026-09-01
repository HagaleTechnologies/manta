# M3 sub-project 1 — `skimmer-spot` validation crate: Design

Design for the first of M3's independent sub-projects (ROADMAP.md "M3 —
Spots: validation + servers + RBN parity benchmark"). M3 decomposes the same
way M2 did: this sub-project builds `skimmer-spot` (callsign/CQ-DE
validation, dedupe/scoring) as a standalone, fully tested crate.
`skimmer-server` (telnet + JSON/WS output), TOML config loading, the wiring
of `skimmer-spot` into `skimmer-engine`'s live pipeline, and the RBN parity
benchmark (which additionally needs ≥ 2 h of recorded contest-weekend IQ with
RBN reference spots — a data dependency, not yet resolved) are all separate,
later sub-projects.

## 1. Scope

In scope:

- New workspace crate `skimmer-spot`, implementing ARCHITECTURE §6's
  pipeline end to end as a pure, deterministic transform: decoder event
  stream in, validated `Spot`s out.
- `context`: CQ/DE/beacon regex scan → `(callsign, SpotType)` candidates
  from a track's rolling text window.
- `grammar`: callsign structural validation (prefix-digit-suffix, portable
  designators `/P /QRP /3`), pure function, no data file.
- `cty`: bundled `cty.dat` snapshot, longest-prefix match, gates (rejects
  unallocated prefixes).
- `scp`: bundled `master.scp` snapshot, membership check, boosts confidence.
- `confidence`: SPEC §4.6's `c_call` formula plus the cty/SCP adjustment the
  spec explicitly defers to this crate (numbers pinned in §2 below).
- `gate`: ≥ 2-distinct-decode repetition requirement within a 90 s window
  before first spot.
- `dedupe`: re-spot suppression keyed by `(callsign, freq_bucket)`.
- `Validator`: the public entry point tying the above together.
- New SPEC-decode-core.md golden vectors V11–V15 (§4 below), extending the
  existing §7 convention.

Explicitly out of scope (later sub-projects):

- Wiring `skimmer-spot` into `skimmer-engine`'s live pipeline, and swapping
  `golden_v8_v8w.rs`/`golden_v2_v3.rs` off their exact-match-against-fixture
  stub onto the real validator. Fast follow-up once this crate exists.
- `skimmer-server` (telnet cluster + JSON Lines/WebSocket), TOML config
  loading, metrics endpoint — none of `skimmer-spot`'s output has a
  transport yet; that's `skimmer-server`'s job.
- The RBN parity benchmark and its golden IQ corpus.
- cty.dat/master.scp refresh tooling/automation — the vendored snapshot is
  refreshed by hand, like any other fixture, for now.

## 2. Components

### `cty` — prefix table

Bundled as `crates/skimmer-spot/data/cty.dat` (`include_str!` at build
time), parsed once in `Validator::new()` into a `Vec<PrefixEntry>` sorted by
prefix, longest-prefix match via binary search. Source, retrieval date, and
license recorded in `crates/skimmer-spot/data/SOURCES.md`. Binary search
over a sorted `Vec` instead of a trie: lookup volume is bounded by track cap
× repeat rate (at most a few hundred/sec, never a hot path), so a trie's
extra complexity isn't earning its keep here.

A callsign with a prefix absent from the table is rejected outright — no
partial credit, no spot. This is the "0 bogus callsigns" enforcement point.

### `scp` — super-check-partial

Bundled `crates/skimmer-spot/data/master.scp` snapshot (same
`SOURCES.md`), parsed into a `HashSet<String>`. Membership is a pure
boolean lookup that cannot affect output ordering, so `HashSet` is fine here
even though other state below must be order-stable.

### `context` — CQ/DE/beacon parse

`regex` crate, patterns compiled once via `std::sync::LazyLock`:

- `CQ <call>`, `CQ TEST <call>` → `SpotType::Cq`
- `DE <call>` → `SpotType::De`
- `<call> UP` → `SpotType::De` (UP is a DE-context convention, not a
  distinct spot type)
- `V V V <call>` → `SpotType::Beacon`

Scans a track's rolling text window (bounded to the 90 s repetition window
plus a small margin, so old text can't resurrect a stale candidate) for the
first match; if no context pattern matches, falls back to
`SpotType::Unknown` and grammar-only validation (undirected calls still get
spotted — ARCHITECTURE's design goal that rare/weak calls aren't
gated on politeness conventions).

### `grammar` — structural callsign validation

Pure function: prefix (1-2 letters, optionally +digit), digit, suffix
(1-4 letters), optional portable suffix (`/P`, `/QRP`, `/<digit>`, `/MM`,
etc.). No data file; this is the cheap reject-obvious-garble pass before the
cty.dat lookup.

### `confidence` — `c_call` + adjustments

Implements SPEC §4.6 verbatim:

```
c_call = (Π cᵢ)^(1/n) · (1 − 0.5^r)
```

then applies, in order:

1. **cty.dat**: gate, not a multiplier — unallocated prefix ⇒ reject (no
   `Spot` emitted), allocated prefix ⇒ no change.
2. **SCP**: multiplicative boost capped at 1.0 —
   `c_call ← min(1.0, c_call · 1.15)` on membership; absence leaves
   `c_call` unchanged (matches ARCHITECTURE §6.3: "absence only lowers it
   [relatively, by not getting the boost], never gates").

### `gate` — repetition requirement

Per track, keyed by normalized callsign: records `sample_ts` of each
distinct decode. A candidate is eligible for first spot once ≥ 2 distinct
decodes fall within a 90 s window (sample_ts-based — no wall clock, per SPEC
§6 determinism rule 2). Backing store: `BTreeMap<TrackId, TrackGateState>`
— determinism rule 3 (no `HashMap` on any output-order-affecting path).

### `dedupe` — re-spot suppression

Key `(callsign, freq_bucket)` where `freq_bucket = round(freq_hz / 300.0)`
(±0.3 kHz bucket per ARCHITECTURE §6.5). Backing store:
`BTreeMap<(String, i64), LastSpotRecord { sample_ts, snr_db, spot_type }>`.
Suppresses a re-spot unless ≥ 10 min (converted from sample_ts via the
known sample rate) has elapsed, OR SNR improved ≥ 6 dB, OR `spot_type`
changed.

### `Validator` — public API

```
pub struct Validator { /* cty table, scp set, per-track buffers, gate, dedupe state */ }

impl Validator {
    pub fn new(cty_dat: &str, master_scp: Option<&str>) -> Self;
    pub fn ingest(&mut self, event: DecoderEvent) -> Vec<Spot>;
}
```

`DecoderEvent` is SPEC §5's existing enum (`CharDecoded`, `WordBoundary`,
`SpeedUpdate`, `TrackMeta`) — `skimmer-spot` takes a dependency on whatever
crate currently defines it (`skimmer-engine`, per current layout) rather
than redefining it. `Spot`:

```
pub struct Spot {
    pub callsign: String,
    pub freq_hz: f64,
    pub snr_db: f32,
    pub wpm: f32,
    pub spot_type: SpotType,
    pub confidence: f32,
    pub track_id: u32,
    pub sample_ts: u64,
}
```

No wall-clock timestamp — that conversion (`stream_start_time +
sample_ts / fs`) happens at the `skimmer-server` boundary per SPEC §5, not
here.

## 3. Data flow

```
DecoderEvent stream (per track, already sequenced by (sample_ts, track_id))
  → per-track rolling text buffer (CharDecoded/WordBoundary accumulation)
  → context::parse → (callsign, SpotType) candidates
  → grammar::validate → structurally-plausible candidates only
  → cty::lookup → gate: reject unallocated prefixes
  → confidence::c_call (SPEC §4.6) → scp::lookup boost
  → gate::eligible (>= 2 reps / 90s)
  → dedupe::should_emit (freq-bucket + suppression window)
  → Spot
```

## 4. Testing

- **Unit tests per component**: grammar edge cases (portable designators,
  malformed calls), cty.dat longest-prefix-match correctness, SCP
  membership boost arithmetic, gate window-boundary behavior (exactly 2
  reps at exactly 90 s), dedupe suppression/override conditions.
- **New SPEC-decode-core.md golden vectors** (extending §7's V1–V10):
  - **V11** — CQ/DE/beacon context parse sets the correct `SpotType` for
    each pattern family (`CQ`, `CQ TEST`, `DE`, `<call> UP`, `V V V`).
  - **V12** — a structurally-valid callsign with an unallocated/bogus
    prefix is rejected (0 bogus spots even though `grammar::validate`
    passes) — this is the crate-level analog of V8/V8w's "0 bogus
    callsigns" criterion.
  - **V13** — SCP membership measurably raises `c_call`; absence does not
    reject.
  - **V14** — repetition gate: 1 rep never spots, 2 reps does.
  - **V15** — dedupe: re-spot suppressed inside the window; allowed after
    an SNR jump ≥ 6 dB or a type change.
- **Determinism**: same input event sequence → byte-identical `Spot`
  sequence across repeated runs (this crate's contribution to the overall
  byte-identical spot-log requirement, SPEC §6).

## 5. Determinism

All state that affects output ordering (`gate`, `dedupe`) lives in
`BTreeMap`, never `HashMap`, per SPEC §6 rule 3. No wall clock anywhere in
`Validator::ingest` — all timing is `sample_ts`-based, per rule 2. No RNG,
per rule 1. `scp`'s `HashSet` is the sole exception, justified above (pure
boolean membership, cannot affect ordering).

## 6. ROADMAP update

Once this lands, ROADMAP.md's M3 section notes `skimmer-spot`'s validation
crate as complete, with engine wiring, `skimmer-server`, and the parity
benchmark as the remaining sub-projects.
