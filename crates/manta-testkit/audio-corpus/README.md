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

- `vp8geo_cw_48khz.wav` (30 MB) — the loadable fixture. Mono PCM16 @
  48 kHz, converted from `vp8geo_cw.mp3` via:
  `ffmpeg -i vp8geo_cw.mp3 -ac 1 -ar 48000 -c:a pcm_s16le vp8geo_cw_48khz.wav`.
  `manta-input::AudioIqSource` (used by both `manta decode` and `manta
  listen --source`) hard-requires exactly 48 kHz and does not resample —
  the original MP3 is 11,025 Hz and can't be loaded directly.
- `vp8geo_cw.mp3` (1.2 MB) — the original capture. ~5 min CW pileup on
  VP8GEO, listener tuning around within an SSB-bandwidth (~2.4-3 kHz)
  capture, with wideband noise present. Source: George, 2026-09-02.
  Not committed pending confirmed redistribution rights (see Provenance
  and licensing below) — regenerate the WAV above from a local copy.
- `wpx_cw_iq_96khz.wav` (389 MB) — WPX CW contest capture, last 10
  minutes, 96 kHz. Likely a true I/Q capture (stereo channels = I/Q), per
  George: "different stuff." Original filename `WPXCWLast10min96KHz.wav`,
  dated 2008-06-08. Source: George, 2026-09-02. Not committed: too large
  for this repo's git history without LFS, and also not format-compatible
  as-is (96 kHz vs. the required 48 kHz) if it's ever brought in.

## Ground truth

No verified ground truth (capture UTC, band/frequency, or a time-aligned
transcript) is available for either recording beyond what's stated above —
`vp8geo_cw.mp3`'s filename implies the pileup was calling VP8GEO, but that
is not confirmed against a transcript or RBN spots. Until a transcript or
enough capture metadata is supplied to recover one (per
`ARCHITECTURE.md:318-321`'s recorded-corpus strategy), treat these as
real-conditions robustness fixtures only — decoder output against them
cannot yet be scored for recall/precision or turned into regression
assertions.

## Provenance and licensing

Both files were received from George on 2026-09-02 as real-radio test
material for the manta audio corpus. **Redistribution rights are not
established**: provenance is recorded as "George" only, with no confirmed
ownership/authorship of the recording and no explicit license or
permission for redistribution under this repo's MIT/Apache-2.0 dual
license. Unlike the vendored data documented in
`crates/manta-spot/data/SOURCES.md`, this does not give downstream users a
basis for redistributing the recording — so the audio stays off git
entirely (not just out of this PR) until that's confirmed. If rights get
confirmed later, update this section with the license/permission
statement before re-adding the file to git.
