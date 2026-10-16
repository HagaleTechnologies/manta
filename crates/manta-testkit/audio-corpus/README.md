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
Reasons differ per file (below). This directory tracks the documentation,
`manifest.json` and the ground truth, not the audio.

## Using the corpus (MAN-119)

- `manifest.json` lists each recording: file name, `kind` (`iq` or
  `audio`), sample rate, bit depth, centre frequency when known, ground
  truth, score floors, and a SHA-256 pin. Only pinned entries are ever
  downloaded.
- `scripts/corpus.py fetch --dest DIR` downloads every pinned recording
  from `<base-url>/<file>` and checks its SHA-256. The base URL comes from
  `--base-url` or `MANTA_CORPUS_BASE_URL`.
- `scripts/corpus.py score --corpus-dir DIR` decodes every manifest
  recording present in DIR, scores it against its ground truth, and exits
  non-zero on a failed decode or a missed floor.
- `MANTA_CORPUS_DIR=DIR cargo test -p manta-testkit --test
  real_audio_corpus -- --nocapture` streams every IQ recording present in
  DIR end to end through `WavIqSource` and checks its header against the
  manifest. DIR must be an absolute path, because cargo runs the test from
  `crates/manta-testkit`. With `MANTA_CORPUS_DIR` unset it only parses the
  manifest.
- `.github/workflows/corpus.yml` runs fetch, the test and score nightly. It
  is skipped until a maintainer sets the `MANTA_CORPUS_BASE_URL` repository
  variable, which waits on redistribution rights (see *Provenance and
  licensing*).

Steps for each of these, including hosting and transcribing:
[docs/RUNBOOKS/real-audio-corpus.md](../../../docs/RUNBOOKS/real-audio-corpus.md).
Decisions:
[docs/DECISIONS/2026-10-14-man119-streaming-wav-and-real-audio-corpus.md](../../../docs/DECISIONS/2026-10-14-man119-streaming-wav-and-real-audio-corpus.md).

## Files (all local-only)

- `vp8geo_cw_48khz.wav` (30 MB) — mono PCM16 @ 48 kHz, 323 s, converted
  from `vp8geo_cw.mp3` via:
  `ffmpeg -i vp8geo_cw.mp3 -ac 1 -ar 48000 -c:a pcm_s16le vp8geo_cw_48khz.wav`.
  Loadable only as real audio, via `manta run --source` or `manta listen
  --source` (`AudioIqSource`, which requires exactly 48 kHz mono and does
  not resample; the original MP3 is 11,025 Hz and can't be loaded
  directly). Not loadable via `manta decode`: `WavIqSource` requires
  2-channel I/Q and rejects a mono file. Not pinned in `manifest.json`
  yet.
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
  without LFS. `manta decode` reads it as received: `WavIqSource` reads
  24-bit PCM and streams the file, so no conversion is needed and memory
  does not grow with its size. It has no capture metadata, so there is no
  centre frequency: `decode` warns and reports baseband-relative
  frequencies. Not pinned in `manifest.json` yet; pin it before uploading
  it (runbook §2).
