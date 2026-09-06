# MAN-7: boundary-bias-cancelling WPM estimation (element-gap symmetric dit)

## Background

`report["wpm"]` under-reported speed for signals sitting near a channelizer
channel edge. SPEC §7 V2 (offset -8200 Hz = -0.4667 channels from its nearest
channel center, only 3.125 Hz short of the exact -0.5-channel worst case) is
keyed at 35 WPM but read ~29.1 WPM, flat across scene duration (90 s: 29.12,
200 s: 29.07, 400 s: 29.05). The same scene keyed on an exact channel center
(+6000 Hz = channel k=64's center) read 33.94 WPM, close to truth. Filed as
GitHub issue #24 during M2 sub-project 2 Task 11 (see
`docs/DECISIONS/2026-07-19-m2-detector-track-pool-pins.md` item 10); this doc
records the root-cause investigation and fix.

By contrast, V2's *character-accuracy* gate is fine on the same tree: CER
0.0325 at 90 s, shrinking with duration (200 s: 0.0128, 400 s: 0.0064) —
ordinary SPEC §2.1 warmup-floor dilution, not a decode defect, and left
`#[ignore]`d as its own, separate, still-open issue (see "Follow-up" below).
The originally-diagnosed pin 7/8 CER bug
(`docs/DECISIONS/2026-07-18-m2-pfb-channelizer-pins.md`, "keying jitter
interacts badly with the WOLA channelizer's transient response at
near-channel-edge residual frequencies", 8.94-23% CER under the old
placeholder detector) is resolved under the current detector/track manager —
that document's `combined_magnitude` fix attempt is unrelated to the fix
below and remains fully reverted.

## Root cause

`SpeedTracker::on_mark` (`crates/manta-decode/src/timing.rs`) computed PARIS
WPM as `1200 / μ_dit_ms`, where `μ_dit_ms` is a 2-means EMA centroid fed one
measured **mark duration** at a time
(`dur_ms = run.hops * HOP_MS`, `crates/manta-decode/src/decoder.rs`). Mark
durations come from `Demod`'s hysteresis+debounce keying state machine
(`crates/manta-decode/src/envelope.rs`, SPEC §3.2-§3.4), which is already
documented (`timing.rs`'s `CHAR_GAP_DITS` comment,
`docs/DECISIONS/2026-07-18-char-gap-threshold-fix.md`) to add "a roughly
constant ~15-20ms overshoot to every measured mark" — a consequence of the
asymmetric hysteresis (key-down at `1.25*T`, key-up at `0.80*T`): a rising
edge crosses its threshold sooner after the true keying edge than a falling
edge does, so every measured mark is stretched by
`delta = (fall-crossing delay) - (rise-crossing delay) >= 0`.

**What's new here**: `delta` is not fixed — it scales sharply with how slowly
the recovered envelope transitions through the hysteresis band, and channel-
edge proximity is one thing that slows it a lot. Two mechanisms were
investigated (Phase 1 below determines which dominates):

- **(a) Per-hop channel-selection "flicker"**: `Track::select_channel`
  (`crates/manta-engine/src/track.rs`) reselects the max-power channel among a
  track's 3 owned channels every hop. Near the exact edge, two channels sit
  near-tied at the SPEC §1.2 -6 dB crossover
  (`channel_edge_is_minus_6_db_in_both_neighbors`,
  `crates/manta-dsp/src/channelizer.rs`), so the argmax is free to alternate
  under ordinary AWGN, feeding the demod an order-statistic (max of two
  near-iid samples) with extra variance right at the threshold-crossing
  region.
- **(b) Narrowed effective keying-envelope bandwidth**: a carrier sitting 93 %
  of the way into the prototype filter's ~60 Hz transition band
  (`crates/manta-dsp/src/proto.rs`) has its keying sidebands asymmetrically
  shaped by a filter response that is steep on the outward side and flat on
  the inward side, slowing the recovered envelope's rise/fall even though the
  filter is provably linear-phase (no differential group delay).

