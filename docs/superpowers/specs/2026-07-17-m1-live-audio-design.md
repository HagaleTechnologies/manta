# M1 — Live Audio, One Signal: Design

Design for ROADMAP.md's M1: live audio input via `coppa-audio`, decoding a
real off-air CW signal end-to-end, no PFB, no track pool (those are M2). Gate
is SPEC-decode-core.md §7 vectors V1–V6, plus a manual live-copy run.

## 1. Scope

Three work-streams, one milestone:

1. A streaming live-audio decode pipeline (new).
2. Golden vectors V2–V6 (`skimmer-testkit`).
3. A fix for the M0 all-dah-opener decode bug, which M0's pinned decision
   #20 explicitly gates on M1 (see §9).

Explicitly out of scope (ROADMAP defers to M2/M3): PFB channelizer, decoder
pool, multi-track, SoapySDR/KiwiSDR input, TOML config, spot server.

## 2. Input layer — `skimmer-input::AudioIqSource`

A new `IqSource` impl wrapping a `coppa_audio::AudioSource`:

- Real hardware: `coppa_audio::cpal_backend::CpalSource`, requesting 48 kHz.
  If the device won't give 48 kHz, wrap it in `coppa_audio::ResamplingSource`
  to convert. 48000 / 93.75 = 512 exactly (a power of two), so
  `SingleChannelExtractor::new` accepts it unmodified — no extractor changes
  needed.
- File replay: `coppa_audio::file_backend::WavSource`/`RawF32Source`, paced
  to real time (sleep between reads matched to the device rate) so the same
  streaming code path is exercised as with live hardware. This is also the
  soak harness's traffic source (§7).

Each `f32` chunk pulled from the `AudioSource` is pushed through the new
Hilbert transformer (§3) to produce `Complex32`, satisfying the existing
`IqSource::read` contract unchanged. `AudioIqSource::sample_rate()` reports
the post-resample rate; `center_freq_hz()` returns 0.0 (audio has no RF
reference — SPEC's `freq_hz` reporting for audio-sourced tracks is
offset-only, center 0).

## 3. `skimmer-dsp::hilbert` — analytic-signal FIR

New component: an odd-length windowed-sinc Hilbert transformer
(`h[n] = 2/(πn)` for odd n, 0 for even n), Kaiser-windowed the same way
`skimmer-dsp::proto` designs the PFB prototype. Causal, fixed group delay,
processes samples incrementally (no whole-buffer requirement) — usable both
for live chunks and one-shot vector rendering.

Two consumers:

- **Live audio** (§2): real mic samples → analytic `Complex32`.
- **V4/V5 vector generation** (§6): the reverse role. A per-signal complex
  baseband tone is taken to its real part (a real passband tone at the
  signal's offset), run through coppa's `watterson_preset()`, then the faded
  real output is converted back to complex baseband via this same Hilbert
  transformer and re-embedded into the scene.

One new component serves both needs; no duplicate analytic-conversion code.

**Alternatives considered:** FFT block Hilbert (matches coppa's internal
`watterson.rs::analytic()` approach) adds buffering/latency for no accuracy
benefit at single-channel 48 kHz scale, and still needs a separate real-time
variant for the live path. Weaver/phasing (dual mixer + lowpass) avoids a
Hilbert filter but adds two mixers and two lowpass filters for no gain here —
we don't need SSB-grade image rejection, just a decent analytic signal for
the existing extractor. FIR Hilbert wins on "one component, both consumers."

## 4. Streaming engine — `skimmer-engine::listen`

M0's `decode_samples`/`decode_wav` (whole-buffer batch) are untouched — a new
module, not a refactor. Single-threaded loop:

```
loop {
    let n = audio_src.read(&mut chunk)?;   // blocks on coppa-audio's ring
    if n == 0 { break }                     // EOF (file replay only)
    let iq = &chunk[..n];
    let channel = extractor.process(iq);
    for y in channel {
        for ev in decoder.push_envelope(y.norm(), sample_ts) {
            emit(ev);
        }
    }
}
emit_all(decoder.finish());
```

runs until Ctrl-C (live) or EOF (file replay). No dedicated actor threads or
`rtrb` rings — `coppa_audio::CpalSource` already runs its own callback thread
internally and hands off via its own ring buffer, so `AudioSource::read` is
already a safe blocking pull from this thread's perspective. M1 has exactly
one track; there is nothing here to parallelize. This loop body becomes the
channelizer thread's inner loop at M2 with minimal rework — the incremental
`SingleChannelExtractor`/`TrackDecoder` API (`buf`/`read`/`n_in` state,
`push_envelope` per-hop) was already built streaming-capable at M0.

**Startup transient:** M0's whole-buffer lead-in padding (prepend one filter
length of zero IQ, feed every output) becomes a one-time zero-pad written
into the extractor's internal buffer before the first real chunk arrives,
using the same "feed every output, rebaseline `sample_ts`" logic — the fix is
unchanged, just applied once at stream start instead of per-file.

**Shutdown:** `listen` installs a Ctrl-C handler (`ctrlc` crate) that
sets an atomic flag checked each loop iteration; on shutdown, calls
`decoder.finish()` and flushes remaining events before exiting, matching M0's
end-of-file handling.

