# manta

Open-source, cross-platform, wideband multi-signal CW skimmer in Rust. Decodes
every CW signal across an SDR passband and emits RBN-compatible spots — a
second, independent implementation alongside CW Skimmer Server, for the
Linux/ARM/headless platforms it has no native build for.

## Status

M1 implemented (live audio decode; manual W1AW live-copy run still
outstanding). All M2 sub-projects implemented (PFB channelizer;
detector/track manager + decoder pool; V8/V8w pileup + CPU-budget bench;
SoapySDR input; KiwiSDR input) — see docs/DECISIONS/2026-07-1[7-9]*.md and
2026-07-2[4-5]*.md. V1/V3/V4/V7/V8/V9/V10 green; V2's WPM gate is green too
(MAN-7/103's near-channel-edge WPM bug is fixed — keying-threshold placement
and a symmetric mark/gap dit-period estimate, see
docs/DECISIONS/2026-09-07-man103-keying-edge-placement.md) — and V2's
separate CER gate is green since MAN-115 removed its warmup-floor dilution.
V6 now passes and V8w meets its 0-bogus criterion with a >= 20/50
validated floor (MAN-213's fade-tracking keying rails, measured 28/50 — see
docs/DECISIONS/2026-10-05-man213-fade-tracking-keying-rails.md). **V5's
and the V8w per-signal CER gate's fading-robustness gap is classical-DSP
work to fix before M4** (MAN-107 through MAN-113), not deferred to M4 ML
fusion by design — see docs/DECISIONS/2026-09-06-broad-review-decisions.md
D8. **M2 acceptance
is still open**: Pi4 CPU-budget leg (also paused pending MAN-100 through
MAN-113 landing in full, not just MAN-107-113 above — D6), 24 h live-SDR
soak, and **VR1–VR8** (ROADMAP.md's M2 "Accept when", a standing gate
since 2026-09-09, not a one-time redesign check — 3/9 pass, see below)
are unmet — the first two need physical hardware not reachable from this
environment.
`manta-dsp::single`/`freqest` deprecated in place. `decode.engine` default
is still `legacy`; `hsmm` (MAN-166 decode-core-v2) is implemented and
CLI-reachable but not promoted — stage-2 gate measured FAIL 2026-09-09
(oracle as_word 56%/framed 32% vs required 60%/40%; 4/11 V-vectors, 3/9
VR-vectors pass) — see
docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md. Variable-width
capture (issue #169, `--capture-rate-hz`) implemented -- see
docs/superpowers/specs/2026-09-09-variable-width-capture-design.md.
MAN-261: one TOML file (`[server]`/`[[rbn_uplink]]`/`[input]`/`[spot]`/
`[detector]`/`[decode]`, CLI > `MANTA_*` > file) configures
`run`/`soak`/`doctor` — see docs/DECISIONS/2026-10-06-man261-config-surface.md.
MAN-76: `manta config check` validates a config (run's pre-I/O pipeline, no
source or listener) and `manta config init` writes a commented every-key
scaffold — see docs/DECISIONS/2026-10-07-man76-config-check-init.md.
MAN-96: secondary-skimmer field-node kit (outage counters,
`scripts/field-node.py`, `scripts/shadow-compare.py`, runbook) implemented;
the 30-day hardware run itself is outstanding — see
docs/DECISIONS/2026-10-07-man96-secondary-skimmer-field-node.md.
MAN-132: the metrics listener (`/metrics`, `/healthz`) binds
`[server].metrics_bind_addr`, default `127.0.0.1`, while telnet/JSON keep
`bind_addr` (`0.0.0.0`) — see
docs/DECISIONS/2026-10-08-man132-metrics-loopback-bind.md.
MAN-78: SIGHUP to a `[server]` daemon reloads `[spot]` lists and
`[[rbn_uplink]]` `dry_run`; invalid reloads are logged and ignored.
`cty_path`/`scp_path` and every other key need a restart. See
docs/DECISIONS/2026-10-10-man78-live-reload.md.

MAN-126: `manta doctor` runs named setup checks (config refusals, audio,
server ports, clock via SNTP plus Linux `adjtimex`, RBN uplink TCP reach)
before its signal check, prints PASS/WARN/FAIL/SKIP with a fix per problem,
and exits 1 on any FAIL — see
docs/DECISIONS/2026-10-10-man126-doctor-setup-checks.md and
docs/RUNBOOKS/setup-checks.md.

MAN-79: `--cty`/`--scp` and `[spot].cty_path`/`scp_path` override the
vendored `cty.dat`/`master.scp` at run time. `run`/`listen`/`soak`/`doctor`/
`config check` warn when the built-in `cty.dat` is more than 180 days old.
See docs/DECISIONS/2026-10-10-man79-operator-cty-scp-override.md.
MAN-124: `run`'s log is plain text off a terminal (colour only on a TTY,
`NO_COLOR` honoured); `-v`/`-q`/`--log-level` are shorthand for `RUST_LOG`;
`--log-format json` makes every stderr line a JSON record — see
docs/DECISIONS/2026-10-10-man124-log-output.md.
MAN-116: `manta bench sensitivity` produces the recall/CER-vs-SNR (500 Hz)
table over AWGN/Watterson good/poor × WPM, deterministic per build and
flags — see docs/DECISIONS/2026-10-10-man116-sensitivity-benchmark.md.

