# Reseating connections and adding a common-mode choke resolves the live-hardware detection gap

Follow-up to the whole 2026-09-09/09-10 live-hardware investigation thread
(`2026-09-09-soapy-gain-is-inverted-attenuation-scale.md`,
`2026-09-09-20m-dial-shift-edge-artifact-confirmed.md`,
`2026-09-10-synchronized-rbn-capture-proves-detection-gap-is-manta-side.md`,
issues #166, #167, #171) and directly to PR #174's independent repro
attempt, which reached the opposite conclusion from #171 (RF-path, not
manta code) using a WWV null-signal test on what its own PR description
suggests was a different physical machine (a `claude.ai/code/session_...`
cloud session, not this dev box).

## Method

1. **Verify PR #174's WWV claim directly on the actual RSP1B in use all
   session**, independent of manta's code entirely: tune to 9.990 MHz,
   correlate the raw IQ stream against a single frequency bin at exactly
   +10,000 Hz offset (WWV's 10.000000 MHz carrier) using a from-scratch
   Goertzel-style single-bin DFT, with four arbitrary nearby offsets as
   controls. Run twice for reproducibility.
2. Tony physically reseated all antenna/feedline connections and added a
   common-mode choke.
3. Repeat the exact same WWV correlation check post-change.
4. Repeat the exact synchronized manta-capture-vs-live-RBN methodology
   from `2026-09-10-synchronized-rbn-capture-proves-detection-gap-is-manta-side.md`
   (full 192 kHz passband, `--soapy-gain 15`, 14030 kHz center) for 5
   minutes, cross-checked against a live RBN telnet capture over the
   identical window.

## Result 1: PR #174's "no antenna signal at all" claim does not hold on this hardware

Pre-fix WWV check (two runs): WWV's exact carrier bin measured -57.9 dB
and -54.1 dB relative to broadband RMS, consistently and reproducibly the
strongest of five tested bins (arbitrary control offsets ranged -58 to
-79 dB). This is a real, present, frequency-locked signal -- not "no
antenna signal at all." PR #174 almost certainly tested a different
physical rig; its finding doesn't describe this dev box's actual RF path.

That said, WWV at only 10-20 dB above the broadband noise floor is weak
for what should be one of the strongest, most trivially-received signals
in North America (commonly S9+ on any functioning antenna) -- consistent
with a real, degraded-but-not-dead antenna path. This was the actionable
signal that motivated the physical fix below.

## Result 2: the physical fix measurably cut broadband noise without changing WWV's own strength

Post-fix WWV check, same method: WWV's own relative prominence is
essentially unchanged (-54.0 dB) -- expected, since a common-mode choke
can't make WWV itself stronger. But **total broadband RMS power dropped
by roughly 13.5 dB** (avg_rms 0.195-0.196 pre-fix -> 0.041 post-fix, a
~4.7x reduction) -- consistent with a common-mode choke suppressing
common-mode RFI/noise pickup on the feedline shield, exactly the
mechanism it's designed for.

## Result 3: the synchronized live-CW test now produces genuine confirmed spots, and the dead zone is gone

Post-fix synchronized capture (full 192 kHz passband, same methodology as
the original dead-zone finding): **3 confirmed spots, all the same real
callsign, `WI9Q`** -- a verified, currently-registered US amateur
callsign (Midwest/region-9 district; independently confirmed via QRZ.com,
not just RBN). Confirmed across 3 separate track instances:

```
WI9Q  freq=14050.999kHz  snr=12.2dB  confidence=0.428  wpm=16.6  type=De
WI9Q  freq=14051.007kHz  snr=9.6dB   confidence=0.469  wpm=17.6  type=Cq
WI9Q  freq=14051.009kHz  snr=12.2dB  confidence=0.542  wpm=15.9  type=De
```

Every signature that marked the overnight session's 29 false Beacon spots
as artifacts (`2026-09-09-overnight-40m-soapy-field-test.md`) is absent
here: confidence is 0.43-0.54 (vs. the artifact band's 0.12-0.17), WPM is
plausible hand-sent speed (vs. ~51.5 WPM average implausibly fast), SNR
is real signal-level (9.6-12.2 dB, vs. the -8.2794 dB floor-clamp value),
and the spot types (`De`, `Cq`) are legitimate operating patterns, not
the `Beacon` exemption path. RBN did not happen to have a skimmer on
14051 kHz in this exact window (a different real station, N2AW/N2AWE,
was active at 14050.8-14051.1 kHz per RBN in the same window -- plausible
band-sharing, not evidence against WI9Q).

**More importantly than the one catch: the mid-band dead zone from
`2026-09-10-synchronized-rbn-capture-proves-detection-gap-is-manta-side.md`
is gone.** That test found zero `TrackPromoted` events anywhere in
14024-14045 kHz against confirmed-real RBN signals. This capture shows
real track activity in every 5 kHz bin across the entire 14000-14070 kHz
CW segment (33-295 promotions per bin). SNR distribution shifted
fundamentally: 5,912 of 25,437 `TrackMeta` readings (23%) now read above
0 dB, 2,500 (10%) above 6 dB, peak 47 dB -- every prior capture this
session had positive-SNR readings as rare exceptions, not thousands of
them.

## What's still present, now correctly understood as secondary

- **The passband-edge artifact** (`2026-09-09-20m-dial-shift-edge-artifact-confirmed.md`,
  issue #166) is still measurable in absolute terms (13935/14120 kHz
  bins: 135 and 133 promotions) but its *share* of total activity dropped
  from 28-55% in earlier sessions to ~8% here -- a real detector
  artifact, not resolved by this fix, but now correctly sized as a minor
  cost rather than the dominant signal in the data.
- **The ~14075 kHz cluster** (suspected RTTY/digital-mode, same doc) is
  still the single largest bin (370 promotions) -- consistent with the
  earlier hypothesis of real continuous digital-mode traffic in the US
  20m RTTY/data segment (14070-14095 kHz), not something this fix would
  be expected to change.
- **A startup-transient cluster** still appears at the capture's earliest
  `sample_ts`, but its shape differs from the original "~8kHz comb": this
  time it's several tight clusters (near 14031, 14075, 14120, 13936 kHz)
  rather than one broad, evenly-spaced grid across the whole passband.
  Plausibly the same underlying track-birth-suppression-lifting
  mechanism manifesting differently now that the band has much more real
  (and artifact) energy everywhere -- not independently re-investigated
  here.

## Conclusion

Tony's standing suspicion throughout this investigation -- that the
receive chain "wasn't configured for success" -- is confirmed correct,
but at the physical RF-path layer (antenna/feedline connections, common-
mode noise), not the RSP1B software configuration layer this session
spent the most effort auditing (gain scale, LNA state, bias-T, notch
filters, bandwidth, IQ/DC correction all checked out clean and were not
the cause). manta's own decode pipeline, running unmodified in its full
192 kHz wideband multi-signal mode, now produces genuine validated CW
spots once real signal actually reaches the ADC at a usable level. Issues
#166, #167, and #171 should be updated to reflect this: #171's "CONFIRMED
manta-side" verdict does not hold on this hardware -- the dead zone it
found is resolved by a physical fix, not a code change, and its own doc
should be marked superseded rather than left as the standing conclusion.
