# Real-audio corpus

Real-world (non-synthetic) recordings for decode-robustness testing, as a
complement to the synthetic fixtures generated in-test (e.g.
`manta-testkit::keyer`, `manta-engine/tests/listen_audio.rs`). Existing
tests only exercise clean synthetic tones; these are for validating against
actual band conditions (SSB-bandwidth capture, pileup QRM, wideband noise,
real IQ).

## Files

- `vp8geo_cw.mp3` (1.2 MB, committed) — ~5 min CW pileup on VP8GEO, listener
  tuning around within an SSB-bandwidth (~2.4-3 kHz) capture, with wideband
  noise present. Source: George, 2026-09-02.
- `wpx_cw_iq_96khz.wav` (389 MB, **not committed**, see `.gitignore`) — WPX
  CW contest capture, last 10 minutes, 96 kHz. Likely a true I/Q capture
  (stereo channels = I/Q), per George: "different stuff." Original filename
  `WPXCWLast10min96KHz.wav`, dated 2008-06-08. Source: George, 2026-09-02.
  Kept local-only — too large for this repo's git history without LFS. Ask
  before re-adding via LFS (storage cost).

## Provenance

Both files received from George on 2026-09-02 as real-radio test material
for the manta audio corpus. No further metadata (station, band, receiver)
was provided beyond the descriptions above.
