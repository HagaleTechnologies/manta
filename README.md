<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-dark.svg">
    <img src="assets/logo-light.svg" alt="manta" width="160">
  </picture>
</p>

<h1 align="center">manta</h1>

<p align="center">
  Open-source wideband CW skimmer. Every CW signal in an SDR passband, decoded at once,
  emitted as RBN-compatible spots.
</p>

<p align="center">
  <a href="https://github.com/HagaleTechnologies/manta/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/HagaleTechnologies/manta/actions/workflows/ci.yml/badge.svg"></a>
  <img alt="License: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg">
  <img alt="Rust 1.85+" src="https://img.shields.io/badge/rust-1.85%2B-orange.svg">
</p>

`manta` is a headless daemon written in Rust. It takes wideband IQ from a
commodity SDR, a KiwiSDR over the network, or a WAV file, channelizes the
whole passband with a polyphase filterbank, runs an independent CW decoder on
every signal it finds, validates the callsigns, and emits spots. Output is the
standard `DX de` cluster format over telnet plus a JSON Lines stream, so
existing aggregators (including the Reverse Beacon Network) and modern
consumers (such as [cqdx](https://cqdx.app)) can ingest it without changes.

No GUI. CLI, a config file, and metrics.

## Why

The Reverse Beacon Network is infrastructure the whole amateur radio hobby
leans on for CW spotting, contest scoring, propagation awareness, and antenna
testing. It runs almost entirely on **CW Skimmer**, a closed-source,
Windows-only program maintained by a single author. That is a single point of
failure for a shared resource. `manta` is an open, cross-platform replacement
with documented, testable algorithms.

## Quickstart

Requires Rust 1.85 or newer.

```sh
# Build
cargo build --release -p manta-cli

# Decode a synthetic golden vector from a file (deterministic, no hardware)
manta gen v1 --out /tmp/v1
manta decode /tmp/v1/v1.wav

# Copy live CW from a public KiwiSDR on 40 m
manta listen --kiwi-host kiwi.example.org --kiwi-freq 7030000

# Copy from a local SDR via SoapySDR (build with --features soapy)
manta listen --soapy-driver driver=rtlsdr --soapy-freq 7030000 --soapy-rate 240000

# Copy from the default audio input (rig audio passband, 48 kHz)
manta listen

# Any of the above as JSON Lines instead of text
manta listen --json --kiwi-host kiwi.example.org --kiwi-freq 7030000
```

`manta --help` lists every subcommand and flag.

## Inputs

| Source | How | Status |
| --- | --- | --- |
| IQ / audio WAV file | `decode`, `listen --source` | Working |
| Sound card (rig audio passband) | `listen --device` | Working, 48 kHz input only |
| KiwiSDR over the network | `listen --kiwi-host` | Working |
| RTL-SDR, Airspy, SDRplay, HackRF, and anything else SoapySDR drives | `listen --soapy-driver`, feature `soapy` | Working, needs hardware soak |

Targets Linux (x86-64 and ARM, Raspberry Pi 4 class), macOS, and Windows.
The CPU budget is a full 192 kS/s passband inside one Raspberry Pi 4 core,
enforced by criterion benches.

## Outputs

- Decoded text or JSON Lines on stdout today.
- RBN-format `DX de` spots over the DX cluster telnet protocol (port 7300)
  and a JSON Lines / WebSocket stream (port 7301): in progress, see
  [ROADMAP.md](ROADMAP.md) milestone M3.

The decode path is deterministic: the same file in produces byte-identical
spot logs out. That is a hard requirement, and CI enforces it with golden
test vectors.

## Status

Pre-1.0. What exists and what does not:

- **Done:** single-signal decode from files and live audio (M1); the full
  wideband pipeline of polyphase channelizer, detector, track manager, and
  decoder pool, with SoapySDR and KiwiSDR inputs (M2 sub-projects); callsign
  validation, CQ/DE parsing, cty.dat and SCP cross-checks, dedupe, wired into
  the engine (M3, in part).
- **Open acceptance gates:** the Raspberry Pi 4 CPU-budget measurement and a
  24 h live-SDR soak both need physical hardware.
- **Next:** the telnet and JSON spot servers, TOML config, metrics, and an RBN
  parity benchmark on recorded contest IQ.
- **Known limits:** the classical decoder loses copy under heavy HF fading on
  a few golden vectors (issues #25 and #28). Closing that gap is the M4 ML
  fusion stage, gated on beating the classical baseline under simulated
  fading.

[ROADMAP.md](ROADMAP.md) has the milestone breakdown with acceptance
criteria.

## Non-goals

- Not an interactive receiver or panadapter. Use SDR++ or similar for a
  waterfall.
- Not a general digital-mode skimmer. FT8 and RTTY are out of scope for 1.0,
  though the channelizer architecture does not preclude them later.
- Not a cluster network. `manta` is a spot source, not an aggregator.
- Not a logger. No QSO state.

## Documentation

- [ARCHITECTURE.md](ARCHITECTURE.md): the seven-crate workspace, data flow,
  and the channelizer, decoder, validation, and output design.
- [docs/SPEC-decode-core.md](docs/SPEC-decode-core.md): the
  implementation-level algorithm spec. Channelizer constants, noise-floor
  estimator, track state machine, decoder equations, confidence formulas,
  determinism rules, golden vectors, and the config-key table.
- [ROADMAP.md](ROADMAP.md): milestones M0 to M4 with acceptance criteria.
- [docs/DECISIONS/](docs/DECISIONS/): dated design decisions and
  implementation pins.
- [wiki/INDEX.md](wiki/INDEX.md): accumulated gotchas and cross-references.

## Related projects

- [coppa](https://github.com/HagaleTechnologies/coppa): `manta` reuses its
  FFT and its AWGN / Watterson HF channel models for the DSP core and test
  harness.
- **dit**: `manta`'s decoder is the wideband, headless evolution of dit's
  single-channel CW engine.
- [cqdx](https://cqdx.app): `manta`'s JSON spot stream is designed as a
  first-class cqdx ingest source.

## Contributing

Open an issue or a pull request. Main moves only by PR, CI must be green, and
the golden-vector determinism tests are the bar every decoder change has to
clear. See [SECURITY.md](SECURITY.md) for vulnerability reporting.

## License

MIT OR Apache-2.0, at your option. Unless you explicitly state otherwise, any
contribution intentionally submitted for inclusion in this project shall be
dual licensed as above, without any additional terms or conditions.
