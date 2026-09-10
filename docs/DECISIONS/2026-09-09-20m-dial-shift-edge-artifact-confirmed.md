# Dial-shift test confirms a passband-edge track-promotion artifact on 20m; the interior 20m cluster is real (RTTY), not artifact

Follow-up to `docs/DECISIONS/2026-09-09-post-pr154-20m-daytime-validation.md`
and the open passband-edge/interior-cluster question from
`docs/DECISIONS/2026-09-09-overnight-40m-soapy-field-test.md` Finding 2
("only one of the observed clusters is dial-shift-confirmed as tracking
the input passband edge specifically... treat the interior-passband
clusters as a separate, still-untracked question until proven otherwise").
Triggered by cross-correlating a live 20m capture against the real Reverse
Beacon Network telnet feed (`telnet.reversebeacon.net:7000`, login
`W5AU`) for the same time window, to sanity-check why manta caught so few
spots while RBN showed abundant real CQ activity in the 14000-14070 kHz
CW segment.

## Method

Two back-to-back `manta run --json` captures, same RSP1B/gain (40 dB),
same 192 kHz sample rate, dial shifted +30 kHz between them:

- Run 1: `--soapy-freq 14030000` (passband 13934000-14126000 Hz), ~10 min,
  232 `TrackPromoted` events.
- Run 2: `--soapy-freq 14060000` (passband 13964000-14156000 Hz), ~4 min,
  123 `TrackPromoted` events.

Histogrammed `freq_hz` from `TrackPromoted` events in both runs.

## Result

| cluster | Run 1 (center 14030k) | Run 2 (center 14060k) | moved with dial? |
|---|---|---|---|
| low edge | mean 13,936,980 Hz (39 tracks), edge+2980 Hz | mean 13,966,677 Hz (19 tracks), edge+2677 Hz | **yes — tracks the edge** |
| high edge | mean 14,123,235 Hz (43 tracks), edge-2766 Hz | mean 14,152,758 Hz (15 tracks), edge-3243 Hz | **yes — tracks the edge** |
| interior | mean 14,075,288 Hz (106 tracks) | mean 14,074,992 Hz (52 tracks) | **no — fixed absolute frequency** |

**Both passband edges are a confirmed detector/channelizer artifact**, not
real RF: track-promotion rate spikes ~3 kHz in from each true edge of the
SDR's tuned passband, in both runs, regardless of what real spectrum sits
there. Together these two edge clusters account for 82/232 (35%) of all
promoted tracks in run 1 and 34/123 (28%) in run 2 — a substantial share
of total track-manager load spent on a boundary effect (plausibly PFB
prototype-filter rolloff/edge-channel behavior — `manta-dsp::proto` is
the Kaiser-prototype channelizer designer named in this repo's
`CLAUDE.md` "Key constraints" as new code with no coppa precedent to
crib from). None of these edge tracks confirmed a spot in either run
(consistent with them being noise/rolloff artifacts, not real signals).

**The interior cluster is not an artifact of the same kind** — it sits at
the same absolute ~14075 kHz in both runs despite the 30 kHz dial shift,
and track count scaled with capture duration (106 in ~10 min vs. 52 in
~4 min — roughly linear). 14075 kHz falls inside the US 20m RTTY/data
segment (14070-14095 kHz), not the CW segment RBN's spots were clustered
in (14000-14070 kHz) during the same session. The likely explanation is
a real, continuous digital-mode signal (RTTY/PSK31) at that frequency
whose keying pattern trips the CW track/envelope detector enough to
promote tracks, which then never validate into spots because the
decoded text is never real Morse. Not confirmed against a live waterfall
in this session — a plausible, not proven, explanation.

## Why this matters for "why so few confirmed spots"

RBN showed abundant real CW activity in the 14000-14070 kHz segment
during both capture windows (dozens of unique US stations, several at
20-39 dB across a dozen+ skimmers — e.g. one station at 14006 kHz logged
20-39 dB by 15+ skimmers simultaneously). Checking manta's own
`TrackMeta` events against five of those known-strong real frequencies
found **track activity at only one of the five** (14032.5 kHz, and even
there `snr_2500_db` read -6.6 to -7.8 dB, near the floor-clamp value the
wiki already flags as unremarkable) — the other four had zero manta
track activity at all. Meanwhile over a third of all track-manager
activity was consumed by the two edge-artifact clusters and the RTTY
interior cluster, none of which can ever produce a real spot. This
doesn't yet prove the edge/RTTY artifacts are *starving* real-signal
detection (no resource-contention mechanism confirmed) — but it reframes
the open question from "why is 20m so quiet for us" (propagation) to
"why does this receive chain see so little of what a dozen other
stations hear clearly on the same frequencies, right now" (antenna/RF
chain/detector sensitivity at this specific QTH) — a materially
different, and more actionable, question.

## Open follow-ups (not fixed here — diagnostic session, no code changed)

1. Root-cause the passband-edge track-promotion artifact in
   `manta-dsp`'s channelizer/detector (PFB edge-channel behavior is the
   leading suspect, unconfirmed).
2. Confirm the 14075 kHz interior cluster is RTTY/digital via a live
   waterfall or narrowband listen, not just band-plan inference.
3. Investigate why 4/5 known-strong (per RBN) real CW frequencies in the
   14000-14070 kHz segment produced zero manta track activity in this
   session — antenna, feedline, gain/attenuation, or detector-threshold
   candidates, in that rough order of likelihood given a single edge
   frequency (14032.5 kHz) *did* produce a track.
