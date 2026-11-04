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
for unowned peaks too, when a track is within 2.5 channels (an isolated
birth keeps the old rule; see the hand-landing below). Missing phase observations retain the unowned
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
was ignored with the failed promotion criterion in its reason; the
hand-landing's channel path passes it, so it now runs.
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

Follow up after landing on the quarter-bin 100 Hz and mid-bin 175 Hz
association gaps (mid-bin 100 Hz passes after the hand-landing), the
100/125 Hz 15 dB strong/weak gap, and the full SignalTrack/TransmissionEpisode/
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

## Hand-landing (2026-11-03)

Catalyst's implement phase stopped before its gate (exit 143, a stopped
process) and its four remediation rounds pushed only un-gated
`catalyst/wip/MAN-112/remediate/*` partials, so the ticket was landed by
hand. The branch was merged with main at `33340f54` (MAN-204, MAN-201,
MAN-322; only CHANGELOG and wiki index conflicted). At that merge the
full workspace gate failed nine tests: `close_pair_mid_bin_125`, both V7
engines, two MAN-78 listen tests that need V7's N1AA, and four
`non_cw.rs` tests.

### Channel selection (from remediation round 7)

Two fixes from the last remediation partial are kept, unchanged and now
gated. Before a neighbor has its own track, max-power selection could
read that neighbor's carrier: an outer owned channel is rejected when two
consecutive observations clear the phase SNR gate and its measured
carrier is more than 0.6 channel from this track's centroid; the
strongest remaining exclusive, non-cooling owned channel passing that
check is used instead. The straddle override applies from 0.40 channel
off the nearest integer, not 0.35: restored V7's 106.667 centre drifting
to 106.64 had switched its input to channel 106. These fix
`close_pair_mid_bin_125` (CER 0.206 before), both V7 engines and the two
listen tests.

The hand-landing found one more selection defect with an A/B through the
CLI binary against main. Quarter-bin 200 Hz (107.25/109.38) put the upper
spot 17-18 Hz high, outside the 15 Hz gate that main meets. Its centre
drifted past 109.40, the straddle override switched it to channel 110,
away from a neighbor 2.13 channels off, and phase read from the away
channel pulled the centre to 109.5. A neighbor two or more channels away
no longer triggers the override: its main lobe does not reach the facing
channel (about 30 dB down 1.75 channels away in that scene).
`close_pair_quarter_bin_200` and a selection unit test pin it.

### Swept carriers

Three sweep tests failed: `swept_carrier_7000_to_7040_starts_no_cw_track`
(4 promotions), `single_channel_sweep_bumps_do_not_qualify_as_short_transmissions`
(37) and `non_cw_only_input_reports_no_keyed_cw`. Frequency-anchored
association keeps a sweep's tracks where they were born, so each
track's edge channel kept reading the carrier after it left. Traces of
the promoted tracks showed three readings counted as key-down: the
carrier one or two channels away, 10-25 dB louder than the selected
channel; the channel filter's ringing after a fast sweep, phase-coherent
with the channel's own frequency (so a phase gate cannot see it); and, at
+40/+60 dB, the departed carrier's far skirt about 45 dB below its
in-channel level. These hops flattened the slope fit (−93 to −290 Hz/s
for a +2000 Hz/s sweep) and let D′ or D qualify the track. In the
1000 Hz/s scene the queued amplitudes rippled into 3-7 `keyed_marks`
while the track's key-down evidence was one run.

Key-down evidence (SPEC §2.8 B) now also requires that no channel within
two channels of `k` is 10 dB or more louder, and that the hop is no more
than 30 dB below the track's strongest key-down so far. D′ also needs
`SHORT_MIN_MARKS` debounced marks in that evidence. A mark ends 10 dB
under its own maximum and the next starts 10 dB over the gap's minimum,
each held for four hops. Two simpler counters failed: an absolute
`on_snr_db` threshold merged a 35 WPM element gap that the channel filter
keeps above it (`regression_short_high_wpm`, "DA" at +28 dB, two runs),
and a threshold 10 dB under the track's strongest key-down let a loud
onset transient hide every later mark (`listen_audio`'s noiseless audio
tone, one mark). None of these touch decoding: they decide only whether
an unqualified track qualifies.

