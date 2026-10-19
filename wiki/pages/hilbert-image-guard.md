---
id: hilbert-image-guard
title: Why did the live-audio path spawn several tracks on one clean tone?
kind: gotcha
status: current
maintainer: agent
sources:
  - docs/DECISIONS/2026-10-16-man273-hilbert-image-guard.md
  - crates/manta-dsp/src/hilbert.rs
  - crates/manta-engine/src/track.rs
  - docs/SPEC-decode-core.md#25-adjacent-channel-ownership-one-signal--one-track
verified:
  commit: 4529021
  date: 2026-10-16
links:
  - detector-tracks
  - pfb-channelizer
---
The channelizer's channel index is **circular** over the full complex baseband (SPEC §1.1): channel 0 and the highest index are neighbours in frequency, and ±Nyquist meet in the middle of the index range. A front end that makes its own analytic signal from real audio (`manta-dsp::hilbert::HilbertTransformer`, behind `AudioIqSource`) leaks a negative-frequency image of each tone, and the image of a near-DC tone lands at a *high* channel index. Look for leakage near the index wrap, not beside the tone.

## Symptom

Before MAN-273, one clean live-audio tone spawned several tracks with churning IDs, some locked onto the same signal. The complex-IQ path (`decode`, every golden vector) has no Hilbert stage and never showed it.

## Cause and fix

The 129-tap FIR rejected images by only 43.1 dB at 750 Hz and 4–27 dB below 600 Hz. MAN-273 made two changes, and each one needs the other:

- **511 taps** (`HILBERT_TAPS`, evaluated sparsely over the 256 nonzero taps): 104.4 dB at 750 Hz, >= 70 dB (`HILBERT_MIN_IMAGE_REJECTION_DB`) across [600, 23 400] Hz at 48 kHz.
- **A spawn guard declared by the source**: `IqSource::analytic_guard_hz()` returns `hilbert_guard_hz(fs)` (600 Hz at 48 kHz) for audio sources and `0.0` for everything else. `listen` merges it with the internal `DetectorConfig::guard_hz` (not a TOML key) and `TrackManager` stops spawns on channels within the guard of DC or Nyquist, in the same two slots as MAN-196's trusted band — except that the Nyquist rim never vetoes a trusted channel: after `--capture-rate-hz` decimation the leak it guarded (at the 48 kHz input's Nyquist) is already filtered away, and at 6000 Hz the rim masked the top of the advertised band instead. Spawns only: a running track is never closed by it.

**The guard comes from the source, not the operator.** A complex-IQ source declares no guard because it has no Hilbert stage; do not add one to "be safe", because it costs 22 of 512 channels at 48 kHz. A channel is masked only when its whole band lies inside the guard, so the 600 Hz sidetone (channel 6, 515–609 Hz) still decodes; tones below about 515 Hz, like the 400–500 Hz sidetones some rigs default to, do not.

**The image clears its floor even at −104 dB.** The Hilbert FIR strips the real input's noise from the negative half of the spectrum as deeply as it strips the image, so a −750 Hz image still sits ~20 dB over its own channel's floor and promoted a second, interleaved-decoding track. An analytic source therefore also vetoes spawns on the circular mirror `(n − k) % n` (±1 channel) of an owned or stronger channel (`mirror_vetoes_spawn`, raw `hop.power`, 3 dB margin, never the smoothed SNR). It is enabled from `analytic_guard_hz() > 0.0`, never from `DetectorConfig::guard_hz`: on a complex-IQ source two real stations symmetric about the dial must not suppress each other.

**Testkit fixtures still use 129 taps** (`HILBERT_TAPS_M1_LEGACY`, Watterson and RTTY synthesis only), so changing `HILBERT_TAPS` moves no golden-vector byte. Do not "tidy" them onto the new constant.

Full rationale, measured tables and channel arithmetic: `docs/DECISIONS/2026-10-16-man273-hilbert-image-guard.md`.
