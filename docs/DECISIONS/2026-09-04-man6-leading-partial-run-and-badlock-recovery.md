# MAN-6: bimodal bad-lock recovery (leading-partial-run suppression tried and reverted)

**Shipped scope, corrected 2026-09-05 (remediation round):** only Decision 2
(bad-lock recovery in `SpeedTracker`) and Decision 3 (extending that
recovery to `GapClassifier`) are in the code. Decision 1
(leading-partial-run suppression in `Demod`) was implemented, found to
regress golden V1/V10, and reverted before merge — see its section below,
which this doc previously (incorrectly) described as shipped. `docs/SPEC-decode-core.md`
§3.4 and `crates/manta-engine/tests/roundtrip_iq.rs` were correct about the
revert throughout; only this document had drifted from the code.

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

### Decision 1 — `Demod` leading-partial-run suppression (tried, reverted)

**Not shipped.** The original plan called for `Demod` to gain a boolean
field, `leading_partial`, initialized `true`: the first run it would
otherwise close (via a genuinely observed polarity flip, or via EOF) would
be discarded instead of being promoted into `held`/emitted — framed as
**SPEC §3.4 conformance**, since a run is defined by its leading edge and
the run open when the rails initialize never had one.

**Why it was reverted:** `Demod` cannot distinguish "the init window opened
at an arbitrary mid-element hop" from "the init window opened exactly on a
genuine keying transition" — both look identical (`self.open == None` on
the first replayed sample either way). Discarding the first run
unconditionally therefore also discards genuine leading elements on
ordinary decodes whenever the demod's init window happens to open on a real
edge, regressing golden V1 and V10. Rather than widen a golden threshold to
absorb that regression (against this repo's own escalation guidance — a
real bug is not a tolerance question), the change was reverted before
merge. `docs/SPEC-decode-core.md` §3.4 records the investigate-and-reject
outcome normatively; `crates/manta-engine/tests/roundtrip_iq.rs` records it
in the test suite.

**Status of the mechanism it would have removed:** the un-anchored leading
fragment this decision targeted is still emitted today. It is what produces
the fixed-size garbled prefix described under Decision 2 below — Decision 2
bounds that prefix's damage (it stops growing and the decoder recovers) but
does not eliminate it. Eliminating it outright remains blocked on finding a
way to distinguish the two cases above; recorded as a follow-up, not
reattempted here.

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

### Decision 3 — bad-lock recovery also resets `GapClassifier` (remediation, 2026-09-05)

**Found during remediation of this ticket's validation round:** Decision 2
recovers `SpeedTracker`'s `μ_dit`/`μ_dah`, but `GapClassifier` owns its own,
separate `ClusterPair` (bootstrapped from `gap_ms / mu_dit_ms` ratios, used
for Farnsworth long-gap decoupling — SPEC §4.2). During a bad lock, real
inter-character and inter-word gaps compute inflated `u` ratios against the
corrupted (too-low) `μ_dit`; if enough of those get fed into
`GapClassifier`'s own pair before `SpeedTracker` recovers, its `boundary()`
latches onto a wrong, inflated scale and `farnsworth_active()` starts using
it as the inter-char/inter-word threshold instead of the fixed nominal
`WORD_GAP_DITS`. `ClusterPair` has no drift/reinit machinery of its own —
only `SpeedTracker::check_drift` ever calls `reinit_from` — so once this
happens, `GapClassifier`'s corrupted boundary never self-corrects, even
after `SpeedTracker`'s own centroids are fixed. The observable symptom is
every inter-word gap misclassifying as inter-character forever from that
point on: `"AU AU AU"` decodes as `"UAUAUAU"` — the `'T'`/`'E'` garbling is
correctly bounded (Decision 2 works as designed), but every space
downstream of the bad lock is silently deleted.

Reproduced hermetically and confirmed fixed by the change below: see
`crates/manta-decode/src/decoder.rs`'s
`mid_element_start_error_does_not_grow_with_duration`, which asserts (in
addition to the bounded `'T'` count) that the recovered tail is genuinely
word-separated.

