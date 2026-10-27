---
id: live-hardware-field-testing
title: How do I run manta against real RF and trust what it tells me?
kind: howto
status: current
maintainer: agent
sources:
  - docs/DECISIONS/2026-09-08-first-live-rsp1b-run.md
  - docs/DECISIONS/2026-09-09-overnight-40m-soapy-field-test.md
  - docs/DECISIONS/2026-09-09-post-pr154-20m-daytime-validation.md
  - docs/DECISIONS/2026-09-10-synchronized-rbn-capture-proves-detection-gap-is-manta-side.md
  - docs/DECISIONS/2026-09-10-man171-dead-zone-is-rf-path-not-manta-code.md
  - docs/DECISIONS/2026-09-09-20m-dial-shift-edge-artifact-confirmed.md
  - docs/DECISIONS/2026-09-09-soapy-gain-is-inverted-attenuation-scale.md
  - docs/DECISIONS/2026-09-10-antenna-path-fix-resolves-detection-gap.md
  - docs/DECISIONS/2026-09-10-post-antenna-fix-90min-soak-and-service-reliability.md
  - docs/DECISIONS/2026-10-11-man131-actionable-errors.md
verified:
  commit: d286147
  date: 2026-10-13
links:
  - spot-validation
  - overview
---
# How do I run manta against real RF and trust what it tells me?

`manta doctor` and `manta listen` behave differently on a live SDR than
either does against `type=null`/CI-only test coverage. This page is the
map for running a real field test (SoapySDR/SDRplay confirmed so far,
ARCHITECTURE §3) and reading the result correctly. The normative findings
this distills are `docs/DECISIONS/2026-09-08-first-live-rsp1b-run.md` and
`docs/DECISIONS/2026-09-09-overnight-40m-soapy-field-test.md` — read
those for the full evidence and reasoning.

## Check sample delivery first

Run `manta devices` to find an audio name or Soapy selector, then
`manta check` with that selector or `--config manta.toml`. This measures samples
without starting decoding. Use `doctor` next to inspect decoder activity.
The [source diagnostics decision](../../docs/DECISIONS/2026-10-10-man125-source-diagnostics.md)
defines the reported stream rate, dBFS floor, incomplete-input statuses and
blocking-call limitation. These diagnostics add no new hardware acceptance
claim to the historical findings below.

## `doctor` vs `listen` for a field test

