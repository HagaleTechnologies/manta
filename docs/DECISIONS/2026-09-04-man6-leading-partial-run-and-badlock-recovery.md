# MAN-6: leading-partial-run suppression and bimodal bad-lock recovery

## Background

MAN-6 (GitHub `HagaleTechnologies/manta#23`): some `(text, wpm, offset, snr,
noise_seed)` combinations decode into a **persistent, non-converging garbled
stream** — continuous garbage for as long as the signal plays, with CER
*growing* with scene duration instead of stabilizing. This is a third,
distinct failure mode from #12 (total silence at `offset_hz == 0`) and #22
(the WPM ≈ 10.0–10.15 garbling cliff).

Ticket repro (`loop_text: true`, `fs=96_000.0`, `center_freq_hz=0.0`,
text `"AU"`, `wpm=18.117826`, `snr=28.039232`, `offset_hz=-20_000.0`,
`noise_seed=2893936330082095`):

| duration | decoded | CER |
|---|---|---|
| 3 s | Err (no signal found, too short) | — |
| 5 s | `"ETT TT TTT TT"` | 1.00 |
| 8 s | `"ETT TT TTT TT TTT TT TTT TT"` | 1.375 |
| 12 s | similar repeating pattern | 1.60 |
| 20 s | same repeating `"TT"`/`"TTT"` pattern, longer | 1.80 |
| 40 s | pattern continues (denominator finally outgrows the numerator) | 0.93 |

The decoded stream is dominated by spurious `'T'` (single dah) classifications
in a repeating pattern, never re-syncing to the actual keyed `"AU"` pattern
after the first mark.

Found during Task 11 Step 0 (M2 sub-project 2,
`docs/superpowers/plans/2026-07-19-m2-detector-track-pool.md`), recorded
unfixed in `docs/DECISIONS/2026-07-19-m2-detector-track-pool-pins.md` item 10.

## Mechanism

`TrackManager::step_hop`'s `Promoted` arm (`crates/manta-engine/src/track.rs:519-534`)
constructs a fresh `TrackDecoder` **at the promotion hop** — an arbitrary
phase relative to the signal's element boundaries, since that hop is set by
the SPEC §2.4 lifecycle FSM firing on noisy per-hop `rise`/`drop` booleans
near the ~2.05 s warmup+confirm floor, i.e. it is a function of the AWGN
realization (`noise_seed`). No backfill of CANDIDATE-period samples exists.

`Demod::new` (`crates/manta-decode/src/envelope.rs`) buffers `INIT_HOPS = 375`
hops, computes the keying rails, and replays the whole window through
`step()` (pinned decision 4: elements inside the first second must still be
decoded). On the first replayed sample `self.open` is `None`, so a run opens
at whatever polarity the promotion hop happens to be — not an observed edge.
If the window opened mid-mark, that run's eventual measured duration is
"however much of the element was left when the rails initialized", a value
with no relationship to the signal's true element durations. It survives
debounce whenever it exceeds `debounce_hops = ms_to_hops(12.0) = 5` hops.

`TrackDecoder::on_run` feeds every mark run to `SpeedTracker::on_mark` while
`!tracker.ready()`, so that fabricated duration becomes sample #1 of
`ClusterPair::initialize`'s 5-sample bootstrap
(`crates/manta-decode/src/timing.rs`). That routine sorts the 5 values and
splits at the single largest ratio gap, with **no outlier rejection and no
minimum-cluster-size guard**, then immediately sets `ready = true` and
`confirmed = true`.

**Worked numeric example** (this repro's nominal, noise-free element
durations at `wpm = 18.117826`: dit ≈ 66.2 ms, dah ≈ 198.7 ms once
`Demod`'s hysteresis+debounce overshoot is included, per
`docs/DECISIONS/2026-07-18-char-gap-threshold-fix.md`). `"AU"` looped gives
the period-5 mark sequence `dit, dah, dit, dit, dah`. Let `F` be the measured
leading fragment; the bootstrap set is `[F, 84, 217, 84, 84]` (some
rotation).

- Sorted: `[F, 84, 84, 84, 217]`. The largest-ratio-gap split isolates `F` as
  the entire dit cluster whenever `84/F > 217/84 ≈ 2.58`, i.e. **`F < 32.5
  ms`**. `F` survives debounce from ~13 ms up — a real, non-empty failure
  window.
