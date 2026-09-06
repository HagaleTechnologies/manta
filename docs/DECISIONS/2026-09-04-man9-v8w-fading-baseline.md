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
(unanimous F2, concurrent spectral spread); its `merge_radius_channels`
lever was **promoted** in Round 4 (`1.0 -> 2.0`, re-verified against the
CLI-based V8 AWGN golden path with no regression on that gate) but only
**partially** fixed the symptom — one of the three fragmented signals
fully resolved, the other two improved but remained fragmented.
**Round-5 remediation reverted the promotion**: validate-plan found and
reproduced a real production regression outside the AWGN gate's coverage
(signals 140-200 Hz apart spawn-and-merge every hop and never decode —
see "Round-5 remediation" below). `merge_radius_channels` ships at its
SPEC default `1.0` again; the field, its doc comment, and its two unit
tests stay as the right home for a future, correctly-scoped fix. See
"Track continuity" below for the full sweep, re-verification, and revert
record. The three CER-ladder rungs (Rungs 1-3, below) were all swept to
completion this round and none cleared their accept test — every one
stays at its inert default.

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
| V5 CER (WattersonPreset::Poor, single signal, default seed) | `cargo test -p manta-cli --test golden_v2_v3 v5_passes_end_to_end_from_wav -- --ignored --nocapture` | **1.4167** (pre-existing `#[ignore]`d classical-decoder fading gap, unrelated to this ticket -- see the test's own doc comment; recorded here only because Phase 0 asked for it, not re-investigated) |
| V6 CER (WattersonPreset::Poor, single signal, default seed) | `cargo test -p manta-cli --test golden_v2_v3 v6_passes_end_to_end_from_wav -- --ignored --nocapture` | **0.1429** (same status: pre-existing `#[ignore]`d, issue #25, unaffected by this ticket) |
| Full workspace suite | `cargo test --workspace` | green (480+ passed, non-V8w-ladder `#[ignore]`d tests unaffected) |
| fmt / clippy | `cargo fmt --all --check`; `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| CPU budget | `cargo bench -p manta-engine --bench cpu_budget` | **Not re-measured this round** (see "CPU budget" section below for why); last recorded: mean 5.5455 s wall-clock/iteration, 95% CI [5.5341 s, 5.5578 s] (≈0.37x realtime), `cpu_budget_mac_under_half_core` confirms 0.360x < the 0.5x Mac budget -- `docs/DECISIONS/2026-07-24-m2-pileup-cpu-budget-pins.md` item 5. Pi4 leg still outstanding (item 6, same doc). |

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

**Round-5 note**: this was briefly untrue between Round 4 and Round 5 —
Round 4 promoted `merge_radius_channels` to `2.0`, which this table's
numbers (measured at `826bdd8`, before that promotion) never reflected.
Round 5 reverted the promotion (see "Track continuity" and "Round-5
remediation" below) after validate-plan found it regressed production
decode, so the table is byte-identical again, this time because the
branch's shipped default genuinely matches what was measured, not because
a stale doc failed to record a change.

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

**Round-4 remediation ran all three sweeps** (`cargo test -p manta-engine
--test v8w_lever_sweep -- --ignored --nocapture`, 335.1 s total for all 21
points across 4 sweep functions, one shared render), after fixing CR-beta
(see "Round-4 remediation" below) — the numbers below are post-fix and are
what promotion/revert decisions are made against.

### Rung 1 — debounce_dits ∈ {0.0, 0.15, 0.25, 0.35}

| `debounce_dits` | passes/34 | median CER | Δ vs. baseline |
|---|---|---|---|
| 0.00 (baseline) | 1 | 0.2752 | — |
| 0.15 | 1 | 0.2752 | 0.0000 |
| 0.25 | 0 | 0.2752 | 0.0000, **loses the one pass** |
| 0.35 | 0 | 0.2781 | **+0.0029 (regresses)** |

No cell improves the median; 0.25 loses the sweep's only passing signal and
0.35 additionally regresses the median CER itself. **Fails the ≥ 0.010
accept test outright — reverted to the default 0.0** (field and its 5 unit
tests, including Round-3's CR-A/CR-B fixes, are kept; they are correct and
cheap regardless).

### Rung 2 — (width_low_q, q_low) ∈ {8,12,16}×{0.5,0.6,0.7} (width_low_q=4 skipped: identical to baseline for every q_low, since `effective_width` always returns 4)

| cell | passes/34 | median CER | frag[25,41,44] |
|---|---|---|---|
| width_low_q=4 (baseline) | 1 | 0.2752 | 6,5,15 |
| every one of the 9 cells (8/0.5, 8/0.6, 8/0.7, 12/0.5, 12/0.6, 12/0.7, 16/0.5, 16/0.6, 16/0.7) | 1 | 0.2752 | 6,5,15 |

**Byte-identical to the baseline for every cell tested.** Widening the beam
under low channel quality changes nothing measurable on this scene at any
swept width/threshold combination — a clean negative result, not a
measurement gap. **Fails the accept test — reverted to the default
`width_low_q = width`** (field and unit tests kept).

### Rung 3 — mark_admission (lo, hi) ∈ {(0.0,inf), (0.55,1.9), (0.45,2.2), (0.35,2.6)}

| `(lo, hi)` | passes/34 | median CER | Δ vs. baseline |
|---|---|---|---|
| (0.0, inf) (baseline) | 1 | 0.2752 | — |
| (0.55, 1.9) | 2 | 0.2730 | **+0.0022 (best cell)** |
| (0.45, 2.2) | 2 | 0.2733 | +0.0019 |
| (0.35, 2.6) | 1 | 0.2752 | 0.0000 (reverts to baseline: band too wide to exclude anything) |

The best cell, `(0.55, 1.9)`, is a genuine small improvement (one extra
strong-signal pass, median CER down 0.0022) but is less than a quarter of
the plan's ≥ 0.010 absolute bar. **Fails the accept test — reverted to the
default `(0.0, inf)`** (field and unit tests kept).

**Verdict for all three rungs: none clears its accept test.** The largest
single-rung movement measured (mark admission, −0.0022 median CER) is
roughly 4.5× too small to matter against a 0.2752 baseline that must drop
below ~0.10 for the gate itself, confirming this plan's own up-front
assessment (Decision 9's framing, and the "Concern with the request"
section) that classical tuning of these three mechanisms cannot close a gap
of this size. All three levers ship, all three stay at their inert
defaults, and this is a completed, numeric negative result -- not a
skipped one.

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
resolves idx 25 (down to the single expected track) by `2.0`; idx 41
improves from 5 tracks to 2 but is not fully resolved; idx 44 plateaus at 6
tracks within 300 Hz even at the `2.5` hard ceiling — both a residual, not
a full fix. (**Correction, Round-4 remediation**: the previous revision of
this doc claimed `2.0` "fully resolves... for idx 25 and 41" -- idx 41's
own row in the table above has always read 2, not 1; that was a
transcription error in the prose, not in the measured data, caught while
re-confirming these numbers post-CR-beta.) Strong-signal pass count and
median CER are essentially unchanged (partial continuity recovery on 2/34
signals doesn't move a 34-signal median measurably), which is expected:
Phase 5 is a track-continuity fix, not a CER-ladder rung.

**Promoted in Round 4, reverted in Round 5.** Phase 5's own accept test
additionally requires the V8 (AWGN, no fading) sibling to stay ≥ 45/50
validated with 0 bogus calls via the CLI-based golden path
(`golden_v8_v8w.rs`) — the same quantization caveat above applies, so this
check needs the real WAV round trip, not the in-process harness. Round-4
remediation ran it:
`cargo test -p manta-cli --test golden_v8_v8w v8_pileup_validates -- --nocapture`
at `merge_radius_channels = 2.0` passes (`v8_pileup_validates_at_least_45_of_50_with_no_bogus_calls
... ok`, 14.5 s) — no regression on that gate. On that evidence,
`DetectorConfig::default()`'s `merge_radius_channels` was flipped from
`1.0` to `2.0` in Round 4 (the smallest value reaching the plateau).

**That evidence was insufficient, and Round-5 remediation reverted the
promotion.** `2.0` is not just "smaller than the 2.5 hard ceiling" — it
exceeds `Track::owned()`'s own ±1-channel ownership radius
(`crates/manta-engine/src/track.rs`), which is what actually inhibits
`spawn` on a channel. A second signal sitting 1.5-2.0 channels
(140-200 Hz) away is therefore on a channel `Track::owned()` does not
claim: it legally spawns a new candidate track every hop, and
`merge_converged` — now comparing against the widened `2.0` radius —
merges that candidate away on the very same hop, forever. Neither track
ever survives long enough to promote and emit, so **both** signals in
that separation band are lost. Reproduced end-to-end on a throwaway
integration test (two clean +20 dB, 20 WPM CW signals, 30 s, 96 kS/s, no
fading): at 150 Hz separation (1.60 channels), `merge_radius_channels =
1.0` decodes `["W1AW"]` from 7 tracks, while `2.0` returns
`Err("no signal found")` — zero events. At 200 Hz (2.13 channels), `1.0`
decodes both `["W1AW", ...]`-equivalent while `2.0` loses both. The V8
AWGN sibling gate above cannot see this: `manta_testkit::vectors`'s
pileup scene enforces `MIN_SEPARATION_HZ = 300.0` (3.2 channels) between
every pair, which sits entirely outside the affected 1.5-2.0 channel
band — a passing 45/50-with-0-bogus result was never capable of ruling
this out.

`DetectorConfig::default()`'s `merge_radius_channels` is back at `1.0`
(SPEC behavior, byte-identical to the pre-Round-4 baseline). The field,
its doc comment, the two unit tests that pin `2.0`/`2.5` behavior via an
explicit override (unaffected — they never relied on the default), and
`v8w_lever_sweep_merge_radius_channels` all stay: they are correct code
and remain the right starting point for a fix that is actually scoped to
the problem, e.g. widening `Track::owned()`'s ownership radius to match
whatever merge radius ships, or gating the merge on independent evidence
the two tracks are the same signal rather than on proximity alone. See
"What closes this ticket vs. what is follow-up" below. CPU-budget impact
of the revert is nil for the same reason the Round-4 promotion's was
expected to be nil: the changed comparison,
`(ca - cb).abs() < cfg.merge_radius_channels`, is an O(1) per-pair
threshold check already on the hot path, now simply back at its original
constant — not separately re-benched this round, same "CPU budget"
section below.

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

## Round-4 remediation (this round)

Four more `CONFIRMED` correctness defects from the round-3 validate-plan
code-review, fixed here, plus the work those fixes unblocked (the three
deferred sweeps and the `merge_radius_channels` promotion decision, both
now recorded above):

- **CR-alpha — `hang_hops_emitting` is silently capped by `gc_hops`, and
  the cap was neither tested nor documented**
  (`crates/manta-engine/src/track.rs`, `Lifecycle::on_hop`'s `Hang` arm).
  The GC silent-timer is checked *before* the `hang_hops_emitting` timer,
  and `silent_count` (the GC timer) is never reset on the ACTIVE -> HANG
  transition — only a real `CharDecoded` resets it. A track already
  partway through its `gc_hops` budget when a fade begins gets less than
  the full `hang_hops_emitting` of coast, and any value at or above
  `gc_hops` is unreachable outright. Latent today (`hang_hops_emitting`
  defaults to `hang_hops`, both far under `gc_hops`), but undocumented and
  untested, exactly as the original plan anticipated ("check that this
  ordering still holds and assert it in the test"). Fixed by documenting
  the ceiling on `DetectorConfig::hang_hops_emitting` and on
  `docs/SPEC-decode-core.md`'s `hang_emitting_ms`, and adding
  `hang_hops_emitting_is_capped_by_gc_hops_carried_over_from_before_the_fade`,
  which pins a track closing `Silent` after only 4 HANG hops despite
  `hang_hops_emitting = 1000`, because it entered the fade already 10 hops
  into a `gc_hops = 15` budget.
- **CR-beta / CR-gamma — the in-process sweep harnesses matched
  text-less tracks the CLI-pinned golden harness cannot see**
  (`crates/manta-engine/tests/v8w_lever_sweep.rs`'s `nearest_text` and
  `crates/manta-engine/tests/v8w_fading_diagnostics.rs`'s `nearest_track`).
  Both built one map per track_id from every event kind including
  `TrackMeta`, then took an unbounded `min_by` frequency search over it —
  so a `TrackMeta`-only fragment (no `CharDecoded`/`WordBoundary` ever)
  could be selected as a signal's nearest match, scoring a spuriously bad
  CER (lever_sweep) or a spuriously fragmented `len_ratio`
  (fading_diagnostics) for a signal whose real, farther track actually
  decoded fine. `golden_v8_v8w.rs`'s own `per_track`/`match_tracks_by_freq`
  never had this bug (its `texts` map only gets an entry from a real
  `CharDecoded`/`WordBoundary`). Fixed by restricting both `nearest_text`
  and `nearest_track` to tracks with a decoded-text event, matching the
  golden harness's semantics exactly, while leaving `tracks_near`
  (fragmentation *counting*, a deliberately wider population) untouched in
  both files. Verified harmless to every number already published in this
  doc: re-running all four sweeps post-fix reproduced the beam-width,
  debounce, and merge-radius baseline/fragmentation-count rows
  byte-for-byte (see "Why nothing was promoted" and "Track continuity"
  above) — this scene's `TrackMeta`-only fragments never happened to sit
  closer in frequency than the real track for any point actually swept, so
  no previously-published number was silently wrong, only unguarded.
- **CR-delta — `golden_v8_v8w.rs`'s `tracks_near` counted a different,
  narrower population than the figure its own doc comment cited.** It
  filtered `per_track`'s texts-only map, which structurally excludes every
  `TrackMeta`-only fragment — so `v8w_per_signal_cer_report`'s
  `tracks_within_300hz` column could never reproduce the "5-15 tracks
  within 300 Hz" figure the `v8w_fading_diagnostics.rs` harness (which
  does count them) reports for idx 25/41/44. Fixed by adding
  `all_track_freqs` (every track_id's last `TrackMeta.freq_hz`, regardless
  of decoded text) and repointing `tracks_near` at it, so both harnesses'
  fragmentation counts now describe the same population.
- **CR-epsilon — this doc shipped four literal `TODO_*` placeholders**
  in "Why nothing was promoted" under a heading claiming the sweeps had
  been run. Fixed by actually running them (post CR-beta/gamma, so the
  numbers are trustworthy) — see Rungs 1-3 above and "Track continuity"
  for the `merge_radius_channels` re-run and promotion decision.

None of CR-alpha/beta/gamma/delta touch the shipped decode path: CR-alpha
only adds a doc comment and a unit test on `Lifecycle`, whose
`hang_hops_emitting` input is still inert; CR-beta/gamma/delta are
entirely inside `#[cfg(test)]`/`tests/` harness code. The only decode-path
change this round is the `merge_radius_channels` promotion recorded under
"Track continuity" above, which is production behavior, deliberately, and
is re-verified there against the real CLI/WAV golden path (not just the
in-process harness these four fixes touch).

## Round-5 remediation (this round)

One `CONFIRMED` correctness defect from the round-4 validate-plan
code-review — serious enough to revert Round-4's one production-behavior
change:

- **Finding 1 — the Round-4 `merge_radius_channels` promotion regresses
  decode of signals 140-200 Hz apart.** Full mechanism, reproduction, and
  rationale moved into "Track continuity" above (the "Promoted in Round
  4, reverted in Round 5" subsection) rather than duplicated here, since
  that section is the durable record for this lever.
  `DetectorConfig::default()`'s `merge_radius_channels` is back to `1.0`;
  the field, its doc comment, and its two unit tests (which pin `2.0`/
  `2.5` behavior via an explicit override, not the default) are unaffected
  and stay.
- **Two documentation-correctness findings, fixed alongside it**: this
  doc's "Baseline table" section claimed "no default was promoted" while
  its own "Track continuity" section said the opposite — true again now
  that the promotion is reverted, and the top summary / "Track
  continuity" / "What closes this ticket" sections above are reworded to
  describe the promote-then-revert history rather than re-asserting the
  now-stale "promoted" framing. The pinned baseline numbers
  (`MEASURED_MEDIAN_CER`, the golden test's ignore-comment CER list, and
  the runbook's "Runs" entry) were measured at commit `826bdd8`, before
  the Round-4 promotion, so they describe a config the branch briefly
  stopped shipping (`merge_radius_channels = 2.0`) and then, this round,
  resumed shipping (`1.0`) — re-measured and re-confirmed still current;
  see the runbook's new "Runs" entry.
- `cargo fmt --all` also applied this round, fixing two branch-added
  formatting violations (`track.rs`'s
  `hang_hops_emitting_is_capped_by_gc_hops_carried_over_from_before_the_fade`
  test and `v8w_fading_diagnostics.rs`'s `nearest_track` helper) that
  `cargo fmt --all --check` — a required CI step — was failing on.

No unit test or golden-vector assertion changed; `merge_radius_channels`'s
two explicit-override unit tests (`track.rs`) never depended on the
default, so they were green both before and after this revert.

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
surface. Round-4 remediation's CR-alpha/beta/gamma/delta fixes (below) are
likewise outside that surface: CR-alpha only adds a doc comment and a unit
test on `Lifecycle::on_hop` (its `hang_hops_emitting` input stays at its
inert default), and CR-beta/gamma/delta only touch `#[cfg(test)]`/`tests/`
harness code, never the decode/pipeline hot path. The Mac/Pi4 CPU-budget
legs remain as recorded in
`docs/DECISIONS/2026-07-24-m2-pileup-cpu-budget-pins.md` item 5 (**Mac: mean
5.5455 s wall-clock/iteration, 95% CI [5.5341 s, 5.5578 s], ≈0.37x
realtime; `cpu_budget_mac_under_half_core` confirms 0.360x, under the 0.5x
Mac budget**) and `docs/DECISIONS/2026-09-02-man18-pi4-cpu-budget-gate.md`
(**Pi4 leg still outstanding, pending real Pi4 hardware**) — unaffected by
this ticket either way, since every lever here ships inert.

## What closes this ticket vs. what is follow-up

Closes MAN-9 (issue #28) as **measured and pinned, not fixed**: the gate
stays `#[ignore]`d with its thresholds intact, the classical baseline is
now a durable, machine-checked artifact
(`v8w_per_signal_cer_report`/`v8w_classical_baseline_does_not_regress`),
all three CER-ladder rungs were swept to completion and none cleared their
accept test (Rungs 1-3 above), issue #26's track-continuity mechanism is
understood (unanimous F2, concurrent spectral spread) and a
`merge_radius_channels` promotion was measured, attempted, and then
reverted after Round-5 remediation found it regressed production decode
outside the AWGN gate's coverage (see "Track continuity" above), and
`ROADMAP.md`'s M4 criterion now names a coherent SNR partition to close
against (Decision 9).

Follow-up, not this ticket's scope:

1. A config/CLI surface for `hang_hops_emitting`, `width_low_q`/`q_low`,
   `mark_admission`, and (again, following the Round-5 revert)
   `merge_radius_channels` (CR-5) — none of the five levers has a shipped
   config-file key or CLI flag, so promoting any of them still needs one.
2. Real fading-robustness work at M4, gated on beating this pinned
   baseline under simulated fading, per this repo's design — the primary
   remaining path, since none of MAN-9's three classical CER-ladder rungs
   moved the median CER by more than 0.0022 against the 0.010 accept bar
   (Rung 3 was the closest).
3. `merge_radius_channels`'s track-continuity fix, correctly scoped this
   time: idx 25/41/44's fragmentation (6/5/15 tracks within 300 Hz) is
   still unfixed at the reverted `1.0` default. A future attempt must
   either widen `Track::owned()`'s ±1-channel ownership radius to match
   whatever merge radius it ships (closing the spawn/merge-churn gap
   Round-5 found), or gate the merge on independent evidence the two
   tracks are the same signal rather than on center-distance proximity
   alone, and must add golden or unit coverage in the 1.0-2.9 channel
   (90-270 Hz) band the existing 300 Hz-minimum-separation scenes cannot
   see, before any default is flipped again.
