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
  baseband-relative rather than correct RF frequencies. **Exact
  conversion command** (Codex review, PR #144 -- without a pinned command
  and checksums, "this recording, unmodified pipeline" doesn't actually
  identify the benchmark input; bit-depth/normalization choices in the
  conversion can shift weak-signal samples and so the reported spot
  count):
  `ffmpeg -i B2_20251129_000000_7080kHz.source.wav -c:a pcm_s16le B2_20251129_000000_7080kHz.wav`
  (distinct input/output paths -- ffmpeg refuses to edit a file in place;
  rename the as-received source to the `.source.wav` stem above, or point
  `-i` at wherever it actually lives, before running this)
  (source, as received from George K5TR, unmodified: SHA-256
  `74eba71e00fede9cab8067f9ff5a043396066015bbf8c23232788279b9f1c5f2`;
  converted, this repo's benchmark input: SHA-256
  `d2ea524744fd9d551bd4e5f09103a1771588eb11d37054c4334078edd4d4a621`).

## Ground truth

`B2_20251129_000000_7080kHz.wav` has real ground truth: `ground-truth/
B2_20251129_000000_7080kHz.rbn.csv` is the Reverse Beacon Network's own raw
spot dump (`data.reversebeacon.net/rbn_history/20251129.zip`, RBN's public
daily archive) pre-filtered to this recording's exact window --
2025-11-29 00:00:00-00:14:59 UTC, 6984-7176 kHz (the file's embedded
center freq ± half its 192 kHz capture bandwidth) -- 26,801 raw skimmer
spot lines covering 957 unique DX callsigns / 1,451 unique call+kHz bins,
straight from real skimmers copying the real air during CQ WW CW 2025.
Score a `manta decode --json` report against it with
`scripts/score-against-rbn.py` (repo root); see that script's docstring
for usage, including `--spotter`/`--min-spotters` filtering -- spotter
filtering is useful for co-located reference validation (K5TR's signal
path is nearly identical to manta's hardware); consensus filtering
(multi-spotter agreement) is useful for rejecting receiver artifacts that
don't appear on multiple independent systems. This is the ARCHITECTURE.md
§9 "Golden IQ corpus" benchmark (recorded band segment + RBN's own spots
as reference labels -> recall/precision) for real M3-grade validation, not
just synthetic fixtures.

First baseline run (2026-09-08, this recording, unmodified pipeline):
**197 manta spots vs. 1,441 scoreable RBN truth bins -> 29.9% precision,
4.1% recall** (revised after a Codex review on PR #144 caught two real
scorer bugs -- unbounded-time matching, and RBN truth rows manta's own
grammar can structurally never accept counted as misses; both fixed in
`scripts/score-against-rbn.py`, see that script's docstring), alongside
41,174 distinct tracks opened over the 15 minutes (613k characters
decoded) for those 197 spots -- the detector is opening far more tracks
than the real simultaneous-signal count implies, and most never survive
to a validated spot. Filed as MAN-166.

**Legacy-vs-hsmm and K5TR-only comparison**: see
`docs/DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md` §2 for the
canonical follow-up measurement against this same recording (MAN-166's
decode-core-v2 stage-2 gate) -- both engines, both K5TR-only and all-RBN
recall/precision. Its all-RBN legacy figures (59/1441 = 4.09% recall,
197 spots -> 29.95% precision) are the same measurement as the baseline
above, to within rounding; don't duplicate a second, independently-drifting
copy of these numbers here -- point to that doc instead.

`vp8geo_cw.mp3` and `wpx_cw_iq_96khz.wav` have no verified ground truth
(no confirmed capture UTC/band/frequency, no time-aligned transcript) --
`vp8geo_cw.mp3`'s filename implies the pileup was calling VP8GEO, and
`wpx_cw_iq_96khz.wav`'s filename implies a WPX CW contest capture, but
neither is confirmed against a transcript or RBN spots, and WPX's own WAV
header carries no embedded metadata to anchor a date (checked directly --
`fmt ` chunk runs straight into `data`, no LIST/INFO/bext chunk; the
zip's internal timestamp, 2008-06-08, doesn't land on a CQ WPX CW contest
weekend, so it's presumed to be a save/copy date, not a capture date).
Both still decode to plausible real callsigns (WPX: 45 spots / 41 unique
calls; vp8geo: 2,060 tracks / 12k characters decoded, no crashes) --
useful as real-audio robustness/regression baselines (crash/hang/throughput,
"still decodes something plausible"), just not scoreable for
recall/precision until a transcript or capture metadata turns up.

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