- Take `F = 30`: `μ_dit = 30`, `μ_dah = mean(84, 84, 84, 217) = 117.25`,
  ratio 3.9 — *inside* `[2.2, 4.5]`, so the ratio-constraint clamp doesn't
  even re-anchor it. `boundary_ms() = sqrt(30 · 117.25) ≈ 59.3 ms`.
- Every real mark (84 ms and 217 ms) now exceeds 59.3 ms → **every element
  classifies as a dah**.
- Every real inter-element gap (measured ≈ 48 ms) computes
  `u = 48/30 = 1.6` dits — right at `CHAR_GAP_DITS = 1.6` → **InterChar**, so
  `"A"`'s two elements become two separate characters `"TT"` and `"U"`'s
  three become `"TTT"`.
- Every real inter-character gap (measured ≈ 181 ms) computes `u = 6.0 >
  WORD_GAP_DITS = 5.0` → **InterWord** → a space between the `"TT"` and
  `"TTT"` groups.

Net output: `"TT TTT TT TTT …"` — the ticket's reported stream, including the
2-then-3 grouping that maps exactly onto `"A"`'s and `"U"`'s element counts.
The ticket's occasional leading `"E"` in `"ETT TT TTT TT"` is the fragment
itself, decoded as a lone dit (`30 ms < 59.3 ms` boundary) — direct
corroboration that a spurious short leading mark exists and is being emitted
as a character.

**Why it never recovers:** `SpeedTracker::check_drift`
(`crates/manta-decode/src/timing.rs`) — the only designed recovery path —
requires 12 consecutive same-cluster marks with **CV < 0.35** and a mean off
the pre-streak centroid by > 40 %. Under the bad lock all 12 marks land in
the `hi` cluster, but their durations are a mixture of 84 ms and 217 ms
values: mean 137.2, sd 65.2, **CV ≈ 0.475**. The CV gate blocks the only
recovery path. This is not "confused then converges"; it is a stable wrong
fixed point, because the wrong centroids and the wrong classifications they
themselves produce are mutually reinforcing — there is no statistical
"off-centroid" anomaly left for `check_drift` to see.

**Why prior hardening doesn't cover this:**

- Pinned decision 20 (`unimodal_ceiling`/`placeholder_is_lo`,
  `docs/DECISIONS/2026-07-11-m0-implementation-pins.md` item 20) fixes the
  *unimodal* 5-mark init case (an all-dah opener). MAN-6 takes the *bimodal*
  branch — the split fires — so that machinery is never consulted.
- `CHAR_GAP_DITS` `2.0 → 1.6` (`docs/DECISIONS/2026-07-18-char-gap-threshold-fix.md`)
  corrects a mild, continuous overshoot bias that runs `μ_dit` **high**.
  MAN-6 collapses `μ_dit` **low** — the opposite direction.
- The warmup/duration-floor + `loop_text` fix (Task 11 Step 0) is what makes
  this repro reach a promoted, decoding track at all, but
  `roundtrip_iq.rs`'s own doc comment already recorded that it is not
  sufficient and that #23 is a separate, real bug.
- No existing test fed `ClusterPair::initialize` anything but clean nominal
  durations.

## Decisions

### Decision 1 — `Demod` suppresses its leading partial run

`Demod` gains one boolean field, `leading_partial`, initialized `true`. The
first run it would otherwise close (via a genuinely observed polarity flip,
or via EOF) is discarded instead of being promoted into `held`/emitted —
framed as **SPEC §3.4 conformance**: a run is defined by its leading edge,
and the run open when the rails initialize never had one. See
`crates/manta-decode/src/envelope.rs` (`step()`'s polarity-flip arm and
`finish()`), and `docs/SPEC-decode-core.md` §3.4's amendment.

**Behavioral note:** when the *second* run (the first real one) is shorter
than `debounce_hops`, `held` is now `None` where it previously held the
fragment, so that short run takes the "short leading run absorbed into the
new run" path instead of "merge held + short + continuing". This only
occurs for runs under 12 ms, which SPEC §3.3 defines as noise, and the new
behavior is strictly preferable: the fabricated fragment duration no longer
leaks into the merged run.

