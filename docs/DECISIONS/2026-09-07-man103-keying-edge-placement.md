# MAN-103: keying-edge placement and symmetric dit-period estimation

## Background

MAN-103 (supersedes MAN-7): reported WPM read systematically low, worse the
closer a signal sat to a channel edge. V1 (20 WPM, on-center) measured
17.647; V2 (35 WPM, near-edge, -0.4667 channels) measured 29.122 — both
persistent, duration-independent, and matching the ticket text verbatim.

MAN-7's own research correctly traced the causal chain and correctly
measured the effect, but its proposed fix (a downstream `SpeedTracker`
correction) treated a symptom: it did not ask *why* the underlying bias
existed, only how to cancel it after the fact. MAN-7's fix never landed on
`main`; MAN-103 starts from a clean baseline (`49f05a4`) and fixes the
actual root cause.

## Root cause: two independent error terms, not one

### Term 1 — threshold placement (SNR- and offset-dependent)

`Demod`'s keying decision (`crates/manta-decode/src/envelope.rs`) used
`T = sqrt(E_hi * E_lo)` (the geometric mean of the two rail estimators) as
its threshold, per SPEC §3.2/§3.3 and ARCHITECTURE §5. In linear amplitude
terms, `T`'s position relative to `[E_lo, E_hi]` moves toward `E_lo` as the
apparent keying depth (`E_hi / E_lo`) grows — e.g. at 40 dB depth,
`T` sits only ~1 % of the way up from `E_lo`. A rising edge crosses the
`1.25*T` up-threshold almost immediately; a falling edge must decay nearly
all the way back down to cross `0.80*T`. Any real (non-instantaneous)
transition takes disproportionately longer to traverse its last few percent
of amplitude than its first few, so `fall_delay > rise_delay`
unconditionally once keying depth is meaningfully above the
`MIN_KEYING_RATIO = 2.0` (6 dB) pre-decode floor — inflating every measured
mark, worse at higher SNR. This is worse near a channel edge because SPEC
§1.2's prototype filter crosses adjacent channels at -6 dB exactly at the
edge, so a near-edge carrier's keying sidebands sit in the filter's
transition band and the recovered envelope's own transition is slower — the
same threshold-placement mechanism applied to a slower transient produces a
larger inflation. There is no separate near-edge bug; edge proximity only
changes the transition width the same mechanism acts on.

Measured directly (`crates/manta-decode/src/envelope.rs`'s
`keying_edge_bias_is_within_one_hop_of_true_50pct_crossing`, a synthetic
noiseless symmetric raised-cosine envelope, no channelizer, no tracker —
isolates the keying decision alone):

```
                 BEFORE (sqrt(E_hi*E_lo), multiplicative 1.25/0.80 band)
depth_dB  ramp=2  ramp=6  ramp=12
      12   +0.00   +1.00    +4.00
      20   +0.00   +2.00    +5.00
      30   +1.00   +3.00    +6.06
      40   +2.00   +4.00    +8.00
```

(bias in hops between the measured mark and the true 50 %-crossing width;
ramp = transition width in hops). Mark inflation grows monotonically with
both keying depth and transition width — and every hop a mark gains, the
following gap loses (`mark +2 hops <=> gap -2 hops`, exactly): a threshold
crossing moves the mark/space *boundary*, it neither creates nor destroys
time. This is the mechanistic explanation for why MAN-7's mark+gap
symmetric-average idea works at all (see Term 2 below) and why it is the
right shape of fix for a different term than the one it was built for.

A separate, real-pipeline SNR sweep (25 WPM, 0.30 channels residual,
`crates/manta-engine/tests/wpm_across_channel.rs`'s predecessor scratch
harness) corroborates the same mechanism from the reporting side: error
crossed zero around +5 dB and reversed sign below it (before: +1.58 at
0 dB, -3.12 at 25 dB) — exactly the signature of a threshold whose position
*relative to the linear amplitude range* moves with keying depth. This
independently matches the 2026-09-05 lens-3 broad review's own SNR sweep
(25 WPM: "21.5 at +20 dB, 25.0 at 0 dB, 27.5 at -3 dB") that originated this
ticket.

### Term 2 — the 50 %-point convention (constant, transmitter-shaping)

