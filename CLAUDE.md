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

## Knowledge wiki

`wiki/INDEX.md` is the map of accumulated knowledge — read it before deep
exploration; open pages relevant to your task. After substantive work, run
/wiki-update: distill new gotchas/decisions/corrections into the wiki (or
into docs/ if normative — the wiki points, it never restates). The wiki is
descriptive and always loses conflicts with code and docs/.

## Key constraints

- Reuses `coppa-dsp` (FFT) from the sibling coppa repo. Note: coppa has NO
  Kaiser filter designer — the PFB prototype designer is new code here
  (`skimmer-dsp::proto`).
- **Watterson upstream fixes landed** (`coppa` main 2026-07-07, commits
  `9ab1547`, `34aec5f`, `fc35895`): the two bugs identified in the
  SPEC-watterson audit (Doppler spread ~41% too fast vs ITU-R F.1487;
  per-block SNR renormalization erasing fading dynamics) are fixed. Golden-
  vector freeze for V4/V5/V8w is **unblocked** — pin the exact coppa
  commit used when the vectors are generated.
  - **Convention verified against coppa's current `watterson.rs`:** the
    fixed convention matches what skimmer's spec expects — `doppler_spread_hz`
    is the **2σ width** of the Gaussian Doppler PSD (sigma = spread / 2,
    via `doppler_sigma_hz()`), and normalization is **ensemble-only**
    (E|g|² = 1 across realizations; per-realization normalization is
    explicitly rejected in the module doc and code comment). No divergence
    from skimmer's expected convention.
  - **SNR convention (still live):** this repo's spec froze SNR-in-2500-Hz;
    the shared `awgn_ref_bw()` design in SPEC-watterson reconciles it with
    the benchmark harness's 3 kHz convention.
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

## Multi-agent hygiene

You are never alone in this repo — other agents may be working concurrently
in other clones, branches, or worktrees.

- **Start fresh:** `git fetch` and rebase onto `origin/main` before reading
  code or making decisions; stale context produces wrong work.
- **Claim before work:** search open PRs/issues first; open a draft PR early —
  the draft PR *is* the claim. Don't duplicate in-flight work.
- **Isolate:** always a branch (worktree preferred), never a shared checkout's
  main. Use per-session scratch dirs; don't bind fixed ports.
- **Flush at the end:** push (`--force-with-lease` only) and open/update your
  PR before finishing. Unpushed work is invisible work.
- **Main moves only by PR merge.**