**Known, accepted regression risk:** this fix cannot distinguish "the
window opened at an arbitrary mid-element hop" from "the window opened
exactly at a genuine mark/space transition that happens to coincide with
recording start" — both look identical to `Demod` (`self.open == None` on
the first sample either way). Any test harness that starts feeding samples
mid-mark with no leading silence loses its first element under this fix.
Every existing `manta-decode` test fixture that starts directly with a mark
(`rect_envelope`, several `envelope.rs` unit tests) was updated to prepend
one dit/hop of leading silence so the first mark gets a genuine observed
edge; this is a test-fixture concern only — every real signal chain
(detector → track promotion) already has this property trivially, since a
freshly-promoted track's `Demod` always starts on live channel noise/signal
mid-stream, never on a synthetic zero-origin.

### Decision 2 — `SpeedTracker` gains bimodal bad-lock recovery

`check_drift`'s existing rule occupies the `cv < 0.35` half of the
"12-consecutive-same-cluster" space. The complementary half — a same-cluster
streak whose durations are *wide* — is exactly the fingerprint of a boundary
sitting on the wrong side of a genuinely bimodal population. A new branch
claims it: if 12 consecutive marks assign to one cluster *and* `cv >= 0.35`
*and* those 12 raw durations themselves split (largest-ratio-gap, shared
helper `largest_ratio_gap` also used by `ClusterPair::initialize`) into two
clusters with **≥ 3 members per side** (`BADLOCK_MIN_CLUSTER = 3`) and a
centroid ratio inside `[2.2, 4.5]`, reinitialize `ClusterPair` from those 12
marks (not `recent`'s 5 — the full 12-mark ring has more evidence).

The `≥ 3`-per-side and `[2.2, 4.5]` guards are deliberately **stricter**
than `ClusterPair::initialize`'s own bootstrap gate (`>= 2.0` ratio, no
minimum size): a merely jittery-but-genuinely-homogeneous run of dits (high
CV from noise alone, not from a hidden second population) must never be
mistaken for a bad lock. Verified directly: a 12-value jittery-dit test case
with CV ≈ 0.34–0.4 produces a largest-ratio-gap split with a centroid ratio
of ≈ 1.94 — below the 2.2 floor — so the guard correctly declines to fire.

**Convergence bound:** worst case the tracker spends `DRIFT_LEN = 12` marks
in the bad state before the ring fills, then recovers. At 18 WPM that is
under 2 seconds. The ticket's criterion — error stabilizes rather than grows
— holds by construction, independent of the specific repro tuple.

See `docs/SPEC-decode-core.md` §4.1's amendment for the normative statement.

## Alternatives considered and rejected

1. **Lloyd's 2-means refinement** (seed at min/max, iterate) instead of the
   largest-ratio-gap split. Considered: seeding 2-means at min/max and
   iterating does rescue the worked example (`μ_dit = 54.7`, `μ_dah = 200`,
   boundary 104.6, which classifies correctly). **Rejected** for this
   ticket because it changes the bootstrap split for *every* decode,
   including all ten golden vectors, for a robustness gain Decisions 1 and
   2 already deliver at far smaller blast radius. Recorded here as a
   follow-up if the bad-lock recovery branch is ever observed to fire in
   production telemetry (it should be rare: Decision 1 already removes the
   one concrete trigger found).
2. **Raising the 5-mark bootstrap quorum to SPEC §9's nominal `min_count =
   8`.** Rejected: with 8 samples the corrupted seed
   `[F, 84, 217, 84, 84, 217, 84, 84]` still sorts to `[F, 84×5, 217×2]` and
   still splits at the same place whenever `84/F > 2.58`. More samples do
   not help when the split rule itself has no outlier rejection.
3. **Backfilling CANDIDATE-period samples into the newly-promoted
   `TrackDecoder`.** Rejected: it moves the demod's window start earlier,
   but the window still begins at an arbitrary hop relative to element
   boundaries, so the leading partial run — and therefore the bug — would
   survive unchanged. It would also change every golden decode for no
   benefit.
4. **Narrowing `roundtrip_iq.rs`'s proptest strategy to exclude MAN-6's
   region and un-ignoring it.** Rejected: unlike #12 (`offset_hz == 0`) and
   #22 (a narrow WPM band), MAN-6's mechanism is a *phase* condition on the
   track-promotion hop, not confined to an excludable band of the
   WPM/offset/SNR space — narrowing the strategy would misrepresent
   coverage. `roundtrip_iq.rs` stays `#[ignore]`d (because of #12/#22,
   unrelated to this fix); MAN-6's regression coverage instead lives in a
   new, deterministic, fixed-grid test,
   `crates/manta-engine/tests/regression_man6_persistent_garble.rs`.

