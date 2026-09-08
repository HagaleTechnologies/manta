---
id: replay-input-dispatch
title: Why did `listen --source` reject `gen`'s own output, and how was it fixed?
kind: gotcha
status: current
maintainer: agent
sources:
  - crates/manta-input/src/lib.rs
  - crates/manta-input/src/audio.rs
  - crates/manta-input/src/pace.rs
  - crates/manta-input/src/replay.rs
  - docs/DECISIONS/2026-09-07-man121-hardware-free-replay.md
# `verified` is deliberately UNSET: this page is new in the same change it
# describes, so no already-merged revision contains both the page and the
# `open_replay_wav` dispatch / `pace` / `replay` modules it documents. A
# marker naming a revision that predates the code would misstate this
# page's provenance to anyone trusting the field (round-3 review). Set it
# in a later pass, against a revision that actually carries the behavior.
links:
  - spot-output-contract
---
`manta_engine::listen()` itself is **rate-agnostic** — `Channelizer::new`
accepts any `fs` where `fs / 93.75` is a power of two (96000 is; so is
48000). The 48 kHz constraint belongs to exactly one `IqSource`
implementation, `AudioIqSource` (`crates/manta-input/src/audio.rs`), not to
the streaming pipeline. Misreading that constraint as belonging to
`listen()` itself is what made MAN-121 look like a resampling problem for
longer than it was: it wasn't. `manta gen`/`decode` always use
`WavIqSource` (2-channel complex IQ, any channelizer rate,
sidecar-carried center freq); `listen --source`/`--device` used to
hard-code `AudioIqSource` (a real, single-channel rig-audio passband
converted to analytic form via Hilbert transform) regardless of what the file actually contained — so
`gen`'s own 96 kHz stereo IQ output couldn't feed the server the README's
quickstart told you to point it at.

Note also that "rate-agnostic" means *any channelizer rate*, not any rate
at all: `fs / 93.75` must be a power of two (12/24/48/96/192/384 kHz and
so on). A 44.1 kHz or 100 kHz IQ WAV is still rejected, by
`Channelizer::new`, not by the reader.

## The fix: dispatch on channel count, with a rate-and-sidecar tie-break at 48 kHz

`manta_input::open_replay_wav(path)` picks the reader from the WAV header,
and **not** from channel count alone — the shipped rule is:

| WAV header | Reader |
| --- | --- |
| 2 channels, rate ≠ 48 kHz | `WavIqSource` (complex IQ) |
| 2 channels, 48 kHz, parseable `<stem>.json` sidecar | `WavIqSource` (complex IQ) |
| 2 channels, 48 kHz, no/unparseable sidecar | `AudioIqSource` (downmix + Hilbert) |
| anything else (e.g. mono) | `AudioIqSource` (unchanged, still 48 kHz-only) |

The 48 kHz row is the whole subtlety, and it is deliberate: 48 kHz is
`AudioIqSource`'s *only* rate, so a stereo soundcard recording of a
receiver's passband is indistinguishable from IQ by channel count alone,
and reading it as `Complex32::new(I, Q)` would silently corrupt it. Away
from 48 kHz there is no such collision — `AudioIqSource` never accepted
those rates — so channel count decides on its own. Do not "simplify" this
back to `channels == 2`.

Two separate sidecar probes back that table, and conflating them was a
real round-2 review bug:

- `manta_input::replay_wav_has_iq_sidecar(path)` — the **format**
  question, and the one the 48 kHz tie-break asks. True for 2 channels
  plus a parseable sidecar with a *finite* `center_freq_hz`, **including
  `0.0`** (baseband, or a dial frequency the operator will pass with
  `--dial-freq-hz`).
- `manta_input::replay_wav_center_freq_hz(path)` — the **RF** question,
  and the one the CLI's `--dial-freq-hz` gate asks. `Some` only for a
  finite *positive* `center_freq_hz`.

Using the RF probe for the format tie-break (as the first cut did) sends a
48 kHz IQ file declaring `center_freq_hz: 0.0` down the Hilbert path,
discarding Q, even when `--dial-freq-hz` was supplied.

Both probes are written to **never fail**, even for a nonexistent path —
the gate must report a missing flag before a missing file, and a probe
that could itself error would invert that ordering. See
`docs/DECISIONS/2026-09-07-man121-hardware-free-replay.md` for the full
rationale and rejected alternatives (resampling, a new 48 kHz vector
family).

## The other half: file replay had no pacing at all

`listen()`'s read loop had no pacing at all before MAN-121 — a file source
drained at whatever speed `read()` could return data (measured ~30-40x
realtime), so the process (and its telnet/JSON servers) could exit before
any client had a realistic chance to connect. `PacedSource`
(`crates/manta-input/src/pace.rs`) is a pure sleep wrapper around any
`IqSource`: it computes sleeps from *cumulative* delivered samples against
one `Instant` (started on the first `read()`, so CLI setup between
construction and the first sample — epoch resolution, whole-file hashing,
server bind — is not charged to the recording's clock), so a slow consumer
degrades to unpaced instead of ever
stalling or drifting — and because it never touches which samples are
delivered, `--realtime` output is byte-identical to unpaced output. The
sleep happens **after** the inner `read()`, against the count that
*includes* the buffer about to be returned: pacing against the previous
count (the first cut) returned every chunk a chunk early, and `listen()`'s
first read is the whole two-second calibration buffer, so a spot decoded
from it could reach the servers before any client could connect. Opt-in
only (`--realtime`), since the default path (tests, `soak --source`) still
wants full-speed drain. `--loop` (`LoopingWavSource`,
`crates/manta-input/src/replay.rs`) reopens the file at EOF for a
demo left running; combined with the 600 s spot-dedupe window, a short
looped file yields roughly one spot per 10 minutes, not one per pass — by
design, not a bug to chase.

## Gotcha: `SpotBus`'s live channel has no history for late subscribers

A client that connects to the telnet/JSON servers *after* a file-replay
spot has already been published will **not** see it on the live stream —
confirmed by hand, connecting well after the one spot a short `--realtime`
run produces yields nothing further, `--loop` or not, until the next
10-minute dedupe cycle. `sh/dx`, however, reads the bus's retained
history rather than the live subscription, and *does* return a spot no
matter when the client connected during the run. This is why the README's
hardware-free demo tells the reader to try `sh/dx` if nothing arrives
within the first ~12 s, rather than relying on split-second terminal
switching.