**Key insight (the fix's premise)**: threshold crossings only *move the
boundary* between a mark and the space that follows it — they neither create
nor destroy time. So for the i-th dit and the inter-element gap immediately
after it:

```
measured mark_i  = dit + delta_i
measured egap_i  = dit - delta_i + (next mark's rise-delay - this mark's rise-delay)
mark_i + egap_i  = 2*dit + O(rise-delay jitter)  ->  2*dit in the mean
```

So `(mu_dit + mu_egap)/2` is an unbiased estimator of the true dit period
*regardless of how large delta is or what causes it* — channel-edge
proximity, QSB troughs, weak SNR.

## Phase 1: measured evidence

`crates/manta-engine/tests/edge_offset_timing_bias.rs` renders a dit-only
"H5" payload (every mark a dit, every intra-character gap one dit — no
dit/dah separation needed) through the real `Channelizer` -> per-hop
channel-selection -> real `Demod` path at 35 WPM / 20 dB SNR / 30 s, swept
over fractional channel offset, with two channel-selection arms: `fixed`
(the scene's single highest-mean-power owned channel, isolating mechanism
(b)) and `argmax` (SPEC §2.5's real per-hop reselection, exercising (a) too).
True dit period at 35 WPM: 34.286 ms.

| frac | argmax | mean mark (ms) | mean element-gap (ms) | sum (ms) | overshoot (ms) |
|---|---|---|---|---|---|
| 0.0 | fixed | 37.538 | 31.033 | 68.570 | 3.252 |
| 0.0 | argmax | 36.699 | 31.889 | 68.588 | 2.413 |
| 0.25 | fixed | 38.452 | 30.149 | 68.600 | 4.166 |
| 0.25 | argmax | 38.290 | 30.314 | 68.605 | 4.005 |
| 0.4667 | fixed | 42.688 | 25.865 | 68.553 | 8.402 |
| 0.4667 | argmax | 42.366 | 26.238 | 68.604 | 8.080 |
| 0.5 | fixed | 43.602 | 24.967 | 68.569 | 9.316 |
| 0.5 | argmax | 42.989 | 25.575 | 68.564 | 8.704 |

(`want = 2 * true_dit_ms = 68.571`.)

Two findings from this table, both checked into the test as permanent
assertions:

1. **`mark + element-gap` sums to `2*true_dit` within ~0.1 %** at every
   fractional offset and in both selection arms — the complementarity
   invariant the fix rests on holds exactly, not just approximately.
   (`mark_and_element_gap_stay_complementary_across_channel_offsets`)
2. **Mark overshoot grows ~2.9x from center (0.0) to near-edge (0.4667)** and
   ~3.4x at the exact edge (0.5) — reproducing the ticket's finding as a
   controlled, swept measurement.
   (`mark_overshoot_grows_toward_the_channel_edge`)

**Verdict on mechanism (a) vs (b)**: at every fractional offset, `argmax`
overshoot is *slightly smaller* than `fixed` overshoot (e.g. 8.080 ms vs
8.402 ms at 0.4667), not larger. Per-hop channel-selection flicker (a) is not
the dominant driver in this controlled sweep; the filter-shape/narrowed-
bandwidth mechanism (b) accounts for essentially all of the edge-proximity
scaling. (V2's real bias is somewhat larger than this table's 0.4667 row —
6.9-7.7 ms measured on V2 itself vs. ~8.1 ms here at 20 dB/no-jitter on a
different payload/SNR — consistent with the same mechanism under different
signal conditions, not a different mechanism.)

Per the plan's decision rule: the invariant holds at every offset (not just
on-center), so the fix proceeds as designed.

## Fix

**[DEVIATION]** `SpeedTracker` (`crates/manta-decode/src/timing.rs`) gains an
EMA of inter-element gap durations (`mu_egap_ms`, same `CLUSTER_ALPHA = 0.15`
as the mark centroids) and a `dit_estimate_ms()` method:

```
delta = clamp((mu_dit - mu_egap) / 2, 0, DIT_BIAS_CAP_FRAC * mu_dit)
dit_estimate_ms = clamp(mu_dit - delta, DIT_CLAMP_MS)
```

`on_mark` now computes `raw = 1200.0 / dit_estimate_ms()` instead of
`1200.0 / mu_dit_ms()`. `mu_dit_ms()` itself, and everything computed from
it other than the WPM report, is byte-for-byte unchanged:

- `GapClassifier::classify`'s `u = gap_ms / mu_dit_ms` (SPEC §4.2)
- the beam decoder's log-normal likelihood (SPEC §4.3)
- `Demod::set_dit_ms`'s `tau_hi = clamp(5*dit_ms, 100, 400)` (SPEC §3.2)
- `check_flush`'s `7*mu_dit` safety net (SPEC §4.2)

These four consumers were all empirically tuned against the *biased* `mu_dit`
— most explicitly `CHAR_GAP_DITS = 1.6` (vs. SPEC's nominal `2.0`), whose
500-case sweep exists precisely because `mu_dit` runs high. Correcting
`mu_dit` itself would reopen all four calibrations at once (see "Deferred:
the larger fix" below); this fix instead treats `mu_dit` as a classification
centroid (consistent with the marks being classified) and only the *report*
as wanting an absolute physical estimate, splitting the two cleanly.

`GapClassifier`-classified inter-element gaps are wired to
`SpeedTracker::on_element_gap` in `TrackDecoder::process_run`
(`crates/manta-decode/src/decoder.rs`), on both the live and the
retroactive pending-buffer-drain path — gaps are never observed before this
point on either path, so there is no double-counting. `check_drift`'s
regime-change reinit (`crates/manta-decode/src/timing.rs`) also drops
`mu_egap_ms` back to `None`, so a stale gap centroid from the old speed never
applies a wrong-scale delta during a QRQ/QRS transition.

### `DIT_BIAS_CAP_FRAC = 0.35`

The asymmetric hysteresis (up `1.25*T` > down `0.80*T`) makes a *negative*
measured bias physically impossible for a clean mark/gap pairing — if
`mu_egap > mu_dit`, the pairing itself has broken (merged or dropped runs,
e.g. from debounce eating short element gaps at extreme WPM), and the
correction falls back toward the uncorrected estimate rather than inflating
the report further. The cap is symmetric protection for the positive side:
V2's worst measured `delta/mu_dit` is 0.168 (6.92 ms / 41.21 ms), so `0.35`
leaves ~2x headroom for legitimately larger biases while bounding the
reportable correction at `1/(1-0.35) = 1.54x` if the pairing degrades.

## Verification

### Unit (Phase 2, `crates/manta-decode/src/timing.rs`)

Seven tests feeding `SpeedTracker` synthetic biased mark/gap streams in
milliseconds (no DSP): the no-op case (no gaps observed yet, bit-identical to
pre-fix), V2's exact measured bias (6.92 ms, corrects 29.1 -> 35.0 WPM), the
on-center control's small bias (1.07 ms), a zero-bias no-op check, the
negative-bias fallback, the cap, and gap-centroid invalidation on a
regime-change reinit. All pass; every pre-existing `timing.rs` test is
unchanged and still passes (51/51 `manta-decode` tests green).

### Component (Phase 3, `crates/manta-decode/src/decoder.rs`)

`rect_envelope_skewed` extends the existing rectangular-envelope test helper
with a `+skew`/`-skew` split between marks and gaps (same shape as the real
bias, no DSP). At 18 hops/dit (25 WPM) with an 3-hop (8 ms) skew: pre-fix
estimator would report `1200/56 = 21.4` WPM; the corrected one reports ~25,
and the decoded text is byte-identical (`mu_dit` is deliberately left
biased, so beam-decode gap ratios are unaffected). A companion zero-skew test
guards the no-op case.

### Integration (Phase 3-4, golden vectors)

`crates/manta-cli/tests/golden_v2_v3.rs`'s single `#[ignore]`d
`v2_passes_end_to_end_from_wav` (which asserted both CER and WPM, and could
never pass because of the still-open CER-warmup-floor issue) is split into:

- `v2_wpm_is_within_spec_tolerance` — **enabled**, MAN-7's actual gate.
- `v2_char_accuracy_meets_spec` — stays `#[ignore]`d; unrelated, still-open
  warmup-floor issue (see "Follow-up" below).
- `v2_wpm_is_duration_stable` — `#[ignore]`d (690 s of scene generation is too
  slow for the default suite), MAN-7's second acceptance clause. Measured:

  | duration | WPM |
  |---|---|
  | 90 s | 35.25 |
  | 200 s | 34.32 |
  | 400 s | 35.29 |

  Spread 0.97 WPM (< the test's 1.0 WPM gate), vs. pre-fix's flat-and-wrong
  29.12 / 29.07 / 29.05.

Post-fix full-suite results (all green, run against coppa pin `f8a4d16d`):

- V1 (`golden_v1.rs`), V2's new WPM gate, V3, V4, V7, V9, V10, V8 — pass.
- V5, V6, V8w — remain `#[ignore]`d for their own documented, unrelated
  classical-decoder fading-robustness limitations (issues #25/#28), untouched
  by this change.
- `manta-decode` (51 tests), `manta-engine` (chunking determinism,
  channelizer chunking determinism, `pipeline.rs`, `spots.rs`,
  `regression_char_gap_high_wpm.rs`, the new `edge_offset_timing_bias.rs`) —
  all pass.
- `roundtrip_iq.rs`'s `iq_roundtrip_with_noise` stays `#[ignore]`d for its
  own pre-existing, unrelated reasons (documented issues #12/#22/#23 in that
  file) — not run as part of this change; nothing about MAN-7's fix touches
  the code paths those issues are filed against.
- `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` clean.

Determinism: the new code is a single `f32` EMA update in fixed call order
(one `on_element_gap` per classified inter-element gap, in stream order), no
allocation, no threading — the file-input byte-identical-spot-log contract is
structurally unaffected. `chunking_determinism.rs` and
`channelizer_chunking_determinism.rs` both pass.

## Deferred: the larger fix (not done here)

De-biasing `Run` durations at their source in `Demod` (subtracting `delta`
from marks and adding it to gaps before *either* estimator sees them) would
make `mu_dit` itself physically true, and would let `CHAR_GAP_DITS` return to
SPEC's nominal `2.0`, removing that pre-existing documented deviation too.
Rejected for MAN-7's scope: it requires knowing `delta` *before* computing
`mu_dit`/`mu_egap` (a chicken-and-egg problem `Demod` would need new state to
resolve), and it would reopen `CHAR_GAP_DITS`'s 500-case empirical sweep, the
beam decoder's `sigma = 0.25` log-normal calibration, `tau_hi`, and the 7-dit
flush threshold all at once — changing every golden vector's character
stream in a single change. Recorded here as a candidate for the M4 timing
rework, where a broader recalibration is already in scope.

## Follow-up (not filed as a separate ticket in this environment)

This container has no Linear or GitHub write credential, so the CER-specific
follow-up below is recorded here, ready to file, rather than filed directly:

> **Title**: V2's char-accuracy gate should be measurable at the vector's own
> 90 s duration instead of being warmup-floor-limited
>
> ```gherkin
> Scenario: V2's SPEC §7 char-accuracy gate is enforceable
>   Given SPEC §7 V2 is a 90 s scene and the detector has a fixed ~2.05 s
>     warmup(750 hops) + confirm(19 hops) floor before any track can promote
>   When char accuracy is measured over the whole scene
>   Then the measurement is not dominated by the leading text lost to that floor
>   # CURRENTLY: V2 measures CER 0.0325 against a <= 0.01 gate at 90 s, shrinking
>   # as 1/duration (200 s: 0.0128, 400 s: 0.0064) with a clean decode through the
>   # middle of the scene -- the gate is unreachable at 90 s for any
>   # warmup-limited decoder, so `v2_char_accuracy_meets_spec` is #[ignore]d.
>   # Decide between measuring CER post-warmup, lengthening the vector, or
>   # recording a measured floor -- do not silently widen the tolerance.
> ```
>
> Split out of MAN-7, which fixed V2's *WPM* gate only. Context in
> `crates/manta-cli/tests/golden_v2_v3.rs`'s `v2_char_accuracy_meets_spec` doc
> comment and `docs/DECISIONS/2026-07-19-m2-detector-track-pool-pins.md` item
> 10.

## Related documents

- `docs/DECISIONS/2026-07-18-m2-pfb-channelizer-pins.md` items 7-8 — the
  earlier, CER-focused near-edge investigation under the old placeholder
  detector (resolved differently, unrelated to this fix).
- `docs/DECISIONS/2026-07-19-m2-detector-track-pool-pins.md` item 10 — where
  this WPM finding was first recorded (issue #24).
- `docs/DECISIONS/2026-07-18-char-gap-threshold-fix.md` — the general
  (non-edge-specific) mark-duration-overshoot mechanism this fix builds on.
- `docs/SPEC-decode-core.md` §4.1/§4.2/§9 — updated **[DEVIATION]** blocks and
  config-key table entry (`dit_bias_cap_frac = 0.35`).
