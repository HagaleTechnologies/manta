# 2026-09-05 — `AudioIqSource` gains an operator-supplied RF reference (MAN-34)

**Status:** accepted (implemented in this PR).

## Decision

`AudioIqSource` (`crates/manta-input/src/audio.rs`) gains a validated builder
method, `with_center_freq_hz(f64) -> Result<Self>`, that records an
operator-supplied RF dial frequency and reports it from `center_freq_hz()`.
`manta listen` and `manta soak` both gain a `--dial-freq-hz` flag (already
present on `listen`; newly added to `soak` for parity) that feeds this
builder when the opened source is a plain audio device or `--source` WAV
file. A `manta listen`/`manta soak` invocation on an audio source with no
`--dial-freq-hz` now prints a one-line stderr warning instead of silently
emitting baseband-offset frequencies. `--server-config` continues to
hard-error in the same case, unchanged.

## Context

MAN-15's capability-matrix investigation
(`docs/DECISIONS/2026-09-01-legacy-capability-matrix.md`) originally
disposed all CAT/rig control as a non-goal, citing CW Skimmer's own manual:
CAT is only required for 3-kHz/SoftRock-IF (narrowband) modes and is
explicitly not used with wideband SDRs. Codex's review on PR #57 correctly
narrowed that: the reasoning holds for manta's wideband target hardware
(OpenHPSDR/Hermes, SoapySDR, KiwiSDR), which already report their own tuned
frequency and don't need CAT at all — but manta also ships a narrowband
rig-audio input mode (`listen`/`listen --device`, `AudioIqSource`) where an
audio passband genuinely carries no RF reference of its own, and the
original non-goal wrongly swept that case in too.

By the time this ticket was filed, most of the machinery this scenario
needs already existed: `manta-engine::listen()` already read
`IqSource::center_freq_hz()` unconditionally and folded it into every
emitted `Spot.freq_hz`, and `manta-cli` already had a `--dial-freq-hz` flag
that could wrap *any* source in a private `FixedCenterFreqSource` decorator
(`crates/manta-cli/src/main.rs`). A prior, independent investigation
(`docs/DECISIONS/2026-09-03-cross-source-dedup-algorithm-spike.md`, MAN-53,
two review rounds) had already re-derived this same conclusion while
investigating a different problem, and explicitly asked that a future
per-source config surface (MAN-13) be able to require an RF reference for
any source that isn't already RF-aware — which is the hook this decision
provides.

What was genuinely missing, confirmed against source at commit `5b9e747`:
`AudioIqSource` itself had no way to carry a center frequency (the
capability only existed as a `manta-cli`-private decorator, unreachable
from any other library consumer or future MAN-13 config surface); a bare
`manta listen --device` silently emitted baseband-offset frequencies with
no diagnostic; and `manta soak` had no `--dial-freq-hz` at all.

## Why a manual dial frequency, not CAT/OmniRig

The ticket's own technical notes scope this to "a manual operator-supplied
frequency argument (equivalent to entering the dial frequency once, not
live rig polling)". Full CAT/OmniRig integration (live rig polling,
driver-selection and band-scope-alignment machinery) remains a non-goal for
the same reason it always was: it's the legacy Windows multi-program
sprawl `manta`'s single-binary architecture has nothing to orchestrate, and
nothing in this ticket's scenario requires live retuning — an operator
audio-monitoring a fixed-frequency rig only needs to state that frequency
once.

## Why a builder method, not a required constructor parameter

Every sibling `IqSource` implementor takes `center_freq_hz` as a required
constructor argument (`KiwiIqSource::connect`, `SoapySdrIqSource::open`,
`HpsdrDevice::open`), and matching that would be the more consistent choice
on its face. Rejected here because `AudioIqSource::new`/`from_device`/
`from_wav_file` have call sites where there genuinely is no RF reference
(this module's own unit tests, `crates/manta-engine/tests/listen_audio.rs`,
and any future harness), and forcing each to pass a literal `0.0` would
recreate the exact hardcoded-zero anti-pattern this ticket closes, just
spread across more files. A consuming builder —
`AudioIqSource::from_device(name)?.with_center_freq_hz(hz)?` — makes "I am
supplying a reference" an explicit, validated act and "I have none" a
visible omission, with zero churn to existing call sites. The Kiwi/Soapy/
HPSDR sources keep required parameters because for them a center frequency
is a *tuning command* the source acts on; for audio it is an annotation of
a passband someone else already tuned.

`with_center_freq_hz` rejects non-finite and non-positive values, matching
the CLI's existing `parse_dial_freq_hz` rule exactly. The duplication is
deliberate: the CLI parser fails at parse time, before any device is
opened; the library method protects any other consumer (MAN-13's future
per-source config surface included). "No reference" is expressed by not
calling the method, never by passing `0.0` — `0.0` remains the sentinel for
"unset" internally, but is never a value a caller is meant to pass.

## Why a warning, not a hard error, outside `--server-config`

Hard-failing every bare `manta listen --device` would break a legitimate,
already-documented use: an operator watching decoded CW text on a local
terminal with no interest in frequency at all (`README.md`, the M1
manual-acceptance runbook). The harm this ticket exists to prevent — a
wrong frequency reaching the network — is already a hard error via
`--server-config`'s existing check (`!has_rf_aware_source &&
dial_freq_hz.is_none()`), which is unchanged. A one-line stderr warning
covers the local-use case (naming the flag and stating that reported
frequencies are baseband offsets) without regressing it. `manta soak`
gets the identical warning for the same reason and for flag-set parity
with `listen`.

## Why `FixedCenterFreqSource` survives, narrowed rather than deleted

It keeps a real job after this change: overriding an already-RF-aware
source (KiwiSDR/SoapySDR/HPSDR) whose own reported frequency the operator
wants to override with `--dial-freq-hz`. Audio and file-replay sources now
carry the operator's dial frequency natively via
`AudioIqSource::with_center_freq_hz`, so `open_audio_source` applies it
directly instead of wrapping the result; `FixedCenterFreqSource` is now
only reached when `has_rf_aware_source` is true. Deleting it would be a
silent behaviour regression for the RF-aware sources' override path.

## MAN-13 hook

`docs/DECISIONS/2026-09-03-cross-source-dedup-algorithm-spike.md` (MAN-53)
asked that MAN-13's future per-source configuration surface be able to
require an RF reference for any non-RF-aware source it manages, the same
requirement `--server-config` already enforces today for a single source.
`AudioIqSource::with_center_freq_hz` is the library-level capability that
surface will bind to — it did not exist before this ticket; only the
CLI-private decorator did.

## What's out of scope

- CAT/OmniRig/live rig polling (see above).
- Removing or hard-erroring the no-`--dial-freq-hz` case outside
  `--server-config` (see above).
- `LoopingAudioIqSource` (`crates/manta-soak-harness/src/main.rs`) — a
  synthetic looping test harness with genuinely no RF reference; its own
  hardcoded `0.0` is correct and untouched.
- `WavIqSource`/`manta decode`/`manta gen` — that path already has its own
  working `<stem>.json` sidecar mechanism for center frequency and is not
  the rig-audio input mode this ticket names.
- A multi-source configuration surface — that is MAN-13; this ticket only
  makes the capability reachable from a library so MAN-13 can bind to it.
