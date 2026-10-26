# Overnight 40m SoapySDR field test: fixed a real crash, still no confirmed off-air copy

Follow-up to `docs/DECISIONS/2026-09-08-first-live-rsp1b-run.md` (same
RSP1B, hwVer 6, serial 2402041760, `--soapy-gain 40`). That run left
Finding 3 open: no validated `Spot` ever emitted, negative SNR throughout,
not diagnosable from software alone. This run adds ~5.5 hours of
additional real-hardware data via `manta doctor` and `manta listen
--json`, 7000-7125 kHz US 40m CW segment (`--soapy-freq 7025000
--soapy-rate 192000`, 192 kHz passband ≈ 6930-7120 kHz).

File:line code cites in this record refer to `f48e980`, the `main` commit
PR #151 branched from. MAN-102 (PR #134, merged 2026-09-11) later made
`TrackMeta` report the detector's peak-held S−F instead of the keying-rail
ratio recorded here.

## Finding 1 (fixed): a single SoapySDR stream overflow killed the whole session

`SoapySdrIqSource::read` (`crates/manta-input/src/soapy.rs`) retried
`ErrorCode::Timeout` internally but treated any other error, including
`ErrorCode::Overflow`, as immediately fatal -- tearing down the entire
source and losing all accumulated track/SNR stats. Two consecutive
30-minute `manta doctor` runs died this way, several minutes in, not at
startup: the RSP1B's ring buffer overflowed once under sustained 192 kS/s
USB streaming, and that alone was enough to kill an otherwise-healthy
session.

Fixed in #146 (merged to `main`): `Overflow` is now retried like
`Timeout`, but against its own bound (`MAX_OVERFLOW_RETRIES = 100`,
separate counter from `MAX_TIMEOUT_RETRIES`) rather than folded into the
silence budget -- `Overflow` returns immediately without waiting out
`TIMEOUT_US`, unlike `Timeout`, so retrying it unboundedly risked exactly
the busy-loop-that-never-returns hazard `MAX_TIMEOUT_RETRIES`'s own doc
comment already warns about for silence. Confirmed live afterward: one
`listen` window alone logged 200+ `ErrorCode::Overflow` occurrences over
its ~30-minute run (SoapySDR's own `O` status character, one per
occurrence) and still completed and shut down cleanly on Ctrl-C -- an
aggregate count across many separate `read()` calls, each recovering via
a normal read afterward, not 200 in a single unbroken run against the
100-count `MAX_OVERFLOW_RETRIES` bound (which would itself abort the
session at the 100th consecutive occurrence within one `read()` call).

Retrying an overflow keeps the session alive but does not restore the
samples the ring buffer dropped. At this run `IqSource` had no gap or
discontinuity marker, so `listen` timestamped samples by delivered count
and decoded the samples on either side of each loss as contiguous. A
lost dit is not a key-up interval (see
`2026-09-09-decoder-recall-research.md` §12). This record does not say
which window logged the 200+ overflows or how many the others logged,
so it cannot separate affected windows from clean ones. Every spot and
statistic in Findings 2–4 may come from discontinuous input and cannot
validate continuous-RF behaviour. MAN-233 later added overflow-event
counts and warnings, still with no lost-sample count (see
`2026-10-15-man233-soapy-overflow-visibility.md`).

This was a real reliability gap for any unattended run longer than a few
minutes, not a hypothetical -- worth keeping in mind for the still-open
M2 Pi4 CPU-budget/24h-soak acceptance legs (CLAUDE.md Status): a
resource-constrained Pi4 is *more* likely to overflow the ring buffer
under load than this Mac Mini was, not less.

## Finding 2 (new, unresolved -- likely distinct from MAN-7/103): 29 false-positive-looking spots, one cluster consistent with a passband-edge artifact

Across 10 completed 30-minute `listen --json` windows plus one partial
(11 total, ~5.5h), 29 `Spot`s were confirmed -- but every one shares the
same signature:

