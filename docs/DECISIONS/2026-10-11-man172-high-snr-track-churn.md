# MAN-172: high-SNR track churn

## Decision

A rising, unowned channel may start a spawn decision only at a local maximum of
`Gate::smoothed_db`, including ties and circular neighbours. Equal-SNR
merges keep the older track, identified by the lower `track_id`.
The detector thresholds, ownership width, merge distance and both decoder
implementations are unchanged.

The existing same-hop pairing branch is preserved, as the plan requires.
It chooses between adjacent rises by instantaneous power, so it can select
`k+1` even if only `k` is a smoothed-power peak. The plan's claim that this
branch matters only on exact smoothed-power ties was incorrect: the two
comparisons use different power estimates. The normative wording describes
the outer scan's eligibility and the pairing exception separately. Changing
that arbitration is outside the measured detector fix adopted here.

## Mechanism and evidence

The 2026-10-11 MAN-172 plan measured the baseline at
`d3bfa7c0c47a8b6527985955d446147718ff20cd`. Its scratch instrumentation
located PFB leakage 1.4-1.6 channel spacings from a strong signal. This is
outside the incumbent's three owned channels. Above about +24 dB SNR in
2500 Hz, a skirt channel can cross the 12 dB detector rise threshold.
Its candidate then selects the real tone from its own owned window,
promotes and drifts towards the incumbent.

Both tracks eventually read the same max-power channel and have identical
`current_snr_db`. The old `<=` comparison discarded the lower, older id
on every such tie. That repeatedly replaced an established decoder with
a cold decoder, under legacy as well as HSMM. The batch summary selected
the first track's stub, explaining its empty text and seed-like speed.

Excerpt from the plan's VR1 legacy trace (hop count, channel and SNR):

```text
SPAWN hop=751 id=1 ch=132 prof=[-2:8.7 -1:29.8 +0:38.5 +1:8.5 ...]
SPAWN hop=878 id=2 ch=130 prof=[... +0:12.3/- +1:32.3/1 +2:36.8/1 ...]
MERGE hop=942 a=1@131.72(snr36.97) b=2@130.73(snr36.97) loser=1 survivor=2
SPAWN hop=943 id=3 ch=133 prof=[... -1:37.5/2 +0:17.5/- ...]
MERGE hop=1079 a=2@131.19(snr17.81) b=5@132.19(snr17.81) loser=2 survivor=5
```

The tone is at channel 131.63. Channels 130 and 133 are falling skirts,
not independent peaks. The plan counted 368 births and 284 merges in this
trace. The following baseline and experiment tables are retained from
that plan; they are planning measurements, not new implementation runs.

| vector (SNR₂₅₀₀) | engine | TrackPromoted | TrackClosed | CharDecoded | SpeedUpdate | top-level `text` | `wpm` | `freq_hz` |
|---|---|---|---|---|---|---|---|---|
| VR1 (+30) | hsmm | 286 | 258 (257 merges) | 149 | 5095 | `""` | 12.16 | 14000000.0 (fallback) |
| VR1 (+30) | legacy | **286** | 3 | 12 | 6 | `"A TES"` | 42.2 | 14012353.2 |
| VR2 (+30) | hsmm | 286 | 274 | 191 | 5192 | `""` | 47.0 | 14000000.0 |
| VR3 (+45) | hsmm | 132 | 119 | 1300 | 12297 | `""` | 31.1 | 14000000.0 |
| VR4 (+25) | hsmm | 85 | 84 | 939 | 9800 | 1 char | 33.9 | — |
| V1 (+20), VR5–VR8 (≤ +25 co-channel / ≤ +20) | both | 1 | 1 | normal | — | normal | — | — |

HSMM makes more of the churn visible as `TrackClosed` because its early
`SpeedUpdate` sets `has_emitted`. The detector's promotion counts are
engine-independent.

## Planning measurements

A is the local-peak spawn rule. B is the incumbent-wins merge tie.

| vector | engine | baseline promoted / text len | (A) peak rule only | (B) tie rule only | (A)+(B) |
|---|---|---|---|---|---|
| VR1 | hsmm | 286 / 0 | **1 / 498** | 314 / 142 | 1 / 498 |
| VR1 | legacy | 286 / 5 | **1 / 496** | 314 / 140 | 1 / 496 |
| VR2 | hsmm | 286 / 0 | **1 / 570** | 323 / 567 | 1 / 570 |
| VR3 | hsmm | 132 / 0 | **1 / 349** | 59 / 49 | 1 / 349 |
| VR4 | hsmm | 85 / 1 | **1 / 355** | 146 / 110 | 1 / 355 |
| V1, VR6a, VR6b, VR8 | both | 1 | **byte-identical JSON** | identical | identical |
| VR5, VR7 | both | 1 | 1. Same `text` and `spots`; only the promotion-time `freq_hz` (birth channel moved to the peak) and a few early `TrackMeta`/`CharDecoded` values differ | identical | same as (A) |

V1's existing HSMM startup CER gap is unrelated: it already holds one
track. VR4's collapse, however, is the same churn mechanism as VR1-VR3;
the stage-2 gate's original element-split/merge attribution for VR4 was
incorrect. This does not reclassify V10's word-gap failure.

## Alternatives rejected

- The tie rule alone left 314 promotions on VR1 under HSMM and duplicate
  decodes. Preventing skirt births is necessary.
- Widening ownership or adding a fixed two-channel guard is SNR-dependent:
  VR3 at +45 dB spawns three channels out. A guard would also tighten the
  measured roughly 2.5-channel separation floor for independent signals.
- Raising `on_snr_db` loses weak-signal sensitivity, while leakage grows
  with signal strength.
