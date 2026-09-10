# manta — codebase map

Open-source, cross-platform, wideband multi-signal CW skimmer in Rust: decodes
every CW signal across an SDR passband and emits RBN-compatible spots — an open
alternative to the single closed-source Windows program the Reverse Beacon
Network currently depends on. 9-crate Cargo workspace under `crates/`. Tickets
are `MAN-<n>`. Status: M1 implemented; all M2 sub-projects implemented; M2
acceptance still open pending physical-hardware legs (Pi4 CPU budget, 24 h
live-SDR soak) — see CLAUDE.md's "Status" section.

## Layer 0 — no internal manta-* deps
- **manta-decode** — CW keying state machine, timing, Morse decode
  (`decoder.rs`, `envelope.rs`, `timing.rs`, `tree.rs`, `beam.rs`, `events.rs`).
  Its only dependency is `serde` — the cheapest crate in the workspace to build.
- **manta-dsp** — PFB channelizer, noise-floor estimator, frequency estimation.
  Depends on the sibling repo's `coppa-dsp` (FFT); the channelizer and
  noise-floor estimator are new code here, not reused from coppa.

## Layer 1 — build on layer 0
- **manta-spot** — callsign/CQ-DE validation, cty.dat/SCP cross-check, repetition
  gate, dedupe, blocklist, notch, confidence. Depends on manta-decode.
- **manta-input** — IQ sources: file, live audio, KiwiSDR, SoapySDR, HPSDR.
  Depends on coppa-audio and manta-dsp.
- **manta-testkit** — synthetic CW generator and golden-vector harness. Depends
  on coppa-channel (Watterson HF fading, AWGN) and manta-decode/-dsp.

## Layer 2 — orchestration and output
- **manta-engine** — pipeline orchestration: input → channel → track → decoder
  (`track.rs` holds the detector and TrackManager; `soak.rs`/`soak_metrics.rs`
  the long-run instrumentation). Depends on manta-decode, manta-dsp,
  manta-input, manta-spot.
- **manta-server** — telnet DX-cluster server + JSON Lines/WebSocket spot stream
  (ARCHITECTURE.md §7). Depends on manta-spot.

## Layer 3 — binaries
- **manta-cli** — the `manta` daemon and CLI binary. Depends on manta-decode,
  manta-engine, manta-input, manta-server, manta-spot, manta-testkit.
- **manta-soak-harness** (MAN-19) — long-duration unattended soak harness looping
  a synthetic 40 m CW pileup scene through the live pipeline. A separate binary,
  not part of manta-cli; depends on manta-dsp, manta-engine, manta-input,
  manta-testkit.

## Non-crate areas
- **docs/SPEC-decode-core.md** — normative algorithm spec: channelizer constants,
  noise-floor estimator, track state machine, decoder equations, confidence
  formulas, determinism rules, golden vectors V1–V10, config-key table.
  Implement from this.
- **docs/DECISIONS/** — dated implementation-pin and design-decision digests.
- **docs/RUNBOOKS/** — operational runbooks (W1AW live copy, Pi4 CPU budget,
  network exposure).
- **wiki/INDEX.md** (`wiki/pages/*.md`) — accumulated gotchas and decisions,
  descriptive only; always loses conflicts with code and docs/.
- **ARCHITECTURE.md** — data flow and crate graph. Its layout diagram still says
  "8-crate workspace"; the real member list in `Cargo.toml:3-13` has 9
  (`manta-soak-harness` was added later). Trust Cargo.toml.

## Conventions
- Tests: `cargo test --workspace`, plus `--features soapy` / `--features hpsdr`
  for the input-hardware lanes CI runs separately.
- CI has no paths filter — every PR runs the full ubuntu+macos matrix.
- Deterministic decode path is a hard requirement: file input must produce
  byte-identical spot logs across platforms.
- Serena here is **read_only**: navigation only, never edits.