- confidence pinned to 0.118-0.172 (the low end of the scale)
- `snr_db` pinned to -8.28 to -7.29 dB. This is emitted spot-side from
  `TrackMeta.snr_2500_db`, which at this run was `Demod::snr_2500_db()`
  (`crates/manta-decode/src/envelope.rs`):
  `20*log10(e_hi / e_lo.max(E_LO_FLOOR)) - 14.3 dB`,
  where `e_hi`/`e_lo` are adaptive keying rails *within the track's own
  decode window* -- seeded from the 90th/10th-percentile envelope
  amplitude at initialization (`envelope.rs:193-199`), then EMA-updated
  per sample and occasionally rescaled (`envelope.rs:244-266`), not a
  literal running percentile at every `TrackMeta` (the decoder's keying-
  rail contrast). It is **not** the detector's `(S-F)` signal-vs-local-floor
  SNR -- an earlier revision of this finding wrongly attributed it to
  that quantity and derived a "+6 dB above the noise floor" claim from
  it that doesn't follow from what's actually computed. **Also not
  strong evidence by itself**: `envelope.rs`'s rail-collapse clamp
  enforces `e_hi >= 2*e_lo` using the *raw* `e_lo` (SPEC §3.2), while
  `snr_2500_db()` divides by `e_lo.max(E_LO_FLOOR)` -- and `e_lo` is only
  floored to `E_LO_FLOOR` at track init, never again during the per-
  sample EMA update that follows. If `e_lo` drifts below `E_LO_FLOOR`
  afterward, the ratio actually used for SNR can be less than 2, so
  `-8.2794 dB` is **not a proven hard lower bound**, just the value any
  weak/flat-envelope track lands at or near under the common case where
  `e_lo` stays at or above the floor. Retracting the "hard bound" framing
  from an earlier revision of this finding -- the near-constant `snr_db`
  values here are *consistent with* that clamp being hit repeatedly, not
  guaranteed proof of it. Either way, the real signal in this data is the
  *frequency* clustering below, not the SNR values.
- WPM scattered widely and often implausible for real CW traffic/beacons
  (30.8-60.0 wpm)
- callsigns frequently malformed (`3EMEEEE`, `ER1EEAE`, `4AEEEEE` --
  repeated-dit noise artifacts), occasionally coincidentally
  plausible-looking (`N4IIR`, `I8ET`)

Spot frequencies cluster tightly at a handful of near-identical values
across *separate, non-overlapping* windows (6931.2-6931.9, 6936.0,
6944.0, 6968.0, 7064.0 kHz). A fixed decode artifact would repeat at
absolute frequencies like this, but so would a persistent real carrier
or a local RF spur. The clustering calls for retune checks and does not
by itself identify a mechanism.

Only the 6931.x cluster has dial-shift evidence, and it supports a
passband-edge hypothesis without confirming it. At 7025 kHz that
cluster sat 93.1–93.8 kHz below center, near the lower edge. After the
retune to 7075 kHz (passband ~6979-7171 kHz), spots appeared at 7168.2
kHz, 93.2 kHz above center, near the upper edge. These are opposite
edges. 6931 kHz was outside the retuned passband, so the retune could
not show whether that cluster persisted. No per-spot identity, IQ or
spectral signature links the 7168.2 kHz spots to it. The retune rules
out one fixed-frequency carrier producing both clusters.

The 6936.0, 6944.0, 6968.0, and 7064.0 kHz clusters sit well inside the
6930-7120 kHz passband, not at either edge. The dial-shift test was
never repeated against any of them, so grouping them under the same
"passband-edge artifact" label is not established. Every 40m window
used the same 7025 kHz center and 192 kS/s, so the 93.75 Hz channel
boundaries sat at the same absolute frequencies in every window.
Whether each interior cluster lies near one is untested here, and the
0.1 kHz frequencies above are too coarse to tell. A cluster near a
boundary would be closer to MAN-7/103's per-channel mechanism than the
passband-edge cluster is. A distinct decoder false-positive path also
remains possible -- unresolved pending a retune test repeated against
each interior cluster specifically.

