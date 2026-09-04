# manta

Open-source, cross-platform, wideband multi-signal CW skimmer in Rust. Decodes
every CW signal across an SDR passband and emits RBN-compatible spots — an open
alternative to the single closed-source Windows program the Reverse Beacon
Network currently depends on.

## Status

M1 implemented (live audio decode; manual W1AW live-copy run still
outstanding — its blocker, MAN-4's spurious-track bug, is now fixed; see
docs/DECISIONS/2026-09-04-man-4-hilbert-guard-pins.md — but the run itself
still needs real rig hardware and an operator, per
docs/RUNBOOKS/m1-w1aw-live-copy.md). All M2 sub-projects implemented (PFB
channelizer; detector/track manager + decoder pool; V8/V8w pileup +
CPU-budget bench; SoapySDR input; KiwiSDR input) — see
docs/DECISIONS/2026-07-1[7-9]*.md and 2026-07-2[4-5]*.md.
V1/V3/V4/V7/V8/V9/V10 green; V2/V5/V6/V8w are tracked known
classical-decoder fading-robustness limitations (`#[ignore]`d, issues
#25/#28), deferred to M4 ML fusion by design, not M2 blockers. **M2
acceptance is still open**: Pi4 CPU-budget leg and 24 h live-SDR soak are
unmet — both need physical hardware not reachable from this environment.
`manta-dsp::single`/`freqest` deprecated in place.

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
  (`manta-dsp::proto`).
- **Watterson upstream fixes landed** (`coppa` main 2026-07-07, commits
  `9ab1547`, `34aec5f`, `fc35895`): the two bugs identified in the
  SPEC-watterson audit (Doppler spread ~41% too fast vs ITU-R F.1487;
  per-block SNR renormalization erasing fading dynamics) are fixed. Golden-
  vector freeze for V4/V5/V8w is **unblocked** — pin the exact coppa
  commit used when the vectors are generated.
  - **Convention verified against coppa's current `watterson.rs`:** the
    fixed convention matches what manta's spec expects — `doppler_spread_hz`
    is the **2σ width** of the Gaussian Doppler PSD (sigma = spread / 2,
    via `doppler_sigma_hz()`), and normalization is **ensemble-only**
    (E|g|² = 1 across realizations; per-realization normalization is
    explicitly rejected in the module doc and code comment). No divergence
    from manta's expected convention.
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
- M0 testkit generates its own ref-bandwidth AWGN (see
  docs/DECISIONS/2026-07-11-m0-implementation-pins.md); migrate to coppa
  awgn_ref_bw when it ships.
- coppa pin bumped to `f8a4d16d` for M1 (Watterson fixes; see
  docs/DECISIONS/2026-07-17-m1-implementation-pins.md). `AudioIqSource`
  requires exactly 48000 Hz input — coppa-audio's resampler is unreachable
  (no `mod resampler;`, no `rubato` dep); see that doc's pin 3.

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
- **Auto-merge is on, repo-wide** (overrides the global "Tony merges"
  default for this repo specifically): every PR gets `gh pr merge --auto
  --squash` right after opening; GitHub merges it unattended once required
  CI (`test (ubuntu-latest)`, `test (macos-latest)`) is green. See
  docs/DECISIONS/2026-07-25-pr-auto-merge-policy.md.


## Code review convergence

Every review round fixes P1 findings inline. From round 2 onward, P2-and-
lower findings are not fixed inline — they're captured verbatim into a
follow-up ticket instead, so the PR converges instead of chasing
progressively finer findings across rounds. Round 1 is unrestricted (fix
everything reasonable). Full policy:
docs/DECISIONS/2026-08-07-pr-review-convergence-policy.md.
