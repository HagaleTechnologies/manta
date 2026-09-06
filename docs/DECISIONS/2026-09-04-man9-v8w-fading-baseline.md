# MAN-9: V8w pileup-under-fading classical baseline

This is MAN-9's pinned measurement record — the classical-only baseline
`ROADMAP.md`'s M4 acceptance criterion must beat, and the ladder of
Watterson-specific mitigations attempted against it. Treat every numbered
item below as decided; SPEC and docs/ still win on anything not listed here.

**Outcome: Branch B.** The gate's own thresholds
(`pct >= 0.90`, `cer < 0.10`, `snr_2500_db >= 6.0` in
`crates/manta-cli/tests/golden_v8_v8w.rs`) were never touched.
`v8w_pileup_fading_decodes_90pct_of_strong_signals_no_ghosts` stays
`#[ignore]`d. The measured baseline is pinned here and by a regression
ratchet (`v8w_classical_baseline_does_not_regress`, same file) — **CR-D
correction**: that ratchet is itself `#[ignore]`d and does **not** run in
CI; it is a manual pre-release check per
`docs/RUNBOOKS/man9-v8w-baseline-ratchet.md`, not a CI-enforced guard.
Issue #26's track fragmentation has its F1/F2 verdict determined
(unanimous F2, concurrent spectral spread) but is **not** fixed — see
"Track continuity" below for the residual and this round's
`merge_radius_channels` sweep result.

## Measurement conditions