## Implementation

**Incidental fix, unrelated to MAN-6, kept small and separate below:**
`clippy-driver` (run standalone against `manta-decode` — see Measurements)
flagged `decoder.rs`'s pre-existing `self.hop_count % META_INTERVAL_HOPS ==
0` (`manual_is_multiple_of`, a lint this workspace's `stable`-pinned CI
toolchain would also enforce) plus two lints in this change's own new test
code (`double_ended_iterator_last`, `manual_repeat_n`). All three are
one-line, behavior-preserving substitutions
(`.is_multiple_of()`/`.next_back()`/`iter::repeat_n()`); fixed inline rather
than left to fail this PR's CI for a reason a reviewer would have to
re-discover.

- `crates/manta-decode/src/envelope.rs`: `Demod::leading_partial` field;
  discard branch in `step()`'s polarity-flip arm; early-return in
  `finish()`. Tests: `leading_partial_run_is_suppressed` (new),
  `init_replay_recovers_first_second` (rewritten to start from silence).
- `crates/manta-decode/src/decoder.rs`: `rect_envelope` prepends one dit of
  leading silence; `char_timestamp_is_end_of_last_mark`'s expected
  timestamp shifted by that one dit (`198*256 → 216*256`). New hermetic
  tests: `mid_element_start_does_not_lock_bad_timing`,
  `mid_element_start_error_does_not_grow_with_duration`, reproducing the
  ticket's exact `"TT"/"TTT"` symptom shape from a zero-noise rectangular
  envelope — the strongest available evidence that this is a deterministic
  timing-bootstrap defect, not a noise-robustness limit.
- `crates/manta-decode/src/timing.rs`: `largest_ratio_gap` extracted as a
  shared helper (used by both `ClusterPair::initialize` and the new guard);
  `BADLOCK_MIN_CLUSTER` constant; `is_credible_bimodal` guard; new branch in
  `check_drift`. Tests: `bimodal_badlock_recovers`,
  `healthy_stream_never_triggers_badlock_recovery`,
  `jittery_single_cluster_does_not_trigger_badlock_recovery`.
- `crates/manta-engine/tests/regression_man6_persistent_garble.rs` (new):
  the ticket's real tuple through the full channelizer + detector + track
  pool + decoder, plus a 6-case deterministic sweep across MAN-6's
  parameter region (excluding `offset_hz == 0` and the #22 WPM band by
  construction).
- `crates/manta-engine/tests/roundtrip_iq.rs`: doc comment updated to record
  #23/MAN-6 as fixed, distinct from the still-open #12/#22.
- `docs/SPEC-decode-core.md` §3.4 and §4.1: normative amendments (see
  Decisions 1 and 2 above).

## Measurements

**`cargo` itself is unusable in the implementation environment**: `manta-dsp`
pins `coppa-dsp` at a git revision
(`f8a4d16df7e5776a0756943c05712038774e6c70`) that this container has no
network egress to fetch, and no cached checkout of it exists on disk here —
confirmed directly (`cargo test -p manta-decode --no-run` fails with `could
not read refs from remote repository` even when scoped to the one crate in
this workspace with no `coppa` dependency in its own graph, because
workspace-wide dependency resolution still touches every member, including
`manta-dsp`). This blocks **every** `cargo` invocation in this workspace,
not just the engine-level tests the original plan anticipated would need
network.

**Worked around for the `manta-decode` crate** (no `coppa` dependency at
all, source or transitive) by driving `rustc` directly, bypassing Cargo's
workspace-wide resolution entirely: `rustc --edition 2021 --crate-name
manta_decode --test crates/manta-decode/src/lib.rs -o <bin>`, then running
the resulting binary as a normal libtest harness. This is not a
network-independence claim about the real build (`cargo test -p
manta-decode` still hits the same workspace-resolution failure as everything
else, and should be re-run once network access exists, to confirm parity)
— it is this environment's only available substitute, and it exercises the
*exact* same source files, full borrow/type checking, and the real `#[test]`
functions, with no mocking. Result: **all 48 tests pass**, including every
new/modified one (`leading_partial_run_is_suppressed`,
`init_replay_recovers_first_second`, `mid_element_start_does_not_lock_bad_timing`,
`mid_element_start_error_does_not_grow_with_duration`,
`char_timestamp_is_end_of_last_mark`, `bimodal_badlock_recovers`,
`healthy_stream_never_triggers_badlock_recovery`,
`jittery_single_cluster_does_not_trigger_badlock_recovery`) and every
pre-existing one, unmodified assertions included (`step_speed_change_reinitializes`,
`clean_keying_yields_alternating_runs`, `all_dah_opener_decodes_correctly`,
`ratio_constraint_reanchors_dah`, etc.) — zero regressions.

