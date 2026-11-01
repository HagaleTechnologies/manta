# MAN-112: frequency-anchored close-pair resolution

Date: 2026-11-01. Base: `d4f9220ca5e8a154e7c5d151b089d349c8456cf3`.

Supersedes item 4 of docs/DECISIONS/2026-07-19-m2-detector-track-pool-pins.md.

## Decision

Keep the 93.75 Hz channelizer and 375 Hz hop rate. Associate channels by
circular frequency distance, with deterministic id ties and the existing
cooldown checks. Use gated lag-1 phase observations for the centroid.
Below 0.5 channel, merge as before. Between 0.5 and 1.0 channel, require
150 shared envelope observations and Pearson correlation at least 0.6.
Neither the ownership window nor the merge radius is widened.

The decoder reads its exclusive home channel while a neighbor is within
2.5 channels (F1). Isolated tracks retain max-power selection for fading
and drift. At a straddle, a nearby neighbor can contaminate the channel facing it.
Tracking and decoding consistently choose the other straddle channel,
subject to the same cooldown and exclusivity checks. This last
rule addresses the weak mid-bin 150 Hz case missed by the original six
CI scenes. It has its own golden and selection unit test. The first instantaneous-strength comparisons, with 6 dB and then 12 dB
margins, still corrupted startup in the mid-bin 125 Hz scene (CER 0.202
and 0.142). The final selection depends on the nearest neighbor frequency,
so it stays fixed through key-up and key-down. An additional mid-bin
125 Hz golden protects that distinction.

A split within an owned window needs phase evidence, a non-DORMANT owner,
a peak at least 0.8 channel from that owner, and a measured carrier at least
100 Hz away. Two consecutive observations must exceed the floor by 6 dB.
A usable phase position more than 0.6 channel from a peak bin rejects birth
for unowned peaks too. Missing phase observations retain the unowned
power-only test path. These guards preserve MAN-108's dormant identity and
reject the roughly one-channel keying sidelobe exposed by the loud-CW
filter-on/off regression. None of these rules is a TOML setting.

Restore V7's literal 10,000/10,150 Hz offsets. Its two-track, 95% character
accuracy and 15 Hz frequency gates are unchanged. The spot wire is unchanged;
track identities, copy and close-pair spot frequencies can change.

## Reproduction and association choices

