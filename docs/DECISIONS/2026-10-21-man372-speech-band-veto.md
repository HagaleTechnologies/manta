# MAN-372: zero CW tracks from SSB voice at +30 dB — the speech-band veto

Date: 2026-10-21. Ticket: MAN-372. Branch `MAN-372`, base `0131e44`
(research commit, one ahead of `main` @ `ab424e6`). Prior art:
[MAN-313](2026-10-13-man313-non-cw-rejection.md) (the non-CW filter),
[MAN-271](2026-10-16-man271-short-transmission-decode.md) (rule D′),
SPEC §2.8 (rules and constants; C′ is new).

## What shipped

A **speech-band marking** in `manta-dsp::comod::CoModDetector` (SPEC §2.8
C′): at every classifier evaluation the classifier judges the *band* as a
whole, not only per-channel co-modulation. A hot region — maximal hot
run bridging ≤ `COMOD_SPEECH_GAP` (2) cold channels — that is
**speech-shaped** (≥ `COMOD_SPEECH_MIN_HOT` (3) hot channels, aggregate
envelope carrying ≥ `COMOD_SPEECH_RATIO` (2.2)× more 1–5 Hz than 8–30 Hz
DFT magnitude) and is **wide** (≥ `COMOD_SPEECH_MIN_WIDTH` (9) channels)
**or has a speech-shaped sibling within `COMOD_SPEECH_MAX_GAP` (30)
with aggregate-envelope Pearson rho ≥ `COMOD_RHO` (0.5)**
marks `[first−1, last+1]` for `COMOD_HOLD_HOPS` (5 s), exactly as a span.
`in_speech_band(k)` then gates rules D and D′ alike and the spawn veto,
spawns only (MAN-314's lesson: a veto closes nothing).

Result: `ssb_voice_at_14040_starts_no_cw_track` un-ignored and green in
debug and release — **0 in-band `TrackPromoted` at +30 and +45 dB**
(testkit voice, seed 1, 40 s). No regression anywhere: every golden
vector decodes exactly as with the filter off (all 20, heavy gate), the
CW beside the SSB at 14043 kHz still decodes and spots, weak CW beside
+30 dB RTTY and +30 dB CW still decodes, the sweep matrix and RTTY suite
stay green.

## Baseline and the branch before this change

In-band `TrackPromoted` counts with the filter on, testkit voice seed 1,
by level:

| level | `main` @ `ab424e6` | X8 (`0131e44`, `COMOD_LAG_MAX` 30) | MAN-372 (this change) |
|---|---|---|---|
| +15 dB | 21 | 21 | 8 |
| +20 dB | 24 | 18 | 2 |
| +30 dB | 7 | 2 | **0** |
| +45 dB | — | 0 | **0** |
| +60 dB | — | 0 in band | 0 in band |

(81 at +30 dB with the filter off; nothing is ever spotted.) The MAN-313
record's "+15: 7, +20: 2" figures belong to the prototype voice, not
this generator. X8 — the research commit's `COMOD_LAG_MAX` 16 → 30
re-measurement, kept — makes the co-modulation span form at the first
evaluation; the last two survivors qualified (D at hop 713, D′ at hop
1763) exactly where the span's coverage has gaps: k 452 in the upper
harmonic run and k 445 in the band's internal formant gap, hot with no
correlated partner within range (ρ < 0.5).

## The measurement (Phase 1) and what it killed

Go/no-go harness: `measure` mode in
`crates/manta-engine/examples/man372_ssb.rs` dumps, per evaluation, every
hot region's occupancy (`hot`), aggregate-envelope DFT sums over 1–5 Hz
(`low`) and 8–30 Hz (`high`), and ratio = low/high (`#[doc(hidden)]
diag_speech_regions`), plus the per-track D/D′ trace, over seven scenes:
`ssb 30`, `ssb 45`, `coexist`, `weakrtty`, `loudcw`, `cw`, `v8w`.

- **F1 alone** (occupancy threshold): dead. The binding kill region
  (hop 1763) holds 3 hot channels; the keeps include an 8-hot RTTY run
  (weakrtty) and the loud CW's 11-hot skirt run. No threshold separates.
- **F2 alone** (ratio threshold): dead. V8w's 2-hot pileup regions reach
  ratio 2.54 while the kill needs ≤ 2.53.
- **F1∨F2**: dead (fires strictly more than either).
- **F1∧F2** (hot ≥ 3 ∧ ratio ≥ R): alive on the SSB+coexist/weakrtty/cw
  data with R ∈ (1.92, 2.53] — until the RTTY suite's golden gate ran:
  **VR3** (single +45 dB, 30 WPM CW) has its own carrier+skirt region
  (6 wide, 5–6 hot) reach ratio 2.92 at many evaluations, and D′ fires
  under just such a marking — VR3 stopped decoding entirely. No
  occupancy floor fixes this: the kill region is *narrower* than VR3's.
- **The structural clause is what separates**: a voice band is a *wide
  structure* — its sub-bands (strong lower harmonics, upper harmonic
  run, split by the formant gap) are speech-shaped *simultaneously*,
  3–15 channels apart, while every control's speech-shaped region is
  alone or far from any sibling. Measured across all seven scenes: the
  voice's shaped regions are 3–25 wide with siblings 3–15 away (or one
  bridged 25-wide region); V8w's coincidental shaped regions never
  exceed 7 wide and never come closer than **128** to each other; VR3's
  is 6 wide and isolated. `COMOD_SPEECH_MIN_WIDTH` = 9 and
  `COMOD_SPEECH_MAX_GAP` = 30 (the module's existing whole-voice-band
  scale, `COMOD_LAG_MAX`) are the measured interval midpoints.
