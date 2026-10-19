# manta codebase map

manta is an open-source, cross-platform, wideband multi-signal CW skimmer in
Rust. It decodes every CW signal across an SDR passband and emits
RBN-compatible spots. 9-crate Cargo workspace under `crates/`; the member list
in the root `Cargo.toml` is the source of truth. Tickets are `MAN-<n>`.
Project status and key constraints live in CLAUDE.md, not here.

## Layer 0: no internal manta-* dependency
- **manta-decode**: CW keying state machine, timing and Morse decode
  (`decoder.rs` holds `TrackDecoder`; `envelope.rs`, `timing.rs`, `tree.rs`,
  `beam.rs`, `events.rs`, `evidence.rs`, and the opt-in `hsmm/` engine).
  Depends on `serde` only.
- **manta-dsp**: the PFB channelizer (`channelizer.rs`), noise floor
  (`floor.rs`), carrier estimation, decimation, static-crash detection and the
  input sample guard. Uses the sibling coppa repo's `coppa-dsp` for FFTs.
  `single` and `freqest` are deprecated in place.

## Layer 1
- **manta-spot**: callsign/CQ-DE validation, cty.dat/SCP cross-check,
  repetition gate, dedupe, blocklist, notch, confidence. Depends on
  manta-decode.
- **manta-input**: IQ sources: WAV file, live audio, KiwiSDR, ka9q-radio,
  HPSDR, and SoapySDR behind the `soapy` feature. Depends on manta-dsp and
  `coppa-audio`.

## Layer 2
- **manta-engine**: pipeline orchestration, input to channel to track to
  decoder (`track.rs` holds the detector and `TrackManager`; `soak.rs` and
  `soak_metrics.rs` the long-run instrumentation; `doctor.rs`, `listen.rs`,
  `multi.rs`). Depends on manta-decode, manta-dsp, manta-input, manta-spot.
- **manta-server**: telnet DX-cluster server, JSON Lines/WebSocket spot
  stream, RBN uplink, RBN comparison, metrics and health. Depends on
  manta-spot.
- **manta-testkit**: synthetic CW generator and golden-vector harness.
  Depends on manta-decode, manta-dsp, manta-input and `coppa-channel`
  (Watterson fading).

## Layer 3: binaries
- **manta-cli**: the `manta` daemon and CLI (`main.rs`; `build.rs` stamps the
  git commit). Depends on manta-decode, manta-engine, manta-input,
  manta-server, manta-spot, manta-testkit and `coppa-audio`.
- **manta-soak-harness**: long-duration unattended soak binary (MAN-19),
  separate from the `manta` binary. Depends on manta-dsp, manta-engine,
  manta-input, manta-testkit.

## Outside crates/
- `docs/SPEC-decode-core.md` (and `-v2`): the algorithm spec to implement
  from, with the golden vectors in section 7.
- `docs/DECISIONS/`: one dated record per ticket outcome, indexed in its
  README.
- `docs/RUNBOOKS/`, `docs/GUIDES/`, `packaging/`: operator material.
- `wiki/INDEX.md`: gotchas; it points at code and docs and loses any
  conflict with them.
- `scripts/`: Python and shell tooling for releases, CI gates and RBN
  scoring, with unittest suites in `scripts/tests/`. Not indexed by Serena's
  Rust language server; use pattern search or grep there.

## Using Serena here
- The project is read-only: navigation tools only.
- Code behind the `soapy` feature is compiled out for rust-analyzer, so
  references into `manta-input/src/soapy.rs` may not resolve. Grep for them.
