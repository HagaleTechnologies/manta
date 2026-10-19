# MAN-273: a 511-tap Hilbert FIR and a source-declared DC/Nyquist spawn guard for live audio

## Decision

The live-audio path (`manta-input::AudioIqSource` →
`manta-dsp::hilbert::HilbertTransformer`) stops spawning spurious and
churning tracks on one clean CW tone. Three changes, all needed:

- **D1 — a wider Hilbert FIR.** `HILBERT_TAPS` goes 129 → 511. Every
  tap at an even offset from the centre is zero, so the transformer
  evaluates only the 256 nonzero taps (sparse; bit-identical to the dense sum).
  `HILBERT_MIN_IMAGE_REJECTION_DB = 70.0` is enforced by a contract test
  over [600, 23 400] Hz in 50 Hz steps at 48 kHz.
- **D2 — a source-declared spawn guard.** `hilbert_guard_hz(fs) =
  600 * fs / 48000` (600 Hz at 48 kHz) is where the FIR stops
  guaranteeing that floor. `IqSource::analytic_guard_hz()` (default
  `0.0`) returns it from `AudioIqSource` and the soak harness's
  `LoopingAudioIqSource`. `DetectorConfig::guard_hz` (default `0.0`) is
  not a TOML key: the loader rejects `[detector] guard_hz` with
  "detector.guard_hz is not configurable: it is derived from the source's
  audio front end (manta-dsp::hilbert, MAN-273)" (`SOURCE_DERIVED_KEYS` in
  `manta-cli` `config.rs`). `listen` takes
  `max(configured, source)` and calls `TrackManager::set_guard_hz`, which
  builds a mask of channels whose whole band lies below the guard (DC
  rim) or above `fs/2 − guard` (Nyquist rim). The Nyquist rim yields to
  an installed trusted band: `DecimatingSource` forwards the 600 Hz guard
  unchanged, but the leak the rim guards sat at the 48 kHz input's
  Nyquist, which the decimator's stopband removes — applied at the
  decimated rate's own Nyquist it masked the top of the advertised band
  (remediate round: at `--capture-rate-hz` 6000 Hz a 2600 Hz tone decoded
  nothing; at 1500 Hz no channel could spawn). The §2.5 spawn
  conjunction vetoes masked channels in both the `k` and `k+1` slots,
  exactly like MAN-196's trusted band. **Spawns only** (the MAN-314
  lesson): a running track is never closed, de-weighted or held by it.

Every complex-IQ source keeps the trait default `0.0`: no mask,
behaviour-identical to before. No config key, CLI flag, metric family or
wire field is added. A spawn census
(`TrackManager::spawns_by_channel()`/`total_spawns()`, `u64`) exists for
tests only; it is not a metric.

## Root cause

A real tone at `+f` leaves a negative-frequency image at `−f` whose
level is the FIR's image rejection at `f`. With 129 taps that rejection
is weak near DC and near Nyquist, so the image of a tone a few hundred
Hz from either edge clears the detector's floor and promotes its own
track. The channel index is circular (SPEC §1.1), so a near-DC image
lands at a *high* channel index, not beside the tone. Several tracks
then lock onto the same signal with churning IDs. The complex-IQ path
(`decode`, every golden vector) has no Hilbert stage and is unaffected.

## Measured image rejection (research, 48 kHz)

| Tone | 129 taps | 511 taps |
|---|---|---|
| 100 Hz | 4.1 dB | — |
| 200 Hz | 8.3 dB | — |
| 400 Hz | — | 94.6 dB |
| 562.5 Hz | 27.4 dB | — |
| 600 Hz | — | 89.1 dB |
| 750 Hz | 43.1 dB | 104.4 dB |

511 taps hold >= 81 dB from 300 Hz up. The 600 Hz guard keeps the
contract's 70 dB floor with margin.

## Guard rims and their cost

At 48 kHz the channelizer has 512 channels of 93.75 Hz. A channel is
masked only when its whole band lies inside the guard: the DC rim masks
the 11 channels whose centres are within ±468.75 Hz (upper edge
515.6 Hz); the Nyquist rim masks the 11 whose centres are at or beyond
±23 531.25 Hz. That is 22 of 512 channels, about 4.3 %. Channel 6
(centre 562.5 Hz, 515.6–609.4 Hz) straddles the edge and still spawns, so
the common 600 Hz sidetone stays skimmable (a centre-based mask vetoed it;
self-review finding), and the FIR holds ≥ 81 dB from 300 Hz.

Tones below about 515 Hz, or within about 515 Hz of Nyquist, are not
skimmable through the audio path. That includes the 400–500 Hz sidetones
some rigs default to; the M1 live-copy runbook says to pick 600 Hz or
higher. An asymmetric guard (a narrower DC rim where 511 taps already
clear the floor) is a follow-up, not done here.

## D3 — a spawn-time mirror-image veto for analytic sources