**Phase-isolation experiment** (the plan's own suggested manual-verification
step, actually run rather than assumed): three additional builds of this
same crate, each with one Decision's code reverted, run against a small
probe that sweeps the ticket's `"AU"` repro at `reps ∈ {4, 10, 24, 60}` and
reports the decoded text's `'T'` count (`t`) against its total non-space
character count (`total`):

| build | reps=4 (t/total) | reps=10 | reps=24 | reps=60 | shape |
|---|---|---|---|---|---|
| neither decision (pre-fix baseline) | 18/19 | 48/49 | 118/119 | 298/299 | `"E TTT TT TTT TT TTT …"` repeating forever, **t/total → ~1.0 and never recovers** — this is the ticket's exact reported failure, reproduced hermetically |
| Decision 2 alone (bad-lock recovery, no leading-run suppression) | 15/17 | 15/29 | 15/57 | 15/129 | `"E TTT TT TTT TT TTT TT UAUAUA…"` — a **fixed-size** garbled prefix (`t` pinned at 15 regardless of `reps`), then clean recovery; CER shrinks monotonically as `total` grows around the fixed numerator |
| Decision 1 alone (leading-run suppression, no bad-lock recovery) | 0/7 | 0/19 | 0/47 | 0/119 | clean from the very first character at every length |
| both (shipped) | 0/7 | 0/19 | 0/47 | 0/119 | byte-identical to Decision 1 alone — Decision 2's branch never fires for this repro, exactly as expected once Decision 1 removes its one known trigger |

This is a stronger, more precise result than the plan anticipated (it
expected Decision 2 alone to make the hermetic test *pass*): Decision 2
alone does **not** clear this repo's strict "zero `'T'`s" hermetic
assertion — it leaves a bounded, one-time garbled prefix before recovering
— but it unambiguously satisfies the ticket's actual Gherkin criterion
("error rate stabilizes or shrinks... does not enter a *persistent*,
non-converging garbled state") on its own, independent of Decision 1: `t`
stops growing entirely once locked correctly, versus the pre-fix baseline's
unbounded growth in lockstep with `total`. This is exactly the
independent-safety-net role Decision 2 is designed for, now demonstrated
rather than assumed.

Outstanding, for whoever next has network access to build this workspace:

1. Run `cargo test -p manta-decode` for real (should match the `rustc
   --test` result above: all tests green) and the
   `cargo test -p manta-engine --test regression_man6_persistent_garble`
   invocation Phase 1 of the implementation plan specifies as this
   change's Red step, to confirm they fail on the pre-fix commit
   (`5b9e747`) and pass after it.
2. Re-measure every golden vector's CER floor comment
   (`golden_v1.rs`, `golden_v2_v3.rs`, `golden_v7_v9_v10.rs`,
   `golden_v8_v8w.rs`) three times each for determinism, since Decisions 1
   and 2 change the run stream and timing bootstrap for every decode, not
   just MAN-6's region. Update the comments' measured values; do not widen
   any threshold — if a vector regresses, that is a blocker requiring
   further investigation, not a tolerance question (`CLAUDE.md`'s "don't
   force a real bug to pass").
3. Record the pre-fix CER for `regression_man6_persistent_garble.rs`'s
   6-case sweep (`man6_region_sweep_decodes_cleanly`) by running it against
   `5b9e747`, and replace any case that fails there for a reason
   attributable to #12/#22 with a different seed, per the implementation
   plan's Phase 1 §5 pre-decision.
4. Run `crates/manta-engine/benches/cpu_budget.rs` once for confirmation of
   the "negligible" performance-impact claim (Decision 1 adds one boolean
   check per track; Decision 2 adds a 12-element sort only when a 12-mark
   single-cluster streak has already formed).
