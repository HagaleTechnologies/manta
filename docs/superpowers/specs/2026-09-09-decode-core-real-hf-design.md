# Decode core for real HF contest CW — design (MAN-166)

Status: **proposed** (2026-09-09, overnight brainstorming pass on MAN-166).
Companion documents: `docs/SPEC-decode-core-v2.md` (normative spec for the
recommended approach) and
`docs/superpowers/plans/2026-09-09-decode-core-v2.md` (implementation
plan). Nothing in this document is implemented.

Scope: the per-track decode core — `manta-decode` (envelope → keying →
timing → Morse decode) plus the two `manta-dsp` inputs it depends on
(noise floor, the envelope handed to it). Explicitly **out of scope**: the
track lifecycle state machine (`manta-engine/src/track.rs`,
`merge_converged`, ownership) and `manta-spot`'s repetition gate — both
under separate investigation. Where this design needs something from
those layers it names an interface, not a change.

Clean-room note: the literature survey behind §3 was done as a narrative
report (algorithms described in prose with citations). No code from any
other decoder was read for reuse or transcribed; every mechanism below is
specified from first principles and the measurements in §1.

---

## 0. Summary

1. **The gap is not noise sensitivity.** The real signals in the B2
   recording that K5TR's own CW Skimmer spotted have 25–60 dB of keying
   depth inside manta's 93.75 Hz channel. Fed the correct channel by an
   oracle (tracker bypassed), manta's decode core recovers the callsign as
   a whole word on **27 %** of them, with `CQ`/`TEST`/`DE` framing on
   **13 %**, and the callsign as a substring (word boundaries ignored) on
   **48 %**. The strongest third (≥ 25 dB in 500 Hz) does no better than
   the middle third.
2. **Root cause 1, measured: keying-threshold placement.** SPEC §3.2 puts
   the threshold at the geometric mean of the two rails. On real signals
   that is 17–30 dB below the mark. The channel filter's step response and
   the transmitter's edges then make every mark ≈ +15 ms long and every
   gap ≈ −15 ms short. At contest speed (dit 30–40 ms) that is a 40–50 %
   timing error: word gaps read as character gaps (`CQTESTNJ3K`, so the
   validator never sees `CQ <call>`), reported WPM runs at 0.82× truth,
   and at ≥ 37 WPM inter-element gaps fall under the 12 ms debounce and
   dits fuse. Moving the threshold to half amplitude of the mark rail
   restores true timing (dit 1.04, dah 3.03, gap 0.94 in true-dit units,
   vs 1.44 / 3.48 / 0.64 today).
3. **Root cause 2, structural: four irreversible hard decisions in
   series** (key state, debounce, 2-means cluster assignment, gap class),
   each a threshold tuned against synthetic fixtures, none revisitable
   once made. Real signals carry QRM blips, QSB, and speed changes that a
   sequential thresholder cannot reconcile after the fact; the width-4
   character-local beam only sees what survives those four filters.
4. **Recommendation: replace the chain with a probabilistic sequence
   decoder (Approach B, §4)** — a hidden semi-Markov Viterbi/token-passing
   decoder over per-hop *edge-centered* evidence, with element durations,
   gap types, and speed all carried as hypotheses and the Morse tree as
   the grammar. It subsumes MAN-107 (fading normalization), MAN-110
   (dit-scale integration), MAN-111 (gap scoring), and MAN-113
   (multi-hypothesis speed) in one model, and it produces per-character
   posteriors and alternatives that `manta-spot` can rescore. Build it in
   three stages; stage 1 (the evidence front end) is independently
   shippable behind the legacy decoder and delivers most of the timing fix
   on its own.
5. **The headline benchmark number was wrong.** The RBN truth file
   contains K5TR's *own* SkimSrv spots for the recording (210 call+kHz
   bins from the same antenna and receiver). Against that like-for-like
   reference tonight's run scores **26.2 % recall / 24 % precision**, not
   4.7 % / 30 % — most of the 1,451 worldwide bins are EU stations heard
   only by EU skimmers. Both numbers should be reported; the K5TR-only one
   is the parity metric.

---

## 1. Evidence

All measurements: B2 recording (`crates/manta-testkit/audio-corpus/`),
2026-09-09, throwaway scripts in the session scratchpad (a WOLA
channelizer replica in numpy with SPEC §1's exact constants; a 40-line
example that feeds a raw 375 Hz power stream into `TrackDecoder`). They
are reproducible and are the seed of the stage-0 harness in §5.

### 1.1 What the real signals look like

RBN truth for the 15-minute window (26,801 rows, 177 spotters):

| | all spotters | K5TR only |
|---|---|---|
| unique call+kHz bins | 1,451 | 210 |
| median WPM | 34 | 33 |
| WPM ≥ 30 | 84 % | 78 % |
| median SNR (500 Hz) | 21 dB | 24 dB |
| bins heard by exactly one skimmer worldwide | 817 (56 %) | 5 |

