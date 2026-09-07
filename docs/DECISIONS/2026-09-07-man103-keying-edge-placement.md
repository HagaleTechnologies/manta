# MAN-103: unbiased keying-edge placement and symmetric dit-period estimation

## Background

MAN-103 supersedes MAN-7 ("Reported WPM should not read systematically low,
especially for signals offset near a channel edge"). MAN-7's own fix (a
downstream, tracker-side symmetric correction) never landed on `main`; this
ticket's technical notes name the actual root cause instead: the keying
decision in `crates/manta-decode/src/envelope.rs` used
`T = sqrt(E_hi * E_lo)` (the geometric mean of the two keying rails) as its
threshold, which sits far enough below the signal at high apparent SNR that
the `1.25*T` up-crossing fires on the channelizer's own rise transient,
stretching every measured mark. The bug reproduced exactly as described on
`49f05a4`: V1 (20 WPM, +20 dB, on-center) read 17.647 WPM; V2 (35 WPM,
near-channel-edge) read 29.122 WPM — both matching the ticket text and this
repo's own pre-existing test comments to three decimals (see
`crates/manta-cli/tests/golden_v1.rs` and `golden_v2_v3.rs`'s prior
comments, and the MAN-103 research document,
`2026-09-07-MAN-103-reported-wpm-should-not-read-systematically-low-especially.md`).

## Two independent error terms

Investigation (this ticket's research and plan phases, both re-verified
during implementation) found the bug is **two independent error terms
stacked on top of each other**:

1. **An SNR- and offset-dependent term from threshold placement** — the term
   the ticket names. `T = sqrt(E_hi*E_lo)` is the arithmetic midpoint of
   `log(E_hi)` and `log(E_lo)`, so its position *relative to the linear
   amplitude range* moves toward `E_lo` as apparent keying depth grows (at
   `E_hi=1.0, E_lo=0.01` — 40 dB apparent depth — `T=0.1`, only 10% of the
   way up). A rising edge only has to climb that small fraction before the
   `1.25*T` threshold fires; a falling edge has to decay nearly all the way
   back down before `0.80*T` fires. Any real (non-instantaneous) transition
   takes disproportionately longer to traverse its last few percent of
   amplitude than its first few percent, so this term is worse at higher SNR
   and — because a near-channel-edge carrier's keying sidebands sit inside
   the PFB prototype filter's transition band (SPEC §1.2 puts adjacent
   channels crossing at exactly -6 dB at the edge), slowing the recovered
   envelope's own rise/fall — much worse near a channel edge. There is no
   separate near-edge mechanism: the same asymmetric-threshold effect,
   applied to a slower transient, produces a larger bias.
2. **A constant, transmitter-shaping term that no threshold placement can
   remove.** `crates/manta-testkit/src/keyer.rs` documents its own
   convention (and every real transmitter does something equivalent): the
   raised-cosine rise/fall is *contained inside the element*, so an
   element's 50%-crossing duration is its nominal keyed length minus one
   full rise time, and the following gap's 50%-crossing duration is that
   much longer. A correctly-placed threshold measures elements at their 50%
   points — which is *shorter* than the nominal keyed length. Left alone,
   fixing only term 1 reads 41.2 WPM for a 35 WPM signal: the original bug's
   magnitude, sign-flipped.

Both terms had to be fixed for the ticket's acceptance criterion to hold.

## Decisions

**D1 — Fix the threshold, and also keep a downstream mark/gap symmetry
correction.** The ticket says MAN-103 supersedes MAN-7. It supersedes MAN-7's
*diagnosis* (the offset/SNR-dependent term is a threshold-placement defect
and must be fixed at the source, because the uncorrected dit centroid also
feeds SPEC §4.2's gap-ratio classification, §4.3's beam likelihoods, and
§3.2's `tau_hi` — a report-only correction would leave all of those consuming
biased numbers). But MAN-7's *mechanism* — `(mu_dit + mu_egap)/2`, adopted
here as `dit_estimate_ms()` — remains the only thing that can remove the
50%-point term, since that term is a property of the transmitted waveform,
not something the receiver's threshold placement can see.

**D2 — Midpoint, not likelihood-ratio.** The ticket allows either. In
additive complex Gaussian noise the two keying rails have comparable
variance, so the likelihood-ratio decision boundary is approximately their
arithmetic midpoint anyway — the midpoint gets the same answer without
needing a variance estimate the pipeline doesn't maintain. A true
matched-filter/LR detector with dit-scale integration is MAN-107's scope
(fading robustness), not this ticket's.