- `B2_20251129_000000_7080kHz.source.wav` (1.0 GB) — first 15 minutes of
  CQ WW CW 2025, 40 meters, as received. Genuine stereo I/Q (2-channel
  PCM24 @ 192 kHz, 00:14:59), captured by CWSL (`HrochL/CWSL`, per the
  file's own `encoder` RIFF chunk). Its embedded metadata gives capture
  start `2025-11-29 00:00:00 UTC` and center frequency 7,080,000 Hz.
  Source: George K5TR, received 2026-09-06. SHA-256
  `74eba71e00fede9cab8067f9ff5a043396066015bbf8c23232788279b9f1c5f2`,
  pinned in `manifest.json`. This file is the benchmark input: `manta
  decode` reads it natively (24-bit, streamed, native 192 kHz), and
  `scripts/corpus.py score` passes `--center-freq-hz 7080000` from the
  manifest. `WavIqSource` ignores the WAV's embedded center-frequency
  metadata; it only reads a same-stem `<name>.json` sidecar or
  `--center-freq-hz`, and without either it reports baseband-relative
  frequencies. Decoding it by hand therefore needs
  `--center-freq-hz 7080000` or a `B2_20251129_000000_7080kHz.source.json`
  sidecar with `{"center_freq_hz": 7080000}`.
- `B2_20251129_000000_7080kHz.wav` — the PCM16 conversion the 2026-09-08
  and 2026-09-09 baselines below were measured on, from before
  `WavIqSource` read 24-bit samples. It is no longer needed. It was made
  with (Codex review, PR #144, which asked for a pinned command and
  checksums so "this recording, unmodified pipeline" identifies one input):
  `ffmpeg -i B2_20251129_000000_7080kHz.source.wav -c:a pcm_s16le B2_20251129_000000_7080kHz.wav`
  (converted SHA-256
  `d2ea524744fd9d551bd4e5f09103a1771588eb11d37054c4334078edd4d4a621`),
  with a `B2_20251129_000000_7080kHz.json` sidecar holding
  `{"center_freq_hz": 7080000}`. Scores from the native 24-bit file and
  from this conversion can differ slightly, because the conversion
  rounds weak-signal samples.

## Ground truth

B2 has real ground truth: `ground-truth/
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

The baselines below were measured on the PCM16 conversion with the legacy
engine, and decoding has changed since (MAN-172, MAN-115, MAN-213,
MAN-315, MAN-312, MAN-314). The `manifest.json` floors are set well below
them as catastrophe floors until the first hosted nightly run re-measures
B2 on the native file.

First baseline run (2026-09-08, the PCM16 conversion, unmodified pipeline):
**197 manta spots vs. 1,441 scoreable RBN truth bins -> 29.9% precision,
4.1% recall** (revised after a Codex review on PR #144 caught two real
scorer bugs -- unbounded-time matching, and RBN truth rows manta's own
grammar can structurally never accept counted as misses; both fixed in
`scripts/score-against-rbn.py`, see that script's docstring), alongside
41,174 distinct tracks opened over the 15 minutes (613k characters

Follow-up (2026-09-09, `docs/DECISIONS/
2026-09-09-man166-confirm-hops-and-track-cap.md`): root-caused the
41,174-tracks figure to two structural closure reasons
(`TrackManager::close_counts()`, `crates/manta-engine/examples/
close_counts.rs`) -- `unconfirmed` (Candidate never sustains
`confirm_hops`, 68.8% of all churn) and `evicted`/`merged`
(`track_cap=500` pinned at its ceiling the entire recording, 29.7%). Fixed
a real repetition-gate bug (was keyed by the ephemeral `track_id`, so a
track closing and reopening under real fragmentation reset its
confirmation count every time) and raised `track_cap` to 1200 (measured
real organic peak demand: 886). Result: **198 spots, 31.8% precision, 4.4%
recall** -- a small but genuine improvement on both metrics, zero
regressions across the full workspace test suite. `confirm_hops` itself
was investigated and left unchanged: the dit-timing mismatch that causes
the `unconfirmed` storm is real, but lowering the constant breaks a
different golden vector (V10, Farnsworth timing) via an unrelated
decoder-side coupling -- see the decision doc. Two deeper, unfixed
findings from this pass: `merge_converged`'s SPEC §2.5 "one signal one
track within 1.0 channel" assumption doesn't hold in dense real contest
packing, and most surviving real spots still reflect only one clean
decode ever captured (not a repetition/tracking problem) -- pointing at
raw decode-core accuracy under genuine HF noise as the dominant remaining

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

### Transcript ground truth

A recording with no RBN reference, such as VP8GEO, is scored against a hand
transcript: `ground-truth/<recording-stem>.transcript.csv`. The file is
UTF-8 CSV with a header row; lines starting with `#` are comments.

| column | meaning |
|---|---|
| `start_s` | seconds from the recording's first sample when the transmission starts (float ≥ 0) |
| `end_s` | when it ends (≥ `start_s`) |
| `pitch_hz` | approximate audio tone in Hz, read from a spectrogram; empty when unknown |
| `text` | what was sent, uppercase `A–Z 0–9 / ? . , =`, single spaces between words. Write only copy you are sure of; leave unreadable characters and words out (no placeholders) |

Example:

```
start_s,end_s,pitch_hz,text
12.4,15.9,640,VP8GEO UP
16.2,17.5,710,K5TR
18.0,20.1,640,K5TR 5NN
```

`scripts/score-transcript.py` scores a `manta run --source <file> --json`
capture against it: spot precision, call recall and word recall, within 10 s
and 100 Hz by default. Spot precision is a lower bound, because a transcriber
can miss weak callers that manta copied. Once the manifest entry has
`"truth": {"kind": "transcript", ...}`, `scripts/corpus.py score` runs it.
The `ground-truth/` directory is tracked in git, so a new transcript there is
not ignored. How to transcribe VP8GEO and record its first numbers:
[docs/RUNBOOKS/real-audio-corpus.md §5](../../../docs/RUNBOOKS/real-audio-corpus.md#5-transcribing-vp8geo).
No transcript exists yet.

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
statement before re-adding a file to git. That statement (George K5TR's
redistribution permission) must be recorded here before any recording is
uploaded to a public host or `MANTA_CORPUS_BASE_URL` is set.
