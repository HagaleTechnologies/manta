# skimmer

An open-source, cross-platform, wideband multi-signal CW skimmer.

## Quickstart

```
cargo run -p skimmer-cli -- gen v1 --out /tmp/v1 && cargo run -p skimmer-cli -- decode /tmp/v1/v1.wav
```

## Why

The Reverse Beacon Network — infrastructure the entire amateur radio hobby depends
on for CW spotting, contest scoring, propagation awareness, and antenna testing —
runs almost entirely on **CW Skimmer**, a closed-source, Windows-only program
maintained by a single author. That is a single point of failure for a critical
piece of shared infrastructure.

`skimmer` is an open replacement: a headless daemon that consumes wideband IQ from
commodity SDRs, detects and decodes every CW signal in the passband simultaneously,
validates callsigns, and emits RBN-compatible spots over the standard DX cluster
telnet protocol — so existing aggregators (including the RBN itself) can consume
them with zero changes.

## Goals

- **Full-band decoding**: every CW signal in a 96–768 kHz passband, concurrently.
- **RBN-compatible output**: standard `DX de` spot format over telnet; drop-in for
  existing cluster/aggregator tooling. Plus a JSON stream for modern consumers
  (e.g. [cqdx](https://cqdx.app) ingest).
- **Cross-platform daemon**: Linux (x86/ARM — Raspberry Pi class), macOS, Windows.
  No GUI. CLI + config file + metrics.
- **Commodity hardware**: RTL-SDR, Airspy, SDRplay via SoapySDR; KiwiSDR over the
  network; rig audio passband as a degenerate single-channel mode.
- **Open algorithms**: the DSP chain, decoder, and validation logic are documented
  (see [ARCHITECTURE.md](ARCHITECTURE.md)) and testable against synthetic and
  recorded IQ with known ground truth.

## Non-goals

- Not an interactive receiver or panadapter (no waterfall UI; use SDR++ etc.).
- Not a general digital-mode skimmer (FT8/RTTY are out of scope for 1.0; the
  channelizer architecture doesn't preclude them later).
- Not a cluster *network* — skimmer is a spot **source**, not an aggregator.
- Not a logger; no QSO state.

## Relationship to sibling projects

- **[coppa](https://github.com/HagaleTechnologies/coppa)** — skimmer reuses
  `coppa-dsp` (FFT) and `coppa-channel` (AWGN / Watterson HF fading models)
  for its DSP core and test harness rather than reimplementing them. (FIR
  prototype design and AGC are new code here — see SPEC-decode-core §10.)
- **dit** — skimmer's decoder design is the wideband, headless evolution of
  dit's single-channel CW engine (envelope → keying state machine → adaptive
  speed tracking → character decode, with an optional ML decoder fused by
  confidence). Lessons from dit's fusion engine inform the M4 ML stage.
- **cqdx** — skimmer's JSON spot stream is designed to be a first-class cqdx
  ingest source.

## Status

**M1 implemented** (live audio decode) and **M2 sub-project 1 implemented**
(PFB channelizer — real N-channel polyphase filterbank wired into the
engine, replacing the M0/M1 single-channel shim). Next up: M2 sub-project 2
(detector/track manager). See [ROADMAP.md](ROADMAP.md) for the full
milestone breakdown and [ARCHITECTURE.md](ARCHITECTURE.md) for the design.

## License

MIT OR Apache-2.0 (matching coppa).
