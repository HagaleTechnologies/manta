# MAN-114: the spot frequency comes from a power-weighted phase estimator

Date: 2026-10-17. Ticket: MAN-114, which absorbs MAN-187 (groom comment,
2026-10-13: "also characterise the error per channel-table size").
Found by the 2026-09-05 broad review, lens 3, item 14.

## Problem

SPEC §1.4 promises ±10 Hz spot accuracy. A spot's frequency came from
`Track::center`, an EMA of the dB-domain three-point quadratic
(`manta_dsp::channelizer::interpolate_offset`). Lens 3 measured +19 to
+32 Hz at 0.63 channels from channel centre and −8 Hz bin-centred.

Reproduced on `main` at `e539c9b` with V1 keying (20 WPM,
`CQ CQ DE W1AW W1AW K` looped, 20 s, `noise_seed` 1) at
`(131 + frac) × 93.75 Hz`, decoded by `manta_engine::decode_samples`:
96 kHz, frac 0.60/0.65 read +12.7/+9.9 Hz at +20 dB and +22.2/+22.8 Hz at
+6 dB; frac 0.0 read −7.3 Hz at +20 dB. The worst point is on the other side
of the channel: frac 0.35 read −38.2 Hz at +6 dB.

Three tests had been widened around this error: `pipeline.rs` (15 Hz),
`golden_decimated_capture.rs` V31 (25 Hz) and `roundtrip_iq.rs` (25 Hz).
SPEC §1.4 and the V31 note blamed the channel-table size (N). That was
wrong (below).

## Mechanism

On the bare channelizer (96 kHz, strongest of `k0−1..=k0+1` per hop, hops
gated at SNR ≥ 6 dB, worst |error| over frac −0.5…+0.5):

| input | dB parabola | phase, per-hop mean | phase, power-weighted |
|---|---|---|---|
| clean continuous tone | 12.13 Hz (S-curve, 0 at 0 and ±0.5) | 0.000 | 0.000 |
| continuous tone + AWGN (+20 dB) | 23.33 Hz | 0.040 | 0.052 |
| keyed, noiseless | 5.14 Hz | 7.98 Hz | 3.67 Hz |
| keyed + AWGN (+20 dB) | 16.02 Hz | 7.19 Hz | 3.78 Hz |

AWGN on the parabola's weak neighbour bin drives the pipeline error, more
than the static S-curve does: at small offsets the neighbours sit 60–80 dB
down, below the noise. Keying transients (the 32-hop filter window is longer
than a 60 ms dit) pull a single channel's phase slightly toward channel
centre; power weighting keeps that pull near 3 Hz.

## Decision

`Track::freq_hz` reports a separate, phase-based position. Everything else
that reads `center` is unchanged.

- `manta_dsp::channelizer::phase_lag_product(prev, cur, k)`:
  `z = X̃[k,m]·conj(X̃[k,m−1])·e^{jπ(k mod 4)/2}`, with `X̃` the
  `odd_channel_sign_correction`-ed output. With hop = N/4 on every table,
  `arg z = 2π·p/4` for a tone at channel position `p`, so products from
  different channels of one track share a frame. `None` on a hop gap or a
  power-only hop (empty `x`).
