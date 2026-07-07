# skimmer

Open-source, cross-platform, wideband multi-signal CW skimmer in Rust. Decodes
every CW signal across an SDR passband and emits RBN-compatible spots — an open
alternative to the single closed-source Windows program the Reverse Beacon
Network currently depends on.

## Status

Design phase complete; no implementation yet. Next step is M0 in ROADMAP.md
(decode one clean synthetic CW signal from an IQ file).

## Documents (read in this order)

- `README.md` — goals and non-goals
- `ARCHITECTURE.md` — 8-crate workspace, data flow, channelizer/decoder/
  validation/output design
- `docs/SPEC-decode-core.md` — implementation-level algorithm spec: exact
  channelizer constants, noise-floor estimator, track state machine, decoder
  equations, confidence formulas, determinism rules, golden test vectors
  V1–V10, and the full config-key table. Implement from this; the design
  decisions are already made.
- `ROADMAP.md` — milestones M0–M4 with acceptance criteria

## Key constraints

- Reuses `coppa-dsp` (FFT) and `coppa-channel` (Watterson fading for tests)
  from the sibling coppa repo. Note: coppa has NO Kaiser filter designer — the
  PFB prototype designer is new code here (`skimmer-dsp::proto`).
- Deterministic decode path is a hard requirement: file input → byte-identical
  spot logs.
- Classical decoder first; ML fusion (dit's pattern) only at M4, gated on
  beating the classical baseline under simulated fading.
- CPU budget: full 192 kS/s passband within one Raspberry Pi 4 core, enforced
  by criterion benches.
- JSON spot schema is an ecosystem contract — belongs in the `dispensa` repo
  (ADR pending), not solely here.
- No GUI: daemon + CLI. Outputs: RBN-format cluster telnet (:7300) and JSON
  Lines/WebSocket (:7301) for cqdx ingest.
