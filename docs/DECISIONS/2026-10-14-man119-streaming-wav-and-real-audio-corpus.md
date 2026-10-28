# 2026-10-14 — MAN-119: streaming IQ WAVs, 24/32-bit PCM, and the real-audio corpus as a CI fixture

**Status:** Implemented (branch `MAN-119`). The code, tests, manifest, scripts
and nightly workflow are in. Four external steps stay outstanding and are
human-only ([E1–E4](#external-steps-outstanding-human-only)): redistribution
permission for the recordings, hosting them, tightening B2's floors after the
first hosted run, and the VP8GEO hand transcript. Until a maintainer sets the
`MANTA_CORPUS_BASE_URL` repository variable, `corpus.yml` is skipped. Steps:
[docs/RUNBOOKS/real-audio-corpus.md](../RUNBOOKS/real-audio-corpus.md).

## Context

The ticket: manta's real-audio test corpus should be usable as an actual CI
fixture. Its first scenario was that `manta decode` could not read the
corpus's IQ recordings at all. Both are 24-bit PCM, and `WavIqSource` read
only Float32 and 16-bit integer samples. Its second was scoring a recording
against ground truth. B2's RBN ground truth landed in PR #144 and was scored
by hand on a PCM16 conversion. VP8GEO still has no ground truth.

Reproduced at `9636fe0` with `manta gen v1` re-encoded as stereo PCM24 at
96 kHz (`v1_24.wav`, 69 MB, with its sidecar):

`manta decode v1_24.wav` failed with exit 1 and
`error: unsupported WAV format Int/24-bit`, the message naming Float32 and
Int16 as the accepted formats. The Float32 original decodes. A WAV whose header declares 1000
Float32 frames but holds 10 failed inside `open`, naming no file:
`error: Failed to read enough bytes.`

The second problem was memory. `WavIqSource::open` collected every sample
into a `Vec<Complex32>`, an M0 choice for fixtures "<~100 MB" (M0 pin 15).
`decode_wav` then copied the file twice more, once through `read_all` and once
into a zero-padded buffer. B2 is 1.04 GB of PCM24; as Complex32 it is 1.4 GB,
and decode held about twice that. Peak RSS of `manta decode`, dev profile
(optimised), measured on a prototype of this change:

| Input | Before | Source streams, decode still copies | Source and decode stream |
|---|---|---|---|
| V1, Float32 stereo 96 kHz, 92 MB | 281 MiB | 193 MiB | **18 MiB** |
| WPX-shaped, PCM24 stereo 96 kHz, 389 MB, 675 s | fails: unsupported 24-bit | 1007 MiB, 38.7 s | **18 MiB**, 33.5 s |
| B2-shaped, PCM24 stereo 192 kHz, 1.04 GB, 899 s | fails: unsupported 24-bit | 2654 MiB, 105.6 s | **19 MiB**, 96.9 s |

Under a 96 MiB address-space limit (`RLIMIT_AS`) on V1 Float32, the old build
and the source-only build abort (SIGABRT). The fully streaming build
completes, and also completes under 32 MiB. Its `--json` output on V1 Float32
is byte-identical to the old build's (SHA-256 `9230ecc8…ed501` for both).

hound 3.5.1, already pinned in `Cargo.lock`, does the format work: its
`Sample for i32` reads 16-bit, 24-bit (3- or 4-byte containers) and 32-bit
samples, it takes `wValidBitsPerSample` from `WAVE_FORMAT_EXTENSIBLE`
headers, and `WavReader::into_samples` gives an owned lazy iterator that a
source can hold.

## Decisions

**D1 — Formats.** Float32, plus integer PCM at 16, 24 or 32 bits in any
container hound accepts (plain PCM, `WAVE_FORMAT_EXTENSIBLE`, 24-bit samples
in 4-byte containers). Integers are read through `i32` and divided by
2^(bits−1) as `f32`. For 16-bit this equals the old `v as f32 / 32768.0`
exactly, so 16-bit input decodes as before. 8-bit integer (unsigned in WAV)
and 64-bit float stay rejected. The error now reads
`unsupported WAV format Int/8-bit (need Float32 or Int16/24/32)`. 32-bit
integer was accepted along with 24-bit because it is the same code path with
a different divisor.

**D2 — Streaming source.** `WavIqSource::open` checks the header in the old
order (two channels, then the 10 MS/s rate ceiling from MAN-269, then the
sample format) and keeps `reader.into_samples()`; it reads no samples. Each
`read` decodes only the frames it returns and fills the whole buffer unless
it reaches EOF. A data chunk holding half a frame is still refused at `open`
(hound checks the sample count against the channel count in the header), so
`read` only drops a lone trailing I sample defensively. A sample error
is returned from `read` with the context `read WAV <path>`, so a truncated
data chunk now fails when the read reaches the missing data and names the
file: `error: read WAV trunc.wav: Failed to read enough bytes.` It still
exits 1 with no report; a partial recording is never scored silently.

**D3 — Streaming decode.** `manta_engine::decode_source(src: &mut dyn
IqSource, cfg: &PipelineConfig) -> Result<DecodeReport>` reads a source to
EOF and holds no copy of it. It feeds the channelizer the zero pre-roll and
then the source's samples in chunks of exactly 4096 samples (`CHUNK_SAMPLES`),
whatever sizes the source's reads return; only the stream's last chunk may be
shorter. The boundaries matter: `TrackManager`'s GC timer resets once per
`process_hops` call, so different chunking would change output.
`decode_samples` is now a thin wrapper that runs `decode_source` over a
private slice source, and `decode_wav` calls `decode_source` directly. The
digital-silence check moved from before decoding to EOF. One precedence
change follows: a silent recording at a rate the channelizer rejects now
reports the channelizer error instead of `input is digital silence`.
`manta oracle` keeps `read_all`, because it needs random access to windows
around each RBN spot; it still gains 24/32-bit input through `WavIqSource`.

**D4 — Hosting.** The tooling is host-agnostic: `scripts/corpus.py fetch`
downloads `<base-url>/<file>`, with the base from `--base-url` or
`MANTA_CORPUS_BASE_URL`, and checks every file's SHA-256 against
`manifest.json`. The intended host, once rights are confirmed, is a
`corpus-v1` prerelease on this repository, not marked latest. The release
workflows trigger only on `v[0-9]*` tags, so the tag builds nothing. Release
assets have no bandwidth cap and a 2 GiB per-file limit; B2 is 1.04 GB. Git
LFS was rejected: its free 10 GiB a month runs out in about a week of
uncached 1.4 GB nightly pulls, and a clone with git-lfs installed would pull
the audio by default. Cloudflare R2 is kept for an M3 corpus of 2 h or more,
whose files would pass the 2 GiB limit (FU-5).

**D5 — Rights gate.** This change uploads nothing. Redistribution rights are
not established for any recording (corpus README, *Provenance and
licensing*). `corpus.yml`'s job runs only when `MANTA_CORPUS_BASE_URL` is set,
and the runbook makes recording the volunteer operator's permission in the
corpus README the step before any upload or variable change.

**D6 — Consumption.** A dedicated `.github/workflows/corpus.yml` runs
nightly at `41 7 * * *` and on `workflow_dispatch`. It is not a required
check. It has `permissions: contents: read`, no build cache (only
`ci-full.yml` may restore one, MAN-243), `timeout-minutes: 120`, and uploads
`corpus-results.json` as the `corpus-results` artifact. It fetches, builds,
runs `crates/manta-testkit/tests/real_audio_corpus.rs` with
`MANTA_CORPUS_DIR` set, then runs `scripts/corpus.py score`. The required PR
legs run only that test's skip path, which still parses the manifest, plus the
Python unit tests for the corpus scripts.

**D7 — Floors.** B2: `min_spots` 50, `min_precision` 0.20, `min_recall`
0.02, against all RBN spotters. WPX: `min_spots` 10 (45 spots were recorded
on its PCM16 conversion). These are catastrophe floors at about two thirds or
less of the last B2 baseline (198 spots, 31.8 % precision, 4.4 % recall,
2026-09-09, PCM16 conversion, legacy engine). That baseline is stale: MAN-172,
MAN-115, MAN-213, MAN-315, MAN-312 and MAN-314 have changed decoding since.
The K5TR-only score is reported but not gated. FU-1 tightens the floors after
the first hosted run.

**D8 — Ground-truth directory.** `.gitignore` no longer ignores
`crates/manta-testkit/audio-corpus/ground-truth/`. Its only content, B2's RBN
CSV, was already tracked, and a transcript added there would have been
ignored silently.

Transcript ground truth follows the same pattern. The format is
`ground-truth/<recording-stem>.transcript.csv` with columns `start_s`,
`end_s`, `pitch_hz` and `text`, documented in the corpus README.
`scripts/score-transcript.py` scores a `manta run --source <file> --json`
capture against it: spot precision, call recall and word recall, within
10 s and 100 Hz by default. A manifest entry with `"truth": {"kind":
"transcript", …}` is scored by `corpus.py score`.

## Consequences

- `manta decode`, `manta oracle` and `run`/`check --source-iq` read 24- and
  32-bit integer IQ WAVs, so 24-bit captures such as CWSL's are read as
  received; the PCM16 conversion step for B2 and WPX is gone.
- `manta decode`'s memory no longer grows with the recording: the B2-shaped
  1.04 GB file decodes in 19 MiB.
- `run --loop` re-opens a recording without reading it whole, so each pass
  starts at once and holds no second copy. MAN-269 recorded "No change to
  `WavIqSource`'s eager whole-file load; `--loop` re-reads the file once per
  pass"; that is superseded.
- The `--source-iq` sidecar probe (`source_iq_has_real_rf_center`) opens the
  file only to read its sidecar, instead of loading every sample.
- M0 pin 15's "files are <~100 MB" assumption no longer limits any WAV
  input.
- A truncated WAV opens and decodes up to the missing data, then fails with
  exit 1, naming the file.
- A silent recording at a rate the channelizer rejects reports the
  channelizer error, not `input is digital silence` (D3).
- Decoder output is unchanged: spots from every input that decoded before
  are byte-identical, and 24/32-bit files previously produced no output. No
  `### Decoder output` CHANGELOG entry.
- B2's benchmark input is now the as-received `.source.wav` (SHA-256
  `74eba71e…c5f2`) scored with `--center-freq-hz 7080000`. Its scores may
  differ slightly from the PCM16 conversion's.
- The real B2 and WPX headers have not been read by this code: the files are
  not in any container. hound skips unknown RIFF chunks and reads extended
  `fmt ` chunks, but CWSL's extra chunks are unverified. The first hosted
  run of the gated test is the check, and a failure there is a reader bug to
  fix, not a reason to convert the files again.

## Verification

- `manta-input` unit tests (`crates/manta-input/src/lib.rs`):
  `reads_i24_wav_normalized`,
  `reads_pcm24_wav_without_an_extensible_header`,
  `reads_24_bit_samples_in_4_byte_containers`, `reads_i32_wav_normalized`,
  `open_reads_only_the_header`, `a_trailing_half_frame_is_rejected_at_open` and
  `eight_bit_and_float64_wavs_are_rejected_naming_the_formats`, with
  `unsupported_wav_text_names_sample_formats` updated to the new text.
- `crates/manta-engine/tests/decode_source.rs`: `decode_source`'s report is
  identical to `decode_samples`' for source read sizes 1, 7, 4095, 4096, 4097
  and 65,536, compared as `Debug` text (every field, the serde-skipped ones
  too, each float in its shortest round-trip form, so equal text implies
  equal JSON; `serde_json` is not a `manta-engine` dependency); silence, an
  empty source and a read error are reported.
- `crates/manta-cli/tests/decode_wav_formats.rs`: a 24-bit render of V1
  decodes the same text and spot callsigns as the Float32 render; a truncated
  WAV fails naming the file; an 8-bit WAV fails naming the accepted formats;
  `decode_runs_in_bounded_memory` (Linux) decodes the 120 s V1 Float32 render
  under a 96 MiB address-space limit, with `RAYON_NUM_THREADS=2`: each rayon
  worker reserves its own stack, so a one-per-CPU decoder pool on a 32-CPU
  machine fails to start under the limit before any sample is read.
- `crates/manta-testkit/tests/real_audio_corpus.rs`: always parses the
  manifest; skips when `MANTA_CORPUS_DIR` is unset; otherwise streams every
  present IQ recording to EOF and checks its header and frame count.
- `scripts/tests/test_corpus.py`: the manifest's shape and B2's pin and
  floors; `fetch` over `file://` (download, skip when verified, hash
  mismatch, unpinned entries, no base URL); `score` against a stub `manta`;
  `corpus.yml`'s shape (schedule, gate, permissions, timeout, no `vars` in a
  `run:` body).
