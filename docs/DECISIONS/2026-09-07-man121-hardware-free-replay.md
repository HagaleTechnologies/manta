# MAN-121: a hardware-free path to a real spot on the telnet port

MAN-121 (2026-09-05 broad review, hit list row O-01) found the README's own
quickstart couldn't reach the server it documents: `manta gen v1 --out
/tmp/v1` writes 2-channel 96 kHz IQ, but `manta listen --source` only
accepted 48 kHz mono audio (`AudioIqSource`), hard-rejecting with
`"AudioIqSource requires 48000 Hz, got 96000"`. Separately, file replay had
no pacing at all -- a 120 s recording drained in ~3.5 s, so the process
(and its telnet/JSON servers) exited before any client could plausibly
connect and observe a spot. Together, ROADMAP.md's M3 acceptance gate ("a
stock DX cluster client connects and receives well-formed spots") was not
verifiable by anyone without SDR hardware.

## Decision 1 — dispatch on WAV channel count and rate/sidecar, not a new flag or vector family

`manta_input::open_replay_wav(path)` reads the WAV header: 2 channels at any
rate other than `TARGET_RATE_HZ` (48 kHz), or 2 channels at 48 kHz with a
parseable `<stem>.json` sidecar, means complex IQ (`WavIqSource`,
sidecar-aware -- what `gen`/`decode` already use); anything else is treated
as a real rig-audio passband (`AudioIqSource`, still 48 kHz-only, unchanged).

Rejected alternatives:
- **Resampling `AudioIqSource`'s input to 48 kHz.** Not just unnecessary but
  wrong: `manta_engine::listen()` is already rate-agnostic
  (`Channelizer::new` accepts any `fs` where `fs / 93.75` is a power of two,
  and `96000 / 93.75 = 1024`). A resample step would be lossy, slower, and
  would put a non-trivial DSP stage in the determinism-critical path for no
  benefit. This was a reader-choice bug, not a rate-conversion gap.
- **A `gen --audio-48k` variant.** Every vector (V1-V10) hardcodes
  `fs: 96_000.0` and is a frozen golden vector; a new 48 kHz family would
  still leave `gen v1`'s actual output unusable by `listen`, which is
  exactly what the ticket's first scenario complains about.
- **Relaxing `AudioIqSource`'s 48 kHz requirement.** It's correct for its
  scenario and pinned in `docs/DECISIONS/2026-07-17-m1-implementation-pins.md`
  pin 3 (`coppa-audio`'s resampler is unreachable at M1).

**Round-1 review correction:** channel count *alone* is not the
discriminator this section originally claimed. `AudioIqSource` never
accepts anything but 48 kHz, so a 2-channel file at any other rate could
never have been valid rig audio before this dispatch existed --
unambiguous. But at exactly 48 kHz, a 2-channel file is genuinely
ambiguous: it's both a legal `WavIqSource` rate (`48000 / 93.75 = 512`, a
valid channelizer rate) and `AudioIqSource`'s only rate, and a stereo
soundcard recording of a receiver's passband is indistinguishable from IQ
by channel count alone. Dispatching on channel count regardless of rate
silently misread that recording as `Complex32::new(I, Q)` -- wrong output,
no error, no override. The fix keeps channel count as the discriminator
away from 48 kHz, and at 48 kHz breaks the tie with the same `<stem>.json`
sidecar Decision 2 already uses for RF-awareness: present means IQ, absent
means the pre-existing `AudioIqSource` downmix path. `coppa_audio::WavSource`
(which `AudioIqSource` wraps) downmixes any multi-channel WAV by *striding*
(`step_by(channels)`), discarding channel 1 -- correct for rig audio, wrong
for IQ, which is exactly why the 48 kHz case needs the sidecar tie-break
rather than channel count alone.

## Decision 2 — keep the `--dial-freq-hz` gate where it is; make it format-aware with a never-failing probe

Fixing Decision 1 alone still leaves `listen --source <gen output>
--server-config ...` failing with `"--dial-freq-hz is required..."`, since
that gate keyed on *source kind* (file vs. KiwiSDR/SoapySDR), not on
whether the source actually knows its RF frequency. A `gen` IQ WAV *does*
know it -- its `<stem>.json` sidecar carries `center_freq_hz`, which
`WavIqSource::open` already reads.

`manta_input::replay_wav_center_freq_hz(path)` returns the RF center a
replay WAV *declares* (2 channels + a parseable sidecar with a finite,
positive `center_freq_hz`), and returns `None` on **any** error, including
a nonexistent path. The gate folds this into `has_rf_aware_source` and
stays exactly where it was in the CLI handler, ahead of all real file I/O.

**Round-2 review correction:** Decision 1's format tie-break must *not*
reuse this RF probe. The first cut did, so a 48 kHz 2-channel IQ file whose
sidecar declares `center_freq_hz: 0.0` (baseband, or an unknown dial the
operator supplies with `--dial-freq-hz`) failed the positive-frequency test
and was classified as rig audio -- its Q channel discarded and re-synthesized
through the Hilbert path, contradicting Decision 1's own "parseable sidecar"
wording. The two questions are now separate functions over one private
helper: `replay_wav_has_iq_sidecar` (FORMAT -- finite `center_freq_hz`,
`0.0` included) drives `open_replay_wav`, and `replay_wav_center_freq_hz`
(RF -- finite and positive) drives this gate alone. Both still swallow every
error, so the ordering invariant below is untouched.

This preserves an existing, test-enforced invariant:
`crates/manta-cli/tests/cli.rs::server_config_without_dial_freq_for_audio_source_is_a_clean_error`
provokes the gate with `/nonexistent.wav` and asserts the error names the
*flag*, not a file-open failure -- i.e. flag validation must run before any
file is touched. A probe that could itself fail on a bad path would invert
that ordering; swallowing every error (rather than propagating one) is
what keeps it intact.

## Decision 3 — `--realtime` is opt-in, not the default

The ticket's own Gherkin permits this explicitly: "paced in real time by
default (**or an explicit `--realtime`/`--loop` flag is available**)."
Taking the flag branch:

1. The broad review's protect-list states pacing "must be opt-in and never
   touch the decode path."
2. Defaulting would slow `crates/manta-cli/tests/cli.rs`'s replay tests and
   `manta soak --source` (whose entire purpose is compressed-time replay --
   see `crates/manta-cli/tests/soak_ci.rs`, which replays 120 s of scene as
   a CI proxy for an hour-long run) by the same ~30x, defeating `soak`.
3. Determinism is a hard repo requirement. `--realtime` output is
   byte-identical to unpaced output (`PacedSource` only ever decides *when*
   a chunk is delivered, never *which* samples) --
   `crates/manta-cli/tests/cli.rs::realtime_replay_is_byte_identical_to_unpaced_replay`
   enforces this -- so the flag is safe either way, but the default should
   not move.

`PacedSource` (`crates/manta-input/src/pace.rs`) tracks cumulative
delivered samples against a single `Instant` -- captured lazily on the
FIRST `read()`, not at construction, because `manta-cli` resolves the
replay epoch, hashes the whole recording for the session nonce, and binds
the telnet/JSON listeners in between, and charging that setup time to the
recording's own clock shortened the very window `--realtime` exists to
open -- and sleeps only for `due - elapsed` when positive. **Round-2 review
correction:** that sleep happens *after* the inner `read()`, against the
count that includes the buffer about to be returned. Sleeping first, against
the previous count, handed every chunk to the consumer a chunk before its own
recording interval had elapsed -- and `manta_engine::listen`'s first read is
the entire two-second calibration buffer, so a spot decoded from it could be
published before a client had any chance to connect, which is the exact
failure `--realtime` exists to prevent. A consumer that falls behind
the recording simply stops sleeping and degrades to unpaced -- it can never
stall the pipeline or accumulate drift from repeated small delays.

`--realtime` and `--loop` both require `--source` (clap `requires =
"source"`): both are meaningless for a live device or a network SDR
(already realtime, never EOF), and a misuse should be a clap usage error,
not a silent no-op.

## Decision 4 — `--loop` ships, with its dedupe interaction documented, not "fixed"

`LoopingWavSource` (`crates/manta-input/src/replay.rs`) reopens the file at
EOF via `open_replay_wav` (rather than rewinding), which is what lets it
work for both replay flavours -- `WavIqSource` has no public rewind, and
`AudioIqSource` wraps an opaque `coppa_audio::AudioSource` with none either.
Loop wraps the *innermost* source; `--realtime` pacing wraps *around* the
looped stream (so pacing measures one continuous timeline rather than
restarting its clock every pass).

`SUPPRESSION_SECONDS = 600.0` (`crates/manta-spot/src/dedupe.rs`)
suppresses a repeat spot for the same (callsign, 300 Hz freq bucket) for
10 minutes of *simulated* time. A short looped file therefore yields
roughly one spot per 10 minutes of looped playback, not one per pass --
this is correct skimmer behavior (a real skimmer shouldn't re-spot the same
station every time it repeats CQ) and is not special-cased away. It's
stated in the flag's own `--help` text instead of "fixed," since fixing it
would mean weakening dedupe, an unrelated and unwanted change.

**`--loop` with `--server-config` additionally requires `--realtime`**
(round-3 review). Unpaced replay drains ~30-40x faster than wall clock and
*never ends* when looped, so `Dedupe`'s 600 s of simulated time elapses
every ~15-20 s of real time while `SpotBus::unix_ts_for` adds that
fast-advancing `sample_ts` to the fixed replay epoch: a connected telnet or
JSON client would be served an endless stream of repeat spots stamped
progressively further into the future, presented as *current* observations.
A one-pass unpaced replay self-limits to the recording's own length (and is
what the test suite and `soak` depend on, so it stays allowed); an endless
one does not. Rejected as a flag error at startup, alongside the
`--dial-freq-hz` gate and ahead of any file I/O, rather than by suppressing
or rewriting the timestamps -- publishing a *truthful* timestamp is
`SpotBus`'s contract, and `--realtime` already makes simulated time and
wall clock agree.

## Decision 5 — the M3 README demo needs `sh/dx`, not just perfect timing

Measured empirically while writing the README's Quickstart update: with
`--realtime` alone, the vector's one spot is published to the live
`SpotBus` broadcast channel roughly 12 s into the run (2 s calibration +
SPEC §2.1's ~2.05 s warmup/confirm floor + the vector's own lead-in) and,
because `SUPPRESSION_SECONDS` exceeds V1's full 120 s duration, never
again. `SpotBus`'s live channel has explicit no-history-for-late-subscribers
semantics (`crates/manta-server/src/telnet.rs`) -- a client that connects
after that one moment sees nothing on the live stream, confirmed by hand:
connecting at t=15s (with or without `--loop`) produced only the login
banner, no spot. `sh/dx`, however, reads the bus's *retained history*
rather than the live subscription
(`crates/manta-server/tests/telnet_acceptance.rs::sh_dx_replays_recent_spot_history_in_rbn_format`),
and does return the spot regardless of when the client connected during the
run -- also confirmed by hand. The README's Quickstart therefore tells the
reader to type `sh/dx` at the prompt if the spot doesn't arrive on its own,
rather than relying on the reader switching terminals inside a ~12 s
window. This is a documentation fix, not a code change -- `sh/dx` already
existed; the gap was only that the README never told a hardware-free reader
about it.

## Verification

- `crates/manta-input/src/lib.rs` -- `open_replay_wav`/
  `replay_wav_center_freq_hz` unit tests (dispatch, sidecar parsing, the
  never-fails contract).
- `crates/manta-input/src/pace.rs` -- `PacedSource` unit tests (sample
  identity, minimum elapsed time, no drift compounding under a slow
  consumer, EOF pass-through).
- `crates/manta-input/src/replay.rs` -- `LoopingWavSource` unit test
  (reopens and reproduces the same samples past EOF).
- `crates/manta-cli/tests/cli.rs` -- CLI-level coverage for the format
  dispatch, the relaxed/still-enforced `--dial-freq-hz` gate, realtime
  byte-identity, `--loop` liveness, and `requires = "source"` usage errors.
  The pre-existing
  `server_config_without_dial_freq_for_audio_source_is_a_clean_error` and
  `json_output_is_valid_and_deterministic_across_three_runs` regression
  guards stay green.
- `crates/manta-cli/tests/telnet_e2e.rs` (new) -- the M3 acceptance gate,
  automated: a real child process, real sockets, a real `manta gen`-shaped
  fixture, a real RBN line on the telnet port and a real JSON Lines message
  on the JSON port, no SDR and no hand-built recording.