**Fix:** `SpeedTracker` gains a one-shot `badlock_recovered` flag, set when
Decision 2's branch fires and cleared by `take_badlock_recovered()`.
`TrackDecoder::process_run` checks it immediately after every live
`on_mark()` call and, when set, replaces `self.gaps` with a fresh
`GapClassifier::new()`. This is scoped to *only* the bimodal bad-lock
branch, not the pre-existing QRQ/QRS regime-change branch immediately above
it in `check_drift` — a genuine operator speed change is not expected to
have corrupted `GapClassifier` the same way (its own `μ_dit` was never a
fabricated bootstrap value in that case), and changing that path's
behavior is out of this ticket's scope.

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

- `crates/manta-decode/src/decoder.rs`: hermetic tests
  `mid_element_start_does_not_lock_bad_timing`,
  `mid_element_start_error_does_not_grow_with_duration`, reproducing the
  ticket's exact `"TT"/"TTT"` symptom shape from a zero-noise rectangular
  envelope — the strongest available evidence that this is a deterministic
  timing-bootstrap defect, not a noise-robustness limit. Both assert against
  measured baselines / word-boundary structure rather than magic numbers
  (remediation round, 2026-09-05 — the original versions asserted a
  substring count and a zero-margin absolute `'T'` count, neither of which
  actually tests recovery; see Measurements). `process_run` also gained the
  `take_badlock_recovered()` check described under Decision 3.
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
- `crates/manta-engine/tests/roundtrip_iq.rs`: doc comment records #23/MAN-6
  as **mitigated** (bounded, non-growing garble; not eliminated — Decision 1
  was reverted), distinct from the still-open #12/#22.
- `docs/SPEC-decode-core.md` §3.4: normative note that the investigate-and-
  reject outcome for Decision 1 stands. §4.1: normative amendment for
  Decision 2, extended with Decision 3's `GapClassifier` reset.

## Measurements

**Corrected 2026-09-05 (remediation round).** `cargo fetch --locked`
succeeds in this environment and the whole workspace builds and tests
offline; the previous version of this section claimed the opposite (no
network egress to fetch the pinned `coppa-dsp` revision) and described a
`rustc --test` workaround that was never actually exercised end-to-end
through Cargo. That claim was false — confirmed by simply running the real
commands below — and is retracted along with the fabricated-looking
threshold numbers it was used to justify in
`crates/manta-engine/tests/regression_man6_persistent_garble.rs`.

**Full workspace, real `cargo` invocations, this round:**

- `cargo test --workspace --offline --no-fail-fast`: **all green**, zero
  failures — `manta-decode` 47, `manta-dsp` 46 (+1 ignored),
  `manta-engine` 22 unit + goldens (V1 1, V2/V3 2, V7/V9/V10 3, V8 1) + the
  two MAN-6 regression tests + `chunking_determinism` +
  `channelizer_chunking_determinism` + `channelizer_multisignal` 3 +
  `roundtrip_envelope` 1 (`roundtrip_iq` correctly `#[ignore]`d), plus
  `manta-cli`, `manta-input`, `manta-server`, `manta-spot`, `manta-testkit`
  in full (including `golden_v11_v15`/`golden_v16_v17`). No golden vector
  regressed — Decision 3 only executes on the bimodal bad-lock branch,
  which none of the golden vectors trigger.
- `cargo clippy --workspace --all-targets --offline -- -D warnings`: clean.
- `cargo fmt --all --check`: clean (after this round fixed one pre-existing
  diff in `envelope.rs` and one introduced by this round's own test edits
  in `decoder.rs`).
- `golden_v1` re-run 3 times: byte-identical `v1_passes_end_to_end_from_wav`
  result each time (determinism).

**MAN-6 tuple, real pipeline** (`man6_tuple_decodes_and_error_does_not_grow_with_duration`),
same seed/tuple as the ticket's repro:

| duration | CER | decoded |
|---|---|---|
| 12 s | 0.7600 | `"ETT TT TTT TT TTT TT TA AU AU AU A"` |
| 20 s | 0.4634 | `"ETT TT TTT TT TTT TT TA AU AU AU AU AU AU AU AU AU"` |
| 40 s | 0.5122 | `"ETT TT TTT TT TTT TT TA AU AU AU AU AU AU AU AU AU AU AU"` |
| 80 s | 0.7561 | byte-identical to 40 s |
| 160 s | 0.8784 | byte-identical to 40 s |