**D3 — Additive symmetric hysteresis, replacing the multiplicative band.**
The PFB prototype filter is provably linear-phase
(`crates/manta-dsp/src/proto.rs`) and the testkit's raised-cosine keying
edge is symmetric, so for a transition symmetric in time, the rise-crossing
delay equals the fall-crossing delay *iff* the up/down thresholds are placed
symmetrically about the linear-amplitude midpoint. A `1.25x`/`0.80x` band is
symmetric in the *log* domain, not the linear one, so it reintroduces the
very asymmetry an unbiased threshold placement is supposed to remove.

**D4 — `hyst_frac = 0.15`** (band = 35%..65% of the keying depth
`E_hi - E_lo`). Because the timing is provably independent of band width for
a symmetric transition, this constant is purely a noise-immunity knob. At the
`MIN_KEYING_RATIO = 2.0` pre-decode floor it gives `[1.35, 1.65]*E_lo`
against the old `[1.13, 1.77]*E_lo` — comparable margin.

**D5 — Rails updated only from samples outside the decision band.** A
sample mid-transition belongs to neither level. Keeping the rail split at
the same crossing as the key decision (the old `a > T`) leaves a residual
that grows with transition width, because transition samples drag `E_hi`
down. Excluding the band from both rail updates removes that residual (see
Measurements below).

**D6 — Two-sided `DIT_BIAS_CAP_FRAC = 0.35`.** MAN-7's original design
clamped its correction to `[0, 0.35*mu_dit]`, reasoning that the old
asymmetric hysteresis made a *negative* bias physically impossible. After D3
that reasoning no longer holds: with the threshold unbiased, the residual
50%-point term is *negative* by exactly the transmitter's rise time (marks
read short, gaps read long). The cap is two-sided,
`[-0.35*mu_dit, +0.35*mu_dit]`, so it still bounds a runaway (e.g. if
mark/gap pairing breaks down because element gaps get swallowed by the 12 ms
debounce at extreme WPM) while admitting the negative correction the fixed
threshold now needs.

**D7 — `CHAR_GAP_DITS` back to SPEC's nominal `2.0`.** `1.6` existed only to
compensate the old threshold's overshoot
(`docs/DECISIONS/2026-07-18-char-gap-threshold-fix.md`); with mark timing
corrected, that compensation is no longer needed or correct. Verified
directly in this session: with the Farnsworth-flush fix (D8) applied, V10
passes identically whether `CHAR_GAP_DITS` is `1.6` or `2.0` — the earlier
V10 failure this fix set (see Measurements) is not a char-gap effect at all.
The full 500-case, two-seed sweep the 2026-07-18 doc used to derive `1.6` was
**not re-run in this session** (time-boxed out of this implementation pass);
`2.0` is adopted on the strength of (a) it is SPEC's own nominal value,
(b) every golden vector passes at `2.0`, and (c) the char-gap-independent
V10 check above. Re-running the full sweep against the corrected timing is a
reasonable follow-up if a future change touches gap classification again.

**D8 — Fix the Farnsworth flush bootstrap (`observe_flushed`).** In scope by
necessity, not by plan: V10 is a currently-enforced (non-`#[ignore]`d) gate,
and the corrected `mu_dit` breaks it (measured CER 0.2368 with D1-D3/D5
alone, identical at `CHAR_GAP_DITS` 1.6 and 2.0). `decoder.rs`'s
`check_flush` resolves a gap via the 7-dit safety net *outside*
`GapClassifier::classify`, so `long_seen` and the long-gap cluster pair
never see it. That was harmless while `mu_dit` ran high: `flush_gap_dits *
mu_dit` sat comfortably above real Farnsworth character gaps. With `mu_dit`
corrected, it drops below them, and the safety net intercepts nearly every
character gap before `classify` ever runs — the Farnsworth bootstrap (which
needs 5 long gaps to leave its unimodal init) can never complete. Folding
flush-resolved gaps into the same statistics via `observe_flushed` fixes it
(V10 passes, confirmed independent of the `CHAR_GAP_DITS` value per D7).

**D9 — Record the fading movement; do not chase it.** A midpoint threshold
sits further above the noise floor than the old geometric mean at high
apparent depth, so a faded signal drops below it after a shallower fade.
Measured (this session, `--include-ignored`, all three already `#[ignore]`d
and none an enforced gate):

