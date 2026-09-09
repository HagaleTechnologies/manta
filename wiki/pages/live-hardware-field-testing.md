---
id: live-hardware-field-testing
title: How do I run manta against real RF and trust what it tells me?
kind: howto
status: current
maintainer: agent
sources:
  - docs/DECISIONS/2026-09-08-first-live-rsp1b-run.md
  - docs/DECISIONS/2026-09-09-overnight-40m-soapy-field-test.md
verified:
  commit: 89e39f897a2fca4641360c0f4ed9c42ad8f4cde8
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

Don't trust `spots_confirmed > 0` (or a `Decoding` verdict) alone. The
repetition gate ([[spot-validation]]) assumes bogus decodes are random
noise that won't repeat identically — but a deterministic decode artifact
at a fixed frequency repeats identically every time and passes the gate
just as well. Field-confirmed 2026-09-09: 29/29 confirmed spots in an
overnight 40m session shared one signature — confidence pinned to the
low end (~0.12-0.17), `snr_db` pinned to nearly the same value session-
wide, often-implausible WPM — and clustered at a handful of fixed
frequencies near the input passband edge across separate capture windows.
A dial-shift test (retune, see if the artifact moves with the new
passband edge) is the fast way to tell this from a real signal. **Not
confirmed as MAN-7/103** (that's a per-channel WPM-*estimation* bug on a
real signal, not a detection/spot-generation bug) — see
`docs/DECISIONS/2026-09-09-overnight-40m-soapy-field-test.md` Finding 2
for the full reasoning; treat this as a separate, still-untracked
front-end/passband-edge defect until proven otherwise.

Before trusting a spot: check whether `snr_db` sits within about a dB of
a value that recurs across many spots in the same session (not just
whether it's >0 dB — `snr_db` is a 2500 Hz-reference-bandwidth
conversion, so a negative value doesn't by itself mean no signal), and
treat a suspiciously round/low confidence value as another warning sign.

## Setup gotchas

Already covered in `docs/DECISIONS/2026-09-08-first-live-rsp1b-run.md`:
the `libsdrPlaySupport.so` rpath fixup, and staying at or below ~45 dB on
`--soapy-gain` (this specific RSP1B's top ~3 dB fails `activateStream()`
outright). Also: a single `ErrorCode::Overflow` on a live USB stream used
to kill the whole session outright — fixed in `crates/manta-input/src/
soapy.rs` (#146, 2026-09-09) with its own bounded retry, so this is no
longer something you need to work around.
