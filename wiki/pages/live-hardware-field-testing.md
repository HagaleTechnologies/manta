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
verified:
  commit: 455e1af
  date: 2026-09-09
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

## `doctor` vs `listen` for a field test

- `manta doctor --duration <secs> --json` is the right first check: bounded,
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
      --soapy-freq <hz> --soapy-rate 192000 --soapy-gain 40 \
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
until the track's true final close (a genuine observed RF gap), requiring
a confirmed real `SpeedUpdate` before resolving one. A small residual
(structurally plausible, not-implausibly-fast garble) isn't caught and is
a known, accepted gap (tracked in issue #163, deprioritized).

**Confirmed live, one session in** (`2026-09-09-post-pr154-20m-daytime-
validation.md`): a 15-minute 20m daytime capture went from the overnight
run's 29/29 false positives to 1 residual Beacon garble matching the
documented gap above (low confidence, 3-char non-callsign text, WPM just
under `MAX_PLAUSIBLE_WPM`) plus 1 plausible genuine `SpotType::Cq` catch
(realistic WPM, above-artifact-band confidence, well-formed callsign) --
the first non-Beacon confirmed spot across all live sessions to date. One
session isn't a solid statistical answer yet, but the fix is behaving as
designed so far.

The frequency-clustering data point from the original finding is still
worth knowing when you see the same defect: only one of the observed
clusters is dial-shift-confirmed as tracking the input passband edge
specifically; the others sit well inside the passband with an unconfirmed
mechanism. A dial-shift test (retune, see if a cluster moves with the new
passband edge) is the fast way to check a given cluster. **Not confirmed
as MAN-7/103** (that's a per-channel WPM-*estimation* bug on a real
signal, not a detection/spot-generation bug) — see
`docs/DECISIONS/2026-09-09-overnight-40m-soapy-field-test.md` Finding 2
for the full original reasoning; treat the interior-passband clusters as
a separate, still-untracked question until proven otherwise.

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

## Setup gotchas

Already covered in `docs/DECISIONS/2026-09-08-first-live-rsp1b-run.md`:
the `libsdrPlaySupport.so` rpath fixup, and staying at or below ~45 dB on
`--soapy-gain` (this specific RSP1B's top ~3 dB fails `activateStream()`
outright). Also: a single `ErrorCode::Overflow` on a live USB stream used
to kill the whole session outright — fixed in `crates/manta-input/src/
soapy.rs` (#146, 2026-09-09) with its own bounded retry, so this is no
longer something you need to work around.