K5TR is a co-located spotter: same receiver chain as the recording (CWSL
→ SkimSrv). Its 210 bins are what CW Skimmer produced from this exact RF.

Per-channel envelope statistics for 221 K5TR-spotted (call, kHz) windows
(40 s each, own channel chosen as the strongest of the three around the
RBN kHz):

- Keying depth (mark rail − space rail, in the 93.75 Hz channel):
  median **35 dB**, range 24–60 dB; per-second 10th percentile 15–34 dB.
- Off-hops within 6 dB of the mark rail: **0 %** on every signal
  measured. The space rail is not contaminated by co-channel keying at
  the level a threshold decoder would notice.
- Neighbor channels' *space-level* power (±1, ±2, ±3 channels) sits
  within ±5 dB of the own channel's: the local floor is flat across
  channels; strong-neighbor splatter is not the dominant effect at this
  station.
- Two of twelve hand-inspected windows contained a *different, stronger
  station* in the strongest channel than the one RBN listed at that kHz
  (e.g. the 9A2L window decodes W1CSM). Co-channel occupancy within one
  93.75 Hz channel is real at this density; it is a tracker/channel-
  selection problem, noted for §7.

### 1.2 The decode core alone (tracker bypassed)

Each window's own-channel power stream was fed straight into
`manta_decode::TrackDecoder` (default config, `sqrt(power)` per hop, as
`manta-engine` does):

| metric (n = 221) | result |
|---|---|
| callsign appears as a whole word | 59 (27 %) |
| … with `CQ`/`TEST`/`DE` framing the validator needs | 29 (13 %) |
| callsign appears as a substring, word boundaries ignored | 106 (48 %) |
| by RBN SNR: < 15 dB / 15–25 / ≥ 25 dB, as-word | 17 % / 26 % / 30 % |
| reported WPM ÷ RBN WPM, median | **0.82** |

Representative outputs (RBN WPM in parentheses):

```
NJ3K (35):  T GQTESTNJ3K CQTESTNJ3K CQTESTNJ3K CQTESTNJ3K …     reported 26 WPM
K0RF (30):  K0RF K0RF TESE K0RFK0RFTEST K0RFKYRFTEST T0RFK0RFTEST reported 23 WPM
IO3F (31):  STIO3F IO3F TEST TEST IO3F IO3F TEST TESTIO3FIO3FTEST reported 26 WPM
HA3OU (35): 1TMNOMOT J M0 O O Q M 0O O J M0 O O T T O T … HA3OA AT3OU
E7DX (39):  TAT EBDX TRTE7MNE YLDK Y TWEZX 31E7NX NATE,M 6TLDX E E
```

The first three are 30–43 dB deep and perfectly readable by eye on the
envelope; the decoder's only failures are word boundaries and speed.
The last two (≥ 35 WPM) have lost inter-element gaps.

### 1.3 Threshold placement (the mechanism)

Same 12 windows, static rails from the 90th/10th amplitude percentiles,
manta's 12 ms debounce applied, run lengths in *true* dit units using the
RBN speed:

| threshold, relative to the mark rail | dit | dah | element gap |
|---|---|---|---|
| geometric mean of rails (SPEC §3.2) — lands at **−17 … −30 dB** | 1.44 | 3.48 | 0.64 |
| −10 dB | 1.15 | 3.13 | 0.87 |
| **−6 dB (half amplitude)** | **1.04** | **3.03** | **0.94** |
| −3 dB | 0.94 | 2.95 | 1.07 |

At −6 dB the mark histograms collapse to two clean modes (e.g. NJ3K:
90 dits / 104 dahs / 0 elsewhere; at the geometric mean the same marks
spread over four bins). The RBN speeds are confirmed correct.

Why −6 dB is the physically right place: for a linear-phase channel
filter the step response crosses 50 % amplitude at the true keying
instant. SPEC §1.2's prototype (Kaiser β = 7.857, cutoff 46.875 Hz) goes
from −30 dB to −6 dB in **6.1 ms** and 10 %→90 % in **9.7 ms**; a
threshold 20–30 dB down therefore starts each mark ≈ 5–6 ms early and ends
it ≈ 5–6 ms late before any transmitter edge shaping is added. Real rigs
add 3–8 ms raised-cosine edges. The bias is a constant in milliseconds,
so its damage scales with speed — invisible at 12 WPM (dit 100 ms),
fatal at 40 WPM (dit 30 ms).

### 1.4 Why the synthetic vectors did not show it