The previous round's phase-position gate on key-down hops fixed the sweeps
but broke MAN-402's dah-heavy rows; these power rules keep both. The heavy
`sweeps_across_rates_and_levels_start_no_cw_track` matrix fails on main
(4000 Hz/s +20 dB ramped, 1 promotion) and passes here.

### SSB scene guard

`ssb_voice_at_14040_starts_far_fewer_cw_tracks` failed its scene guard,
not its filter check. With the filter off the SSB scene promoted 29
tracks in band at the PR head against a guard of more than 50; with the
filter on it promotes none and spots nothing. Main measures 80 off-filter
tracks: the fragmentation this ticket removes. Remediation round 7 found
41 with phase-seeded birth centres disabled and 42 with the spawn
phase-outlier check also disabled. The owner chose (MAN-488, option A) to
lower the guard to a count the scene still meets: more than 25. The final
branch gives 34 off and 0 on. The 80 % reduction and no-spot checks are
unchanged.

### Isolated births and promotions

The decode A/B grid (`man398_grid`, 288 single-signal cases) against main
found two rows worse at the merged PR head, before any hand change:
row 59 (`IS`, 14.6 WPM) and row 111 (`E 0000`, 28.2 WPM). Neither has a
second signal. In row 59 a rising-edge phase observation, read across the
key-down edge, put the birth 1.04 channels from its peak bin; the 0.6
channel veto rejected it and the track was born two hops later. The
decoder's start then misread the first three seconds (CER at 14 s 0.036
to 0.714, at 30 s 0.016 to 0.328). In row 111 the track was born on the carrier's skirt channel,
0.88 channel from the carrier. The 0.6 channel centroid gate then
rejected every observation of its own carrier, and a split birth took the
carrier channel. Both rules exist for close pairs; neither had a neighbor
to protect against.

A birth with no open track's centre within 2.5 channels now keeps the
pre-MAN-112 rule: unowned peak, power only, centre at the birth bin. At
promotion, a track with no other centre within 2.5 channels re-seeds its
centre to the median of the ungated phase positions it saw as a
candidate, if there are at least 8 within a 0.2 channel interquartile
range. Two alternatives were measured and rejected. Dropping the centroid
gate for isolated tracks, or restoring the power centroid for them,
fixed both rows but broke close pairs (on-bin 100 and 125 Hz with the
power centroid; mid-bin 100 Hz and on-bin 100 Hz strong/weak with the
gate dropped), the SSB scene and the heavy sweep matrix. Requiring the two birth observations to
lie within 3 dB fixed row 59 but made row 35 worse.

### Measurements

All runs use generated IQ; no live-SDR claim is made. Main is
`33340f54`, re-measured on 2026-11-03 rather than taken from the
plan's baseline.

- Decode A/B grid, release build, main against the final branch: 288
  rows, 288 identical CER at 14 s and 30 s, 0 better, 0 worse.
- Close-pair matrix through the CLI binary (`manta decode --json`; pass
  means two promoted and decoded tracks, CER at most 0.05 and frequency
  error at most 15 Hz for both): main passes 9 of the 30 pair cells, the
  branch 24. No cell main passes fails on the branch. V7 at the restored
  150 Hz spacing fails on main and passes on the branch; the old
  four-channel V7 passes on both.