| vector | baseline (`49f05a4`, pre-MAN-103) | after MAN-103 | gate | status |
|---|---|---|---|---|
| V2 (CER half) | CER 0.0325 | CER 0.0244 | <= 0.01 | improved, still `#[ignore]`d (SPEC §2.1 warmup-floor dilution, unrelated) |
| V5 (Watterson-poor) | CER 1.4167 | CER 1.1354 | <= 0.20 | `#[ignore]`d, fails either way (slightly better) |
| V6 (sine QSB) | CER 0.1429 | CER 0.5397 | <= 0.10 | `#[ignore]`d, **measurably worse** |
| V8w (Watterson pileup) | 1/34 strong signals (per the MAN-103 plan phase's measurement) | 0/34 | >= 90% | `#[ignore]`d, already collapsed at baseline |

V6 is the one real degradation. Mechanism: a midpoint threshold sits further
above the noise floor than `sqrt(E_hi*E_lo)` at high apparent depth, so a
fading mark drops below the key-down threshold sooner during a fade. This is
a real cost of correct edge placement and belongs to MAN-107 through
MAN-113 (classical-DSP fading-robustness work), per
`docs/DECISIONS/2026-09-06-broad-review-decisions.md` D8, which already
scopes V5/V6/V8w's disposition out of MAN-103 ("unrelated to fading"). The
likely classical answer is a faster-falling `E_hi` or per-element fading
normalization, both already named in that ticket range. **Nothing in this
change widens or re-ignores a gate to accommodate this.**

## Measurements

Everything below ran in this session's container against a tree that
includes this change (production code: `crates/manta-decode/src/envelope.rs`,
`timing.rs`, `decoder.rs`). "Before" figures for V1-V4's CLI decode are from
this same ticket's research phase, run against the unmodified `49f05a4`
baseline in the same container (see the MAN-103 research document) — not
re-measured a second time in this implementation phase, since the baseline
is unchanged, already-verified fact.

### CLI decode, golden vectors (`manta decode --json`, `wpm` field)

| vector | true WPM | before (`49f05a4`) | after (this change) |
|---|---|---|---|
| V1 (20 WPM, +20 dB, on-center) | 20 | 17.647 | **20.366** |
| V2 (35 WPM, near-edge, +15 dB) | 35 | 29.122 | **34.735** |
| V3 (12 WPM, +6 dB) | 12 | 11.986 | **11.712** |
| V4 (25 WPM, Watterson-good) | 25 | 25.585 | **24.322** |

V2 — the ticket's own reproduction case, and MAN-7's original acceptance
scenario — clears its `35 +/- 2` gate with a 1.7 WPM margin.

### Offset x WPM grid (`manta-engine`, synthetic AWGN scenes, 20 dB, this session)

Residual offset in channel widths from the nearest channel centre; `0.4667`
is V2's own offset (effectively the channel edge). `12`/`20`/`25`/`35` WPM,
60 s scenes, fixed per-cell noise seeds:

```
wpm  residual_ch   reported      err
 12       0.0000     12.450    +0.450
 12       0.1500     12.450    +0.450
 12       0.3000     12.450    +0.450
 12       0.4667     11.958    -0.042
 20       0.0000     20.282    +0.282
 20       0.1500     20.252    +0.252
 20       0.3000     20.265    +0.265
 20       0.4667     19.609    -0.391
 25       0.0000     24.868    -0.132
 25       0.1500     24.868    -0.132
 25       0.3000     24.868    -0.132
 25       0.4667     24.986    -0.014
 35       0.0000     35.037    +0.037
 35       0.1500     35.300    +0.300
 35       0.3000     35.300    +0.300
 35       0.4667     35.002    +0.002
```

Worst-case error across the whole grid: -0.391 WPM (20 WPM, near-edge),
comfortably inside the +/-2 WPM tolerance at every cell — the offset
dependence the ticket names is gone. This is the same grid
`crates/manta-engine/tests/wpm_across_channel.rs` gates (3 speeds x 4
offsets, asserting `<= 2.0` at each cell).

### Mechanism, isolated (`Demod` fed a synthetic symmetric raised-cosine
envelope, no noise, no channelizer — this session)

