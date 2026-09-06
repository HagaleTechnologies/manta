# Real-audio corpus

Real-world recordings for decode-robustness testing, as a complement to the
synthetic fixtures generated in-test (e.g. `manta-testkit::keyer`,
`manta-engine/tests/listen_audio.rs`). Those existing vectors already cover
synthetic-but-impaired signals — V4/V5 apply Watterson fading, V6 applies
QSB, V7 tests adjacent signals, V8/V8w render 50-signal pileups with AWGN,
timing jitter, and optional fading. What these recordings uniquely add is
real receiver and band artifacts (SSB-bandwidth capture, pileup QRM,
wideband noise, real IQ) that a synthetic scene generator doesn't model.

**None of the audio files themselves are committed** — see `.gitignore`.
Reasons differ per file (below); this directory only tracks documentation
and conversion tooling notes, not the bytes.

## Files (all local-only)

- `vp8geo_cw_48khz.wav` (30 MB) — mono PCM16 @ 48 kHz, converted from
  `vp8geo_cw.mp3` via:
  `ffmpeg -i vp8geo_cw.mp3 -ac 1 -ar 48000 -c:a pcm_s16le vp8geo_cw_48khz.wav`.
  Loadable **only** via `manta listen --source` (`AudioIqSource`, which
  hard-requires exactly 48 kHz mono real audio and does not resample — the
  original MP3 is 11,025 Hz and can't be loaded directly). **Not**
  loadable via `manta decode` (`WavIqSource` requires 2-channel I/Q; a
  mono file is rejected outright).
- `vp8geo_cw.mp3` (1.2 MB) — the original capture. ~5 min CW pileup on
  VP8GEO, listener tuning around within an SSB-bandwidth (~2.4-3 kHz)
  capture, with wideband noise present. Source: George, 2026-09-02.
  Not committed pending confirmed redistribution rights (see Provenance
  and licensing below) — regenerate the WAV above from a local copy.
- `wpx_cw_iq_96khz.wav` (389 MB) — WPX CW contest capture, last 10
  minutes, 96 kHz, 24-bit stereo. Likely a true I/Q capture (stereo
  channels = I/Q), per George: "different stuff." Original filename
  `WPXCWLast10min96KHz.wav`, dated 2008-06-08. Source: George,
  2026-09-02. Not committed: too large for this repo's git history
  without LFS. If it's a genuine I/Q WAV, the loading path is `manta
  decode` (`WavIqSource`), not `listen --source` — `WavIqSource` preserves
  the file's native sample rate (96 kHz is fine as-is, no resample
  needed) but only reads Float32 or Int16 samples, so this 24-bit file
  needs a bit-depth conversion first; `WavIqSource` also eager-loads the
  whole file into memory (`crates/manta-input/src/lib.rs`'s own doc
  comment assumes files "<~100 MB"), so at 389 MB this is oversized for
  that path's documented design assumption regardless of bit depth.
- `B2_20251129_000000_7080kHz.wav` (1.0 GB) — first 15 minutes of CQ WW CW
  2025, 40 meters. Genuine stereo I/Q (2-channel PCM24 @ 192 kHz,
  00:14:59), captured by CWSL (`HrochL/CWSL`, per the file's own `encoder`
  RIFF chunk) — embedded metadata gives capture start `2025-11-29 00:00:00
  UTC` and center frequency 7,080,000 Hz. Source: George K5TR, received
  2026-09-06. Loading path is `manta decode` (`WavIqSource`, native 192
  kHz needs no resample), but like `wpx_cw_iq_96khz.wav` the 24-bit
  samples need a bit-depth conversion first (`WavIqSource` only reads
  Float32/Int16), and at 1.0 GB it's ~2.6x further past the "<~100 MB"
  eager-load design assumption than that file already was.
  `WavIqSource` also ignores the WAV's own embedded center-frequency
  metadata — it only reads a same-stem `<name>.json` sidecar
  (`crates/manta-input/src/lib.rs`) and silently defaults to 0.0 Hz when
  absent — so decoding the converted file needs a
  `B2_20251129_000000_7080kHz.json` sidecar with
  `{"center_freq_hz": 7080000}` alongside it, or spots/events will carry
  baseband-relative rather than correct RF frequencies.

## Ground truth

No verified ground truth (capture UTC, band/frequency, or a time-aligned
transcript) is available for any of these recordings beyond what's stated
above — `vp8geo_cw.mp3`'s filename implies the pileup was calling VP8GEO,
and `B2_20251129_000000_7080kHz.wav`'s embedded metadata gives a capture
timestamp and center frequency, but neither is confirmed against a
transcript or RBN spots. Until a transcript or enough capture metadata is
supplied to recover one (per `ARCHITECTURE.md:318-321`'s recorded-corpus
strategy), treat these as real-conditions robustness fixtures only —
decoder output against them cannot yet be scored for recall/precision or
turned into regression assertions.

## Provenance and licensing

`vp8geo_cw.mp3` and `wpx_cw_iq_96khz.wav` were received from George on
2026-09-02; `B2_20251129_000000_7080kHz.wav` was received from George
K5TR on 2026-09-06 — same source. All as real-radio test material for the
manta audio corpus. **Redistribution rights are not established** for any
of them: provenance is recorded as George (K5TR) only, with no confirmed
ownership/authorship of the recordings and no explicit license or
permission for redistribution under this repo's MIT/Apache-2.0 dual
license. Unlike the vendored data documented in
`crates/manta-spot/data/SOURCES.md`, this does not give downstream users a
basis for redistributing the recordings — so the audio stays off git
entirely (not just out of this PR) until that's confirmed. If rights get
confirmed later, update this section with the license/permission
statement before re-adding a file to git.