The plan made a third mechanism conditional on measurement: port the
prior attempt's mirror suppressor only if, with D1 and D2 in, the
acceptance scene still showed a mirror-of-owned spawn or duplicate,
character-interleaved decodes on two `track_id`s. It did. With 511 taps
and the 600 Hz guard, the +20 dB fixture decoded
`"FFQ Q CCQ Q DDE E WW11AAW W WW11AAW W K K CCQ Q"`: track 1 promoted at
+750.6 Hz (SNR 22.7 dB) and track 2 at −749.8 Hz (SNR 19.5 dB), both at
the same hop. The image is −104 dB in raw power, but the Hilbert FIR
removes the real input's noise from the negative half of the spectrum
just as deeply, so the image still clears its own channel's floor. The
research's single-track measurement was taken at 129 taps, where the
negative-side noise is suppressed less.

The fix is smaller than the prior attempt's merge-based suppressor:
`TrackManager::set_mirror_image_guard(true)` (set by `listen` when the
source's `analytic_guard_hz()` is above zero, never from
`DetectorConfig::guard_hz`) adds one more spawns-only conjunct,
`mirror_vetoes_spawn`, in both spawn slots. It refuses a spawn on `k`
when any channel within one of the mirror `(n − k) % n` is owned by a
track, or its raw `hop.power` is within `MIRROR_POWER_MARGIN_DB` (3 dB)
of `k`'s or above it. The comparison is O(1) per candidate (no all-pairs
scan) and uses raw power, not the smoothed SNR (the prior attempt's
EMA-based version flip-flopped). Real audio has no negative
frequencies, so for an analytic source the mirror of a real signal only
ever holds its image. Complex-IQ sources never enable it: two real
stations symmetric about the dial must not suppress each other. With D3
in, the fixture decodes `W1AW` on exactly one `track_id`.

## D4 — fixtures frozen at 129 taps

`HILBERT_TAPS_M1_LEGACY = 129` stays, used only by `manta-testkit`
fixture synthesis (`scene.rs`'s Watterson path, `fsk.rs`'s RTTY path).
Changing `HILBERT_TAPS` therefore moves no golden-vector byte. It is
never used for live decode.

## D5 — the live-audio test runs

`crates/manta-engine/tests/listen_audio.rs::listen_decodes_a_clean_real_audio_signal`
is no longer `#[ignore]`d. Its fixture carries +20 dB SNR-in-2500
real-domain AWGN (new `manta_testkit::noise::add_real_awgn` /
`real_awgn_sigma_for_snr_2500`) so the floor is realistic, and it asserts
exactly one emitting `track_id`. Without D3 that assertion fails (two
`track_id`s, see D3). `a_clean_audio_tone_does_not_churn_candidates`
runs the same scene through `listen_with_observers` and reads
`TrackLifecycleObserver`: measured 4 unconfirmed closes and a peak of 1
open track (before the fix: 263 and 9; the complex-IQ control: 17 and
1); it asserts at most 25 and at most 3. The plan's tone × SNR × seed
sweep and the in-crate per-channel census test were not run in the
implementing session (time), and the manual `vp8geo_cw_48khz.wav` smoke
run was not run either.

## Prior attempt: reused vs re-derived

Reused from `archive/MAN-4-pr103`: the 511-tap width, the 70 dB floor,
the source-declared `analytic_guard_hz()` / `DetectorConfig::guard_hz`
shape, the mirror comparison's raw-power/3 dB/±1-channel rules, and the
test fixture's intent. Re-derived on current main: the
rejection table above, the guard as a function of `fs`, the rim channel
arithmetic, the sparse evaluation, and the veto's placement beside
MAN-196's trusted band in both spawn slots. The tag's own pins record
(`docs/DECISIONS/2026-09-04-man-4-hilbert-guard-pins.md`, on the tag
only) is superseded by this one.

## Cost

`cargo bench -p manta-dsp --bench hilbert -- --warm-up-time 1
--measurement-time 3`, run in the implementing container (shared CPU, so
treat the absolute numbers as indicative), per second of 48 kHz audio:

| Design | Time (median, [low high]) |
|---|---|
| 129 taps (legacy) | 4.81 ms [4.65 4.97] |
| 511 taps sparse (new default) | 19.94 ms [18.65 21.48] |
| 511 taps dense (context) | 28.20 ms [27.41 28.98] |

About 2 % of one core, on the audio path only; the budget-binding
192 kS/s SDR path has no Hilbert stage. The prior attempt measured
15.7–18.7 ms/s for 511 sparse. The mirror veto (D3) is three channel
lookups per spawn candidate.

Normative text: [SPEC-decode-core §2.5](../SPEC-decode-core.md) (the
spawn conjunction), [ARCHITECTURE.md](../../ARCHITECTURE.md) (audio
input), [docs/CONFIG.md](../CONFIG.md) ("Not settable"),
[the M1 live-copy runbook](../RUNBOOKS/m1-w1aw-live-copy.md) (the
sidetone blind spot).