- `phase_position(z, near)`: `arg(z)/2π·4`, unwrapped to the value nearest
  `near` (the track's `center`).
- `TrackManager` keeps the previous hop (`prev_hop`, buffers reused hop to
  hop, kept across `process_hops` calls so chunking cannot change a result).
  Each track folds the product of its selected channel into `phase_z`.

The choices, each with what it rules out:

1. **Single-channel, power-weighted phase difference.** The ticket offered
   two candidates. This is the phase-slope one, accumulated in the complex
   domain. Rejected:
   - *Prototype-response lookup* (invert the parabola's S-curve). It removes
     the clean-tone bias exactly, but AWGN on the weak neighbour bin
     dominates in practice: −23.0 Hz at frac 0.2 and −15.9 Hz bin-centred
     (pipeline, 96 kHz, +20 dB), no better than before.
   - *Per-hop phase with an unweighted EMA.* −5.5 Hz at frac +0.4 on the bare
     channelizer (keyed, +20 dB), +4.4 Hz at 35 WPM. Low-power keying
     transitions count as much as steady key-down.
   - *Two- or three-channel resynthesis before the product.* Best on a lone
     signal (≤ 0.8 Hz power-weighted), but it mixes in an adjacent signal's
     channel. V7 puts two signals 1.6 channels apart. The single-channel
     product sees a neighbour only through the ≥ 74 dB stopband.
   - *Replacing `center` itself.* That would move ownership, merges, the
     refiner and the spectral reference, which MAN-197/MAN-313 and the golden
     vectors are tuned around. The ticket needs only the reported value.
2. **Gate: key-down hops with per-channel SNR ≥ 6 dB contribute**
   (`FINE_FREQ_MIN_SNR_DB`). That is SPEC §1.4's rule, which
   `update_centroid` never implemented. With power weighting the gate barely
   moves the result (96 kHz, +6 dB: worst 2.80 Hz gated, 2.40 Hz ungated),
   but the spec rule now holds for the estimator that feeds spots.
3. **EMA constant: `CENTER_EMA_ALPHA` (0.01).** The spot frequency follows
   drift (V9, V51) on the same time scale as before. Gating to key-down hops
   roughly doubles the time constant in seconds, to about 0.5 s: about
   0.4 Hz of lag at V9's 50 Hz/min.
4. **Fallback:** while `phase_z` is zero (no gated hop yet, or power-only
   test hops), `freq_hz` reports `center` as before.
5. **No per-table correction.** The bias was a property of fractional
   position, AWGN and keying, not of N (below). The phase estimator needs no
   table: one cycle per hop spans 4 channels on every table.
6. **Merges:** the survivor keeps its own `phase_z`; a loser's state is not
   carried (as MAN-272's D5 does for annotations).
7. **Static crashes (Codex review, PR #346):** `phase_z` skips suppressed
   hops (SPEC §2.7), as `center` does. A crash is declared on its third
   lifted hop, so on that hop each track also restores `phase_z` from
   before the two lifted hops that preceded it, as the decoder hold does
   for its input. Without the rollback, a +100 dB, 5 ms crash put a +6 dB
   signal half a channel off centre 19 Hz off after the crash; at +60 and
   +80 dB the two hops moved the estimate by at most 1.2 Hz, because they
   sit near the 3 dB lift threshold (`freq_across_channel.rs`). The
   rollback does not reach values already reported on those two hops: a
   `TrackMeta` (every 375 hops) or a promotion that lands on one of them
   still carries it.

## Per-table characterisation (MAN-187)

Full pipeline, worst |error| over frac 0.00…1.00 step 0.05, V1 keying at
`(131 + frac) × 93.75 Hz`, 20 s, `noise_seed` 1 unless stated. "Before" is
`main` at `e539c9b`; "after" is this change, reproduced by
`cargo test -p manta-engine --test freq_across_channel --locked -- --ignored --nocapture characterize_freq_error_per_table`.

| table | SNR (2500 Hz) | before: worst (at frac) | after: worst (at frac) |
|---|---|---|---|
| 48 kHz / N=512 | +20 dB | 20.96 Hz (0.65) | 2.67 Hz (0.55) |
| 48 kHz / N=512 | +6 dB | 31.99 Hz (0.35) | 3.41 Hz (0.55) |
| 96 kHz / N=1024 | +20 dB | 23.18 Hz (0.30) | 2.44 Hz (0.55) |
| 96 kHz / N=1024 | +6 dB | 38.22 Hz (0.35) | 2.80 Hz (0.50) |
| 192 kHz / N=2048 | +20 dB | 21.75 Hz (0.40) | 2.60 Hz (0.45) |
| 192 kHz / N=2048 | +6 dB | 34.46 Hz (0.35) | 3.68 Hz (0.45) |
| 96 kHz, seed 2 | +6 dB | 31.05 Hz (0.45) | 3.07 Hz (0.45) |
| 96 kHz, seed 3 | +6 dB | 24.14 Hz (0.65) | 2.79 Hz (0.45) |
| 96 kHz, seed 4 | +6 dB | 32.65 Hz (0.35) | 3.09 Hz (0.45) |

The seed 2–4 "after" values were measured on the plan's prototype, which
the six seed-1 rows reproduce to the hundredth of a hertz.

Selected points, 96 kHz, +6 dB, before → after: frac 0.00 −13.67 → +0.06;
0.20 −31.71 → −1.05; 0.35 −38.22 → −1.19; 0.50 −29.46 → −2.80;
0.65 +22.77 → +1.47; 0.80 +12.12 → +0.40.

All three tables show the same S-shape and similar magnitude before the
change. MAN-187's "~15-17 Hz at non-96 kHz" was this fractional-offset and
AWGN error, which appears at every table size. V1 and V31 share the
+12.34 kHz offset, so the same fractional position (0.627).

The tests widened for this bug are back at SPEC's 10 Hz, with the values
measured after the change:

| test | was | now | measured |
|---|---|---|---|
| `crates/manta-engine/tests/pipeline.rs` (V1, 20 s) | 15 Hz | 10 Hz | +1.53 Hz (was 11.51 Hz) |
| `crates/manta-cli/tests/golden_decimated_capture.rs` V31 | 25 Hz | 10 Hz | 1.41 Hz (was 17.40 Hz) |
| same file, rim-inside the trusted band | 25 Hz | 10 Hz | 0.02 Hz |
| `crates/manta-engine/tests/roundtrip_iq.rs` (`#[ignore]`d proptest) | 25 Hz | 10 Hz | not run; covered by `freq_across_channel.rs` |

`crates/manta-engine/tests/freq_across_channel.rs` is the ticket's scenario
as a test: every tenth of a channel at 96 kHz at +6 and +20 dB, and six
offsets on 48 and 192 kHz at +6 dB, each within ±10 Hz.

## What stays on `interpolate_offset`, and why

- `Track::center` (`update_centroid`): ownership, `select_channel`, merges,
  eviction, the refiner's channel and the spectral reference. Changing it
  moves detection and decode behaviour; this ticket changes only the
  reported value.
- The MAN-313 qualification drift fitter (`KeyingEvidence::on_hop`). Moving
  it to the phase estimator would change qualification outcomes; that is a
  separate change ("the drift-slope fitter should use the phase estimator").
- `manta calibrate` (`CarrierEstimator`): ~1 Hz Hann bins, already within
  0.02 Hz in its tests.
- Deprecated `freqest.rs`/`single.rs`, which stay in place.

## Records this resolves

For the spot frequency, this record resolves
`2026-07-19-m2-detector-track-pool-pins.md` item 6 and
`2026-07-18-m2-pfb-channelizer-pins.md` item 9 (the deferred
fine-frequency interpolator bias, and the 25 Hz bound it forced). Item 5 of the former (live EMA instead of
§1.4's lifetime mean) stands, and now covers both estimators.