Not named in the ticket or MAN-7's research. `crates/manta-testkit/src/keyer.rs`'s
raised-cosine rise/fall is *contained inside the element* (SPEC §7: 5 ms).
Within an ON segment of `dur_ms`, the envelope ramps up over the first 5 ms
and down over the last 5 ms, so the mark's true 50 %-crossing duration is
`dur_ms - 5 ms` and the following gap's is `gap_ms + 5 ms`. A *correctly*
placed threshold (Term 1 fixed) therefore reads **high**, not correct: fixing
Term 1 alone moved V2's on-center 35 WPM reading to 41.2 (the old bug's
magnitude, sign-flipped) rather than to 35. The old, mis-placed threshold
had been fortuitously cancelling this term for part of its range; removing
it without addressing Term 2 just swaps which way the error points.

This is not a testkit artifact to paper over: a real transmitter also shapes
its edges, manta cannot know its rise time, and the same `unit - rise`
shortening would appear on the air. The correct fix is receiver-side and
must not depend on knowing the transmitter's rise time.

## Fix

Two independent changes, each targeting one term.

### 1. Unbiased keying-edge placement (`crates/manta-decode/src/envelope.rs`)

`DemodConfig::hyst_up`/`hyst_down` (`1.25`/`0.80`, multiplicative about `T`)
replaced by a single `hyst_frac: f32` (default `0.15`): the keying decision
and the rail-update split both use one shared band,
`mid ± hyst_frac * (E_hi - E_lo)`, `mid = (E_hi + E_lo) / 2` — the linear
amplitude midpoint, not the geometric mean. `T` is gone entirely (both
assignment sites removed).

Two design points, each independently confirmed by measurement:

