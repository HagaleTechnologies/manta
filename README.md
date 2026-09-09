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
commodity SDR, an OpenHPSDR/Hermes device, a KiwiSDR over the network, or a
WAV file, channelizes the whole passband with a polyphase filterbank, runs an
independent CW decoder on every signal it finds, validates the callsigns, and
emits spots. Output is the standard `DX de` cluster format over telnet plus a
JSON Lines / WebSocket stream, so existing aggregators (including the Reverse
Beacon Network) and modern consumers (such as [cqdx](https://cqdx.app)) can
ingest it without changes.

No GUI. CLI, a TOML config file, and Prometheus metrics.

```console
$ telnet manta.example.org 7300
login: W1XYZ
de W5AU-# >
DX de W5AU-#:  14000.7  W1AW     CW   7 dB  20 WPM  CQ  1533Z
```

## Why

The Reverse Beacon Network is infrastructure the whole amateur radio hobby
leans on for CW spotting, contest scoring, propagation awareness, and antenna
testing. It runs almost entirely on **CW Skimmer**, a closed-source,
Windows-only program maintained by a single author. That is a single point of
failure for a shared resource. `manta` is an open, cross-platform replacement
with documented, testable algorithms.

## Installation

```sh
cargo install --path crates/manta-cli --features hpsdr   # Rust 1.85+
```

That puts a `manta` binary in Cargo's bin directory (`~/.cargo/bin`
unless you moved `CARGO_HOME`), which a standard Rust install already has
on `PATH` — every command below assumes a bare `manta` resolves. To build
without installing, `cargo build --release -p manta-cli` leaves the
binary at `target/release/manta`; run that path instead.

No tagged release yet, so there is no prebuilt binary or Docker image to
pull — build from source for now. Both publish automatically, for every
platform, from the first tag:

```sh
# once a release exists:
docker run --rm ghcr.io/hagaletechnologies/manta:latest --help
```

**Notes:**
- Linux binaries need `libasound2` installed (audio input is an
  unconditional dependency, even if you only ever use file, KiwiSDR, or
  HPSDR input) — `sudo apt install libasound2` on Debian/Ubuntu/Raspberry
  Pi OS, or the equivalent ALSA runtime package elsewhere.
- Stop a long-running container with `docker stop -t 30 <container>` —
  Docker's default 10-second grace period is shorter than manta's drain
  window for a slow client's final write (up to 25 s), so the default
  can cut a graceful shutdown off mid-drain.
- Input backends are cargo features, and a build that did not ask for
  one has no flags for it — `--hpsdr-host` / `--soapy-driver` fail with
  `error: unexpected argument` on a build without them. `hpsdr`
  (OpenHPSDR/Hermes) has no native dependency, which is why the install
  line above turns it on. `soapy` (RTL-SDR, Airspy, SDRplay, HackRF via
  SoapySDR) needs the native SoapySDR system library installed first;
  once you have it, add it: `--features hpsdr,soapy`.
- Windows binaries need the [Visual C++
  Redistributable](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist)
  installed if it isn't already.

## 60-second demo

No SDR, no radio. Install, generate a synthetic golden vector, decode it:

```sh
cargo install --path crates/manta-cli --features hpsdr   # puts `manta` on PATH
manta gen v1 --out /tmp/v1              # 120 s of synthetic CW, 20 WPM, +20 dB
manta decode /tmp/v1/v1.wav
# CQ DE W1AW W1AW K CQ CQ DE W1AW W1AW K CQ CQ DE W1AW W1AW K CQ …
```

Then point it at a real signal — a public KiwiSDR needs no hardware of
your own:

```sh
manta listen --kiwi-host kiwi.example.org --kiwi-freq 7030000
```

File replay (`listen --source`) currently accepts only 48 kHz mono audio
and runs faster than realtime, so the hardware-free path above stops at
`decode`; a paced replay that can drive the servers below is being
worked on.

`manta --help` lists every subcommand and flag.

## Run it as a node

One flag starts the DX cluster telnet server, the JSON/WebSocket stream,
and the metrics endpoint alongside the decoder:

```toml
# server.toml
[server]
station_callsign = "W5AU"   # your call; becomes `DX de W5AU-#:` and JSON `deCall`
bind_addr = "127.0.0.1"     # 0.0.0.0 to accept remote clients
telnet_port = 7300
json_port   = 7301
metrics_port = 7302
```

```sh
manta listen --kiwi-host kiwi.example.org --kiwi-freq 7030000 --server-config server.toml
telnet localhost 7300          # DX de … lines
nc localhost 7301              # one JSON object per spot
curl -s localhost:7302/metrics # Prometheus text
```

Forwarding to an upstream RBN-style collector is a `[[rbn_uplink]]`
block. **Set `dry_run = true` first** — the default is `false`, so an
uplink block starts transmitting as soon as you add it.

Before exposing any port beyond loopback, read
[docs/RUNBOOKS/network-exposure.md](docs/RUNBOOKS/network-exposure.md).

## Inputs

| Source | How | Status |
| --- | --- | --- |
| IQ / audio WAV file | `decode`, `listen --source` | Working (`decode` takes IQ; `listen --source` takes 48 kHz mono audio) |
| Sound card (rig audio passband) | `listen --device` | Working, 48 kHz input only |
| KiwiSDR over the network | `listen --kiwi-host` | Working |
| OpenHPSDR / Hermes (Hermes-Lite 2, Red Pitaya, QMTech) | `listen --hpsdr-host`, feature `hpsdr` — on in the install line above, no native dependency | Working; protocol verified against reference sources, not yet against hardware |
| RTL-SDR, Airspy, SDRplay, HackRF, anything SoapySDR drives | `listen --soapy-driver`, feature `soapy` — **not** in the install line above; needs the SoapySDR system library, then `--features hpsdr,soapy` | Working, needs hardware soak |

Targets Linux (x86-64 and ARM, Raspberry Pi 4 class), macOS, and Windows.
The CPU budget is a full 192 kS/s passband inside one Raspberry Pi 4 core,
enforced by criterion benches.

## Outputs

All four ship today; `manta listen --server-config <file>` starts the
first three together.

- **DX cluster telnet server** (`:7300`) — standard login prompt and
  RBN-format `DX de` lines, with enough command grammar (`sh/dx`,
  `set/dx/filter`) for stock clients. This is the RBN/aggregator
  compatibility surface.
- **JSON Lines / WebSocket stream** (`:7301`) — one full-fidelity spot
  object per line; a raw TCP client and a WebSocket client share the
  port. This is the [cqdx](https://cqdx.app) ingest surface.
- **Outbound RBN uplink** — forwards validated spots to an upstream
  collector in the same `DX de` wire format, with reconnect and multiple
  simultaneous targets. Working against a collector; **not yet validated
  against RBN's live ingest**.
- **Prometheus metrics** (`:7302/metrics`) — spot, client, uplink and
  source-health counters. Some gauges are still placeholders;
  ARCHITECTURE §8 says which.

`decode` and `listen` also print decoded text or `--json` events on
stdout. That output is a debugging aid, not a stable interface — the
servers above are.

The decode path is deterministic: the same file in produces byte-identical
spot logs out. That is a hard requirement, and CI enforces it with golden
test vectors.

## Status

Pre-1.0, and pre-first-release. What is true today:

- **Shipped:** the full wideband pipeline — polyphase channelizer,
  noise-floor detector, track manager, decoder pool — with five input
  sources, callsign validation (cty.dat, SCP, CQ/DE parsing, dedupe), and
  all four output surfaces above.
- **Not yet measured:** the RBN parity benchmark (recall vs. RBN on
  recorded contest IQ), the Raspberry Pi 4 CPU budget, and a 24-hour
  live-SDR soak. The first needs reference data; the other two need
  physical hardware. The CPU budget's desktop leg *has* been measured and
  currently reads as a **fail** — ≈0.53x–0.58x realtime against a <0.5x
  budget, pending a clean rerun on a quiet machine ([ROADMAP.md](ROADMAP.md) M2).
- **Known limits:** the classical decoder loses copy under heavy HF
  fading on several golden vectors, and at low SNR the validator still
  admits occasional bogus callsigns from noise. Closing the fading gap is
  classical-DSP work in flight, with ML fusion behind it at M4.
- **No tagged release yet**, so the container image above is empty until
  the first tag.

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

**Running a node:** [docs/RUNBOOKS/network-exposure.md](docs/RUNBOOKS/network-exposure.md)
(exposing the servers safely) · the "Run it as a node" section above ·
`manta <subcommand> --help` for every flag.

**Contributing:** [ARCHITECTURE.md](ARCHITECTURE.md) — the nine-crate
workspace, data flow, and the channelizer, decoder, validation and output
design · [docs/DECISIONS/](docs/DECISIONS/) — dated design decisions and
implementation pins · [wiki/INDEX.md](wiki/INDEX.md) — accumulated
gotchas.

**Algorithms and research:** [docs/SPEC-decode-core.md](docs/SPEC-decode-core.md) —
channelizer constants, noise-floor estimator, track state machine,
decoder equations, confidence formulas, determinism rules, golden
vectors, config-key table · [ROADMAP.md](ROADMAP.md) — milestones M0 to
M4 with acceptance criteria.

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