- Promotion-time absorption misses births on more distant skirts.

## Test fixtures

`fast_w1aw` deliberately disables warmup so an operator-list offer can
be tested during the calibration read. The uninitialised -140 dB floor
creates hundreds of noise tracks. Their layout changes with the spawn
rule and can emit unrelated garbage such as `AEE7EER`. The fixture now
retains only W1AW spots before checking list timing; its warmup setting
and timing assertions are unchanged.

The new 30-second VR1/VR3 integration test checks both engines, one
promotion, no merge closure, non-empty text and frequency within half a
channel. The plan's 25 Hz bound did not hold on the shortened VR1 fixture:
its final estimate was 14012369.490777716 Hz, 29.49 Hz from the tone.
A short cut can stop during a centroid transient. Full 120-second vectors
retain the separate 25 Hz measurement check. No frequency estimator was
changed for this ticket.

## Residual HSMM speed reporting

After churn is removed, VR1 and VR2 pass their character gates, and VR1
passes its word-boundary gate. Their full tests remain ignored because
reported WPM still misses the gate: 36.10 versus 40 +/- 2, and 41.24
versus 45 +/- 3. The plan measured this distribution over the second
half of the file:

| vector | true | `SpeedUpdate` median | p10–p90 | last (reported) |
|---|---|---|---|---|
| VR1 hsmm | 40 | 40.48 | 37.2–43.6 | 36.10 |
| VR2 hsmm | 45 | 45.43 | 42.1–48.7 | 41.24 |
| VR1 legacy | 40 | 40.61 | 39.5–40.8 | 40.77 |

The plan tried three reporting-only smoothing experiments:

1. Per-committed-character EMA, alpha 0.125: VR1 40.82, VR2 43.06,
   VR3 29.81; SpeedUpdate counts fell from about 5,000 to 3-38. But
   `hsmm_speed_converges_from_seeds` regressed to 37.17 against 34.6 +/- 2,
   because its correct value comes from the converged `finish()` flush.
2. Apply smoothing only to the live path and leave `finish()` raw: the
   unit test passes, but the VR1/VR2 final flush restores 36.1 / 41.2.
3. Median-of-5/9 likewise failed to satisfy both the converged flush and
   the mid-transmission report. None of these estimators is adopted.

Even a clean 34.6 WPM rectangle gave per-commit samples from 31.8 to 40.4.
The roughly 5,000 SpeedUpdate events per 120 seconds are a separate
reporting problem. Neither that problem nor MAN-171's missing Silence
anchor stride explains engine-independent track churn.

## Implementation verification (2026-10-11)

`cargo build --release -p manta-cli --bin manta`, followed by `manta gen`
and `manta decode --json --engine <engine>` for each full 120-second
vector, produced these results. Every row has exactly one promotion,
exactly one `SignalEnded` closure and zero merge closures. Each decoded
text contains the repeated `CQ TEST <CALL> <CALL> TEST` message.
All final frequencies are within 25 Hz of 14,012,340 Hz.

| Vector | Engine | Text length | CER | WPM | Frequency Hz |
|---|---|---:|---:|---:|---:|
| VR1 | hsmm | 498 | 0.017857 | 36.095154 | 14012345.950 |
| VR1 | legacy | 496 | 0.015873 | 40.772446 | 14012345.950 |
| VR2 | hsmm | 570 | 0.024263 | 41.243694 | 14012341.118 |
| VR2 | legacy | 568 | 0.019064 | 43.734695 | 14012341.118 |
| VR3 | hsmm | 349 | 0.019718 | 30.267162 | 14012360.009 |
| VR3 | legacy | 337 | 0.053521 | 29.999985 | 14012360.009 |
| VR4 | hsmm | 355 | 0.016667 | 30.259249 | 14012354.314 |
| VR4 | legacy | 354 | 0.016667 | 31.855637 | 14012354.314 |

VR3's 2% CER gate is an HSMM stage-2 criterion; legacy's 5.35% here is
reported for comparison, not claimed as passing that gate. Both engines
retain the one-track invariant.

Test-first checks on the base production code failed as intended:

- The synthetic skirt test promoted six tracks instead of one.
- The 30-second VR3 integration case promoted 36 tracks under legacy.
- The exact-SNR-tie unit test discarded track 1.
- The new VR1/VR2 CLI regression observed 286 promotions; VR3 and VR4
  failed their character gates. Restoring the fixes made all three pass.

The full `cargo test -p manta-engine --locked` suite passes. The debug
`golden_v1`, `golden_v2_v3` and `golden_v7_v9_v10` suites preserve their
passing tests and ignore sets. `cargo test -p manta-cli --locked --test
golden_vr` gives 16 passed and four ignored, with VR3/VR4 enabled.
The release run with `--include-ignored` gives 16 passed and four failed:
VR1/VR2 fail only at `wpm 36.095154` / `wpm 41.243694`, and VR5/VR8
retain their co-channel/fading CER failures (0.118644 / 0.656347).
No gate or estimator threshold was relaxed to change those results.

## Follow-ups

- Design the HSMM reported-speed estimator and reduce the SpeedUpdate
  flood without corrupting the end-of-stream estimate.
- Revisit `primary_track_id` selection for multi-signal batch summaries.
- Investigate validation of noise-garbage `AEE7EER` with warmup disabled.
- MAN-171: implement the Silence-anchor `round(u)` stride and measure CPU.
- Fix V1-HSMM's startup CER gap separately.

The default remains legacy and the overall stage-2 acceptance stays open.
No oracle or Pi4 CPU-budget result is claimed by this change.
