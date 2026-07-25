# M2 sub-project 2 — Detector, Track Manager, Decoder Pool: Design

Design for the second of M2's independent sub-projects (ROADMAP.md "M2 —
Wideband: PFB + detector + decoder pool"; sub-project 1, the PFB
channelizer, is complete — see
`docs/superpowers/specs/2026-07-18-m2-pfb-channelizer-design.md`). This
sub-project replaces sub-project 1's placeholder single-channel-argmax
detector (`skimmer-engine::detect::calibrate_channel`) with SPEC §2's real
order-statistic noise floor, hysteresis-gated detection, and track lifecycle
state machine — and folds in the decoder-pool *mechanism* from ARCHITECTURE
§10, merging what ROADMAP originally listed as two separate remaining
sub-projects ("detector/track manager" and "decoder pool") into this one.
ROADMAP.md is updated accordingly once this lands.

## 1. Scope

In scope:

- `skimmer-dsp::floor` (new): per-channel order-statistic floor estimator
  (SPEC §2.1), neighborhood/effective floor (§2.2), and the per-hop smoothed
  power + rise/drop boolean gate (§2.3) — pure, stateful-per-channel, no
  lifecycle or timing persistence.
- `skimmer-engine::track` (new): the track lifecycle state machine (§2.4:
  IDLE → CANDIDATE → ACTIVE → HANG → CLOSED, all hop/ms counters — confirm,
  hang, gc, warmup), adjacent-channel ownership and track-convergence merging
  (§2.5), and the decoder pool.
- **Decoder pool mechanism**: tracks are work items, `TrackDecoder`
  instances are `Send` with no shared state, processed via `rayon` across
  the active-track set each hop-batch (see §2 below for the chosen shape).
  This is the *mechanism* only — the surrounding real-time architecture
  (rtrb SDR-input ring, tokio runtime for validator/servers/metrics from
  ARCHITECTURE §10) stays deferred; those depend on SoapySDR/KiwiSDR input
  and `skimmer-spot`/`skimmer-server`, none of which exist yet.
- Wiring the new detector + track manager into **both** `decode_samples`/
  `decode_wav` (batch) and `listen` (streaming) — replacing `detect.rs`'s
  `calibrate_channel` and each call site's single hardcoded `TrackDecoder`.
- `DetectorConfig`: a plain struct + `Default` impl mirroring SPEC §9's
  `[detector]` table, added as a field on `PipelineConfig` — same pattern
  `DecodeConfig` already uses. No TOML config loader exists anywhere in the
  repo yet; this doesn't add one.
- Golden vectors V7 (adjacent-channel), V9 (drift), V10 (Farnsworth); V2
  (`#[ignore]`d in sub-project 1, pin 8) un-ignored; pins 9/10's widened
  freq-error (10→25 Hz) and WPM (±2→±3) tolerances re-measured and tightened
  back toward SPEC's original values.

Explicitly out of scope (later work):

- V8/V8w (50-signal pileup) and the CPU-budget criterion bench (300 active
  tracks under budget) — real scene/fixture authoring and performance work,
  deferred as a follow-up once this sub-project's correctness lands.
- The rtrb SDR-input ring and tokio async runtime split (ARCHITECTURE §10)
  — no producer or consumer exists on either end yet (SoapySDR/KiwiSDR
  input; `skimmer-spot` validator; `skimmer-server` telnet/JSON surfaces).
- `skimmer-spot`'s real callsign validation (cty.dat, SCP, dedupe) — M3
  work. Golden tests continue approximating "validated" as substring-match
  against decoded text, same as V5's existing test.
- TOML config file loading for any config struct, including the new
  `DetectorConfig` — not built anywhere yet, not this sub-project's job.

## 2. Components

### `skimmer-dsp::floor` (new)

Per SPEC §2.1–§2.3, driven once per hop per channel from `HopOutput.power`:

- **Floor estimator** (§2.1): every 15th hop (25 Hz), push `PdB[k,m]` into a
  per-channel 250-entry ring (10 s). Maintain a 280-bin `u8` histogram (0.5
  dB bins, −140..0 dBFS) alongside the ring — increment on push, decrement
  on evict. `F_ch[k]` = 25th-percentile bin via cumulative scan (O(1)
  amortized, no sorting).