- `scripts/tests/test_score_transcript.py`: the transcript parser, word
  assembly from `CharDecoded`/`WordBoundary`/`TrackClosed`, each metric and
  its edge cases, the `--json` keys and the exit codes.
- The existing golden suites (V1–V10, VR, VW, decimated capture) run
  unchanged through `decode_wav`.

### External steps (outstanding, human-only)

These are not done and cannot be done from a container. They are not
acceptance criteria for this change.

- **E1.** Obtain the volunteer operator's redistribution permission and record
  it in the corpus README's *Provenance and licensing* section.
- **E2.** Upload the as-received B2 source (and WPX, after pinning its hash in
  `manifest.json`) to the `corpus-v1` prerelease, set the
  `MANTA_CORPUS_BASE_URL` repository variable, and trigger `corpus.yml` once
  with `workflow_dispatch`.
- **E3.** Tighten B2's floors to the first hosted run's measured values minus
  a margin (FU-1).
- **E4.** Hand-transcribe at least 300 s of `vp8geo_cw_48khz.wav` in the
  documented format, commit it, add the manifest `truth`, and record the
  first VP8GEO precision number.

## Follow-ups (not in this change)

- **FU-1.** Tighten every floor after the first hosted nightly run (E3).
- **FU-2.** Read CWSL's embedded centre frequency from the WAV, so B2 needs
  no `--center-freq-hz` or sidecar. Until then the manifest supplies it.