MAN-298: v0.1.0 release prep. The GitHub Release shares
`docker-publish-release`'s `ghcr-publish` approval but not its outcome, so a
failed image push no longer skips it; release notes and README say
"pre-stability alpha, expect breakage". Tony pushes the tag. See
docs/DECISIONS/2026-10-11-man298-first-release.md.

MAN-123: `run`'s plain-text mode prints decoded text on stderr grouped one
line per track and `SPOT:` lines on stdout; with a `[server]` table decoded
text is off unless `--decoded-text` — see
docs/DECISIONS/2026-10-10-man123-grouped-decoded-text.md.

MAN-125: `manta devices` lists audio inputs and optional Soapy selectors;
`manta check [SOURCE]` reports stream rate, input power and passband noise
floor without decoding or services. See
`docs/DECISIONS/2026-10-10-man125-source-diagnostics.md`.

MAN-131: live audio reads wait for samples (5 s stall error naming the
device, 48000 Hz and a macOS microphone-permission hint); digital-silence
warning; `decode` warns without a sidecar and takes `--center-freq-hz` —
see docs/DECISIONS/2026-10-11-man131-actionable-errors.md.

MAN-138: JSON spots' `deGrid`/`deLat`/`deLon` are the operator's
`[server].operator_grid` (its square's centre) or the new `operator_lat`/
`operator_lon`, `null` when none is set, never a `cty.dat` country
centroid; an inconsistent location fails config loading before I/O. See
`docs/DECISIONS/2026-10-11-man138-station-location.md`.

MAN-93: `[server].history_minutes` controls telnet replay retention
(default 30 minutes, newest-50 floor, 10,000 ceiling). Bare `sh/dx` stays
at 10 rows; loggers request `sh/dx 30m`. Changes need a restart. See
`docs/DECISIONS/2026-10-11-man93-sh-dx-history-window.md`.

MAN-149: `docs/CONFIG.md` (every key's default and what reads it),
`docs/METRICS.md`, `docs/SPOT-SCHEMA.md`, `docs/README.md` (start here, by
audience) and `docs/DECISIONS/README.md`. A new config key, metric family,
wire field, runbook or decision record needs a row in its document; tests
in `docs_consistency.rs` and `spot_message.rs` enforce it — see
docs/DECISIONS/2026-10-11-man149-docs-reference-and-index.md.

MAN-172: high-SNR track churn fixed in `TrackManager` (local-peak spawn
rule, incumbent-wins merge ties); VR3/VR4 pass, VR1/VR2 pass CER but not
WPM. See docs/DECISIONS/2026-10-11-man172-high-snr-track-churn.md.
MAN-269: `run --realtime`/`--loop` (`input.realtime`/`input.loop`) pace and
loop file replay so a hardware-free replay can feed telnet/JSON clients; IQ
WAVs declaring more than 10 MS/s are rejected at open — see
docs/DECISIONS/2026-10-10-man269-paced-looping-replay.md.

MAN-90: the direct `[[rbn_uplink]]` is experimental (docs, a `config check`
note, and the live-transmit WARN lines); the Aggregator path is the supported
route into RBN. Claims C1–C7 stay open until a consenting Aggregator
operator's capture — see
docs/DECISIONS/2026-10-11-man90-uplink-protocol-verification.md and
docs/RUNBOOKS/aggregator-rbn-capture.md.

MAN-115: `TrackManager` skips the channelizer's zero pre-roll, `FloorBank`
seeds a young channel's effective floor from its block median for 1 s, and
`[detector].warmup_ms` defaults to 25 (was 2000). V1 promotes at 0.07 s
(was 2.06 s) and decodes its first character at 0.62 s (was 2.66 s).
`TrackMeta` is still not emitted at promotion. See
docs/DECISIONS/2026-10-11-man115-startup-latency.md.

MAN-81: with no arguments the Docker image runs the daemon from a config
mounted at `/etc/manta/manta.toml` (or `MANTA_CONFIG`), else prints
`--help`; its `HEALTHCHECK` runs the new `manta health` (exit 0/1 from
`/healthz`, metrics address read from the daemon's config); the image is
also built for `linux/arm/v7`. See
`docs/DECISIONS/2026-10-11-man81-docker-config-mount-and-healthcheck.md`.

MAN-127: `manta calibrate` measures the receiver's ppm offset against a
known carrier and saves `[input].freq_correction_ppm` after confirmation —
see docs/DECISIONS/2026-10-11-man127-calibrate-command.md.

MAN-312: `[detector].crash_filter` (`full` default / `starts-only` / `off`)
stops band-wide static crashes (≥ 3 dB on three quarters of the passband's
channels for ≥ 8 ms) from starting tracks or decoding inside them; counted
as `manta_static_crashes_total`; VC1–VC3 vectors — see
docs/DECISIONS/2026-10-13-man312-static-crash-filter.md.

MAN-118: `/metrics` reports decode quality node-wide, with no `track_id`
label: `manta_decode_chars_total`/`manta_decode_garbles_total`,
`manta_track_rail_ratio` (each `legacy` track's keying-rail ratio once per
second) and `manta_candidate_repetitions` (the repetition gate's count per
candidate), via `manta_engine::DecodeQualityObserver` — see
docs/DECISIONS/2026-10-12-man118-decode-quality-metrics.md.

MAN-314: RTTY (two-tone FSK) is recognised after `[detector] rtty_dwell_ms`
(20 s) and starts no CW track on its tones or keying sidebands
(`manta-dsp::fsk`, SPEC §2.6); `decode --json` gains `rtty`, `run` logs
marks/clears and `/metrics` gains `manta_rtty_signals{band}`. See
docs/DECISIONS/2026-10-11-man314-rtty-recognition.md.

MAN-301: `[rbn_compare]` (off by default, needs `[server]`) reads RBN's
CW telnet feed read-only (login callsign only) and grades each spot
`match`/`disagree`/`elsewhere`/`none`/`unknown` after a 180 s window, as a
`spotGrade` record on :7301 and `manta_rbn_compare_*` metrics; `manta
grade` and `score-against-rbn.py --node-log` replay offline through the
same Rust engine. RBN's real login prompt and the live/offline agreement
rate are unmeasured. See
docs/DECISIONS/2026-10-11-man301-live-rbn-comparison.md.

MAN-152: the in-repo `wiki/` is manta's canonical wiki. GitHub's Wiki tab
is not used; disabling it remains an owner step. The wiki pages'
`verified` stamps and `manifest.json` are refreshed and pinned by a
`docs_consistency` test. See
`docs/DECISIONS/2026-10-13-man152-one-wiki.md`.

MAN-316: `[spot].refresh_lists` (default off) makes `run` from a live
receiver download `cty.dat` every 7 days and `MASTER.SCP` daily over plain
HTTP, swap a download that passes the acceptance checks into the
validator, spot geography and JSON stream without a restart, and keep it
under `[spot].list_cache_dir`; a failure keeps the list in use. File replay
never downloads; `--cty-sha256`/`--scp-sha256` pin a version by hash. See
docs/DECISIONS/2026-10-12-man316-reference-list-refresh.md.

MAN-140: `--ka9q-group` (with `--ka9q-port`/`--ka9q-ssrc`/
`--ka9q-interface`) or `[input] type = "ka9q"` reads one existing
ka9q-radio (radiod) IQ channel over RTP multicast, in every build (no
Cargo feature). Rate, encoding, frequency and filter edges come from the
status radiod sends to the data group on port 5006; manta also joins on
127.0.0.1 (radiod's default `ttl = 0`); a retune ends the run with exit 1.
Written clean-room from ka9q-radio's docs and headers at `f44b3a3ee8dd`;
six facts (D-HW) still need a live radiod. See docs/GUIDES/ka9q-radio.md
and docs/DECISIONS/2026-10-13-man140-ka9q-radio-input.md.

MAN-324: `[server].live_port` (unset = off; `live_bind_addr` default
`127.0.0.1`) starts a live-state stream for dashboards: `bandState`,
`trackState`, `candidateDecision` (the why-not-spotted record, from the
validator's opt-in decision log) and `trackClosed` records on their own
port and broadcast channel, served by `json_stream`'s fan-out code (4096
lag, 32 clients), plus `manta_live_*` metrics. Spot output is unchanged.
Fields: `docs/LIVE-STATE.md`; see
docs/DECISIONS/2026-10-12-man324-live-state-stream.md. The dispensa
`liveState.v1` fragment is not yet filed.

MAN-133: `manta completions <shell>` (bash/zsh/fish/powershell/elvish) and
`manta man-pages --out DIR` render from the clap tree `--help` uses; every
release archive ships `man/man1/*.1` rendered natively by the release
workflows' `validate-tag` job. clap_complete publishes hidden commands
and flags, so nothing in the CLI may be hidden. See
docs/DECISIONS/2026-10-13-man133-shell-completions-and-man-pages.md.

MAN-156: CI denies rustdoc warnings (`RUSTDOCFLAGS=-D warnings cargo doc
--no-deps --workspace --locked` in `test (ubuntu-latest)`; `manta-cli`'s
clap help text is exempt). The "Supply Chain" check runs `cargo deny
--locked check` against `deny.toml` (advisories, licenses, bans, sources;
also daily on main). Trusted PRs get a `test (windows-latest)` check-run
(required once MAN-371's owner step O1 is applied). `release-publish.yml`
refuses a tag whose CI push run failed any test leg. The `PR title` check
requires a Conventional Commits title. See
docs/DECISIONS/2026-10-13-man156-ci-regression-gates.md.

MAN-315: the legacy decoder learns per-track dot/dash/gap timing
(`SenderTiming`, SPEC §4.1b; `TimingUpdate` event, `decode --json`
`timing`, a `decode --timing` stderr line); gaps are measured in the
symmetric dit (`dit_estimate_ms`), the word boundary is the sender's own,
and the dash/dot ceiling is 6.0. VW1–VW5 (SPEC §7.3, `golden_vw.rs`)
pass. Open: MAN-111 (scored boundaries) and MAN-113 (WPM hypotheses; the
planned dash re-split was dropped after a bogus V8w spot). See
docs/DECISIONS/2026-10-11-man315-sender-timing.md.

MAN-174: within a `process_hops` batch a merge loser's `TrackClosed`
precedes its survivor's, and a merge survivor that never emitted still
gets its own `TrackClosed`, so a deferred Beacon migrated along a merge
chain resolves at the final survivor's close. `SuppressionCounts` adds
`pending_beacon_rejected_wpm` and `pending_beacon_stale`; pending beacons
are kept in `sample_ts` order, so the cap evicts the true oldest, and an
equal-timestamp one goes through dedupe. SPEC §5/§6 and V22/V25 match. See
docs/DECISIONS/2026-10-14-man174-deferred-beacon-merge-bookkeeping.md.

MAN-192: the HSMM decoder reseeds after a committed Silence segment, not
only on a present re-onset (SPEC v2 §4.7's second trigger), carrying the
closing segment in `HistEntry::closing_segment` and a scored-prefix
count (`scored_live_len`) that replaces the zero-score seed heuristic in
`best_u`/`finish`. No cooldown: every distinct committed Silence
reseeds; snapshots stay within `beam + seed set`. The `hsmm` engine
stays non-default with its stage-2 gate still failing. See
docs/DECISIONS/2026-10-14-man192-silence-reseed.md.

MAN-95: `[spot].validation_level` defaults to `normal`: common, listed and
unlisted callsign patterns need 2, 3 and 5 distinct-message repetitions.
`minimal` keeps flat 2. The independent list derived from MASTER.SCP is
replaced by `--patterns` / `[spot].pattern_path`; allowlist and beacon
exemptions stay. Live decisions report `requiredReps` and `patternTier`.
See docs/DECISIONS/2026-10-14-man95-pattern-rarity-repetition-tiers.md.

MAN-210: golden vector V51 (V9's drift scene, `refine_bw_hz = 30` via
`--config`) exercises the narrowband refiner's refine-enabled path
end-to-end through `manta decode --json` — the evidence any future
default-on change must keep green.

<<<<<<< HEAD
MAN-189: the HSMM decoder no longer re-emits a committed character under
a different closing anchor. `commit::Frontier` (each entry's character
start against the latest emitted timestamp) replaces `sealed`. V7/hsmm
and V8/hsmm now pass, and V5/VR5/VR6a/VR8 CER improve. See
docs/DECISIONS/2026-10-13-man189-hsmm-commit-frontier.md.
=======
MAN-371: `ci-full.yml` creates all three `test (...)` check-runs on a
trusted PR's head fail-closed, and tests that need several free ports
hold every listener until all are read (`free_ports::<N>()`). Adding
`test (windows-latest)` to `main-protection` is owner step O1, followed
by a 20-run merge-queue watch with rollback triggers;
`test-hpsdr (windows-latest)` stays advisory. See
docs/DECISIONS/2026-10-15-man371-windows-required-check.md.
>>>>>>> 6596e3a3e9c3400e6111f2cd02294f74ecd0f383

## Documents (read in this order)

- `docs/README.md` — where operators, contributors and researchers each
  start; every runbook and reference doc
- `README.md` — goals and non-goals
- `ARCHITECTURE.md` — 9-crate workspace, data flow, channelizer/decoder/
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
    the benchmark harness's 3 kHz convention. **Superseded at the
    wire-output boundary only** by MAN-102 / decision D3
    (2026-09-06-broad-review-decisions.md): telnet/RBN-uplink spot lines now
    quote SNR in the 500 Hz RBN/CW Skimmer reference bandwidth, converted
    from the pipeline's native 2500 Hz value at render time; the JSON stream
    keeps the native 2500 Hz value plus an explicit `snrRefHz` field. The
    testkit's input-side 2500 Hz convention this bullet otherwise describes,
    and the `awgn_ref_bw()` reconciliation, are unaffected.
- Deterministic decode path is a hard requirement: file input → byte-identical
  spot logs.
- Classical decoder first; ML fusion (dit's pattern) only at M4, gated on
  beating the classical baseline under simulated fading.
- CPU budget: full 192 kS/s passband within one Raspberry Pi 4 core, enforced
  by criterion benches.
- JSON spot schema is an ecosystem contract — belongs in the `dispensa` repo
  (ADR-0011 `spots.v1`), not solely here. dispensa is private, so
  `docs/SPOT-SCHEMA.md` is the in-repo field reference.
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
- **PR titles are commit subjects:** squash merge uses the title, so it must
  read `<type>(<scope>): <subject>`; the `PR title` check enforces it
  (MAN-156).
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