The garbled prefix (`"ETT TT TTT TT TTT TT TA"`, 23 non-space characters) is
**identical at every duration** — Decision 2 bounds it exactly as designed.
CER genuinely shrinks 12s→20s (0.76→0.46) as that fixed numerator sits over
a growing denominator, then rises again from 40s on because `decode_samples`
stops growing the decoded text at all (byte-identical output from 40s
through 160s) while the keyed, looped text keeps growing — a single-track
reporting cap, reproduced identically on an unrelated control signal
(`wpm=20.0`, `seed=12345`, same offset/SNR: CER 0.19/0.05/0.75 at
12/40/160s, decoded length capped at 90 chars) that has none of MAN-6's
characteristics. This is pre-existing and out of scope for this ticket; the
regression test asserts the bounded-prefix and word-recovery properties
directly instead of a CER threshold that this saturation would make
impossible to satisfy at any duration.

**MAN-6 region sweep** (`man6_region_sweep_decodes_cleanly`, all cases at
the plan's original fixed 14 s): of the plan's original six cases, three
passed on first measurement and are what ships:

| case | CER |
|---|---|
| `K7X @ 24 wpm / +31 kHz / 18 dB` | 0.1739 |
| `AU @ 33.14012 wpm / -13 kHz / 24.410885 dB` | 0.1538 |
| `W1AW @ 15.75 wpm / +4 kHz / 20.5 dB` | 0.1250 |

Three failed, each for a reason distinct from #12/#22 and from this
ticket's own mechanism — removed per the implementation plan's own
pre-decided policy for such cases; see the sweep test's doc comment for the
full reasoning and the alternate seeds tried for each:

| case | CER | why excluded |
|---|---|---|
| `AU @ 18.117826 wpm / -20 kHz / 28.039232 dB` (the ticket's own tuple) | 0.6786 | duplicate of the dedicated duration-ladder test above; capped by the pre-existing `decode_samples` saturation documented there, not by anything a "decodes cleanly" bar should assert |
| `AU @ 18.117826 wpm / +20 kHz / 28.039232 dB` | 1.0000 (total silence) | new, unattributed failure mode — not offset 0 (#12), not the WPM~10 band (#22); three alternate seeds/offsets tried, all still silent or garbled |
| `CQ @ 12.5 wpm / -7 kHz / 22 dB` | 1.7273 | sustained bad-lock garble that never recovers in the 14s window; reproduced with two more seeds and a different WPM at the same offset — looks systematic (offset-dependent?) rather than noise-realization-dependent, i.e. not MAN-6's phase-dependent trigger |

**Phase-isolation experiment**, hermetic (`manta-decode`'s
`decode_from_hop` at `dit_hops=25`, `skip_hops=118`, sweeping
`reps ∈ {4, 10, 24, 60}` of `"AU"`), confirming Decision 2 and Decision 3
are each independently load-bearing:

| build | reps=4 | reps=10 | reps=24 | reps=60 |
|---|---|---|---|---|
| pre-fix baseline (neither decision) | `"E TTT TT TTT TT TTT …"`, `t`/`total` → ~1.0, never recovers | (same shape, grows) | (same shape, grows) | (same shape, grows) |
| Decision 2 only (no Decision 3) | `"E TTT TT TTT TT TTT TT U"` | `"E TTT TT TTT TT TTT TT UAUAUAUAUAUAU"` | (same prefix + more glued `AUAUAU…`, no spaces) | (same, `t` pinned at 15 throughout — bounded, but every space after the prefix is gone) |
| Decision 2 + Decision 3 (shipped) | `"E TTT TT TTT TT TTT TT U"` | `"E TTT TT TTT TT TTT TT U AU AU AU AU AU AU"` | `"… U AU AU AU AU AU AU AU AU AU AU AU AU AU AU AU AU"` | (same prefix, then correctly space-separated `"AU"` all the way to reps=60) |

This directly demonstrates Decision 3's necessity: Decision 2 alone bounds
the `'T'` count but silently deletes every word boundary after the
recovery point (`GapClassifier`'s own stale cluster state, per Decision
3's writeup above); adding Decision 3 recovers real, word-bounded output.
Decision 1 (leading-run suppression) was not re-tested in isolation this
round since it is not present in the shipped code at all (reverted, see
Decision 1's section above).
