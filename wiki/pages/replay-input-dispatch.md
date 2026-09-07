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
verified:
  commit: 49f05a4
  date: 2026-09-07
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
`WavIqSource` (2-channel complex IQ, any rate, sidecar-carried center
freq); `listen --source`/`--device` used to hard-code `AudioIqSource` (a
real, single-channel rig-audio passband converted to analytic form via
Hilbert transform) regardless of what the file actually contained — so
`gen`'s own 96 kHz stereo IQ output couldn't feed the server the README's
quickstart told you to point it at.

## The fix: dispatch on channel count, not a flag

`manta_input::open_replay_wav(path)` picks the reader from the WAV
header's channel count: 2 → `WavIqSource`; anything else → `AudioIqSource`
(unchanged, still 48 kHz-only — that's still correct for its scenario).
`manta_input::replay_wav_center_freq_hz(path)` similarly lets a
sidecar-backed IQ file satisfy the CLI's `--dial-freq-hz` gate on its own,
but is written to **never fail**, even for a nonexistent path — the gate
must report a missing flag before a missing file, and a probe that could
itself error would invert that ordering. See
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
one `Instant`, so a slow consumer degrades to unpaced instead of ever
stalling or drifting — and because it never touches which samples are
delivered, `--realtime` output is byte-identical to unpaced output. Opt-in
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