| factor | golden vectors (SPEC §7) | B2 / K5TR truth |
|---|---|---|
| speed | 12–35 WPM, most 15–25 | median 33, 40 % ≥ 35 |
| keying depth in channel | 17–34 dB (SNR +3…+20 dB in 2500 Hz + 14.3 dB) | 24–60 dB |
| threshold offset from mark | −9 … −17 dB | −17 … −30 dB |
| text | `CQ CQ DE CALL CALL K`, 7-dit word gaps | `CQ TEST CALL`, `TEST CALL CALL TEST`, tighter |
| gap-threshold hack | `CHAR_GAP_DITS = 1.6` absorbs the bias in this range | bias exceeds what 1.6 can absorb |
| co-channel / adjacent QRM | none (V7: two signals 150 Hz apart, both clean) | routine |
| transmitter edges, weighting, chirp | 5 ms raised cosine, 1:3 exact | varied |

The `1.6` deviation recorded in
`docs/DECISIONS/2026-07-18-char-gap-threshold-fix.md` was the first
sighting of this bias ("hysteresis+debounce systematically inflates
measured μ_dit … ~15–20 ms"); it was compensated at the gap classifier
rather than removed at the source.

### 1.5 Prototype of the recommended decoder

A ~150-line numpy prototype of Approach B (hop-level hidden semi-Markov
Viterbi over half-amplitude-normalized evidence, speed chosen by total
path likelihood over a 9-value grid, offline level tracking, no tuning
beyond two constants) was run on the same envelopes. On the first 12
windows it recovered the callsign as a word on 5 and with framing on 3
(the legacy core: 4 and 1), and turned `CQTESTNJ3K ×7` into
`CQ TEST NJ3K ×7` at the correct speed. Its remaining failures are speed
selection under QRM blips (it picked 50 WPM for three 37–39 WPM
signals), which the token-passing design in §4 handles by carrying speed
as per-token state instead of a global choice. The full 221-window
result is in §1.6; the prototype is a feasibility probe, not a reference
implementation.

### 1.6 Prototype batch result (honest: a tie)

The same prototype over all 221 windows, no per-window tuning:

| n = 221 | legacy core (oracle) | HSMM prototype |
|---|---|---|
| callsign as a word | 27 % | 28 % |
| with framing | 13 % | 14 % |
| substring, boundaries ignored | 48 % | 30 % |
| reported WPM ÷ RBN WPM (median) | 0.82 | 1.32 |
| as-word by SNR < 15 / 15–25 / ≥ 25 dB | 17 / 26 / 30 % | 7 / 27 / 34 % |

Read: where the prototype picks the right speed it fixes exactly what
§1.3 predicts (word boundaries, WPM, clean deep signals); where its
global likelihood-over-a-speed-grid choice goes fast (a 39 WPM signal
read at 50 WPM, i.e. dits split in two), it loses even the substring
matches the legacy core keeps. A Bayesian prior on speed from the RBN
speed histogram changed nothing (the fast hypothesis wins by more than
the prior). So tonight's prototype is evidence that the *mechanism* works
and that *speed acquisition is the design's critical path* — which is
why §5.4 carries speed per token, updated per element and merged by
speed bin, rather than choosing one speed per window. The stage-2 gate
in §6 is deliberately empirical; the design doc does not claim the HSMM
wins until the harness says so.

### 1.7 Tonight's end-to-end run (for the record)

`manta decode --json` on B2 with the working-tree detector experiments of
2026-09-09 (16 min 34 s wall): 226 spots.

| reference | TP | FP | precision | recall |
|---|---|---|---|---|
| all RBN spotters (1,451 bins) | 68 | 158 | 30.1 % | 4.7 % |
| **K5TR only (210 bins)** | 55 | 171 | 24.3 % | **26.2 %** |

All 155 false-positive callsigns are `Beacon`-typed (the single-decode
exemption); 30 are one edit from a real call in the truth set, 71 two
edits, 3 truncations. Median confidence TP 0.35 vs FP 0.15; median SNR TP
8.2 dB vs FP −0.1 dB. (Validator observations, §7.)

---

## 2. Requirements for the redesign

Hard (inherited):

- R1 Determinism (SPEC §6): byte-identical output for like-for-like
  builds; no RNG, no wall clock, fixed iteration and tie-break order.
- R2 CPU: the decode stage for 300 tracks must fit in the SPEC §4 budget
  class (tens of MFLOP/s, not hundreds) so the Pi 4 story stays alive.
- R3 Output contract: `DecoderEvent` (SPEC §5) stays valid for existing
  consumers; additions are additive.
- R4 Latency: a character must be committed no later than a bounded
  lookahead after its closing gap (spots must not lag the transmission,
  SPEC §4.2's 7-dit flush rule is the current bound).

Design goals (new):

- G1 Timing fidelity independent of keying depth and speed: measured dit
  within ±10 % of truth from 12 to 45 WPM at 20–60 dB depth.
- G2 No irreversible per-hop or per-run decision before the character
  boundary; ambiguity is carried as probability to where the grammar can
  resolve it.
- G3 Speed acquired within one character and tracked through QRQ/QRS
  without a reinitialization cliff.
- G4 Fading: threshold follows the mark level within a few dits, both
  directions (MAN-107).
- G5 Robustness to dense-band noise: the space-level estimate must not
  be dragged up by the track's own keying, nor spike on neighbor keying
  bursts.
- G6 Calibrated confidence and alternatives per character, so
  `manta-spot` can prefer `NJ3K` over `NJ3N` using SCP/cty instead of
  receiving one hard string (MAN-106).
- G7 Measured on real signal: every stage gated on the K5TR-only
  benchmark and the decode-oracle harness, not only on V1–V10.

---

## 3. What respected decoders do differently (narrative survey)

Distilled from the 2026-09-09 research report (sources listed there;
nothing below is code-derived):

- **CW Skimmer** (VE3NEA) — documented publicly only at the level of
  philosophy and the SNR pipeline: signals are extracted with a **50 Hz**
  filter (half manta's channel width); decisions are explicitly
  *probabilistic* ("compute the probability that the signal is present"
  rather than a hard per-sample decision); signal power is modeled with a
  **Rayleigh-fading envelope**; the noise floor is estimated **globally
  across the whole passband** and scaled to 500 Hz; SNR is averaged over
  ~45 s of key-down samples. No state-space details are published.
- **Classical HMM / correlator lineage** (Gold 1959 "MAUDE"; Blair 1970;
  the 1970s Surrey thesis; AG1LE's 2013 Bayesian decoder): hypothesize
  whole element sequences (dit, dah, three gap types, pause) and pick the
  maximum-likelihood path; speed is a continuously tracked hidden
  variable; element-duration statistics and code structure are priors.
  Reported knee around +6 dB in 2 kHz for the hobbyist reimplementation.
- **RSCW** (PA3FWM): matched filter whose window equals one estimated bit
  period; threshold set by a balance criterion, not a fixed ratio; no
  independent dit/dah classification at all — the received bit pattern is
  correlated against every codeword; the bit clock is recovered from the
  statistical asymmetry of Morse timing.
- **fldigi**: legacy threshold decoder at ≈ +1 dB reliable floor; adding
  a matched filter plus a self-organizing-map element classifier moved
  the floor to ≈ −3 dB (~4 dB gain, mostly the matched filter).
- **Assistive-tech literature (AVRTP)**: two LMS predictors track the dot
  interval and the dot/dash difference and adapt the decision *ratio*, not
  just the reference length — the principled version of manta's 2-means.
- **Noise-floor estimation in dense bands**: Martin's minimum-statistics
  tracker (track the minimum of smoothed subband power over a sliding
  window, then bias-correct) and truncated order-statistics filters are
  the standard tools when signal is present most of the time; they need
  no "signal absent" detector.
- **Learned decoders** (CNN/LSTM/CTC; a YOLO-style dit/dah detector on
  the time-frequency image) report the highest accuracies in papers, at
  the cost of labeled data, non-trivial inference cost, and opaque
  failure modes.

Three things every good decoder in this list shares and manta's chain
lacks: (i) integration over an element's length before deciding, (ii)
probability carried forward instead of a hard key state, (iii) speed as
a tracked hypothesis rather than a cluster assignment.

---

## 4. Approaches

### 4.1 Approach A — fix the front end, keep the chain

Keep envelope → runs → 2-means → character-local beam, but:

- A1 **Edge-centered threshold**: track a mark level `M_t` (decaying
  peak-hold, decay scaled to the tracked dit) and a space level `N_t`
  (minimum statistics); decide key-down/up at `0.5·(M_t − N_t) + N_t`
  with a small hysteresis band in *normalized* amplitude instead of
  `sqrt(E_hi·E_lo)` and ±1.9 dB. This alone removes the constant-ms
  timing bias (§1.3) and lets `CHAR_GAP_DITS` return to 2.0.
- A2 **Dit-scale integration before the decision** (MAN-110): a boxcar
  of ≈ 0.5 dit on the normalized amplitude before thresholding.
- A3 **Soft gap scoring** (MAN-111): the beam scores inter-element vs
  inter-character gaps log-normally like marks, and a word boundary is a
  scored hypothesis with a lookahead of one character.
- A4 **Speed hypotheses** (MAN-113): run two or three trackers seeded at
  different speeds for the first two characters, keep the one whose
  beam scores best.
- A5 **Spectral noise reference** for `N_t` (§4.4.2), shared with B.

Pros: smallest change; each item is measurable in isolation; the SPEC
diff is a handful of sections. Cons: still four sequential hard decisions
(A1–A2 make them *better*, not reversible); A3 and A4 bolt hypothesis
tracking onto a pipeline whose data structures were built for one path,
and their interaction (speed hypotheses × gap hypotheses) is exactly what
a trellis does natively. Expected: word boundaries and WPM fixed (the
27 % → likely 45–55 % as-word range by the §1.3 evidence), speed cliffs
and QRM-blip robustness only partly addressed.

### 4.2 Approach B — probabilistic sequence decoder (recommended)

Replace runs/2-means/gap-classifier/beam with one hidden semi-Markov
model decoded by token-passing Viterbi over per-hop evidence:

- **Evidence** (shared with A): `u_t = (a_t − N_t) / (M_t − N_t)`,
  clipped, and a per-hop log-likelihood ratio mark:space that is
  symmetric about `u = 0.5`. Summed over a hypothesized element interval
  this *is* a matched filter of that element's length (MAN-110), and its
  zero crossing is the half-amplitude edge (§1.3).
- **Hidden state** = (Morse-tree node, phase ∈ {after-mark, after-space},
  speed `u` in hops/dit as a per-token continuous value). Segments:
  mark ∈ {dit (1u), dah (3u)}, space ∈ {element (1u), character (3u),
  word (7u), silence (≥ 10u)}, each with a log-normal duration prior
  about its nominal length and a type prior from Morse text statistics.
- **Decoding**: segments start and end only at *anchors* — hops where
  the evidence crosses zero (a half-amplitude edge) or every 8th hop as a
  fallback — so durations are exact edge-to-edge distances and there is
  no duration grid. At each anchor, tokens from earlier anchors ending a
  segment here are scored (prefix-summed evidence + duration prior + type
  prior + tree transition), the best token per (node, phase, speed-bin)
  survives, and the live set is pruned to a small beam. Speed is updated per
  committed element (`u ← u + α(d/k − u)`) so tokens at different speeds
  compete on likelihood — MAN-113 without a reinitialization rule.
  Characters and word boundaries are committed by partial traceback
  when all live tokens agree on the prefix, or at a bounded lookahead
  (R4). The character-local beam becomes a whole-transmission beam that
  is still cheap because the state is a tree node, not a string.
- **Outputs**: `CharDecoded` as today, plus additive `alternatives`
  (top-2 competing glyphs with posterior mass) and a `WordBoundary`
  posterior; per-character confidence is the log-odds margin between the
  committed glyph and its best competitor at that position, scaled by
  channel quality `q` exactly as SPEC §4.5 does today.

Pros: one model replaces four thresholds; every element-level decision is
made with the whole character's (and the neighboring gaps') evidence;
fading, integration, gap scoring, and speed hypotheses fall out of the
same mechanism; the lattice output gives `manta-spot` what MAN-106 asks
for. It is the design every decoder in §3 that works well converges on.
Cons: a new core (~1,000 lines); duration priors and beam width need
tuning against real signal (but the stage-0 harness makes that a
half-hour loop, not a guess); CPU must be engineered (§4.6); determinism
needs the tie-break rules written before the code (§4.7).

### 4.3 Approach C — learned decoder

A small CTC model over the channel log-power patch (or B's normalized
evidence), trained on testkit synthesis plus B2-style real windows weakly
labeled by K5TR/RBN spots; fused with the classical output as
ARCHITECTURE §5 step 5 describes.

Assessment: highest ceiling on paper, but (i) the project has decided
(D8, 2026-09-06) that classical fixes land first and that an ML stage
trained on the current front end would inherit its defects — §1.3 is
that defect, so C before B would learn to undo a timing bias we can
simply remove; (ii) labels: RBN gives call-level, not time-aligned,
truth — usable for validation, weak for training; (iii) determinism and
Pi-class CPU are open problems for inference. **Defer**; B's per-hop
evidence and lattice are the right substrate for C later, and the
stage-0 harness is the dataset builder C will need.

### 4.4 Trade-off summary

| | A | **B** | C |
|---|---|---|---|
| fixes §1.3 timing bias | yes | yes | learns around it |
| reversible element decisions | no | yes | yes |
| speed as hypothesis | bolt-on | native | implicit |
| fading (MAN-107) | A1 | same front end | needs training data |
| alternatives/posteriors for `manta-spot` | no | yes | yes |
| CPU for 300 tracks | ≈ today | 5–15 M op/s with pruning (§4.6) | 100s of MFLOP/s |
| determinism | trivial | rules in §4.7 | hard |
| new code | ~300 lines | ~1,000 lines | model + runtime + data |
| risk | low | medium | high |
| target K5TR-only decode-oracle as-word (from 27 %); targets, not measurements — §1.6's untuned prototype ties the legacy core | 45–55 % (§1.3 evidence) | 60–75 % (gate, unproven) | unknown |

**Recommendation: B, built as three stages, with stage 1 shipped behind
the legacy chain first.** Stage 1 is Approach A's A1+A5 (the evidence
front end) and is required by B anyway; it carries most of the measured
timing fix and is low-risk. Stage 2 is the HSMM decoder behind
`decode.engine = "hsmm"` (stage 1 ships as `"edge-legacy"`), gated on beating legacy on V1–V10 *and* the
K5TR-only benchmark. Stage 3 is the lattice output and the spot-side
consumer hooks (interface only on the spot side).

---

## 5. Recommended architecture (B) — component view

```
channel sample (complex, 375 Hz)          neighbor channel powers, block floor
        │                                          │
        ▼                                          ▼
 [stage 1a] evidence front end  ◄─────── [stage 1b] track-local noise reference
   M_t (mark level, peak-hold)             N_t = max(min-statistics(own),
   N_t (space level)                                 β · guard-banded neighbor min)
   u_t = (a_t − N_t)/(M_t − N_t)
   llr_t = mark:space log-likelihood ratio (symmetric about u = 0.5)
        │
        ▼
 [stage 2] HSMM token-passing decoder
   tokens: (tree node, phase, speed u, score, history)
   segments: dit/dah/egap/cgap/wgap/silence with duration priors
   partial-traceback commit → CharDecoded / WordBoundary (+ alternatives)
        │
        ▼
 DecoderEvent stream (SPEC §5, additive fields)  ──►  manta-spot (unchanged; may consume alternatives later)
```

### 5.1 Stage 1a — evidence front end (`manta-decode::evidence`)

Replaces SPEC §3.1–§3.3 (`Demod`) as the thing that turns amplitude into
decisions, but its *output* is evidence per hop, not runs.

- Mark level `M_t`: the maximum of the 8 ms-smoothed amplitude over a
  *centered* window of ±`hold_dits` (default 4) tracked dits (initial dit
  40 ms until speed is known). Symmetric, so it follows QSB in both
  directions with no rise/decay asymmetry to tune, and a maximum (not a
  mean) cannot be dragged by a long dah (G4). The evidence stream runs
  `hold_dits` dits behind the input; the decoder already needs that much
  lookahead to commit.
- Space level `N_t`: minimum statistics over a 1.5 s sliding window of
  the 40 ms-smoothed power, bias-corrected by a fixed factor derived from
  the chi-square(2) statistics of `|X|²` (≈ +2.5 dB for this window/
  smoothing), combined with the spectral reference from 1b.
- Normalized amplitude `u_t = clip((a_t − N_t)/(M_t − N_t), −0.5, 1.5)`.
- Per-hop evidence: `llr_t = (u_t − 0.5)/σ_u²` (the difference of two
  equal-variance Gaussian log-likelihoods centered at 1 and 0),
  clipped to ±L to bound the influence of any one hop (impulse QRM).
  `σ_u` default 0.30; `L` default 10.
- Pre-decode gating stays: no tokens are spawned until `M_t/N_t ≥ 2`
  (6 dB apparent keying depth), as SPEC §3.2 requires.

This is where the §1.3 fix lives: the decision surface is at half
amplitude *by construction*, at every keying depth.

### 5.2 Stage 1b — track-local noise reference (`manta-dsp::floor`, additive)

SPEC §2.1's 25th-percentile-over-10 s channel floor is a *detection*
floor; it is too slow and too coarse to be the decoder's space level.
Add, per hop and per track, a spectral reference: the minimum of the
smoothed power over guard-banded neighbor channels `k±2 … k±4` (guard
`k±1`, which the track may itself occupy), bias-corrected for the
minimum of six chi-square samples. `N_t = max(N_temporal, β·N_spectral)`
with `β` default 0.5. Rationale: a keying burst or click from a strong
neighbor that leaks into channel `k` also leaks into `k±2`, so the
reference rises with it and the evidence is discounted for exactly those
hops (order-statistic CFAR, the dense-target standard in radar); a
neighbor that is itself a clean CW signal contributes only when it is
the minimum, which it rarely is with six cells. Cost: six reads and a
min per hop per track.

### 5.3 Stage 1c — optional per-track narrowband refinement (`manta-dsp::refine`)

CW Skimmer's 50 Hz extraction filter is half manta's channel width. For a
track with centroid offset `δ` channels (SPEC §1.4), take the complex
sample of the nearest channel, rotate by `e^{−j2π·δ·Δ·t}`, and apply a
short second-stage lowpass (±30 Hz, 11 taps at 375 Hz); use its magnitude
as `a_t`. Gains: −3 dB noise, 20+ dB more rejection of a neighbor 60–150
Hz away, no per-hop switching between "max-power owned channels" (which
splices two stations' keying when they share the owned set). Cost: 11
complex MACs per hop per track. This is the one item that touches
`manta-engine`'s track feed (a one-line call site) and it interacts with
the tracker work in flight; it is sequenced last and gated separately.
At 45 WPM (dit 27 ms) a ±30 Hz filter's 10–90 % rise is ≈ 12 ms — still
inside G1; the bandwidth is a config key, not a constant.

### 5.4 Stage 2 — HSMM token-passing decoder (`manta-decode::hsmm`)

State and grammar:

- Tree: SPEC §4.4's node table (glyphs and prosigns), root = 0.
- Token: `{node, phase, u_hops: f32, score: f32, hist: small ring of
  committed glyph indices, last_seg_end: hop}`.
- Segment types and nominal lengths in dits: dit 1, dah 3, egap 1, cgap 3,
  wgap 7, silence ≥ 10 (flat prior beyond 10, evaluated on a coarse
  duration grid).
- Duration prior: log-normal, `σ_d` default 0.22 (≈ ±25 % at 1σ; human
  keying jitter 10–20 % plus edge rounding), valid for 0.6×–1.5× nominal;
  durations are exact anchor-to-anchor distances (no grid).
- Type priors (log): dit/dah from Morse-text statistics (≈ 0.6/0.4)
  *plus a per-mark insertion penalty* (default −1.5) that stops noise
  blips from being explained as `E`s; egap/cgap/wgap ≈ 0.62/0.28/0.10.
- Transition: after-space + mark(dit|dah) → child node; after-mark +
  egap → same node; after-mark + cgap|wgap|silence → root, *emit glyph*
  (only from glyph-bearing nodes; a glyphless node can only continue).

Per-hop algorithm (deterministic order):

1. For each earlier anchor `s` in range and each token alive there in
   after-space phase, and each mark type with `d = t − s`: candidate
   score = token score + Σ llr over the segment (prefix sums, O(1)) +
   duration prior + type prior; new token at the child node with `u`
   updated by `u ← u + α_u·(d/k − u)`, `α_u` default 0.2.
2. For each live token in after-mark phase and each space type and
   duration: as above with `−Σ llr`; egap keeps the node, cgap/wgap/
   silence emit the glyph into `hist` and return to root.
3. Merge: tokens with identical (node, phase, round(u to 0.5 hop)) keep
   the best score; prune to `beam` (default 12) by score.
4. Anchors: steps 1–3 run only at anchors (zero crossings of `llr`, or
   every `fallback_hops` = 8 hops) — segment ends are edges; this cuts
   the work ~5× versus evaluating every hop.
5. Commit: when every live token shares the same first `k` entries of
   `hist`, emit them (`CharDecoded`, `WordBoundary`) and drop them from
   all rings; also force-commit any entry older than `lookahead_dits`
   (default 25 ≈ two characters plus a word gap) from the best token.
6. Seeding: when a track starts (or after a silence segment) tokens are
   seeded at root with `u ∈ {9, 13, 18, 26, 38} hops` (≈ 50, 35, 25,
   17, 12 WPM); the first character's evidence prunes them. This is the
   replacement for SPEC §4.1's 5-mark initialization and its
   drift/regime rule.

Confidence and alternatives (G6): at commit, the committed glyph's
confidence is `sigmoid((s_best − s_alt)/κ)·q`, where `s_alt` is the best
score among live tokens whose history differs at that position (or a
fixed floor if none survived), `κ` default 6, and `q` is SPEC §4.5's
channel-quality factor. Up to two alternatives with their `sigmoid`
mass are attached as an additive `alternatives` field. A `WordBoundary`
carries the same margin between the wgap and cgap interpretations.

### 5.5 Output and interfaces

- `DecoderEvent::CharDecoded { …, alternatives: Vec<(Glyph, f32)> }` and
  `DecoderEvent::WordBoundary { …, confidence: f32 }` — additive fields
  with `#[serde(default)]` so the JSON and `manta-spot` contracts are
  unchanged for consumers that ignore them (R3; the wire schema in
  `dispensa` is the spot, not the event, and is untouched).
- `SpeedUpdate` comes from the best token's `u`; `TrackMeta` unchanged.
- `manta-spot` may later rescore a candidate word over the alternatives
  lattice (SCP/cty membership as the language model). That is a
  separate design; this one only guarantees the data is there.

### 5.6 CPU budget

Per hop per track, stage 2 with beam 12, 5 types, ≤ 9 durations, tree
transition O(1): ≈ 12 × 5 × 9 ≈ 540 candidate evaluations of a few
flops each ≈ 3 k op; edge-triggering brings the average to ≈ 0.6 k op.
At 375 hops/s × 300 tracks: **≈ 70 M op/s worst case, ≈ 15 M op/s
typical**, i.e. the same order as the PFB FIR (SPEC §4: 12 MFLOP/s) and
well under the FFT. Stage 1 adds < 30 op per hop per track. Stage 1c adds
11 CMACs. Criterion benches gate all three (plan). If the Pi 4 leg needs
it, `beam = 6` and `E = 1` halve the cost with a measured accuracy cost.

### 5.7 Determinism (R1)

- No RNG, no wall clock; all timers in hops.
- Tokens are stored in a `Vec` and processed in ascending (node, phase,
  u-bin) order; pruning sorts by (score desc, glyph-history lexical, u
  asc) with `total_cmp`.
- Prefix sums of `llr` accumulate in `f64` sequentially; all other state
  `f32` with fixed operation order; no FMA-dependent reductions.
- Commit is a pure function of the token set; pool scheduling cannot
  affect it (SPEC §6.6 unchanged).
- Cross-platform equality asserted at the decoded-text level as today.

### 5.8 Configuration (additive keys, defaults above)

The normative key list, names, and defaults are `docs/SPEC-decode-core-v2.md`
§7 (`engine = "legacy" | "edge-legacy" | "hsmm"`, `sigma_u`, `llr_clip`,
`hold_dits`, `fallback_hops`, `noise_window_ms`, `noise_min_bias_db`,
`spectral_min_bias_db`, `spectral_beta`, `refine_bw_hz`, `dur_sigma`,
`mark_insert_penalty`, `beam`, `lookahead_dits`, `speed_alpha`,
`seed_units_hops`, `conf_kappa`). `engine` keeps the legacy chain
selectable until the HSMM engine wins the gates.

---

## 6. Validation and gates

Stage 0 (harness, first): promote the throwaway scripts into
`manta-testkit`:

- `oracle` — for a recording + RBN CSV + spotter filter, channelize the
  window around each spot with `manta-dsp`'s real channelizer, feed the
  own-channel stream to `TrackDecoder`, and report: callsign-as-word,
  framed, substring, WPM ratio, per-SNR buckets. This isolates the decode
  core from the tracker and runs in ~1 min for the 221 K5TR windows.
- `scripts/score-against-rbn.py --spotter K5TR` (and a `--min-spotters`
  option) so the K5TR-only number is first-class.
- Testkit realism: add contest-speed vectors (35, 40, 45 WPM), keying
  depth to 60 dB in channel, hard-keyed edges (1–2 ms) with click
  sidebands, a co-channel second station at −10 dB, weighting 1:2.6 and
  1:3.4, and `CQ TEST <call> <call> TEST` text with 5-dit word gaps.
  These are the regressions that would have caught §1.3.

Gates:

| stage | gate |
|---|---|
| 1 (front end, behind legacy) | oracle as-word ≥ 45 % on K5TR windows; WPM ratio 0.95–1.05; V1–V10 no regression; `CHAR_GAP_DITS` restored to 2.0 with V2 passing |
| 2 (hsmm engine) | oracle as-word ≥ 60 % and framed ≥ 40 %; end-to-end K5TR-only recall ≥ 50 % at precision ≥ 60 % with the current validator; V1–V10 pass including V5/V6/V8w at their stated bars; determinism CI (3 runs, identical SHA-256); criterion bench within budget |
| 3 (lattice + hooks) | MAN-106's separation test: median confidence of true spots vs bogus differs by ≥ 0.3; no consumer breakage on the JSON stream |

Numbers are targets to be confirmed by the stage-0 harness, not
promises; the harness runs in minutes, so tuning is empirical.

---

## 7. Findings outside this design's scope (handed to their owners)

- **Tracker / channel selection**: co-channel occupancy within one
  93.75 Hz channel was observed in 2 of 12 inspected windows; "max-power
  owned channel" splices signals when two stations share the owned set.
  Stage 1c is the decode-side mitigation; the tracker-side question
  (narrower channels, or one track per sub-channel) belongs with the
  `merge_converged` work.
- **Validator**: every one of tonight's 155 false-positive spots is
  `Beacon`-typed (the `<call> T` / `V V V` single-decode exemption) —
  on real contest audio a stray dah after a garbled word satisfies the
  power-step pattern. Confidence (TP 0.35 vs FP 0.15) and SNR (8 dB vs
  0 dB) already separate them. Not changed here; MAN-105/MAN-106
  territory.
- **Benchmark**: report K5TR-only recall as the parity metric; keep the
  all-RBN number for coverage. 55 of tonight's 68 all-RBN true positives
  are K5TR bins; the other 13 are stations K5TR's skimmer did not spot but
  other North American skimmers did — manta hears roughly what K5TR's
  antenna hears.

---

## 8. Decisions (2026-09-09, Tony)

1. **`decode.engine` stays a runtime switch permanently.** The legacy
   chain is not removed once stage 2 passes its gate — it stays
   selectable (`legacy` / `edge-legacy` / `hsmm`) for live-node A/B and
   as a fallback. No change to §5.8's config or the plan's Task 5/9.
2. **Stage 1c (the narrowband refiner) is deferred, not designed
   jointly.** `manta-dsp::refine` is built now (Plan Task 13); the
   `manta-engine` track-feed call site is **not** touched here — it is
   filed as MAN-168, to be done once the parallel tracker session's
   `merge_converged`/ownership work lands in `track.rs`. This avoids a
   concurrent edit to a file this design is deliberately not touching.
3. **`alternatives` stays internal for now.** No dispensa wire-contract
   change and no `manta-spot` consumer in this plan; `DecoderEvent`
   carries the field (Plan Task 2) but nothing reads it yet. Filed as
   MAN-167 to revisit once the HSMM engine clears its stage-2 gate and a
   real rescoring design exists.
4. **VR1–VR8 are promoted to permanent ROADMAP.md M2 acceptance gates**,
   not a plan-scoped, one-time check — see `ROADMAP.md`'s M2 section
   (updated in this same PR) and SPEC v2 §8.4. Every future decode-core
   change must keep clearing them, the same standing as V1–V10.
