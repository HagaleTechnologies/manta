---
id: hilbert-image-guard
title: Why did the live-audio path spawn dozens of spurious tracks on one clean tone?
kind: gotcha
status: current
maintainer: agent
sources:
  - docs/DECISIONS/2026-09-04-man-4-hilbert-guard-pins.md
  - crates/manta-dsp/src/hilbert.rs
  - crates/manta-engine/src/track.rs
verified:
  commit: e047bb1
  date: 2026-09-06
links:
  - detector-tracks
  - pfb-channelizer
---
The channelizer's channel index is a **circular** FFT-bin ordering over the full complex baseband (SPEC §1.1) — channel 0 and the highest channel index are genuinely frequency-adjacent, and the ±Nyquist bins meet at the midpoint. "Near DC" and "high channel index" are the same physical region. A front end that synthesizes its own analytic (complex) signal from real input — `manta-dsp::hilbert::HilbertTransformer`, used by `AudioIqSource`/`LoopingAudioIqSource` — has weak negative-frequency image rejection near DC and near Nyquist, and that leaked image of a near-DC real tone lands at a *high* channel index, not a low one. Miss the circularity and you'll go looking for the leakage in the wrong place.

## Symptom

`manta-engine::track::TrackManager` (SPEC §2's real per-channel detector) spawns dozens of spurious tracks from a single clean live-audio tone, several locking onto the exact same signal in parallel with churning track IDs (MAN-4, GitHub issue #21). The complex-IQ path (`decode_samples`/`decode_wav`, every golden vector) is completely unaffected — no Hilbert transform in that path at all.

## Cause and workaround

Two independent, non-overlapping fixes were required (a later reader must not decide one is redundant with the other):

- **Widen the Hilbert FIR** (`HILBERT_TAPS` 129 → 511) so real image rejection actually clears a usable floor (`HILBERT_MIN_IMAGE_REJECTION_DB`) across a declared band `[HILBERT_GUARD_HZ, fs/2 - HILBERT_GUARD_HZ]`.
- **A source-declared DC/Nyquist spawn guard** (`IqSource::analytic_guard_hz`, `DetectorConfig::guard_hz`): the guard alone doesn't fix it, because a tone's own negative-frequency image sits at the *same* `|offset|` from DC as the tone itself — a frequency guard can't suppress the image without also suppressing the real signal when the real signal is close enough to DC. That residual needed a third mechanism: an active-track-aware mirror-image suppressor (`TrackManager::is_image_of_owned`/`merge_mirror_images`) that compares raw per-hop power (not EMA-smoothed SNR — the smoothed statistic ramps up at every mark onset and flip-flops right when it matters) between a channel and its circular mirror.

**Don't conflate "operator widened `guard_hz`" with "this front end needs mirror suppression".** They're different facts — a genuine complex-IQ source (SoapySDR/KiwiSDR) can legitimately widen `guard_hz` to suppress an LO-leakage DC spur without ever running a Hilbert front end, and turning on mirror-pair suppression in that case would make two real stations symmetric about the dial frequency suppress each other. Gate mirror suppression on the source's own `analytic_guard_hz() > 0.0` (`DetectorConfig::mirror_image_guard`), never on the merged `guard_hz`.

**When comparing two channels' raw power to decide a winner (spawn-block or merge-loser), use a margin, not a bare `<=`/`>=`.** During a key-up gap both sides of a pair sit near the noise floor and can differ by fractions of a dB — a bare comparison is a coin flip that can close the real, currently-HANG-state ACTIVE track. A few dB of hysteresis (well under the ~90 dB steady-state real/image gap) lets an ambiguous comparison defer instead of firing on noise.

Full rationale, rejected alternatives, and the exact guard-band channel arithmetic: `docs/DECISIONS/2026-09-04-man-4-hilbert-guard-pins.md`.