All scenes use V7's texts, independent 24/28 WPM keying, seed and 120-second
length. On-bin, quarter-bin and mid-bin refer to channel positions 107,
107.25 and 107.5. The baseline CLI sweep reproduced 9/30 passing cells.
On-bin 150 Hz had 8 promoted tracks, CER 0.885/0.989, 78 births and 59
merges (the birth/merge figures are the plan's instrumented baseline).
The plan's home-only prototype passed 25/30 cells but regressed fading
and beacon tests. Its R1/R2/R4/R5 variant without home-only tracking kept
VR1/VR2, V34 and MAN-120 green.

The first implementation used unconditional home-channel decoder input.
The existing dormant second-station test failed, so F1 was selected.
The initial F1 sweep below passed 24/30 cells. One additional failure,
weak/strong mid-bin 150 Hz, had CER 0.972/0.004 and a weak-signal frequency
error of 18.52 Hz. This justified the stronger-neighbor straddle rule.
The initial F1 variant is preserved here as measurement history, not a claim
that every row below describes the final birth guards or straddle rule.
The CLI goldens exercise the final default through the real binary.

For A/B measurement the harness uses the production streaming pipeline for
both modes, with an observer on the same run. This avoids separate binaries
for the Rust-only experimental mode. Every birth is now counted, including
unconfirmed candidates, across source discontinuities. Observer counters do
not affect decoding and add no network metric or wire field.

| Initial F1 scene | Promoted/decoded | CER A/B | Frequency errors Hz A/B | Births/merges | Gate |
| --- | --- | --- | --- | --- | --- |
| mid/100/15/15 | 13/13 | 1.000/0.507 | 12.877/2.968 | 38/13 | fail |
| mid/125/15/15 | 2/2 | 0.004/0.004 | 3.643/0.574 | 7/1 | pass |
| mid/150/10/25 | 2/2 | 0.972/0.004 | 18.518/0.283 | 11/1 | fail |
| mid/150/15/15 | 2/2 | 0.004/0.004 | 3.751/0.287 | 7/1 | pass |
| mid/150/25/10 | 2/2 | 0.004/0.011 | 2.160/0.914 | 7/1 | pass |
| mid/175/15/15 | 2/2 | 0.004/0.004 | 2.680/2.143 | 7/1 | pass |
| mid/200/10/25 | 2/2 | 0.024/0.004 | 7.422/1.983 | 7/1 | pass |
| mid/200/15/15 | 2/2 | 0.004/0.004 | 2.166/2.013 | 7/1 | pass |
| mid/200/25/10 | 2/2 | 0.004/0.004 | 2.230/2.018 | 7/1 | pass |
| mid/250/15/15 | 2/2 | 0.004/0.004 | 2.197/0.589 | 7/1 | pass |
| on/100/10/25 | 2/2 | 0.261/0.076 | 3.127/0.191 | 8/0 | fail |
| on/100/15/15 | 2/2 | 0.004/0.004 | 0.026/0.281 | 6/0 | pass |
| on/100/25/10 | 2/2 | 0.004/0.647 | 0.011/3.771 | 7/0 | fail |
| on/125/10/25 | 2/2 | 0.237/0.004 | 1.250/1.573 | 6/0 | fail |
| on/125/15/15 | 2/2 | 0.004/0.004 | 0.020/1.237 | 9/0 | pass |
| on/125/25/10 | 2/2 | 0.004/0.637 | 0.005/6.825 | 9/0 | fail |
| on/150/10/25 | 2/2 | 0.004/0.004 | 0.378/2.531 | 9/0 | pass |
| on/150/15/15 | 2/2 | 0.004/0.004 | 0.013/2.552 | 7/1 | pass |
| on/150/25/10 | 2/2 | 0.004/0.004 | 0.010/2.608 | 8/0 | pass |
| on/175/15/15 | 2/2 | 0.004/0.004 | 0.003/0.401 | 6/0 | pass |
| on/200/10/25 | 2/2 | 0.004/0.004 | 0.021/0.377 | 18/0 | pass |
| on/200/15/15 | 2/2 | 0.004/0.004 | 0.019/0.406 | 6/0 | pass |
| on/200/25/10 | 2/2 | 0.004/0.004 | 0.008/0.686 | 6/0 | pass |
| on/250/15/15 | 2/2 | 0.004/0.004 | 0.020/1.409 | 6/0 | pass |
| quarter/100/15/15 | 2/2 | 0.004/0.004 | 0.916/1.452 | 9/0 | pass |
| quarter/125/15/15 | 2/2 | 0.004/0.004 | 0.958/2.763 | 6/0 | pass |
| quarter/150/15/15 | 2/2 | 0.004/0.004 | 0.897/0.542 | 6/0 | pass |
| quarter/175/15/15 | 2/2 | 0.004/0.004 | 0.883/0.370 | 6/0 | pass |
| quarter/200/15/15 | 2/2 | 0.004/0.004 | 0.866/2.201 | 6/0 | pass |
| quarter/250/15/15 | 2/2 | 0.004/0.004 | 0.877/0.330 | 6/0 | pass |

## Steered frontend promotion

The experimental frontend coherently combines two complex channels in an
absolute frame, steers with a continuous NCO and applies a 31-tap Blackman
low-pass with a 30 Hz cutoff. Tests through the real PFB show less than
1 dB gain variation across a channel and at least 40 dB rejection at 100 Hz.
A channel retune changes neither the absolute frame nor the FIR history;
this replaces a crossfade between channel-local frames. Its integer delay
is 15 hops. The existing backlog protocol preserves timestamps, raw power,
SNR, frequency and crash evidence, including on closure. Dormancy resets
its state. The Rust-only `DecodeConfig::steered_frontend` defaults to false;
`DecodeConfig` is not serde-derived and its TOML adapter always sets false.
The existing `refine_bw_hz` path remains available.

These A/B tables use the initial F1 association variant above. Each cell
shows exact callsign recall for Channel/Steered, followed by CER for
Channel/Steered. Both pipelines run through the same WAV and listen loop.
The stronger neighbor is +20 dB, 250 Hz away, with the second V7 text.
The wanted signal is +6 dB. No env switch or new CLI flag is used.

### Single carrier

| WPM | 0 | 1/8 | 1/4 | 3/8 | 1/2 channel |
| --- | --- | --- | --- | --- | --- |
| 8 | 1.000/1.000; 0.000/0.000 | 1.000/1.000; 0.000/0.000 | 1.000/1.000; 0.000/0.000 | 1.000/1.000; 0.000/0.000 | 1.000/1.000; 0.000/0.000 |
| 16 | 1.000/1.000; 0.006/0.006 | 1.000/1.000; 0.006/0.006 | 1.000/1.000; 0.006/0.006 | 1.000/1.000; 0.006/0.006 | 1.000/1.000; 0.006/0.006 |
| 24 | 1.000/1.000; 0.004/0.004 | 1.000/1.000; 0.004/0.004 | 1.000/1.000; 0.004/0.004 | 1.000/1.000; 0.004/0.004 | 1.000/1.000; 0.004/0.004 |
| 36 | 1.000/1.000; 0.003/0.003 | 1.000/1.000; 0.003/0.003 | 1.000/1.000; 0.003/0.003 | 1.000/1.000; 0.003/0.003 | 1.000/1.000; 0.003/0.003 |
| 48 | 1.000/1.000; 0.002/0.002 | 1.000/1.000; 0.002/0.002 | 1.000/1.000; 0.002/0.002 | 1.000/1.000; 0.002/0.002 | 1.000/1.000; 0.002/0.002 |
| 60 | 0.983/1.000; 0.008/0.002 | 1.000/1.000; 0.005/0.002 | 0.967/1.000; 0.009/0.002 | 1.000/1.000; 0.002/0.002 | 1.000/1.000; 0.003/0.003 |

### Stronger neighbor

| WPM | 0 | 1/8 | 1/4 | 3/8 | 1/2 channel |
| --- | --- | --- | --- | --- | --- |
| 8 | 1.000/1.000; 0.000/0.000 | 1.000/1.000; 0.000/0.000 | 1.000/1.000; 0.000/0.000 | 1.000/1.000; 0.000/0.000 | 1.000/1.000; 0.000/0.000 |
| 16 | 1.000/1.000; 0.006/0.006 | 1.000/1.000; 0.006/0.006 | 1.000/1.000; 0.006/0.006 | 1.000/1.000; 0.006/0.006 | 1.000/1.000; 0.006/0.006 |
| 24 | 1.000/1.000; 0.004/0.004 | 1.000/1.000; 0.004/0.004 | 1.000/1.000; 0.004/0.004 | 1.000/1.000; 0.004/0.004 | 1.000/1.000; 0.004/0.004 |
| 36 | 1.000/1.000; 0.003/0.003 | 1.000/1.000; 0.003/0.003 | 1.000/1.000; 0.003/0.003 | 1.000/1.000; 0.003/0.003 | 1.000/1.000; 0.003/0.003 |
| 48 | 1.000/1.000; 0.002/0.002 | 1.000/1.000; 0.002/0.002 | 1.000/1.000; 0.002/0.002 | 1.000/1.000; 0.002/0.002 | 1.000/1.000; 0.002/0.002 |
| 60 | 0.983/1.000; 0.014/0.002 | 1.000/1.000; 0.005/0.002 | 0.967/1.000; 0.009/0.002 | 1.000/1.000; 0.002/0.002 | 1.000/1.000; 0.002/0.002 |

Promotion verdict:

1. Not established. Channel centre recall is 0.9972, edge recall 1.0000;
   Steered is 1.0000 at both. The prescribed +6 dB sweep has no positive
   edge deficit to reduce by 50%.
2. Pass. Centre recall improves from 0.9972 to 1.0000.
3. Pass on exact callsign recall. No stronger-neighbor cell loses recall.
4. Fail. Mid-bin 100 Hz still fragments: 13 promoted/decoded tracks and
   13 merges in both modes. CER is 1.000/0.507 for Channel and 0.870/0.471
   for Steered. Filtering decoder input cannot repair that association.

Do not promote. Keep the experimental stage off. The mid-bin 100 Hz golden
remains explicitly ignored with the failed promotion criterion in its reason.
The supported target region is equal-power pairs from 125 Hz at the tested
alignments, plus on-bin and quarter-bin 100 Hz, and strong/weak pairs from
150 Hz. The final matrix and CI results, rather than this initial F1 table,
determine which of those target cells have been delivered.

## Other verification and cost

The restored V7 at 192 kS/s produced exactly two promoted and decoded
tracks, CER 0.00395/0.00360, frequency errors 1.323/1.016 Hz, 16 births and
zero merges. No dedicated 192 kS/s CI gate was added.

The same-worker `cargo bench -p manta-engine --bench cpu_budget` profile
measured 18.909/22.917/26.993 s (lower/mean/upper estimate) before, and
29.752/34.589/40.407 s after the association changes. Criterion reported a
50.9% mean increase. Both runs shared this six-core worker with builds and
other tests, so these are not isolated hardware-budget measurements. They
show a cost that needs attention; they do not establish either Pi 4 or Mac
CPU acceptance. The hot distance calculation avoids floating-point remainder
inside a single FFT span. R1 is O(tracks) per candidate channel; correlation
is O(750) only for pairs closer than one channel. Timestamped envelope
entries use 12 kB per full track ring, about 14.4 MB at the 1,200-track cap.
Timestamp alignment prevents comparing unrelated observations after gaps or
dormancy. This is larger than the plan's f32-only estimate.

The initial regression runs retained the baseline ignored failures in V5
(both engines), VR2, VR5 and VR8. V1, decimated capture, V34 and MAN-120
passed. Both V8w/VC3 zero-bogus gates passed the initial F1 variant. The
final repository gate runs every normally enabled workspace test and the
HPSDR feature tests. Existing ignored failures are not promoted or loosened.

No recorded contest IQ file was available. Validation here uses generated
IQ and WAV input; no live-SDR claim is made.

## Rejected alternatives and follow-up work

Half-spacing the PFB is rejected. It changes the SPEC's N/hop-rate contract,
doubles per-channel state and estimated FFT/filter work, and does not remove
keying sidebands. The two CPU-budget legs remain open. No ML path is added.

Follow up after landing on the mid-bin 100 Hz association gap, the 100/125 Hz
15 dB strong/weak gap, and the full SignalTrack/TransmissionEpisode/
StationHypothesis identity model from recall-research investigation 8.
Joint neighbor fitting or cancellation belongs with investigation 14.
The frontend must repeat its promotion sweep at a level with a measurable
edge deficit before it can become a default.

## Pre-final straddle regression matrix

This rerun includes the phase-based birth guards and neighbor straddle
selection. Its six failures include the mid-bin 125 Hz equal-power cell
(CER 0.202/0.004). That cell exposed a missing 6 dB PFB half-bin gain
allowance in the initial straddle comparison. A 12 dB comparison still gave CER 0.142. The final rule therefore selects
the channel away from the nearest neighbor consistently across keying.
`close_pair_mid_bin_125` pins the correction. The final active CLI goldens
also pin the previously missed weak/strong mid-bin 150 Hz cell. No existing
acceptance threshold was relaxed. The table below is the measurement before
that final selection correction; the repository gate checks the final tree.

| Scene | Promoted/decoded | CER A/B | Frequency errors Hz A/B | Births/merges | Gate |
| --- | --- | --- | --- | --- | --- |
| mid/100/15/15 | 2/2 | 0.862/0.004 | 11.412/3.497 | 6/0 | fail |
| mid/125/15/15 | 2/2 | 0.202/0.004 | 3.040/0.494 | 6/0 | fail |
| mid/150/10/25 | 2/2 | 0.004/0.004 | 4.540/0.252 | 8/0 | pass |
| mid/150/15/15 | 2/2 | 0.004/0.004 | 2.829/0.248 | 12/0 | pass |
| mid/150/25/10 | 2/2 | 0.004/0.011 | 2.065/0.914 | 6/0 | pass |
| mid/175/15/15 | 2/2 | 0.004/0.004 | 2.363/1.539 | 7/0 | pass |
| mid/200/10/25 | 2/2 | 0.004/0.004 | 0.473/1.980 | 6/0 | pass |
| mid/200/15/15 | 2/2 | 0.004/0.004 | 1.833/2.067 | 6/0 | pass |
| mid/200/25/10 | 2/2 | 0.004/0.004 | 2.166/2.177 | 6/0 | pass |
| mid/250/15/15 | 2/2 | 0.004/0.004 | 2.197/0.589 | 6/0 | pass |
| on/100/10/25 | 2/2 | 0.332/0.004 | 3.143/0.191 | 7/0 | fail |
| on/100/15/15 | 2/2 | 0.008/0.007 | 0.146/0.281 | 8/0 | pass |
| on/100/25/10 | 2/2 | 0.004/0.647 | 0.011/3.643 | 7/0 | fail |
| on/125/10/25 | 2/2 | 0.237/0.004 | 1.250/1.573 | 6/0 | fail |
| on/125/15/15 | 2/2 | 0.004/0.004 | 0.020/1.237 | 9/0 | pass |
| on/125/25/10 | 2/2 | 0.004/0.637 | 0.005/6.825 | 7/0 | fail |
| on/150/10/25 | 2/2 | 0.004/0.004 | 0.378/2.544 | 9/0 | pass |
| on/150/15/15 | 2/2 | 0.004/0.004 | 0.013/2.673 | 6/0 | pass |
| on/150/25/10 | 2/2 | 0.004/0.004 | 0.010/3.004 | 6/0 | pass |
| on/175/15/15 | 2/2 | 0.004/0.004 | 0.003/0.401 | 6/0 | pass |
| on/200/10/25 | 2/2 | 0.004/0.004 | 0.021/0.377 | 6/0 | pass |
| on/200/15/15 | 2/2 | 0.004/0.004 | 0.019/0.406 | 6/0 | pass |
| on/200/25/10 | 2/2 | 0.004/0.004 | 0.008/0.686 | 6/0 | pass |
| on/250/15/15 | 2/2 | 0.004/0.004 | 0.020/1.409 | 6/0 | pass |
| quarter/100/15/15 | 2/2 | 0.004/0.004 | 0.916/1.452 | 8/0 | pass |
| quarter/125/15/15 | 2/2 | 0.004/0.004 | 0.958/2.903 | 6/0 | pass |
| quarter/150/15/15 | 2/2 | 0.004/0.004 | 0.897/0.542 | 6/0 | pass |
| quarter/175/15/15 | 2/2 | 0.004/0.004 | 0.883/0.370 | 6/0 | pass |
| quarter/200/15/15 | 2/2 | 0.004/0.004 | 0.866/1.922 | 6/0 | pass |
| quarter/250/15/15 | 2/2 | 0.004/0.004 | 0.877/0.330 | 6/0 | pass |

The final birth guards remove the mid-bin 100 Hz churn in this rerun:
two promoted and decoded tracks, six births and no merges. Copy remains
outside the gate (CER 0.862/0.004), so that cell remains a declared limit.
The original six close-pair goldens, the additional weak/strong mid-bin
150 Hz golden, the loud-CW filter-on/off regression, and steered delay,
drain and FFT-wrap tests passed before the final selection correction.
The final selection correction adds the eighth active close-pair golden.

## Birth-centre correction

The focused mid-bin 125 Hz regression remained red after fixed straddle
selection, with corruption confined to startup. The final repair seeds a
new track's centroid from the same two gated complex observations used for
spawn admission, rather than waiting for the slow EMA to move from an
integer bin. Ownership is recomputed immediately. This repair was made
after the bounded self-review and is verified only by the terminal gate.
The first gate attempt was stopped during its build because the focused
regression was known red; the one allowed gate retry includes this repair.

## Incomplete handover

This implement phase did not pass its terminal gate. An earlier
`cargo build --all-targets` succeeded before the final repairs. The last
focused mid-bin 125 Hz check before birth-centre seeding failed at CER
0.206. The first terminal attempt was stopped during build, exit 143.
The retry started at 20:59:40 UTC, one second after the permitted cutoff,
and was stopped, exit 143. The final birth-centre repair has type-checked
through Clippy but has not completed an all-target build or runtime gate.
No green full-workspace or final 125 Hz claim is made.

UNVERIFIED: made after the last gate run: this incomplete-handover
paragraph records the stopped attempts and directs the next phase to run
the all-target build and close-pair goldens on the current tree first.
The birth-centre seeding and immediate ownership refresh were introduced
after the first stopped attempt; the late retry did not verify them.

## Remediation of the interrupted implement gate

The all-target build completed on 2026-11-01 in the remediation worker.
Exit 143 in the implement receipt was a stopped process, not a compiler error.
Runtime verification exposed two channel-selection defects in the final tree.

Before a neighbor has its own track, max-power selection can read that
neighbor's carrier. In the mid-bin 125 Hz scene this happened during startup
and left signal 0 at CER 0.2055. Selection now rejects an outer owned channel
when two consecutive observations exceed the phase SNR gate and its measured
carrier is more than 0.6 channel from this track's centroid. It chooses the
strongest remaining exclusive, non-cooling owned channel. Missing evidence
and the existing fallback when no channel qualifies are retained. Channels
within one channel of the centroid retain the prior selection behavior.
The synthetic untracked-neighbor test fails without this check and passes
with it. All eight enabled close-pair goldens pass with the check.

Restored V7 also exposed a straddle override that activated too far from
the midpoint. A centroid shift from 106.667 to 106.64 switched input from
107 to 106. Transition sidelobes then biased phase tracking farther away
and corrupted copy. The override now applies within 0.1 channel of the
midpoint, rather than 0.15. The boundary regression protects the observed
106.64 case. Both V7 engines, V9, V10 legacy and V51 pass the focused suite.
These are deterministic fixture results, not a new claim about the full
separation matrix or live input. The experimental frontend remains off.

The class sweep found the production association call in `step_hop` and
its decoder-input selection on the same path. Birth admission already has
its own phase gate. No second production association caller was found.
The full repository gate is recorded in the remediation phase receipt;
the earlier incomplete-handover receipt above remains historical evidence.
