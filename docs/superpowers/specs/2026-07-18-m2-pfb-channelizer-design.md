# M2 sub-project 1 — PFB Channelizer: Design

Design for the first of M2's independent sub-projects (ROADMAP.md "M2 —
Wideband: PFB + detector + decoder pool" decomposes into: **1) PFB
channelizer** (this doc), 2) detector + track manager, 3) decoder pool, 4)
SoapySDR input, 5) KiwiSDR input). This sub-project builds the real
N-channel polyphase filterbank and swaps it into `skimmer-engine`, replacing
the M0/M1 single-channel shim — without yet building the real detector or
multi-track pool.

## 1. Scope

In scope:

- New `skimmer-dsp` module implementing the WOLA polyphase filterbank per
  SPEC-decode-core.md §1 (§1.1 dimensions, §1.2 prototype reuse, §1.3
  per-hop WOLA/FFT/power, §1.4 fine-frequency interpolation).
- Wiring into `skimmer-engine`'s batch (`decode_wav`) and streaming
  (`listen`) paths: the channelizer runs continuously over all N channels;
  a **placeholder detector** (one-time calibration-window peak search over
  real per-channel power, replacing `estimate_peak_hz`'s periodogram) picks
  a single channel `k0` and feeds only that channel's 375 Hz magnitude
  stream to the existing, unmodified `TrackDecoder`.
- New multi-signal wideband test scenes proving the channelizer actually
  separates simultaneous signals (never exercised by V1-V6, which are all
  single-signal).
- Light second-sample-rate coverage (192 kS/s, alongside V1-V6's 96 kS/s) as
  cheap insurance on the `N = fs/93.75` table generalization (SPEC §1.1).

Explicitly out of scope (later M2 sub-projects):

- The real order-statistic noise floor, hysteresis active/inactive
  detection, and track lifecycle (spawn/evict/cap) — SPEC §2. The
  placeholder detector here is deliberately minimal: pick the loudest
  channel once, not a real multi-track detector.
- The decoder pool (multiple concurrent `TrackDecoder`s, one per active
  track) — `skimmer-engine` keeps decoding exactly one track, same as
  today.
- SoapySDR/KiwiSDR input, and the CPU-budget criterion bench (SPEC's
  "300 active tracks" gate needs the decoder pool to mean anything).
- Deleting `skimmer-dsp::single`/`freqest` — deprecated in place (§6),
  revisit pruning later once the new path has proven itself.

## 2. Components

### `skimmer-dsp::channelizer` (new)

Owns the WOLA polyphase filterbank, per SPEC §1.1–§1.3:

- **Dimensions** (§1.1): `N = fs / 93.75` (a power of two for all supported
  table rates — 1024 at 96 kS/s, 2048 at 192 kS/s, etc.), `hop = N/4`,
  output rate `fo = 375 Hz` invariant across input rates.
- **Prototype filter** (§1.2): reuses `skimmer_dsp::proto::design_prototype`
  unchanged — it was already written channel-count-generically at M0
  (`design_prototype(n_channels, taps_per_branch)`), so no new filter-design
  code is needed, only a new *consumer* of it (the M0 shim used it for one
  channel; this module uses it for the real N-channel structure).
- **Per-hop processing** (§1.3, WOLA form): maintain a sliding `LN`-sample
  input window; every `hop` new samples, window+fold to `N` points, apply
  the circular phase-correction rotation (`r = (m·hop) mod N`, needed
  because `hop < N`), FFT via `coppa_dsp::fft::FftProcessor`, compute
  per-channel power `P[k,m] = |X[k,m]|²`.
- **Channel↔Hz mapping** (§1.1): `f(k) = f_center + ((k + N/2) mod N − N/2)
  · Δ` (standard FFT bin order). All internal code works in channel index;
  Hz conversion happens only at the boundary (spot emission, or here, the
  placeholder detector's channel→offset_hz translation for `TrackDecoder`).
- **Fine-frequency interpolator** (§1.4): a per-hop primitive — given a
  channel index `k0` and that hop's three power values `(P[k0-1], P[k0],
  P[k0+1])`, return the quadratic-interpolation offset `δ_m ∈ [-0.5, 0.5]`
  (or "unusable" if no local max). This module exposes the per-hop
  primitive only; accumulating it into a track-lifetime centroid `C` is the
  track manager's job (a later sub-project) — out of scope here.

### Placeholder detector (in `skimmer-engine`)

A deliberately minimal stand-in for SPEC §2's real detector, scoped to keep
today's single-track decode path working through the new channelizer:

- **Calibration**: buffer a fixed startup window (same 2-second window M1's
  `listen()` already uses for `estimate_peak_hz`), run the channelizer over
  it, find the channel `k0` with the highest average power across the
  window's hops.
- **Selection**: for the rest of the run, only channel `k0`'s per-hop
  magnitude is read from the channelizer's output and fed to
  `TrackDecoder::push_envelope` — exactly the interface M0/M1's
  `SingleChannelExtractor` already provided, so `TrackDecoder` needs zero
  changes.
- This is intentionally *not* SPEC §2's hysteresis/eviction logic — it
  picks one channel once and never re-evaluates. Multi-track, re-detection,
  and QSB-survival-via-hysteresis are the next sub-project's job.

### `skimmer-engine` wiring

`decode_samples`/`decode_wav` (M0 batch) and `listen` (M1 streaming) both
currently build a `SingleChannelExtractor` from a periodogram-estimated
offset. Both swap to: build the channelizer, run the placeholder detector's
calibration pass, then read channel `k0`'s output per hop instead of the
extractor's output. The lead-in group-delay handling (M0 pinned decision 19)
carries over conceptually — the channelizer's own `LN`-tap window has the
same causal-filter blind-zone property at stream start, so the same
zero-pad-and-feed-every-output fix applies, just against the new module's
`filter_len()`-equivalent.

## 3. Data flow

```
IQ samples → channelizer sliding window
  → every `hop` samples: WOLA fold → circular rotation → FFT → P[k,m] (all N channels)
  → placeholder detector: calibration-time argmax over k → k0 (once)
  → channel k0's per-hop magnitude → TrackDecoder::push_envelope (unchanged)
```

## 4. Testing

- **V1–V6 regression**: must pass unchanged through the new channelizer +
  placeholder-detector path — proof the swap is behaviorally transparent
  for the single-signal case these vectors already cover.
- **New multi-signal wideband scenes**: `skimmer-testkit::scene::render_scene`
  already accepts multiple simultaneous `SignalSpec` entries (built
  generically at M0, never exercised with >1 signal until now). Add 2-4
  new test scenes with multiple signals at distinct offsets/SNRs in one
  render; assert the channelizer's `P[k,m]` output places each signal at
  the correct channel bin with power matching its requested SNR. This is
  the first real proof in this codebase that the PFB actually separates
  simultaneous signals.
- **Second sample rate**: repeat at least one scene at 192 kS/s (N=2048)
  alongside the existing 96 kS/s (N=1024) coverage, as cheap insurance that
  `N = fs/93.75`'s generalization actually holds, not just for the one rate
  every existing fixture happens to use.
- **Deferred**: CPU-budget criterion benchmarking. SPEC's real acceptance
  gate ("300 active tracks... < 50% of one core") needs the decoder pool to
  mean anything; benchmarking the channelizer alone now would need to be
  re-baselined once tracks exist anyway. Correctness is this sub-project's
  job; performance is the pool sub-project's.

## 5. Determinism

Unchanged constraints apply (SPEC §6): FFT via `coppa_dsp::fft::FftProcessor`
(already deterministic, reused as-is), no RNG/wall-clock anywhere in the
channelizer or the placeholder detector's calibration pass, WOLA
accumulation and the fine-frequency interpolator's arithmetic run in `f64`
per the project's existing per-sample-`f32`/long-accumulation-`f64`
convention.

## 6. Deprecation of `skimmer-dsp::single`/`freqest`

Per Tony's decision: do not delete these M0 modules when the engine stops
using them. Mark both clearly deprecated (module-level doc comment stating
they're superseded by `skimmer-dsp::channelizer` as of this sub-project,
kept only for reference/fallback) and leave them compiled and tested as-is.
Revisit actual removal after the new path has run cleanly for a few months
— tracked as a follow-up, not an open question in this spec.