- **FU-3.** A chunked `manta oracle`, so it no longer loads the whole file
  for its random-access windows.
- **FU-4.** MAN-33's FU-3: measure the RST/QRL confusion rate once the
  VP8GEO transcript exists.
- **FU-5.** Cloudflare R2 hosting for an M3 corpus of 2 h or more, whose files
  pass the 2 GiB release-asset limit.

## References

- Runbook: [docs/RUNBOOKS/real-audio-corpus.md](../RUNBOOKS/real-audio-corpus.md).
- Corpus: [crates/manta-testkit/audio-corpus/README.md](../../crates/manta-testkit/audio-corpus/README.md),
  `crates/manta-testkit/audio-corpus/manifest.json`.
- Code: `crates/manta-input/src/lib.rs` (`WavIqSource`);
  `crates/manta-engine/src/lib.rs` (`decode_source`, `decode_samples`,
  `decode_wav`); `scripts/corpus.py`; `scripts/score-transcript.py`;
  `scripts/score-against-rbn.py` (`score_report`);
  `.github/workflows/corpus.yml`.
- hound 3.5.1: `src/lib.rs` (`Sample for i32`), `src/read.rs`
  (extensible headers, `into_samples`).
- Origin: the 2026-09-05 review, lens 3 (decode quality) §6 item 6, and the
  broad review's D-17 and D-23.
- Superseded points: M0 pin 15's file-size assumption
  ([2026-07-11 M0 implementation pins](2026-07-11-m0-implementation-pins.md));
  the eager-load note in
  [MAN-269 paced replay](2026-10-10-man269-paced-looping-replay.md).
- Precedents for declared external steps:
  [MAN-96 field node](2026-10-07-man96-secondary-skimmer-field-node.md),
  [MAN-140 ka9q-radio input](2026-10-13-man140-ka9q-radio-input.md).
- CI rules: [MAN-243 CI trust boundary](2026-10-10-man243-ci-trust-boundary.md).
- GitHub docs (checked 2026-10-14):
  [Git LFS billing](https://docs.github.com/billing/managing-billing-for-git-large-file-storage/about-billing-for-git-large-file-storage),
  [About releases](https://docs.github.com/en/repositories/releasing-projects-on-github/about-releases).