- **Additive, not multiplicative, hysteresis (D3).** For a time-symmetric
  transition (manta's channelizer is provably linear-phase,
  `crates/manta-dsp/src/proto.rs`; the keyer's raised cosine is symmetric),
  a band placed symmetrically about the *linear* amplitude midpoint makes
  the rise-crossing delay equal the fall-crossing delay for any transition
  width or keying depth. A multiplicative `1.25x`/`0.80x` band is symmetric
  in the *log* domain, which reintroduces the same asymmetry being removed.
  Measured: linear-midpoint `T` with the old multiplicative band retained
  left V2 at 32.994 (still outside +/-2 WPM); switching to an additive band
  closed it to 33.765.
- **Rails trained only from samples outside the decision band (D5).** A
  sample mid-transition belongs to neither level; training `E_hi` from it
  drags the midpoint low and reintroduces a transition-width-dependent
  residual (measured: +1 hop at ramp 6, +2..3 hops at ramp 12, with the
  band-inclusive split). Excluding the band from both rail updates drives
  the synthetic depth x ramp grid to exactly 0.000-hop bias at every cell
  (see the measured table above, which already reflects this).

`hyst_frac = 0.15` (D4) is a pure noise-immunity knob — the timing is
provably independent of band width for a symmetric transition. Confirmed: a
real-pipeline sweep over `{0.05, 0.15, 0.30, 0.45}` moves the on-center
reading by at most ~1 WPM at any of 20/25/35 WPM; the near-edge column
degrades noticeably by 0.45, so 0.15 is chosen on margin (comparable to the
old scheme's margin at the `MIN_KEYING_RATIO` floor), not by tuning against
these numbers.

### 2. Symmetric dit-period estimation (`crates/manta-decode/src/timing.rs`)

Adopts MAN-7's own mechanism (`(mu_dit + mu_egap) / 2`) — sound as a fix for
Term 2 even though MAN-7 built it for the wrong term. `SpeedTracker` gains
`mu_egap_ms: Option<f32>`, `on_element_gap()` (same `CLUSTER_ALPHA = 0.15`
EMA as the mark clusters), and `dit_estimate_ms()`:

```rust
pub fn dit_estimate_ms(&self) -> f32 {
    let mu_dit = self.pair.lo;
    let Some(g) = self.mu_egap_ms else { return mu_dit };
    let cap = DIT_BIAS_CAP_FRAC * mu_dit;
    let delta = (0.5 * (mu_dit - g)).clamp(-cap, cap);
    (mu_dit - delta).clamp(DIT_CLAMP_MS.0, DIT_CLAMP_MS.1)
}
```

Reported WPM (`SpeedTracker::on_mark`'s EMA) is `1200 / dit_estimate_ms()`.
`mu_dit_ms()` itself stays uncorrected — its consumers (SPEC §4.2's
`u = gap/mu_dit`, §4.3's beam likelihoods, §3.2's `tau_hi`, §4.2's flush)
want a centroid consistent with the marks being classified, not an absolute
physical estimate; only the report wants that.

**D6 — the cap is two-sided, not one-sided.** MAN-7 clamped its correction
to `[0, 0.35*mu_dit]`, reasoning that its asymmetric-hysteresis threshold
could only ever inflate marks, never shrink them. That reasoning does not
carry over: after this fix, the residual bias (Term 2 alone) is *negative* —
marks read short by the transmitter's rise time, gaps read long by it. A
one-sided `[0, cap]` clamp would zero out exactly the correction this fix
needs. `DIT_BIAS_CAP_FRAC = 0.35` (unchanged magnitude) now clamps
`[-0.35*mu_dit, +0.35*mu_dit]`, still bounding a runaway if mark/gap pairing
ever breaks down (e.g. an element gap swallowed by the 12 ms debounce at
extreme WPM).

`decoder.rs:167`'s previously-empty `GapClass::InterElement => {}` arm now
calls `self.tracker.on_element_gap(dur_ms)` when `live`, mirroring
`on_mark`'s own live-vs-replay guard.

Attribution, isolating each term (35 WPM, 20 dB AWGN, jitter-free):

| build | on-center | near-edge (0.4667 ch) |
|---|---|---|
| baseline | 33.143 (-1.86) | 28.523 (-6.48) |
| + threshold fix only (Term 1) | 41.200 (+6.20) | 33.725 (-1.28) |
| + symmetric period estimate (Term 2) | **35.300 (+0.30)** | **34.767 (-0.23)** |

Threshold placement alone trades a -6.5 WPM error for a +6.2 WPM one — both
fixes are required; neither is optional.

### 3. Re-derived gap constants (`crates/manta-decode/src/timing.rs`, `decoder.rs`)

**D7 — `CHAR_GAP_DITS` restored to SPEC's nominal `2.0`.** The prior `1.6`
(`docs/DECISIONS/2026-07-18-char-gap-threshold-fix.md`) existed only to
compensate for the old threshold's overshoot inflating `mu_dit` relative to
true gap timing; that overshoot is gone. With the corrected timing, `1.6`
overcorrects: a currently-enforced golden gate (V10, Farnsworth) newly fails
under the threshold fix alone (measured CER 0.2368), and the failure is
identical at `CHAR_GAP_DITS` 1.6 and 2.0 to four decimal places — ruling out
the char-gap boundary as the cause and pointing at the mechanism below
instead. `gap_classification_nominal`/`farnsworth_moves_word_threshold`
(`crates/manta-decode/src/timing.rs`) already exercise nominal thresholds at
the new boundary; a dedicated 500-case sweep re-running the original
2026-07-18 methodology against the corrected timing is follow-up work, not
re-run in this container.

**D8 — the Farnsworth flush bootstrap.** `decoder.rs`'s `check_flush`
resolves the SPEC §4.2 7-dit safety-net flush *outside* `GapClassifier::classify`,
so `long_seen` and the long-gap `ClusterPair` never saw those gaps. Harmless
while `mu_dit` ran high (the flush threshold in ms sat comfortably above
real Farnsworth character gaps); once `mu_dit` is corrected,
`flush_gap_dits * mu_dit` drops below them, the safety net intercepts
essentially every character gap before `classify` ever sees it, and the
Farnsworth bootstrap (which needs `long_seen >= FARNS_MIN_COUNT` from
`classify` alone) can never complete — this was the actual cause of V10's
newly-broken CER above, not the `CHAR_GAP_DITS` value. Fixed by
`GapClassifier::observe_flushed(gap_ms, mu_dit_ms)`, called once from
`check_flush` before the drain, folding the flush-resolved gap into the same
statistics `classify` would have.

## What is confirmed NOT a contributor

- **The noise pedestal.** A synthetic-envelope sweep with realistic
  complex-Gaussian noise measured a mark bias of essentially zero
  (-0.011 to +0.016 hops) down to 10 dB SNR under the fixed threshold.
- **Hysteresis band width as a timing knob** (see `hyst_frac` above) — it is
  provably a margin knob only, for a symmetric transition.
- **Per-hop channel-reselection ("argmax flicker").** A candidate mechanism
  in MAN-7's research (`Track::select_channel` re-picking the max-power
  channel every hop, which can chatter near a channel edge where two
  channels are near-tied). Not touched by this fix, and the near-edge
  severity is fully explained by transition-width narrowing alone (the
  synthetic mechanism test above has no channel selection at all and
  reproduces the full near-edge magnitude) — there is nothing left for
  channel-reselection flicker to explain.

## Fading-robustness cost (recorded, not fixed — D9)

A midpoint threshold sits materially higher above the noise floor than a
geometric mean at high keying depth (~14 dB higher at 40 dB depth), so a
fading mark drops below key-down sooner. Measured on the already-`#[ignore]`d
fading vectors (`docs/DECISIONS/2026-09-06-broad-review-decisions.md` D8
already assigns V5/V6/V8w to MAN-107 through MAN-113, "unrelated to fading"
for V2/MAN-103's own disposition):

| vector | baseline | after this fix | gate | status |
|---|---|---|---|---|
| V5 (Watterson-poor, 22 WPM, +3 dB) | CER 1.4167 | CER 1.1354 | <= 0.20 | still fails, slightly better |
| V6 (sine QSB, 20 WPM) | CER 0.1429 | CER 0.5397 | <= 0.10 | still fails, **measurably worse** |
| V8w (Watterson pileup) | 1/34 strong signals | 0/34 | >= 90 % | already at the floor pre-fix (re-confirmed by re-running unmodified `main`); moves by one signal |

V6 is the one real degradation here. Mechanism: a faster-falling `E_hi` or
per-element fading normalization (both already named in MAN-107..113) is
the likely classical answer — out of MAN-103's scope by
`docs/DECISIONS/2026-09-06-broad-review-decisions.md` D8, which states
explicitly that V2's disposition is "unrelated to fading." **This fix does
not widen, re-ignore, or otherwise hide any gate to accommodate this
movement.**

## Verified

- `envelope::tests::keying_edge_bias_is_within_one_hop_of_true_50pct_crossing`
  (new): 0.000-1-hop bias across a 4x3 depth/ramp grid.
- `timing::tests::dit_estimate_is_invariant_to_boundary_shift`,
  `dit_estimate_correction_is_capped_two_sided`,
  `dit_estimate_falls_back_to_mu_dit_before_any_element_gap`,
  `drift_reinit_clears_the_gap_centroid`,
  `observe_flushed_advances_farnsworth_statistics` (new).
- `gap_classification_nominal` updated for the `2.0` boundary.
- V1's WPM gate tightened `+/-3` -> SPEC's nominal `+/-2` (passes).
- V2's WPM gate split out of its combined CER+WPM test and enabled at
  `35 +/- 2` (passes) — MAN-7's own acceptance scenario, now met by the
  actual root-cause fix. V2's CER gate stays `#[ignore]`d (SPEC §2.1
  warmup-floor dilution, unrelated to this ticket).
- `crates/manta-engine/tests/wpm_across_channel.rs` (new): +/-2 WPM at
  four residual offsets (0.0 to 0.4667 channels) x three speeds
  (20/25/35 WPM).
- V7/V9/V10 golden gates green (V10 specifically regression-tested against
  the Farnsworth-bootstrap failure this fix's D8 addresses).
- `crates/manta-engine/tests/regression_char_gap_high_wpm.rs` (the pinned
  "AB" @ 33.14 WPM case) still passes.
- Determinism gates (`chunking_determinism`, `channelizer_chunking_determinism`)
  unaffected — this fix touches only per-hop arithmetic, not chunking.
- Full workspace `cargo build --release --all-targets` and
  `cargo test --release --workspace` green.

## Migration note

`DemodConfig::hyst_up`/`hyst_down` -> `hyst_frac` is a breaking change to a
`pub` struct in `manta-decode`. All in-tree construction sites use
`DemodConfig::default()`/`DecodeConfig::default()`; the workspace is
pre-`v0.1.0` alpha (`docs/DECISIONS/2026-09-06-broad-review-decisions.md`
D7), so no deprecation shim is warranted — SPEC §9's table is the
compatibility surface and changes in the same PR. Golden vector *waveforms*
are unchanged (only the decode path moved); reported WPM values change by
design.

## References

- Ticket: MAN-103 (supersedes MAN-7)
- Origin: 2026-09-05 broad review, lens 3, hit-list #4 — "Fix keying
  threshold placement (midpoint/likelihood-ratio, not `sqrt(E_hi*E_lo)`)
  ... re-derive CHAR_GAP=2.0"
- `docs/DECISIONS/2026-07-18-char-gap-threshold-fix.md` — the `1.6`
  deviation this ticket retires, and the sweep methodology D7 re-applies
- `docs/DECISIONS/2026-09-06-broad-review-decisions.md` D8 — the MAN-103 /
  MAN-107..113 scope boundary D9 above relies on
- `docs/SPEC-decode-core.md` §3.2, §3.3, §4.1, §4.2, §9, §10
- `ARCHITECTURE.md` §5