- `manta doctor --duration <secs> --json` assesses decoding after checking sample delivery: bounded,
  self-terminating, gives a verdict from track/SNR/decode stats —
  `NoSignal` (no decoder evidence at all), `ActivityNoSnr` (a track opened
  or decoded characters, but the run ended before any `TrackMeta` landed —
  "unmeasured," not "measured and low"; retry with a longer `--duration`),
  `NoisyNoDecode` (every track read at or below the noise floor),
  `WeakNoDecode` (real signal-level SNR, but nothing validated into a
  spot), or `Decoding` (at least one confirmed spot; the full path works
  end to end). It does **not** expose what a confirmed spot actually
  contained — the callback that counts them throws the `Spot` away
  (`crates/manta-engine/src/doctor.rs`'s `|_spot| spots_confirmed += 1`).
- `manta listen --json` is what you need to actually see spot content
  (callsign, SNR, confidence, WPM). It has no `--duration` flag — bound it
  yourself:
  ```bash
  manta listen --json --soapy-driver "driver=sdrplay" \
      --soapy-freq <hz> --soapy-rate 192000 --soapy-gain <gain> \
      > out.jsonl 2>err.log &
  PID=$!
  sleep 1800        # or whatever window you want
  kill -INT $PID     # ctrlc-handled graceful shutdown, same as real Ctrl-C
  wait $PID
  ```
- `scripts/summarize-listen-jsonl.sh out.jsonl` turns a capture into the
  same chars/tracks/SNR summary `doctor` gives you, plus the actual spot
  list `doctor` can't.

## A confirmed `Spot` is not proof of a real signal

Don't trust `spots_confirmed > 0` (or a `Decoding` verdict) alone. Field-
confirmed 2026-09-09: 29/29 confirmed spots in an overnight 40m session
shared one signature — confidence pinned to the low end (~0.12-0.17),
often-implausible WPM, malformed callsign text, and (tellingly) clustered
at a handful of fixed frequencies recurring across separate,
non-overlapping capture windows rather than randomly distributed.

**Root mechanism identified (a later same-day session, follow-up to the
finding below): every one of these 29 was a `SpotType::Beacon` spot**
([[spot-validation]]'s BEACON exemption) — a single noise-decoded glimpse
ending in a lone "T" word, tagged Beacon by the same coarse pattern real
NCDXF-beacon power-steps decode to, and Beacon spots skip the repetition
gate entirely by design. So the original theory below (a deterministic
front-end artifact that repeats *identically* to survive the gate) isn't
required to explain this data — a single occurrence was always enough,
no repeat needed. This doesn't rule out a fixed-bin channelizer artifact
also being the reason the *same* garbled text recurs at the *same*
frequency across sessions (below) — it just means that artifact didn't
need to fool the repetition gate to produce a public spot. PR #154
(`crates/manta-spot/src/validator.rs`, `grammar.rs` — merged 2026-09-09
as `455e1af`) adds a WPM-implausibility check scoped to `SpotType::Beacon`
candidates specifically, and defers non-allowlisted Beacon candidates
until the track's true final close (the track/input's final observed
state -- `ClosureKind::SignalEnded` fires at the capture's end too, not
only an observed RF gap), requiring a confirmed real `SpeedUpdate` before
resolving one. A small residual (structurally plausible,
not-implausibly-fast garble) isn't caught and is a known, accepted gap
(tracked in issue #163, deprioritized).

**Seen live, one session in** (`2026-09-09-post-pr154-20m-daytime-
validation.md`): a 15-minute 20m daytime capture confirmed 2 spots -- 1
Beacon matching the documented gap above (low confidence, WPM just under
`MAX_PLAUSIBLE_WPM`; a 3-char decode in the UK special contest call shape,
so unresolved, not proven garble) plus 1 plausible genuine
`SpotType::Cq` catch (realistic WPM, well-formed callsign) -- the first
non-Beacon confirmed spot across all live sessions to date. Its higher
published confidence reflects Beacon-vs-Cq bookkeeping (repetition
factor, `master.scp` boost), not evidence of better character copy.
Against the overnight run's same-band 20m baseline (10 spots in 1.5h,
most sharing the low-confidence, floor-pinned signature), the rate of
spots with that signature is unchanged within these tiny samples
(≈4.7/h -> 4/h), so this shows the fix running live with the expected
residual shape, not a measured reduction. One session isn't a solid
statistical answer yet.

The frequency-clustering data point from the original finding is still
worth knowing when you see the same defect: only one of the observed
clusters has dial-shift evidence for an input-passband-edge mechanism,
and it supports a hypothesis rather than confirming it (the record's
Finding 2); the others sit well inside the passband with an unconfirmed
mechanism. A dial-shift test (retune, see if a cluster moves with the new
passband edge) is the fast way to check a given cluster. **Not confirmed
as MAN-7/103** (that's a per-channel WPM-*estimation* bug on a real
signal, not a detection/spot-generation bug) — see
`docs/DECISIONS/2026-09-09-overnight-40m-soapy-field-test.md` Finding 2
for the full original reasoning; treat the interior-passband clusters as
a separate, still-untracked question until proven otherwise.

The [20m dial-shift test](../../docs/DECISIONS/2026-09-09-20m-dial-shift-edge-artifact-confirmed.md),
corrected by MAN-226 on 2026-10-23, found `TrackPromoted` clusters ~3 kHz
inside both passband edges after a +30 kHz retune. Both edge clusters
moved approximately with the dial, ruling out one fixed absolute-frequency
external signal explaining them. Their origin remains unresolved: both
captures used the same RSP1B and driver, so SDR front-end, anti-alias/filter
or driver effects remain alternatives to manta detector/PFB effects.
Compare raw IQ and channelizer output before assigning the source.

The edge clusters accounted for 28-35% of promotion events, not a measured
share of processing cost. None produced a confirmed spot in these runs.
An interior cluster around 14075 kHz stayed at approximately the same
absolute frequency. A digital-mode signal in the US 20m RTTY/data
segment (14070-14095 kHz) remains a hypothesis, unconfirmed by a live
waterfall.

Remote RBN skimmers reported 20-39 dB SNR, establishing transmitter
activity and remote reception, not strength or receivability at this
RSP1B's QTH. Local track activity occurred at only one of five selected
frequencies. The [MAN-171 caveat](../../docs/DECISIONS/2026-09-10-man171-dead-zone-is-rf-path-not-manta-code.md)
requires raw-IQ evidence and a local reference check before assigning a
receiver or detector cause. Its later WWV check does not measure these
September 9 captures. An isolated 10 MHz bin is insufficient to identify
WWV without modulation/sideband or equivalent evidence; see the
[antenna-path record](../../docs/DECISIONS/2026-09-10-antenna-path-fix-resolves-detection-gap.md).

**`snr_db` is not a useful signal here on its own.** A weak/flat-envelope
track commonly lands at or near `20*log10(2) - 14.3 = -8.2794 dB` via
`envelope.rs`'s rail-collapse clamp (`e_hi >= 2*e_lo`, SPEC §3.2) — but
that's not a proven hard floor: `e_lo` is only clamped to `E_LO_FLOOR` at
track init, and can drift below it during the per-sample EMA update that
follows, in which case the SNR ratio actually used can go below 2. A
value pinned near -8.2794 dB is *consistent with* the clamp firing, not
guaranteed proof of it — either way it's completely unremarkable for
both this artifact and a real weak signal, and proves nothing on its
own. Frequency recurrence and confidence are the actual warning signs to
check; don't lean on `snr_db` to distinguish real from artifact.

## A quiet `doctor`/`listen` run on a non-CW signal doesn't mean the RF chain is broken

manta's per-channel noise floor (`crates/manta-dsp/src/floor.rs`) is a
25th-percentile order statistic over a ~10s rolling window (`RING_LEN=250`
entries decimated every `DECIMATION_HOPS=15` hops at `HOP_MS=8/3ms` ≈
10s) — deliberately below the mean specifically so CW's on/off keying
(the signal is silent roughly half the time) doesn't inflate the floor
estimate toward the keyed level (SPEC §2.1). A **100%-duty-cycle
continuous tone — FT8, a carrier, anything not keyed on/off — gets
absorbed into its own channel's floor estimate** almost entirely, so
`doctor`'s reported SNR reads near-zero even sitting on top of a real,
strong signal (confirmed: a real FT8 signal measured independently via
raw spectral analysis at +17.8 dB showed near-zero `doctor` SNR in the
same passband). This is intentional CW-specific design, not a bug — but
it means `doctor`/`listen`'s SNR numbers are **not comparable to a raw
spectral SNR measurement** for anything that isn't actually keyed CW.
Don't use a quiet/low-SNR `doctor` verdict on a known-strong FT8/carrier
signal as evidence the antenna/SDR chain is unhealthy — check with a
direct spectral tool instead (or listen for real CW specifically).

## A quiet band is often propagation, not a broken receiver

40m being quiet during the day and 20m being quiet at night is normal HF
ionospheric behavior, not evidence of an antenna/hardware problem: 40m
needs darkness (daytime D-layer absorption kills it), 20m thrives in
daylight. Confirmed 2026-09-09: a 9-point spectral sweep plus a real,
clean 20m daytime CW catch (6 plausible, correctly-formatted callsigns)
showed the antenna/RSP1B/tuner chain is healthy end-to-end — the quiet
40m daytime results in the same session were fully explained by
propagation, not a configuration problem. Before chasing a hardware
explanation for "nothing heard," check whether the band/time-of-day
combination is even expected to be active.

## Before blaming the detector: check if the signal even reaches the channelizer

A "zero tracks despite a strong RBN-confirmed signal" finding feels like a
detector/track-manager bug, but check one level lower first: run *only*
the channelizer (bypassing `floor`/`gate`/`TrackManager` entirely) and
look at raw per-channel power at the target frequency. `crates/manta-
engine/examples/man171_power_map.rs` does this against a captured WAV.
If the target frequency isn't measurably above the ambient floor even in
raw channelizer output, the signal isn't reaching the ADC at a meaningful
level — that's an antenna/RF-path question, not a manta code question,
and no amount of detector tuning will fix it. A synchronized 40m capture
on one physical rig showed RBN-confirmed signals up to 61 dB reading
≤1.5 dB above ambient in raw channelizer power, with WWV (10.000 MHz —
about as strong and reliable as HF gets) showing no distinguishable
carrier either — the most likely explanation was a disconnected or
misconfigured antenna on that rig (the source record recommends a physical
check; it does not record one as done).
`crates/manta-input/examples/iq_probe.rs` (build with `--features soapy`)
captures a fresh WAV+JSON sidecar via manta's own `SoapySdrIqSource` for
this kind of check; `crates/manta-engine/examples/man171_power_map.rs`
does the raw-power-vs-floor comparison itself.

**Correction, same day, unresolved**: don't treat "no distinguishable WWV
carrier" as proof of total antenna disconnection without checking relative
to a proper noise floor — a from-scratch single-bin correlation at
10.000000 MHz on this session's RSP1B found a reproducible,
frequency-locked bin (-58 to -54 dB relative to broadband RMS, the
strongest of several tested bins). But 10 MHz is an exact 8 kHz multiple,
and a known absolute-frequency-locked birdie sits at those, so this does
not yet show a real WWV carrier, and it says nothing about whether the
earlier run used different hardware (the cited record doesn't identify
it). **The fix that
actually resolved this rig's detection gap was physical**: reseating all
antenna/feedline connections and adding a common-mode choke cut broadband
RMS noise by ~13.5 dB (whether any connection was loose or disconnected
beforehand is not recorded; the source only says all were reseated) and
immediately produced the session's first fully validated real spot
(`WI9Q`, a verified-real US callsign, confidence 0.43-0.54, real SNR,
plausible WPM — matching none of the known artifact signatures above) —
see `docs/DECISIONS/2026-09-10-antenna-path-fix-resolves-detection-gap.md`.
The technique (raw channelizer power vs. floor, a WWV sanity check) is
sound and worth reusing; the specific "disconnected antenna" diagnosis
from one session doesn't generalize to every RSP1B setup automatically —
weak-but-present is a real, different failure mode from absent, and
common-mode noise on the feedline is a real, different fix from
reconnecting a cable.

**Confirmed at scale, same day**: a 90-minute wall-clock unattended 4-cycle soak
(about 38 minutes of actual streaming, estimated from event counts; not
90 minutes of live-SDR evidence) post-fix produced 22 confirmed spots, 13 (59%) matching the real-catch
signature (`Cq`/`De` type, confidence 0.22-0.43) and 9 (41%) matching the
known Beacon-exemption residual gap above. Several real-looking calls
repeated across independent cycles (`W3RJ` 4x, `KC4X` 4x); cross-checked
against simultaneously-captured RBN logs, 4 of 7 checked callsigns
matched almost exactly in frequency (within 40 Hz), confirming the single
-capture result generalizes rather than being a lucky one-off. See
`docs/DECISIONS/2026-09-10-post-antenna-fix-90min-soak-and-service-reliability.md`.

**A genuine hardware artifact can still look exactly like a manta bug.**
The same session found a real, absolute-RF-frequency-locked comb of
"birdies" every exact 8 kHz (confirmed on two different bands/dial
settings — it doesn't move with the tuned center, ruling out a
channelizer/rotation bug) — almost certainly an SDR/USB clock-harmonic
artifact. `floor.rs`'s neighborhood-clamp (SPEC §2.2) never lets such a
persistent, unmodulated interferer's channel learn to ignore it, so it
free-runs a spawn/promote/30s-Silent-close/respawn cycle for the whole
session. Don't assume every ~8kHz-spaced or grid-aligned spurious
`TrackPromoted` cluster is a manta bug — check whether it survives a
retune to a different band/frequency first.

## Setup gotchas

Already covered in `docs/DECISIONS/2026-09-08-first-live-rsp1b-run.md`:
the `libsdrPlaySupport.so` rpath fixup, and staying at or below ~45 dB on
`--soapy-gain` (this specific RSP1B's top ~3 dB fails `activateStream()`
outright). Also: a single `ErrorCode::Overflow` on a live USB stream used
to kill the whole session outright — fixed in `crates/manta-input/src/
soapy.rs` (#146, 2026-09-09) with its own bounded retry, so this is no
longer something you need to work around.

**Sound-card input before MAN-131 ended on an empty capture buffer.**
coppa's `CpalSource::read` is non-blocking, and an empty ring read as end
of stream, so `listen --device` failed at startup with `audio source ended
during startup calibration` or exited 0 a few seconds in — a manta bug, not
the device. Live audio reads now wait, and a device that delivers nothing
for 5 s is an error naming it, 48000 Hz and what to check. A denied macOS
microphone gives silence or no callbacks, never an open error: expect the
stall error or the digital-silence warning, and fix it in System Settings >
Privacy & Security > Microphone. See
`docs/DECISIONS/2026-10-11-man131-actionable-errors.md`.

**`--soapy-gain` is a gain-*reduction* (attenuation) scale on this
driver, not a gain scale — bigger number means less sensitive, not
more** (`2026-09-09-soapy-gain-is-inverted-attenuation-scale.md`). `0` is
maximum sensitivity, `48` is minimum. Every fixed-gain field session through
2026-09-09 used `--soapy-gain 40`, which pins the IF stage at its
absolute maximum attenuation (`IFGR=59`, the top of its whole `[20,59]`
range) — chosen only to avoid the top-of-range activation failure above,
never checked against actual sensitivity. The 60-second sweep recorded
more decoded characters at gains 10 and 20 than at 40. In that 10-20
range, only gain 20 exceeded gain 40's peak SNR (6.52 vs. 3.81 dB);
gain 10 recorded 2.43 dB. Gain 30 also exceeded gain 40, at 4.11 dB.
Gain 0 recorded zero characters and a -1.58 dB peak; the sweep does not
establish overload as the cause. No setting produced a confirmed spot,
so these measurements do not establish an optimum for receiving real CW.
Sweep gain using a locally present real signal or raw-IQ dynamic-range
measurements instead of assuming an optimum.

The later six-minute capture at gain 15 recorded an 18.65 dB peak from
the ~14075/14045 kHz artifact/interference clusters. It still showed zero
`TrackMeta` events within ±3 kHz of six frequencies reported active by
multiple remote RBN skimmers. Their reception does not establish local
strength or receivability. The maxima came from different capture
lengths and RF conditions, so they do not demonstrate a gain-induced
improvement in real-CW reception. Nor can the ~55% edge share of
`TrackMeta` events at gain 15 establish a change from the 28-35% share of
`TrackPromoted` events at gain 40: the event denominators differ.

The [gain record](../../docs/DECISIONS/2026-09-09-soapy-gain-is-inverted-attenuation-scale.md),
corrected by MAN-226 on 2026-10-23, preserves the gain-reduction mapping
without assigning a cause for the missing local activity. The
[MAN-171 record](../../docs/DECISIONS/2026-09-10-man171-dead-zone-is-rf-path-not-manta-code.md)
used raw channelizer power and a local WWV check in addition to RBN;
local evidence is needed before ranking RF-path and detector explanations.
That later check does not measure the September 9 captures, and an
isolated 10 MHz bin does not identify WWV without modulation/sideband or
equivalent evidence, as the
[antenna-path record](../../docs/DECISIONS/2026-09-10-antenna-path-fix-resolves-detection-gap.md)
explains.

**The SDRplay stream can stop activating mid-session, apparently with the
`sdrplay_apiService` daemon wedged** (cause not established; root-owned
LaunchDaemon, `/Library/SDRplayAPI/<ver>/bin/sdrplay_apiService`) —
enumeration (`SoapySDRUtil --find`/`--probe`) kept working in the first
occurrences (in the later 90-minute soak, enumeration itself also failed
once with "No devices found!", so a failed `--find` can be this same
failure), but stream
`activate()` starts failing (`sdrplay_api_Fail`/`sdrplay_api_
ServiceNotResponding`) consistently across every gain value, not just
one. No client-side fix is known (a check for other processes holding the
device was not performed) — it needs `sudo launchctl kickstart -k
system/com.sdrplay.service` (or a service restart), a privileged action.
If activation still fails after the restart, a physical USB unplug/replug of
the RSP1B was what actually cleared it
(`2026-09-09-soapy-gain-is-inverted-attenuation-scale.md`, follow-up section).
If a run that worked minutes ago suddenly can't `activateStream()` at
all, check this before assuming it's gain- or code-related.
**SDRplay stream/device failures recur under sustained ~192 kS/s
streaming** (root cause not established: service, USB/device path or
manta-side are all still open) — seen four separate times in one session
(`sdrplay_api_ServiceNotResponding` / `sdrplay_api_Fail`), sometimes
requiring a privileged restart (`sudo launchctl kickstart -k
system/com.sdrplay.service`) plus a physical USB replug, sometimes
self-recovering within a minute or two with no intervention at all — both
behaviors observed, so treat it as genuinely intermittent, not "wedged
until a human fixes it." **For ordinary unattended/long-duration data collection, run
short independent cycles (e.g. ~20-25 min) rather than one long capture**
— a single long run has no resilience against a mid-run crash and can
lose the whole session; cycling means one crash only costs that cycle.
Check device enumeration (`SoapySDRUtil --find`) before each cycle starts
so a skipped cycle is detected rather than silently producing an empty
result. Do not cycle the M2 acceptance soak: `ROADMAP.md` requires 24
uninterrupted hours with no crash, and restarts would defeat it.
