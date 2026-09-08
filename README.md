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
  <a href="https://github.com/HagaleTechnologies/manta/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/HagaleTechnologies/manta"></a>
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

## Installation

No Rust toolchain, no cloning the source. Pick one:

**Download a prebuilt binary** from the [Releases
page](https://github.com/HagaleTechnologies/manta/releases) — macOS
(Intel or Apple Silicon), Windows, or Linux (x86_64 or arm64, including
Raspberry Pi OS 64-bit). Unpack the archive and run the `manta` binary
inside it. **Linux binaries need `libasound2` installed** (audio input is
an unconditional dependency, even if you only ever use file, KiwiSDR, or
HPSDR input) — `sudo apt install libasound2` on Debian/Ubuntu/Raspberry Pi
OS, or the equivalent ALSA runtime package on other distros; without it
the binary fails to start.

**Or run the Docker image** (works anywhere Docker does, `linux/amd64` and
`linux/arm64`):

```sh
docker run --rm ghcr.io/hagaletechnologies/manta:latest --help
```

When running as a long-lived server (not `--help`), stop it with
`docker stop -t 30 <container>` — Docker's own default 10-second grace
period before SIGKILL is shorter than manta's supported drain window for
a slow client's final write (up to 25s), so the default can cut a
graceful shutdown off mid-drain.

Both are built by [`.github/workflows/release-publish.yml`](.github/workflows/release-publish.yml)
directly from each tagged release's commit — every published binary
traces to a specific, auditable source revision. (If the image above
returns an authorization error, the GHCR package needs its one-time
"make public" step in GitHub's package settings after the first real
release — see MAN-65. Windows binaries need the [Visual C++
Redistributable](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist)
installed if it isn't already.)

**Building from source** (if you're developing manta itself, or need a
platform/feature combination the release matrix doesn't cover — the
`soapy` feature below, for instance, isn't in the official release
binaries since it needs the SoapySDR system library) still works exactly
as before, and is what the rest of this Quickstart assumes:

## Quickstart

Requires Rust 1.85 or newer.

```sh
# Build
cargo build --release -p manta-cli

# Decode a synthetic golden vector from a file (deterministic, no hardware)
manta gen v1 --out /tmp/v1
manta decode /tmp/v1/v1.wav

# See a real spot on the DX cluster telnet port -- no radio required.
# --realtime paces the 120 s recording in wall-clock time (instead of
# draining it in ~3 s) so a client has time to connect; the spot itself
# appears about 12 s in.
cat > server.toml <<'TOML'
[server]
station_callsign = "N0CALL"
# Loopback only: without this, bind_addr defaults to 0.0.0.0 and this demo
# would publish the telnet, JSON/WebSocket and metrics ports on every
# interface -- see docs/RUNBOOKS/network-exposure.md before dropping it.
bind_addr = "127.0.0.1"
TOML
manta listen --source /tmp/v1/v1.wav --server-config server.toml --realtime
# then, in another terminal, any time in the next two minutes:
telnet localhost 7300
# at the "login:" prompt, type any callsign and press Enter -- this is a
# real login, not optional, and the server only starts streaming spots to
# you once it lands
# then, at the "de N0CALL-# >" prompt: type sh/dx to see the spot, or just
# wait -- if you logged in within the first ~12 s, the spot arrives on its
# own once decoded, with no further command needed

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
| IQ / audio WAV file | `decode`, `listen --source` | Working -- `listen --source` takes a 2-channel IQ WAV (what `decode`/`gen` use, sidecar-aware; any channelizer rate other than 48 kHz, or 48 kHz with a `<stem>.json` sidecar) or a 48 kHz mono/stereo rig-audio WAV (48 kHz stereo with no sidecar downmixes like mono); add `--realtime`/`--loop` to pace or repeat file replay (`--loop` requires `--realtime` when `--server-config` is given, so looped spots aren't published with runaway future timestamps). A "channelizer rate" is any `fs` where `fs / 93.75` is a power of two -- 12/24/48/96/192/384 kHz and so on, not 44.1 or 100 kHz |
| Sound card (rig audio passband) | `listen --device` | Working, 48 kHz input only |
| KiwiSDR over the network | `listen --kiwi-host` | Working |
| RTL-SDR, Airspy, SDRplay, HackRF, and anything else SoapySDR drives | `listen --soapy-driver`, feature `soapy` | Working, needs hardware soak |

Targets Linux (x86-64 and ARM, Raspberry Pi 4 class), macOS, and Windows.
The CPU budget is a full 192 kS/s passband inside one Raspberry Pi 4 core,
enforced by criterion benches.

## Outputs

- Decoded text or JSON Lines on stdout.
- RBN-format `DX de` spots over the DX cluster telnet protocol (port 7300)
  and a JSON Lines / WebSocket stream (port 7301), started with
  `listen --server-config`; see the Quickstart above for a hardware-free
  demo, and [ROADMAP.md](ROADMAP.md) milestone M3 for acceptance status.

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
- Not a multi-process Windows orchestrator. `manta` is a single Rust binary;
  there is no companion-program sprawl to sequence-launch.
- No CW Skimmer-style dual MME/WDM soundcard configuration surface, and no
  CAT/rig control to align a narrowband receiver with the channelizer.
  `manta` does ingest a local audio device (`listen`/`listen --device`,
  rig-audio passband) — this is about the legacy Windows driver-selection
  and band-scope-alignment machinery around that, not the input itself,
  which the wideband sources (OpenHPSDR/Hermes, SoapySDR, KiwiSDR) don't
  need at all since the channelizer already covers the whole passband at
  once.

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
