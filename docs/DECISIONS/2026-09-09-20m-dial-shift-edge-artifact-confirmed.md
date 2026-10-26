# Dial-shift test confirms a passband-edge track-promotion artifact on 20m; the interior 20m cluster is not the same artifact (likely RTTY/digital, unconfirmed)

MAN-226 correction, 2026-10-23: narrowed the edge-origin and remote-RBN
inferences below; the recorded measurements are unchanged.

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

Both edge clusters moved approximately with the +30 kHz retune, remaining
~3 kHz inside the tuned passband edges. This rules out one fixed
absolute-frequency external signal explaining both edge clusters, but
does not locate their origin. Both captures used the same RSP1B and
driver; SDR front-end, anti-alias/filter or driver effects upstream of
manta remain alternatives to detector/PFB effects.

The two edge clusters account for 82/232 (35%) of `TrackPromoted` events
in run 1 and 34/123 (28%) in run 2. These are event shares, not measurements
of CPU cost, track-manager load or resource contention. None of these
edge tracks produced a confirmed spot in either run; that observation
does not establish that real signals at these frequencies cannot produce
spots.

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

RBN reported CW activity in the 14000-14070 kHz segment during both
capture windows (dozens of unique US stations, several at 20-39 dB across
a dozen+ skimmers; e.g. one station at 14006 kHz logged 20-39 dB by 15+
skimmers simultaneously). These reports establish transmitter activity
and reception at remote skimmers. Their SNR does not establish signal
strength or receivability at this RSP1B's QTH.

Checking manta's own `TrackMeta` events against five of those frequencies
found track activity at only one of the five (14032.5 kHz, where
`snr_2500_db` read -6.6 to -7.8 dB, near the floor-clamp value the wiki
flags as unremarkable). The other four had zero manta track activity.
None of the edge or interior clusters produced a confirmed spot in these
runs. These observations neither establish resource starvation nor
exclude propagation as an explanation for the local reception result.

Establish local reception with raw-IQ evidence and a local reference
check before assigning a receiver or detector cause. The later
[MAN-171 record](2026-09-10-man171-dead-zone-is-rf-path-not-manta-code.md)
explains the remote-SNR caveat and used a WWV check in addition to RBN.
Those later measurements do not measure reception during these September
9 captures. An isolated 10 MHz bin is insufficient to identify WWV
without modulation/sideband or equivalent evidence, as the
[later antenna-path record](2026-09-10-antenna-path-fix-resolves-detection-gap.md)
notes.

## Open follow-ups (not fixed here — diagnostic session, no code changed)

1. Distinguish upstream IQ effects from manta processing as the source of
   the passband-edge promotion pattern. Compare raw IQ with channelizer
   output; keep SDR front-end, anti-alias/filter and driver effects open
   alongside detector/PFB behavior.
2. Confirm the 14075 kHz interior cluster is RTTY/digital via a live
   waterfall or narrowband listen, not just band-plan inference.
3. Establish local reception at the remotely reported CW frequencies
   with raw IQ and a local reference check before ranking antenna,
   feedline, gain/attenuation or detector-threshold explanations for the
   four frequencies with no track activity.