## 5. CLI

```
skimmer listen [--device NAME] [--source file:PATH] [--json]
skimmer soak --duration SECS [--device NAME] [--source file:PATH]
```

`listen` prints decoded characters/spot-relevant events to stdout as they
arrive (plain text by default; `--json` emits `DecoderEvent`s as JSON Lines,
matching `decode --json`'s existing pattern). `--device`/`--source` are
mutually exclusive; default is the system default input device.

## 6. Golden vectors V2–V6

Added to `skimmer-testkit::vectors` and `Gen`'s CLI match arm (`"v2".."v6"`).

- **V2 (fast-35), V3 (slow-weak):** AWGN + jitter, same machinery as V1. New
  `VectorSpec` entries only, no new code.
- **V6 (qsb-sine):** new sinusoidal envelope-multiply step in `scene.rs`
  (`envelope *= 0.55 + 0.45·sin(2π·0.2·t)`) applied to the signal's amplitude
  before mixing into the scene.
- **V4 (fade-good), V5 (fade-poor):** new Watterson integration. Per §3, uses
  coppa's current `watterson_preset()` (real, one-shot) — **not** the
  streaming `WattersonChannel` SPEC-decode-core.md §7 assumes, which is only
  a coppa-adoption proposal (`orchestrator/proposals/coppa-adoption/
  SPEC-watterson.md` §6) that was never implemented. Vector generation is
  offline/batch (a fixed-duration fixture rendered once), so the streaming
  requirement doesn't actually apply here. The exact coppa commit is pinned
  in the vector's `.manifest.json`, per the existing V4/V5/V8w convention
  (CLAUDE.md). If coppa ever ships the streaming redesign, its proposal doc
  states outputs will differ from today's — V4/V5 will need regeneration at
  that point; this is accepted, not a blocker.

## 7. Soak harness — `skimmer soak`

Reuses the `listen` engine (§4) against either a real device or a paced file
replay, for a configurable duration. Tracks and asserts:

- **Panics:** the loop runs inside `catch_unwind`; any panic is a hard
  failure (0 tolerated).
- **Input overrun:** `coppa_audio::AudioRingConsumer::overflow_count()`
  sampled at the end; nonzero is a hard failure ("no input overrun" per
  ROADMAP).
- **Unbounded memory:** process RSS sampled every N seconds; growth beyond a
  fixed threshold after an initial warm-up window is a hard failure. (The
  streaming design in §4 has no per-chunk-growing buffers by construction —
  this check catches a regression, not an expected steady-state cost.)

Exits nonzero with a summary on any violation; this is the tool ROADMAP's "≥1
hour without panic, unbounded memory, or input overrun" gate runs against,
and is written to also serve M2/M3's soak needs (24 h SDR soak, 7-day spot
soak) without redesign.

## 8. Manual acceptance: live W1AW copy

Not CI. A short runbook (in the implementation plan): run
`skimmer listen --device <rig audio>` during a scheduled W1AW code-practice
transmission, confirm the printed text is recognizable copy. Documented as a
manual step you run yourself before declaring M1 done.

## 9. Decoder fix: all-dah opener prior

M0 pinned decision #20 (`docs/DECISIONS/2026-07-11-m0-implementation-pins.md`)
found `ClusterPair::initialize()` (`skimmer-decode/src/timing.rs`) always
assumes a lone unimodal 5-mark cluster is dits, never dahs — an all-dah
opener (message starting T/M/O, or mid-message strings like "TU", "OM", "73")
fails 12/12 in a stress sweep ("TTTTT"→"5", "MMM"→"", "OO"→""), silently
producing plausible-looking wrong text. The pin explicitly gates this on M1:
"it must be addressed before M1 builds live off-air decoding on top of this
code, where dah-heavy openers are realistic."

Fix direction (per the pin): replace the unconditional "assume dits" default
with an absolute-ms prior using the existing SPEC §4.1 `[20, 150]` ms dit
clamp — a lone ~180 ms cluster is far likelier dahs than dits than a lone
~60 ms cluster is. Implemented in `skimmer-decode`, verified by a regression
test sweeping all-dah and all-dit openers (the same stress sweep that found
the bug), and re-verified against V1 (must stay CER=0).

## Testing strategy

- Unit: Hilbert transformer round-trip/frequency-response tests (mirroring
  `proto.rs`'s stopband-margin test style); `AudioIqSource` tests against
  `coppa_audio`'s file/loopback backends (no real hardware needed in CI).
- Golden: V1–V6 all green via `skimmer decode` (batch) — `listen`'s streaming
  loop is validated against the same vectors via file replay, asserting
  byte-identical decoded text to the batch path (determinism requirement,
  ARCHITECTURE §9).
- Soak: `skimmer soak --duration 3600 --source file:<long-synthetic-scene>`
  in CI (bounded runtime via a script-generated long clean scene, not real
  wall-clock hardware time) as an automated proxy for the 1 h gate; a
  real-hardware soak is the manual runbook's job, not CI's.
- All-dah regression: sweep of all-dah/all-dit opener characters through the
  real decode pipeline (§9), plus V1 CER=0 unaffected.
