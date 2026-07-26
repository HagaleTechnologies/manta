# M2 remaining sub-project 1: V8/V8w pileup validation + CPU-budget bench

Status: approved
Date: 2026-07-24

## Purpose

M2's remaining sub-projects (ROADMAP.md) are: V8/V8w pileup-scene
validation + CPU-budget criterion bench, SoapySDR input, KiwiSDR input.
This spec covers the first: SPEC-decode-core.md §7's two pileup golden
vectors (V8, V8w) and the ROADMAP M2 accept criterion "criterion bench:
full pipeline at 192 kS/s with 300 active tracks uses < 50% of one core
on an M-series Mac AND < 1 core on a Raspberry Pi 4."

Out of scope (deferred to a later pass, once SoapySDR/KiwiSDR input
exists): the 24h live-SDR soak test, also part of M2's ROADMAP accept
criteria.

## V8/V8w pileup vectors

### Fixture callsigns

`skimmer-testkit` gains a deterministic 50-call fixture list: composed
from a fixed set of ham-style prefixes crossed with ChaCha8-seeded
suffixes (same determinism discipline as the rest of `skimmer-testkit` —
"all randomness is ChaCha8 seeded per fixture"), not 50 hand-picked
real-looking calls. Uniqueness enforced at generation time.

### Scene generation

`skimmer-testkit::vectors` gains `pileup_scene(watterson: bool) ->
VectorSpec`, shared by `v8()` and `v8w()`:

- `fs = 96_000.0`, `duration_s = 120.0`, per SPEC §7's stated defaults.
- 50 `SignalSpec`s: WPM uniform in 10..35, SNR (2500 Hz) uniform in
  -2.0..25.0 dB, offsets uniform over ±45 kHz with reject-and-redraw
  collision avoidance (minimum separation enforced the same way V7's two
  signals are kept apart, well clear of the 1-channel merge threshold),
  8% keying jitter (each signal gets its own `Jitter::seed`, deterministic
  from the vector's base seed + signal index).
- `v8()`: AWGN only (no `watterson` field set).
- `v8w()`: identical scene/seeds, but every `SignalSpec` also gets
  `watterson: Some(WattersonFade { preset: WattersonPreset::Poor, seed: ... })`
  (CCIR-poor, per SPEC §7's V8w row).
- Both reuse the existing generic `render_scene`/`write_fixture_set`
  path unchanged — `Manifest` already carries `expected_freqs_hz: Vec<f64>`
  and `keyed_texts: Vec<String>` per-signal, so no format changes needed
  there.

### Golden tests

New `crates/skimmer-cli/tests/golden_v8_v8w.rs`, following the existing
CLI-binary-decode pattern (`skimmer` binary, `--json`, parse `report`).

Track-to-signal association: for each decoded track, take its last
`TrackMeta` event's `freq_hz`; match to the closest `manifest.expected_freqs_hz[i]`
(nearest-frequency match — tracks report a live centroid, not the exact
seed offset). This is more precise than V5/V6's plain substring search,
since we now have per-signal frequency ground truth to pair against.

**V8** (`pileup-50`, AWGN):
- "validated": for each of the 50 signals, its matched track's assembled
  decoded text (filtered by that track's `track_id`, in `sample_ts` order)
  contains that signal's callsign as a contiguous substring at least
  twice (mirrors V3's "≥2 reps" convention). Assert ≥45/50.
- "0 bogus callsigns spotted": scan every track's assembled text for
  callsign-shaped tokens (alphanumeric, 3-7 chars, at least one letter
  and one digit) appearing ≥2 reps; assert every such token is one of
  the 50 fixture calls.

**V8w** (`pileup-50-fading`, Watterson CCIR-poor):
- Restrict to the subset of signals with `snr_2500_db >= 6.0`.
- For each, compute CER between its matched track's assembled text and
  its `keyed_texts[i]`. Assert ≥90% of that subset has CER < 10%.
- Same bogus-callsign check as V8.
- "0 cross-channel ghost decodes": assert no fixture callsign's ≥2-rep
  substring appears in more than one distinct track's assembled text.

If the first real run doesn't clear these thresholds, the plan (like
V2/V5/V6 before it) is to treat it as a real bug to diagnose first,
falling back to a measured/pinned tolerance with a documented reason
only if investigation shows it's a genuine, already-known classical-
decoder limitation (e.g. the same fading-robustness gap behind V5/V6) —
never silently widen a threshold to force a pass.

## CPU-budget bench

New `crates/skimmer-engine/benches/cpu_budget.rs`, `criterion` added as
a dev-dependency (`harness = false` bench target).

- Synthetic scene reusing `skimmer-testkit::scene` directly (already a
  dev-dependency of `skimmer-engine`): `fs = 192_000.0`, ~15 s duration
  (tunable after the first real measurement — must clear `warmup_hops`
  (~2 s) plus enough steady-state samples to be a meaningful measurement),
  ~300 keyed tones spread evenly across the passband (2048 channels at
  192 kHz), plain AWGN, no accuracy requirements — this only needs to
  drive the detector into promoting ~300 concurrent ACTIVE tracks so the
  bench exercises real channelizer + detector + decoder-pool cost, not
  decode correctness.
- `cargo bench` target for iterative profiling during future perf work.
- A separate `#[ignore]`d `#[test]` in the same file (same convention as
  `listen_audio.rs`'s environment-dependent ignored test) that does a
  real `std::time::Instant` wall-clock measurement over the same scene
  and asserts wall time < 50% of audio duration (the Mac budget). Run on
  demand (`cargo test --release -p skimmer-engine -- --ignored cpu_budget`),
  not part of default `cargo test --workspace` / CI — perf assertions on
  shared CI runners are flaky, and GitHub-hosted runners aren't Pi4
  hardware anyway, so there's nothing CI could meaningfully gate here.
- I'll run the Mac-budget measurement locally in this session and record
  real numbers in a `docs/DECISIONS/` pin. The Raspberry Pi 4 leg (< 1
  core) stays an explicitly flagged outstanding manual step — same
  pattern as M1's still-outstanding W1AW live-copy run — for Tony to run
  on real hardware later.

## Testing

- `cargo test --workspace` must stay green (existing suite unaffected).
- New golden tests run via the normal `cargo test -p skimmer-cli` path;
  if V8/V8w fail on first real measurement, diagnose per the escalation
  policy above before deciding whether to `#[ignore]` with a documented
  reason (consistent with V2/V5/V6's precedent) or fix a real bug.
- `cargo bench -p skimmer-engine --bench cpu_budget` for the criterion
  profiling target; the `#[ignore]`d wall-clock test is the actual
  Mac-budget assertion, run manually.