- **Neighborhood/effective floor** (§2.2): 32-channel blocks, `F_blk[b]` =
  median of the block's `F_ch` values (recomputed at 25 Hz); effective floor
  `F[k] = min(F_ch[k], F_blk[⌊k/32⌋] + 3 dB)`. Startup: ring partially
  filled for the first 10 s (quantile over whatever's present); track
  creation inhibited for the first 2 s (`warmup_ms`).
- **Gate** (§2.3): EMA-smoothed power `S[k,m]` (τ = 40 ms, α ≈ 0.0645).
  Exposes two per-channel-per-hop booleans only — `rise_met = S ≥ F +
  on_snr_db` and `drop_met = S < F + off_snr_db` — with **no** persistence
  logic. All hop-counting/ms-counting (19-hop confirm, 5000 ms hang) is the
  track lifecycle's job, not this module's; `floor` is a pure function of
  its own per-channel ring/EMA state, with no notion of "track."

### `skimmer-engine::track` (new)

- **`TrackManager`**: owns one `floor` estimator + gate per channel, and
  `BTreeMap<u32, Track>` keyed by a monotonic `track_id` (ascending birth
  order — SPEC §6 determinism rule 3). Consumes one `Channelizer::process()`
  slice (the whole file for `decode_wav`; a few hops per `CHUNK_SAMPLES`
  read for `listen`) via a single entry point, e.g. `process_hops(&mut
  self, hops: &[HopOutput]) -> Vec<DecoderEvent>`.
- **Per-hop, sequential** (ownership/promotion/eviction is inherently
  ordered, hop-by-hop — not parallelized): for each channel, run the floor
  gate; drive the §2.4 state machine (IDLE→CANDIDATE→ACTIVE→HANG→CLOSED,
  including GC-on-30s-silence and cap-eviction-by-lowest-SNR); apply §2.5
  ownership (`{round(c)-1, round(c), round(c)+1}`, max-power channel among
  the owned set feeds the track, CANDIDATE-in-owned-channel absorption,
  same-hop tie-break at the higher-power channel, and merge-on-convergence
  within 1.0 channel). For each currently-ACTIVE track, append `(mag,
  sample_ts)` from its owned max-power channel to that track's pending
  queue — no decoding happens yet.
- **End of slice, parallel dispatch (the decoder pool)**: `tracks
  .values_mut().filter(|t| t.is_active()).par_bridge()` drains each
  track's queued `(mag, sample_ts)` pairs into its own `TrackDecoder`
  (constructed inline when the track first goes ACTIVE — "leasing a decoder
  from the pool" per SPEC §2.4 is this construction; track cap is the one
  and only capacity bound, enforced by `TrackManager` itself, not
  duplicated as a second pool-capacity resource). Each track's decoder is a
  pure function of its own queue (`Send`, no shared state) — satisfies
  ARCHITECTURE §10's "rayon-style fixed worker pool, tracks are work
  items" without a separate slab/lease-handle abstraction, and without
  duplicating track-cap bookkeeping.
- **Resequencing**: collected per-track event vectors are merged and sorted
  by `(sample_ts, track_id)` before returning — SPEC §6 rule 6, verbatim.

### `skimmer-engine` call-site wiring

`detect.rs`'s `calibrate_channel` and each call site's single hardcoded
`TrackDecoder::new(1, ...)` are removed. `decode_samples`/`decode_wav` and
`listen` instead construct one `TrackManager` and feed it each
`Channelizer::process()` slice; `DecodeReport`/`listen`'s event callback
receive the merged multi-track event stream instead of one track's. The
existing lead-in group-delay zero-padding (M0 pinned decision 19, carried
through sub-project 1) is unchanged — it pads before the channelizer, not
after; `TrackManager` just sees more hops.

## 3. Data flow

```
IQ samples → channelizer sliding window → hop slice (HopOutput per hop)
  → TrackManager::process_hops (sequential, per hop):
      floor + gate (skimmer-dsp::floor) → per-channel rise/drop booleans
      → track lifecycle (IDLE/CANDIDATE/ACTIVE/HANG/CLOSED, §2.4)
      → ownership + ownership-window max-power selection + merge (§2.5)
      → ACTIVE tracks: append (mag, sample_ts) to per-track queue
  → end of slice: rayon-parallel decode across ACTIVE tracks' queues
  → resequence events by (sample_ts, track_id)
  → merged DecoderEvent stream (DecodeReport / listen's on_event callback)
```

## 4. Testing

- **`skimmer-dsp::floor` unit tests**: quantile correctness against known
  histograms, neighborhood-floor clamping (`min(F_ch, F_blk + 3dB)`),
  startup/warmup behavior, rise/drop boolean correctness at threshold
  boundaries.
- **`skimmer-engine::track` unit tests**: state machine transitions
  (promotion at exactly 19 hops, hang timer reset on recovery, GC after 30s
  silence, cap eviction of lowest-SNR track), ownership absorption and
  same-hop tie-break, merge-on-convergence.
- **V1–V6 regression**: must keep passing through the new detector/track
  manager (single-signal cases; V5 stays `#[ignore]`d per its own,
  unrelated fading-robustness pin).
- **V2**: `#[ignore]` removed; this is the pin-8-tracked case the real
  detector was expected to fix.
- **V7/V9/V10 golden tests**: added to `skimmer-cli/tests`, using
  `skimmer-testkit::scene::render_scene`'s existing multi-signal support
  (already generic, per sub-project 1's design doc §4 — no testkit changes
  needed for V7; V9/V10 are single-signal).
- **Pins 9/10 tolerance re-measurement**: with real hysteresis gating
  filtering unreliable hops before they reach the fine-frequency
  interpolator (the mechanism pin 9 predicted would fix this), re-run
  V1/pipeline/roundtrip tests at the original 10 Hz / ±2 WPM bounds; tighten
  back to whatever the measurements actually support, recording the result
  as a new pin either way (fully reverted, partially tightened, or
  unchanged with a documented reason).
- **Determinism**: same-IQ-file/same-binary byte-identical JSON spot log
  across 3 runs, including through the rayon-parallel decode stage (SPEC §6
  rule 6 is exactly the guarantee that makes this safe).
- **Deferred**: V8/V8w, CPU-budget criterion bench (see §1 out-of-scope).

## 5. Determinism

Rayon parallelizes *decoding* (§6 rule 6 explicitly permits decoder workers
to run in any order), never the sequential per-hop state-machine bookkeeping
in `TrackManager::process_hops`, which stays single-threaded and
hop-ordered. `BTreeMap<u32, Track>` (never `HashMap`) per rule 3. All
lifecycle timers are hop/ms counters, never wall-clock, per rule 2. No RNG
anywhere in `skimmer-dsp`/`skimmer-engine`, per rule 1.

## 6. ROADMAP update

Once this lands, ROADMAP.md's M2 section is updated to merge "detector/track
manager" and "decoder pool" into one completed sub-project, and to note
V8/V8w's pileup-scene work as the newly-split-out follow-up alongside
SoapySDR/KiwiSDR input.