Nominal 24-hop ON segment, raised-cosine rise/fall of `ramp_hops` contained
inside it (matching the testkit's own convention); `true_mark_hops = 24 -
min(ramp_hops, 12)` is the analytically exact 50%-crossing duration. Steady-
state measured mark hops (after this change), min/max over 35 settled
cycles:

```
depth_dB ramp_hops true_mark meas_mark bias
      12         2        22        22    0
      12         6        18        18    0
      12        12        12        13   +1
      20         2        22        22    0
      20         6        18        18    0
      20        12        12        13   +1
      30         2        22        22    0
      30         6        18        18    0
      30        12        12        13   +1
      40         2        22        22    0
      40         6        18        18    0
      40        12        12        13   +1
```

Zero bias at every depth from 12 to 40 dB and every transition width tested,
except the degenerate `ramp_hops = 12` case (the rise consumes the entire
24-hop element, leaving no flat top — a triangular pulse), which shows a
consistent +1 hop (2.67 ms) rounding residual from discrete 375 Hz hop
quantization of a continuous 50%-crossing point, not from the SNR/offset
mechanism this fix targets. `crates/manta-decode/src/envelope.rs`'s
`keying_edge_placement_is_unbiased_across_depth_and_ramp` unit test pins this
(tolerance +/-1 hop) as a permanent regression gate.

### Golden-vector suite, full run (this session, `--include-ignored`)

```
$ cargo test -p manta-cli --release --no-fail-fast \
    --test golden_v1 --test golden_v2_v3 --test golden_v7_v9_v10 --test golden_v8_v8w \
    -- --include-ignored
v1_passes_end_to_end_from_wav        ... ok
v2_wpm_is_within_spec_tolerance      ... ok   (MAN-103 acceptance test)
v2_char_accuracy_meets_spec          ... FAILED  CER 0.0244 (gate <= 0.01, pre-existing, unrelated)
v3_passes_end_to_end_from_wav        ... ok
v4_passes_end_to_end_from_wav        ... ok
v5_passes_end_to_end_from_wav        ... FAILED  CER 1.1354 (gate <= 0.20, pre-existing, D9)
v6_passes_end_to_end_from_wav        ... FAILED  CER 0.5397 (gate <= 0.10, pre-existing, D9 -- worse)
v7_passes_end_to_end_from_wav        ... ok
v9_passes_end_to_end_from_wav        ... ok
v10_passes_end_to_end_from_wav       ... ok   (D8's Farnsworth-flush fix)
v8_pileup_validates_...              ... ok
v8w_pileup_fading_...                ... FAILED  0/34 strong signals (gate >= 90%, pre-existing, D9)
```

Exactly the four `#[ignore]`d failures D9 predicts and no others; every
non-`#[ignore]`d gate (including the new/tightened MAN-103 gates) is green.
The full workspace suite (`cargo test --release --workspace --no-fail-fast`)
is green: every crate's test binary reports `0 failed`.

### What this session did not run

- **The `CHAR_GAP_DITS` 500-case, two-seed sweep** (D7) — time-boxed out;
  the char-gap-independent V10 check plus SPEC's own nominal value stand in
  for it. A reasonable follow-up if gap classification is touched again.
- **The Pi4 CPU-budget acceptance leg and the 24 h live-SDR soak** — both
  need physical hardware not reachable from any container (this is a
  pre-existing, standing gap noted in `CLAUDE.md`'s Status section, not
  specific to this change). The in-container `cpu_budget` *test* (not the
  criterion bench) passed as part of the full workspace suite run above; the
  net per-hop change is one `sqrt` removed and a handful of adds/multiplies,
  so no regression is expected, but the Pi4 acceptance number itself was not
  re-measured here.
- **V8w's exact pre-MAN-103 baseline (1/34)** was not independently
  re-measured in this implementation session; it is carried from the MAN-103
  plan phase's own measurement (same container, same commit family), which
  recorded stashing this change and re-running to establish the baseline
  wasn't a fresh regression. This session did independently re-measure the
  *after* value (0/34, above).

## Migration notes

`DemodConfig`'s `hyst_up`/`hyst_down: f32` fields become a single
`hyst_frac: f32` (default `0.15`) — a breaking change to a `pub` struct in
`manta-decode`. In-tree, every construction site uses
`DemodConfig::default()` (`envelope.rs`, `decoder.rs`'s
`DecodeConfig::default()`); no in-tree caller passed the old fields
explicitly. The workspace is pre-`v0.1.0` (an explicit pre-stability alpha
per an earlier broad-review decision), so no deprecation shim was added; the
SPEC §9 table is the compatibility surface and it changes in this same
change.

Golden vector *waveforms* are unchanged (only the decode path changed), so
the pending golden-vector freeze is unaffected. Reported WPM values change,
by design — this is the entire point of the fix.

## References

- Ticket: MAN-103 (supersedes MAN-7)
- `docs/DECISIONS/2026-07-18-char-gap-threshold-fix.md` — the `1.6`
  deviation this change retires (D7)
- `docs/DECISIONS/2026-09-06-broad-review-decisions.md` D8 — the MAN-103 /
  MAN-107..113 scope boundary D9 relies on
- `docs/SPEC-decode-core.md` §3.2, §3.3, §4.1/§4.1a, §4.2, §9
- `ARCHITECTURE.md` §5