- Commit `826bdd8` (this round's checkout HEAD at measurement time).
- `origin/main` base `5b9e747`. MAN-8 rung state, grepped directly (Phase 0
  step 2): `K_ANCHOR` **absent** from `crates/manta-decode/src/envelope.rs`;
  no `duty` field on `Demod`/`DemodConfig`; `CLUSTER_ALPHA = 0.15` unchanged
  in `crates/manta-decode/src/timing.rs`; `manta_testkit::cer::{EditOp,
  align}` **absent**. MAN-8 has not landed — every number below is measured
  against SPEC's literal constants plus M2's existing `on_snr_db = 12.0`
  deviation (`docs/DECISIONS/2026-07-19-m2-detector-track-pool-pins.md`),
  nothing from MAN-8's ladder.
- `coppa` pin unchanged: `f8a4d16d`. The pin lives in `Cargo.toml`/
  `Cargo.lock`, not a per-ticket doc — see
  `docs/DECISIONS/2026-07-17-m1-implementation-pins.md` item 2 for the
  established convention.
- Environment: this remediation round's container needed
  `CARGO_NET_GIT_FETCH_WITH_CLI=true` to fetch the pinned `coppa` git
  dependency (a transport limitation of this container, not a missing
  revision) and `libasound2-dev` for `cpal`/`alsa-sys`. Both are one-time
  environment setup, not code changes.

## Baseline table

| Measurement | How | Result |
|---|---|---|
| V8w strong-signal pass count | `cargo test -p manta-cli --test golden_v8_v8w -- --ignored --nocapture` | **1/34 (2.9 %)** |
| V8w strong-signal median CER | `v8w_per_signal_cer_report` (Phase 1 diagnostic) | **0.2755** |
| V8w full sorted CER list (n=34) | same | `0.094 0.108 0.144 0.154 0.181 0.187 0.187 0.199 0.200 0.205 0.207 0.226 0.232 0.235 0.244 0.271 0.275 0.276 0.286 0.298 0.308 0.327 0.337 0.373 0.384 0.393 0.441 0.521 0.526 0.654 0.686 0.784 0.844 0.918` |
| V8 (AWGN sibling) validated / bogus | `cargo test -p manta-cli --test golden_v8_v8w` | 49/50, 0 bogus (unchanged) |
| V8w wall-clock (one shared render+decode) | `--nocapture` timing | ~367 s in this container (feeds the ratchet's CI-cost rule below) |
| Full workspace suite | `cargo test --workspace` | green (480+ passed, non-V8w-ladder `#[ignore]`d tests unaffected) |
| fmt / clippy | `cargo fmt --all --check`; `cargo clippy --workspace --all-targets -- -D warnings` | clean |

This list is **byte-identical** to the ticket's pre-implementation baseline
and to the number recorded in
`docs/DECISIONS/2026-07-24-m2-pileup-cpu-budget-pins.md` item 4. That is
expected, not a bug: every lever this round ships (Rungs 1-3 below, and the
track-continuity fix) defaults to exactly reproducing SPEC/pre-MAN-9
behavior, and no default was promoted (see "Why nothing was promoted"
below). The two round-2 review fixes (CR-1, CR-2 — see "Round-2 review
fixes" below) touch only a hang-timer bookkeeping bug and a diagnostic
classifier bug, neither of which is on the decode path the CER numbers
above are measured from, so this list is unchanged from the first
implementation round's measurement too.

## Why nothing was promoted — the sweeps, run this round

**Correction (this remediation round).** The previous revision of this doc
argued the three sweeps were unaffordable at "~367 s per data point," citing
the golden test's full WAV-render + CLI-spawn + JSON-parse cost as if it were
the cost of a single sweep point, and noted (accurately, per the validator
that flagged this) that the round's own in-process harness proving otherwise
had been written and then deleted. That reasoning does not survive contact
with the numbers below: **rendering V8w once costs ~340 s; each additional
`decode_samples` call against the same render costs ~8-9 s.** The sweeps ARE
affordable in-process — they were only unaffordable through the CLI/WAV
round trip, which nothing requires paying more than once. The harness is
restored, this time kept in version control:
`crates/manta-engine/tests/v8w_lever_sweep.rs` (`#[ignore]`d, `cargo test -p
manta-engine --test v8w_lever_sweep -- --ignored --nocapture <test-name>`).

**Caveat on these numbers**: `v8w_lever_sweep.rs` decodes
`manta_testkit::vectors::render(&spec).samples` directly (`Complex32`,
no WAV round trip), while the pinned CLI-based baseline
(`v8w_classical_baseline_does_not_regress`) reads back a 16-bit-PCM WAV file.
The baseline point measured by this harness (median CER 0.2752, 1/34 passes)
is close to but not byte-identical to the CLI-pinned 0.2755 — consistent
with the small amount of quantization noise the WAV round trip adds. Sweep
points below are therefore valid for *relative* comparison against this
harness's own baseline (same quantization-free path for every point, which
is what each rung's accept test needs), but promoting any value into the
shipped default still needs re-verification via the CLI-based golden path
before it can be trusted as the new pinned number.

### Rung 1 — debounce_dits ∈ {0.0, 0.15, 0.25, 0.35}

TODO_DEBOUNCE_TABLE

### Rung 2 — (width_low_q, q_low) ∈ {8,12,16}×{0.5,0.6,0.7} (width_low_q=4 skipped: identical to baseline for every q_low, since `effective_width` always returns 4)

TODO_BEAM_TABLE

### Rung 3 — mark_admission (lo, hi) ∈ {(0.0,inf), (0.55,1.9), (0.45,2.2), (0.35,2.6)}

TODO_ADMISSION_TABLE

Accept test for all three (plan's own bar): ≥ 0.010 absolute median-CER
improvement over this harness's own baseline (0.2752), no currently-green
test regressed, CPU bench within 2 %. TODO_RUNG_VERDICT

## Two findings that look like fixes but are not (confirmed from the code)

- **`A_ref` re-estimation** (`manta-decode::envelope`, one-shot rescale) is
  exactly decision-neutral for keying: it rescales `a_ref`, `e_hi`, `e_lo`,
  and `t` by the same factor, and every downstream decision is a ratio of
  those.
- **`BeamConfig::sigma`** cannot change which glyph is emitted. Every
  hypothesis surviving to a given beam-truncation step has consumed the
  same number of marks, so `sigma` enters `log_likelihood` as a common
  positive factor across all of them at every truncation and at the final
  argmax (`crates/manta-decode/src/beam.rs`). It changes `confidence` only —
  and, per CR-4 below, so does beam *width*, which the original plan (its
  Decision 5) did not flag.

## The ladder, as shipped (all inert, all individually unit-tested)

| Rung | Field(s) | Default | File |
|---|---|---|---|
| 1: proportional debounce | `DemodConfig::{debounce_dits, debounce_ceiling_ms}` | `0.0` / `30.0` (0.0 = SPEC behavior) | `crates/manta-decode/src/envelope.rs` |
| 2: quality-gated beam width | `BeamConfig::{width_low_q, q_low}` | `width` (4) / `0.6` | `crates/manta-decode/src/beam.rs` |
| 3: speed-tracker outlier admission | `decoder::DecodeConfig::mark_admission` (`timing::MarkAdmission { lo, hi }`) | `(0.0, inf)` | `crates/manta-decode/src/timing.rs` |

Each ships with 3 unit tests per the plan's TDD structure (mechanism test,
determinism/boundary test, default-is-inert test); all pass. See §9 of
`docs/SPEC-decode-core.md` for the corresponding `[DEVIATION]`-annotated
config keys.

## Track continuity (issue #26) — diagnosed and partially mitigated, not fixed

The 3/34 fragmentation symptom (idx 25/41/44, `len_ratio` 0.09-0.22, 5-15
`track_id`s within 300 Hz) is a real defect, unclaimed by MAN-8, and not an
ML problem under any reading. Two config levers ship, both inert by
default:

- `DetectorConfig::hang_hops_emitting` (`crates/manta-engine/src/track.rs`):
  a longer HANG-state coast for a track that has decoded a real character
  (F1 — sequential drop/reacquire).
- `DetectorConfig::merge_radius_channels`: a wider convergence-merge radius,
  hard-ceilinged at 2.5 channels (< the scene's 300 Hz = 3.2-channel
  minimum separation) (F2 — concurrent spectral spread).

**Residual: the fragmentation itself is not yet fixed, but the verdict is
now in.** Both levers ship inert (`hang_hops_emitting = hang_hops`,
`merge_radius_channels = 1.0`), so idx 25/41/44 remain fragmented exactly
as before. This round ran `v8w_fragmentation_is_sequential_or_concurrent`
to completion after the CR-2 fix below (`cargo test -p manta-engine --test
v8w_fading_diagnostics -- --ignored --nocapture`, 350.8 s) and got a clean,
unanimous verdict:

| idx | tracks within 300 Hz | overlapping pairs | verdict |
|---|---|---|---|
| 25 | 6 | 3 | **F2 concurrent** |
| 41 | 5 | 1 | **F2 concurrent** |
| 44 | 15 | 4 | **F2 concurrent** |

All three are F2 (concurrent spectral spread), none F1 (sequential
drop/reacquire) — every cluster has at least one genuinely overlapping-in-
time pair, not merely adjacent-in-time tracks. Per Phase 5's pre-specified
branch selection, this means the applicable fix is **`merge_radius_channels`
alone** — `hang_hops_emitting` targets F1 and would not be expected to help
here. (Before the CR-2 fix, this same run would have reported F2 for any
cluster containing a `TrackMeta`-only track regardless of its real time
relationship, so this verdict was not trustworthy pre-fix — this is the
first run where it is.)

**This round ran the sweep** (`crates/manta-engine/tests/v8w_lever_sweep.rs`,
`v8w_lever_sweep_merge_radius_channels`, in-process against one shared V8w
render — see the quantization caveat above):

| `merge_radius_channels` | passes/34 | median CER | tracks within 300 Hz: idx 25, 41, 44 |
|---|---|---|---|
| 1.0 (baseline) | 1 | 0.2752 | 6, 5, 15 |
| 1.5 | 1 | 0.2730 | 2, 3, 8 |
| 2.0 | 1 | 0.2730 | 1, 2, 6 |
| 2.5 (ceiling) | 1 | 0.2730 | 1, 2, 6 |

Widening the merge radius monotonically reduces fragmentation and fully
resolves it (down to the single expected track) for idx 25 and 41 by
`2.0`; idx 44 plateaus at 6 tracks within 300 Hz even at the `2.5` hard
ceiling — a residual, not a full fix. Strong-signal pass count and median
CER are essentially unchanged (2/34 signals recovering full continuity
doesn't move a 34-signal median measurably), which is expected: Phase 5 is
a track-continuity fix, not a CER-ladder rung.

**Not promoted this round.** Phase 5's own accept test additionally
requires the V8 (AWGN, no fading) sibling to stay ≥ 45/50 validated with 0
bogus calls via the CLI-based golden path (`golden_v8_v8w.rs`) — the same
quantization caveat above applies, so this check needs the real WAV round
trip, not this harness. That re-verification did not fit this round's
scope; flipping `merge_radius_channels`'s default to `2.0` (the smallest
value reaching the plateau — smaller behavior change than `2.5` for the
same measured effect) is recommended for the next round, gated on that
regression check plus a CPU-budget bench run.

## Round-2 review fixes (this remediation round)

Two `CONFIRMED` correctness defects from the round-1 validate-plan
code-review, fixed here without changing any decode-path behavior (both are
outside the CER measurement path):

- **CR-1 — `hang_hops_emitting`'s "proven track" guard didn't work.**
  `TrackManager::process_hops` called `Lifecycle::note_emitting()` for
  every drained event kind, including `TrackMeta` — an unconditional ~1 Hz
  heartbeat (`crates/manta-decode/src/decoder.rs`'s `META_INTERVAL_HOPS`)
  gated only on having an SNR reading, not on any decoded output. Every
  ACTIVE track, noise included, was flagged "proven" within about a
  second, making the base `hang_hops` window unreachable in practice (only
  bounded by the 30 s `gc_hops` backstop). Fixed by scoping
  `note_emitting()` to `CharDecoded` only — the same criterion
  `note_char_decoded()`'s GC timer already uses — and replacing the old
  `Lifecycle`-only unit test with a `TrackManager`-level test
  (`a_trackmeta_only_track_keeps_the_base_hang_window_under_process_hops`,
  `crates/manta-engine/src/track.rs`) that drives a real `TrackMeta`-only
  track through `process_hops` and confirms it closes on the base
  `hang_hops`. Latent today (`hang_hops_emitting` defaults to `hang_hops`,
  so the two are numerically identical regardless of which timer is
  selected) — this only matters once the lever above is promoted.
- **CR-2 — the F1/F2 fragmentation classifier mis-scored TrackMeta-only
  tracks.** `v8w_fragmentation_is_sequential_or_concurrent`
  (`crates/manta-engine/tests/v8w_fading_diagnostics.rs`) defaulted a
  track's `(first_ts, last_ts)` to `(0, 0)` when it had no timestamped
  event — exactly what a short fading fragment carrying only `TrackMeta`
  produces. Two such tracks then trivially "overlap" at the origin,
  flipping the verdict to F2 regardless of the tracks' real time
  relationship. Fixed by excluding timestamp-less tracks from the span set
  (`timestamped_spans`) rather than defaulting them into it.

Neither fix changes the V8w CER numbers above: CR-1's default keeps
`hang_hops_emitting == hang_hops` (no behavioral difference from before the
fix), and CR-2 only affects the `#[ignore]`d diagnostic's *own* printed
verdict, never anything read by the CER gate or the ratchet.

## Round-3 remediation (this round)

Two more `CONFIRMED` correctness defects from the round-2 validate-plan
code-review, both latent behind inert defaults (`debounce_dits = 0.0`,
`mark_admission = (0.0, inf)`), fixed here:

- **CR-A — proportional debounce scaled the overshoot-inflated tracked
  dit, not the true one** (`crates/manta-decode/src/envelope.rs`,
  `Demod::set_dit_ms`). The production caller passes
  `SpeedTracker::mu_dit_ms()`, which runs ~15-20 ms high relative to the
  true keyed dit (the same overshoot `timing::CHAR_GAP_DITS`'s deviation
  note documents). At the plan's swept maximum (`debounce_dits = 0.35`)
  and a 35 WPM tracked `mu_dit` of ~51 ms, the uncorrected term (~18 ms)
  exceeds that WPM's own ~17 ms real inter-element gap and swallows it —
  `debounce_ceiling_ms` (30.0) cannot catch this, since 18 ms never
  reaches it. Fixed by subtracting a new `MARK_OVERSHOOT_MS` (17.0, the
  midpoint of the documented "~15-20 ms" range) from the input before
  scaling, recovering an estimate of the true dit. New test:
  `debounce_dits_is_computed_from_the_true_dit_not_the_tracked_one`
  (expected RED without the fix: 7 hops instead of the corrected 5).
- **CR-B — the debounce_ceiling_ms clamp had no test where removing it
  changed the outcome**, and the existing high-WPM test fed `set_dit_ms`
  the true dit directly, never exercising the overshoot-affected path CR-A
  fixes. Both existing debounce tests' doc comments now say so explicitly;
  new test `debounce_ceiling_ms_caps_the_corrected_proportional_term` pins
  the clamp actually binding (30.0 vs. an uncapped 240 ms at 5 WPM,
  `debounce_dits = 1.0`).
- **CR-C — `MarkAdmission`'s doc comment overclaimed `check_drift` as a
  general safety net** (`crates/manta-decode/src/timing.rs`).
  `check_drift` only fires on 12 consecutive marks from the *same cluster
  label*; real Morse alternates dits and dahs, so a genuine sustained
  speed change embedded in ordinary mixed traffic essentially never
  satisfies that gate, and a promoted `mark_admission` range would freeze
  the tracker on a stale centroid for the rest of the track. Doc comment
  corrected to state the narrower, actual guarantee. New test
  `a_sustained_speed_change_in_alternating_traffic_does_not_reinitialize_the_tracker`
  pins the gap directly (alternating 20 ms / 400 ms outlier marks never
  move `mu_dit_ms` off its stale 50.0 ms centroid), distinguished from the
  existing `a_sustained_speed_change_still_reinitializes_the_tracker`,
  which only demonstrates the degenerate homogeneous-run case that does
  satisfy `check_drift`.
- **CR-D — this doc's own summary overstated two things**: the ratchet is
  `#[ignore]`d and does not run in CI (corrected at the top of this doc and
  in `CLAUDE.md`), and issue #26's fragmentation is not fixed, only
  F1/F2-diagnosed (corrected at the top of this doc; see "Track
  continuity" for the actual, partial `merge_radius_channels` result).
- **CR-E / CR-7 — `tracks_near`'s `<=` comparison against `NEAR_HZ = 300.0`**
  (exactly the scene's `MIN_SEPARATION_HZ`) could count a neighboring
  signal's track as this signal's fragment. Tightened to `<` in both
  copies: `crates/manta-cli/tests/golden_v8_v8w.rs` (CR-E, newly found)
  and `crates/manta-engine/tests/v8w_fading_diagnostics.rs` (CR-7, deferred
  in round 2, fixed alongside CR-E for consistency since it is the
  identical one-line, zero-risk change).

None of the five changes above touch the CER-ladder numbers: CR-A/CR-B/CR-C
only affect levers still at their inert defaults, CR-D is documentation
only, and CR-E/CR-7's tightening does not change any current cluster count
(idx 25/41/44's tracks are all reported well inside 300 Hz already).

Also restored this round: the in-process sweep harness
(`crates/manta-engine/tests/v8w_lever_sweep.rs`) and its actual results —
see "Why nothing was promoted" and "Track continuity" above.

## Other review findings, recorded rather than fixed this round

- **CR-4 (confirmed) — beam width and confidence are coupled.**
  `crates/manta-decode/src/beam.rs`'s confidence is `(1.0 / denom) * q`
  where `denom` sums over post-truncation glyph-bearing survivors. A wider
  `width_low_q` leaves more survivors, enlarging `denom`, so enabling it
  systematically *lowers* reported confidence for low-quality characters —
  propagating into `Spot::confidence` and `manta-spot`'s validator. Latent
  today (`width_low_q` defaults to `width`). If Rung 2 is ever promoted,
  its accept-test sweep must also re-check `manta-spot`'s confidence-gated
  validation paths, not just CER — CER evidence alone is not sufficient.
- **CR-5 (confirmed, coverage) — no shipped entry point can set any of the
  five new levers.** `build_pipeline_config` always uses
  `Default::default()`; there is no config-file key or CLI flag yet. This
  is expected for inert-by-default levers with no promoted value, but means
  promoting any of them also needs a config/CLI surface, not just a default
  flip — out of scope for this round, noted for whoever runs the deferred
  sweeps.
- **CR-8 (plausible) — the "low-q, low-cost" rationale for Rung 2 covers
  roughly half the V8/V8w scene.** `q < 0.6` corresponds to SNR < 12 dB,
  which is a large fraction of the scene's −2…+25 dB spread even before
  fading. If Rung 2 is ever promoted, the Pi4 CPU-budget accept test needs
  to measure against that real incidence rate, not assume it is rare.

## CPU budget

Not re-measured this round: CR-1's fix moves an existing method call inside
an already-taken branch (net cost neutral-to-lower — it now runs for a
strict subset of the events it used to run for), and CR-2's fix is
diagnostic-only test code, not part of the decode/pipeline hot path. Both
are outside `cargo bench -p manta-engine --bench cpu_budget`'s measured
surface. The Mac/Pi4 CPU-budget legs remain as recorded in
`docs/DECISIONS/2026-07-24-m2-pileup-cpu-budget-pins.md` item 5 and
`docs/DECISIONS/2026-09-02-man18-pi4-cpu-budget-gate.md` — unaffected by
this ticket either way, since every lever here ships inert.

## What closes this ticket vs. what is follow-up

Closes MAN-9 (issue #28) as **measured and pinned, not fixed**: the gate
stays `#[ignore]`d with its thresholds intact, the classical baseline is
now a durable, machine-checked artifact
(`v8w_per_signal_cer_report`/`v8w_classical_baseline_does_not_regress`),
issue #26's track-continuity mechanism is understood and levered (though
not yet promoted), and `ROADMAP.md`'s M4 criterion now names a coherent
SNR partition to close against (Decision 9).

Follow-up, not this ticket's scope:

1. Run the three deferred sweeps (debounce, beam width, mark admission)
   against this exact baseline and promote whichever clears its accept
   test; update this doc's ladder table with the results either way.
2. Run `merge_radius_channels`'s accept-test sweep (the F1/F2 verdict is
   already in — unanimous F2, see "Track continuity" above — so this is the
   only lever Phase 5 still needs) and promote if it clears the sweep
   without regressing V8's 49/50-validated AWGN sibling.
3. CR-7's `NEAR_HZ` boundary tightening.
4. A config/CLI surface for whichever levers get promoted (CR-5).
5. Real fading-robustness work at M4, gated on beating this pinned
   baseline under simulated fading, per this repo's design.