**Not the same bug as MAN-7/103 without more evidence, despite the
"edge" coincidence.** MAN-7/103 (`crates/manta-cli/tests/
golden_v2_v3.rs:23-55`, `#[ignore]`d pending
<https://github.com/HagaleTechnologies/manta/issues/24>) is a WPM-
*estimation* error (~29 vs. an expected 35 WPM) for a real keyed signal
near one *individual* 93.75 Hz PFB channel edge -- the golden test shows
a clean decode with correct WPM at channel center, and nothing in that
regression shows the near-edge offset spawning tracks or spots from
noise. The 6931.x cluster points at the edge of the entire 192 kHz
*input passband* (a single boundary, not one of the many per-channel
boundaries scattered through it) and produces spurious *detections*, not
a WPM error on a real one. These could still share a root cause (e.g.
channelizer transient response at a boundary), but that link isn't
established here -- treat it as a separate, untracked passband-edge
false-positive hypothesis until individual-channel
residuals and the detection path (not just the WPM path) are examined
directly.

Operational implication either way, for anyone running `manta` against
real RF right now: `spots_confirmed`/emitted `Spot`s alone are not
sufficient evidence of a real decode, and `snr_db` alone doesn't help
tell the difference (see the clamp-floor caveat above -- a low `snr_db`
is just as consistent with a weak real signal as with this artifact). A
spot recurring at the same frequency across separate, non-overlapping
capture windows, especially near a passband or channelizer boundary, and
carrying a suspiciously low/round confidence value, is the actual warning
sign worth checking for.

**`manta doctor` specifically is fully blind to this.**
`DoctorReport::verdict()` (`crates/manta-engine/src/doctor.rs:143-146`)
returns `Verdict::Decoding` the instant `spots_confirmed > 0`, before
looking at SNR at all -- an affected run driven entirely by this artifact
still reports `"DECODING -- at least one confirmed spot. End to end,
working."` with no hint anything is wrong. This is a real, currently
unresolved consequence of the false-positive defect above, not just a
caution for a human reading raw `listen --json` output: `doctor`'s whole
purpose is to be the quick automated health check, and right now it can
be quick, automated, and wrong at the same time.

## Finding 3 (extends 2026-09-08's Finding 3, still open): no confirmed real off-air CW copy on 40m tonight

All 29 spots passed manta's software validator, and none has an
externally verified callsign: independent confirmation that the decoded
station was on air. The signature in Finding 2 (low confidence,
malformed text, implausible WPM, and recurrence at fixed frequencies
across separate windows -- `snr_db` itself is not independently
informative here, see Finding 2's clamp-floor caveat) is more consistent
with a decode artifact (a dial-shift-supported passband-edge hypothesis
for the 6931.x cluster, no tested mechanism for the rest) than with real
copy, though this is "no *evidence* of real copy," not a
from-first-principles proof each one is noise. Max
instantaneous `TrackMeta.snr_2500_db` touched positive values in
several windows (best full-window case +5.6 dB; one anomalous +18.3 dB
track, `track_id` 5700 at 7076576 Hz, appeared in the final partial
window right as the session was cut short for a band switch to 20m --
zero characters decoded on that track before it was interrupted, so it's
unresolved either way, not confirmed real or confirmed artifact). No
track this session produced a callsign with non-floor confidence. These
runs cannot say whether any track carried above-noise signal power,
because the recorded `snr_2500_db` is rail contrast and the detector's
S−F was not recorded.

Same open question as 2026-09-08: antenna/feedline/band conditions vs.
something else remains undiagnosable from software alone -- manta still
has no built-in RF-level/antenna diagnostic. Given 40m CW is rarely
silent for 5.5 continuous hours in practice, antenna/feedline is now the
more likely suspect than band conditions alone; worth a manual continuity/
SWR check before the next unattended run.

## Finding 4 (brief, inconclusive): 20m the same night, still no confirmed copy

Three 30-minute `listen --json` windows on 20m the same night
(`--soapy-freq 14025000`, passband ~13929-14121 kHz, same RSP1B/gain),
before the session was cut short by operator instruction (not a
technical stop) to hand the RSP1B to a separate configuration
investigation:

- Max instantaneous `snr_2500_db` per window: +13.9, +8.8, +12.9 dB,
  against the best full 40m window's +5.6 dB. These are rail-contrast
  maxima (Finding 2), which a strong envelope contrast on an artifact
  can also produce, so they say nothing about band activity.
- Per window, ~1400 tracks were promoted and ~1250 eventful closures
  logged, against ~1400/~500 on 40m. `TrackClosed` fires only for a
  track whose decoder emitted an event after promotion (`has_emitted`;
  `doctor.rs:118-124`). The difference shows more promoted 20m tracks
  reaching decoder output before closing, not a higher rate of tracks
  opening and closing. The manager's complete close counts and
  open-track counts were not recorded.
- 10 confirmed spots total. Most share the 40m spots' signature:
  confidence 0.12-0.17, `snr_db` at or near -8.28 dB. No frequency or
  retune evidence was recorded for them, so this is a similar signature
  with an unresolved mechanism. A few did not share that signature:
  `SE7CW` (-4.77 dB), `VE3NN` (-5.86 dB, 27.3 wpm -- within plausible
  CW range), `W4PM` (-6.97 dB, 32.6 wpm) sit measurably above the clamp
  floor and have structurally valid callsigns: Swedish (`SE7CW`),
  Canadian (`VE3NN`) and US (`W4PM`) by the bundled `cty.dat` prefixes,
  unlike the malformed-text pattern typical of the 40m spots.
- None of this is asserted as confirmed real copy -- confidence stayed
  low (0.12-0.17) even for these, and three plausible-looking hits out of
  ten in one band switch isn't enough to rule out coincidence. Flagged
  here as a genuine open lead worth a longer, focused 20m session, not a
  conclusion.

## Takeaway

The overflow fix is a genuine reliability improvement, confirmed in the
field, independent of whether 40m ever produces a real decode. This run
also surfaced 29 false-positive-looking spots. One cluster supports a
hypothesised, untracked false-positive defect at the input passband edge
-- plausibly but not confirmedly related to MAN-7/103 -- and the other
fixed-frequency clusters have no tested mechanism. They warrant their
own investigation, with all of these conclusions bounded by potentially
overflow-affected input (Finding 1).
`manta doctor`'s verdict currently can't tell either of these apart from
a real decode. Off-air real-signal confirmation is still the one open
item neither this run nor 2026-09-08's closed. Finding 4 above has the
same-night 20m data -- inconclusive, but the first hint worth chasing
further.

## Revision note

Revised 2026-10-23 for MAN-170 (deferred PR #151 review findings) to
narrow claims the recorded data does not support. No reported
measurement changed.
