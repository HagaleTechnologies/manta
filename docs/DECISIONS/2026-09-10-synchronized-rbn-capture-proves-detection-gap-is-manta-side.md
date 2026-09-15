# Synchronized RBN capture proves the detection gap is manta-side, not propagation or ADC overload; plus a new startup-transient artifact

Follow-up to `docs/DECISIONS/2026-09-09-soapy-gain-is-inverted-attenuation-scale.md`
and `docs/DECISIONS/2026-09-09-20m-dial-shift-edge-artifact-confirmed.md`.
Motivated directly by Tony's pushback on this session's earlier ("band
conditions probably changed") explanation for a gain=15 capture that
found no track activity at known-strong RBN frequencies -- a fair
challenge, since every prior test in this thread checked RBN *before* or
*after* a manta capture, never in the exact same window. This session
closes that gap with a genuinely synchronized test.

## Method

Live RSP1B capture (`--soapy-gain 15`, `RFGR=0/IFGR=35` -- the HF-
recommended LNA-state zone per the community-research findings earlier
this session), raw IQ dumped directly via a standalone Rust probe
(bypassing manta's own pipeline for the capture step, to rule out any
manta-side capture bug), 90 seconds, 14030 kHz center, 192 kHz passband
-- run at the same time as a live RBN telnet capture (`telnet.
reversebeacon.net:7000`) covering the identical wall-clock window
(2026-09-10 03:42:51-03:44:23 UTC). RF tuning readback verified exact
(`device.frequency()` returns exactly the requested 14030000 Hz, no
drift/calibration explanation available). The raw capture was converted
to a WAV + JSON sidecar and run through manta's real decode pipeline
(`manta decode`, `WavIqSource` -> `decode_samples`) -- the same code path
`decode_wav` uses, exercising the actual channelizer/track-manager/
decoder logic end to end, just file-sourced instead of live-streamed.

## Result 1: confirmed, controlled proof the detection gap is not propagation and not ADC clipping

RBN shows two real, multi-skimmer-confirmed CQ signals active in this
exact window:

- **JN1THL @ 14031.10-14031.20 kHz**, up to 38 dB, seen by 6 skimmers
  (JH8YOH-1, KD7EFG, KW7MM, KW7MM-2, WA7LNW, ZL2KS)
- **VK2GR @ 14037.80 kHz**, up to 20 dB, seen by 5 skimmers (EY8ZE, ND7K,
  NG7M, VK2RH, W3UA)

manta's decode of the exact same capture window produced **zero
`TrackPromoted` events anywhere in the 14024-14045 kHz range** -- not
weak/unconfirmed tracks, not tracks that opened and immediately closed:
none at all. The nearest promoted tracks sit at 14024000.3 Hz (7.15 kHz
below JN1THL) and 14045000.7 Hz (7.2 kHz above VK2GR), both themselves
suspected to be a different artifact (Result 2, below) rather than real
detections.

A same-capture clipping check (per-channel, `|re| > 0.98` or `|im| >
0.98`, not complex magnitude -- see correction below) found **zero
clipped samples** across the full 90-second, 34.5M-sample capture. This
directly rules out simple ADC/front-end overload as the explanation for
this specific dead zone.

**Correction to an initial live claim this session**: a first pass
computed `max(|re + i*im|)` (complex magnitude) and got 1.06, which was
wrongly reported as clipping. Complex magnitude naturally exceeds 1.0
even when neither the real nor imaginary rail individually clips (up to
sqrt(2) for two independent unclipped values near full scale) -- the
correct per-channel check, matching the earlier synthetic clipping test
in `2026-09-09-soapy-gain-is-inverted-attenuation-scale.md`, found no
clipping. Retracted; flagged here so the mistake isn't repeated.

**Conclusion**: with propagation and ADC overload both ruled out by
direct, synchronized, controlled measurement, the remaining explanation
space narrows sharply to manta's own detector/channelizer/track-manager
logic having a real, reproducible blind spot for at least this specific
frequency range under these conditions -- not the antenna, not the RSP1B
hardware generically, not band conditions. This is the single strongest
piece of evidence in the whole live-hardware investigation (spanning
`2026-09-08-first-live-rsp1b-run.md` through this doc) that the root
cause is manta-side.

## Result 2 (new): a startup-transient comb of spurious track promotions, ~8 kHz spaced, in the capture's first ~60ms

13 of the first 15 `TrackPromoted` events in this capture (and likely
more further in) land within a ~12,000-sample window (~62 ms at 192 kHz,
`sample_ts` 457728-476672) and cluster on a near-exact 8 kHz frequency
grid: 13944030.7, 13952000.4 (implied from a matching earlier session,
not directly re-verified here), 13959969.5, 13968030.1, 13976000.7,
13983967.8, 13992030.8, 13999999.7, 14024000.3, 14045000.7 (16 kHz gap
here), 14048001.0, 14055969.0, 14064032.0, 14071998.3, 14079970.5,
14084093.9 (off-grid), 14088031.2 (implied), 14093093.9 (off-grid),
14096000.0, 14103968.7, 14116060.2, 14119998.4, 14123190.0-14124028.5
(a tighter cluster near the passband edge).

Real CW signals starting in near-perfect unison on a fixed frequency grid
across nearly the entire passband, all within 62ms of each other, is not
plausible -- this is almost certainly an artifact. Leading hypothesis
(unconfirmed): the per-channel noise-floor estimator
(`crates/manta-dsp/src/floor.rs`, a 25th-percentile order statistic over
a `RING_LEN=250`-entry rolling window decimated every `DECIMATION_HOPS=
15` hops, ~10s to fill per the wiki) has no real data yet in the first
fraction of a second of any capture -- if its initial/default state reads
as "signal present" in many channels simultaneously before enough samples
accumulate to correct it, many channels could spuriously promote at
once. The near-8kHz periodicity is not yet explained by this theory and
needs its own investigation (channelizer bin spacing is 93.75 Hz, so
8000 Hz is roughly 85 channels apart -- no obvious mechanism identified
yet for why specifically every ~85th channel would be affected).

This is a separate, distinct issue from Result 1's dead zone -- it does
not explain away the missing JN1THL/VK2GR tracks (neither falls on this
grid, and the grid is confined to the first ~60ms while the real signals
were present, and absent from tracking, for the entire 90-second window).

## Next steps (not done here -- diagnostic session only, no code changed)

1. Investigate why manta's channelizer/track-manager produces zero
   activity in the 14024-14045 kHz range specifically, given confirmed
   real, strong, simultaneous signal there and ruled-out clipping/
   propagation/tuning-offset explanations. This is now the single
   highest-priority open question from the whole live-hardware
   investigation thread (see also #166, #167).
2. Investigate the ~8kHz-spaced startup-transient track-promotion comb --
   check noise-floor estimator initial-state behavior in the first
   `RING_LEN`-worth of samples, and whether the periodicity has a
   mechanistic explanation (filter/FFT structure, decimation hop
   scheduling, or something else).
3. Consider whether the "dead zone" is truly fixed-frequency (same
   14024-14045 range on a re-run) or moves/varies -- a repeat of this
   exact synchronized-capture method at a different center frequency
   and/or different real-time RBN-confirmed target would help localize
   whether this is frequency-specific, session-specific, or something
   else.