| Scene | Promoted main/branch | CER main | CER branch | Freq error Hz main | Freq error Hz branch | Pass main/branch |
| --- | --- | --- | --- | --- | --- | --- |
| on/100/15/15 | 1/2 | 0.933/0.942 | 0.008/0.004 | 54.7/45.3 | 0.2/0.3 | false/true |
| on/125/15/15 | 1/2 | 0.937/0.946 | 0.004/0.004 | 47.7/77.3 | 0.0/1.2 | false/true |
| on/150/15/15 | 8/2 | 0.885/0.989 | 0.004/0.004 | 29.0/2.9 | 0.0/2.6 | false/true |
| on/175/15/15 | 2/2 | 0.004/0.004 | 0.004/0.004 | 0.3/0.1 | 0.0/0.4 | true/true |
| on/200/15/15 | 2/2 | 0.004/0.004 | 0.004/0.004 | 0.1/0.6 | 0.0/0.4 | true/true |
| on/250/15/15 | 2/2 | 0.004/0.004 | 0.004/0.004 | 0.0/1.4 | 0.0/1.4 | true/true |
| quarter/100/15/15 | 1/1 | 0.933/0.939 | 0.933/0.939 | 41.7/58.3 | 0.6/100.6 | false/false |
| quarter/125/15/15 | 28/2 | 0.996/0.942 | 0.004/0.004 | 0.2/1.4 | 1.0/2.8 | false/true |
| quarter/150/15/15 | 3/2 | 0.036/0.072 | 0.004/0.004 | 0.5/0.1 | 0.9/0.5 | false/true |
| quarter/175/15/15 | 2/2 | 0.004/0.004 | 0.004/0.004 | 0.8/0.9 | 0.9/0.4 | true/true |
| quarter/200/15/15 | 2/2 | 0.004/0.004 | 0.004/0.004 | 0.9/2.7 | 0.9/2.2 | true/true |
| quarter/250/15/15 | 2/2 | 0.004/0.004 | 0.004/0.004 | 0.9/0.2 | 0.9/0.3 | true/true |
| mid/100/15/15 | 1/2 | 0.941/0.950 | 0.004/0.004 | 73.1/26.9 | 4.8/3.5 | false/true |
| mid/125/15/15 | 6/2 | 0.929/0.993 | 0.004/0.004 | 115.7/3.2 | 4.9/0.6 | false/true |
| mid/150/15/15 | 8/2 | 0.988/0.939 | 0.004/0.004 | 0.4/8.8 | 4.9/0.3 | false/true |
| mid/175/15/15 | 35/2 | 0.988/0.946 | 0.004/0.676 | 0.1/7.8 | 4.0/1.2 | false/false |
| mid/200/15/15 | 20/2 | 0.960/0.054 | 0.004/0.004 | 0.1/1.9 | 2.2/2.0 | false/true |
| mid/250/15/15 | 2/2 | 0.004/0.004 | 0.004/0.004 | 2.4/0.7 | 2.2/0.6 | true/true |
| on/100/25/10 | 1/2 | 0.004/0.353 | 0.004/0.647 | 1.3/98.7 | 0.0/3.6 | false/false |
| on/100/10/25 | 1/2 | 0.383/0.004 | 0.261/0.004 | 98.7/1.3 | 3.2/0.2 | false/false |
| on/125/25/10 | 1/2 | 0.004/0.353 | 0.004/0.637 | 1.1/123.9 | 0.0/6.8 | false/false |
| on/125/10/25 | 1/2 | 0.383/0.004 | 0.237/0.097 | 122.2/2.8 | 1.3/27.5 | false/false |
| on/150/25/10 | 42/2 | 0.004/0.993 | 0.004/0.004 | 0.4/3.0 | 0.0/2.6 | false/true |
| on/150/10/25 | 17/2 | 1.000/0.004 | 0.004/0.004 | 111.5/2.5 | 0.4/2.5 | false/true |
| on/200/25/10 | 2/2 | 0.004/0.421 | 0.004/0.004 | 0.0/6.9 | 0.0/0.7 | false/true |
| on/200/10/25 | 2/2 | 0.032/0.004 | 0.004/0.004 | 4.4/0.4 | 0.0/0.4 | true/true |
| mid/150/25/10 | 1/2 | 0.166/0.457 | 0.004/0.011 | 5.5/144.5 | 5.7/0.9 | false/true |
| mid/150/10/25 | 28/2 | 0.996/0.004 | 0.004/0.004 | 14.2/0.6 | 5.3/0.3 | false/true |
| mid/200/25/10 | 2/2 | 0.004/0.004 | 0.004/0.004 | 2.6/0.1 | 2.2/2.0 | true/true |
| mid/200/10/25 | 19/2 | 0.988/1.000 | 0.024/0.004 | 2.2/1.4 | 7.4/2.0 | false/true |
| V7-150 | 1/2 | 0.937/0.946 | 0.004/0.004 | 104.5/45.5 | 1.3/1.0 | false/true |
| V7-old-4ch | 2/2 | 0.004/0.004 | 0.004/0.004 | 0.1/0.1 | 0.0/0.0 | true/true |

Still failing on the branch: quarter-bin 100 Hz, mid-bin 175 Hz, and the
100 and 125 Hz 15 dB strong/weak pairs. They fail on main too and stay
follow-up work. Mid-bin 100 Hz now passes this matrix.