- The kill at hop 1763 itself is covered by the **hold**: the wide
  region (444,454) fired at hop 1688 and marks through hop 3563.

Constants and their binding rows: `COMOD_SPEECH_MIN_HOT` 3 (kill row
(445,447,3 hot); controls' shaped regions ≥ 5 hot but excluded by the
structure clause, the 2-hot keeps sit at 2), `COMOD_SPEECH_RATIO` 2.2
(kills 2.53/3.59; hardest isolated keep 1.92, hardest shaped keep 2.92
— separated by structure, not ratio), `COMOD_SPEECH_MIN_WIDTH` 9
(voice 10–11; controls ≤ 7), `COMOD_SPEECH_MAX_GAP` 30 (voice 3–15;
controls ≥ 128).

## PR #374 correction: sibling regions need shared modulation

The original sibling rule accepted proximity alone. Review identified
that two nearby CW skirts could therefore validate each other even when
their envelopes were unrelated. The classifier regression
`narrow_speech_regions_require_shared_modulation` reproduces this with
two six-channel regions 14 cold channels apart, each above the speech
ratio threshold, with independent 2 Hz and 3 Hz aggregate envelopes.
Before the correction, it fails because both regions are marked.

The sibling rule now also requires Pearson rho ≥ the existing
`COMOD_RHO` (0.5) between the regions' hot-channel sums in the current
window. Each sum is centred and normalized once, with deterministic
`f64` accumulation. Constant envelopes contribute no evidence. The
same test also checks that two narrow regions sharing a 2 Hz envelope
still mark. This adds band-level evidence, not a per-track taint, and
does not change the wide-region rule or the five-second hold.

The separate `nearby_loud_cw_decodes_identically_with_the_filter` control
places +45 dB CW signals 2 kHz apart at 30 and 31 WPM, with different
texts. That scene passes before and after the correction; the synthetic
classifier test is the regression that fails before it. Correlation is
evidence, not proof of source identity: coincidentally correlated CW
envelopes can still satisfy the threshold. The rule makes no universal
claim that every independent CW pair is distinguishable.

## Why per-channel statistics stayed dead

The four families the research phase measured dead stay dead and were
not re-walked: hot-run width taint (X6: no width separates voice runs
3–11 from real-CW runs 2–9), sticky correlation taint (X9: shared
silence correlates a real CW with the voice band to ρ ≥ 0.7 given enough
windows), neighbourhood hot-count (identical counts for a survivor and
the coexistence CW at every radius), gap-bridging span merges (the
voice's internal gap measures as cold as the taper gap beside the
coexistence CW). The rule that ships is per-evaluation band-level
evidence, never a sticky per-track taint; it gates D and D′ with one
clause (the MAN-271 interaction: both survivors' rules were used — D at
713, D′ at 1763) and vetoes spawns only.

## Known limits kept

- CW *inside* an SSB passband is not protected (accepted MAN-313 limit;
  the coexistence gate's CW sits 3 kHz away and stays decoded).
- The zero is scoped to the testkit voice, seed 1: +15/+20 dB now leave
  8/2 unspotted in-band tracks (better than X8's 21/18, still not zero)
  — a different voice may differ; the filter-off path is untouched.
- The abrupt-switch-on click residual (MAN-313 follow-up, unowned)
  stands.

## Bench

`cargo bench -p manta-dsp --bench comod -- --quick`, 60 s of 192 kHz
input per scene (this container; MAN-313's numbers in parentheses were
measured on the 32-core dev container at load 25 — shared-machine
noise applies):

| scene | MAN-313 | MAN-372 (X8 lag 30 + speech detector) |
|---|---|---|
| 10 blocks + 300 tones | 1.013 s (0.017×) | 1.834 s (0.031×) |
| all hot | 3.805 s (0.063×) | 6.023 s (0.100×) |
| quiet | 0.402 s (0.0067×) | 0.391 s (0.0065×) |

The all-hot worst case already missed its 0.05× target at MAN-313; X8's
`COMOD_LAG_MAX` 16 → 30 roughly doubles the pairing loop and the speech
detector adds one region pass plus a ≤ 375-sample × 35-frequency DFT per
region per evaluation. The CPU budget is a target, not a CI gate
(AGENTS.md); realistic scenes sit far below the all-hot worst case (the
quiet scene is unchanged). The hop-major ring / running-sums follow-up
from the MAN-313 record remains the prerequisite for the Pi4 leg.

## Diagnostics kept (D7)

`MANTA_MAN372_DIAG=1` traces every evaluation's hot runs, spans, speech
regions (with ratio) and speech bands, and every unqualified track's
gate-by-gate verdict (now including `!speech`); `MANTA_MAN372_BAND=lo:hi`
dumps a band profile. `#[doc(hidden)]`, `OnceLock`-cached, zero cost
when unset. `crates/manta-engine/examples/man372_ssb.rs` (kept: it
renders every gate scene and the `measure` mode that produced this
record's tables) exercises them.
