# Runbook: the real-audio corpus (MAN-119)

The real-audio corpus is a few off-air recordings that manta decodes and
scores against ground truth: B2 (15 minutes of CQ WW CW 2025 on 40 m, IQ,
with RBN's own spots as truth), WPX (10 minutes of a WPX CW contest, IQ, no
truth) and VP8GEO (5 minutes of a pileup, mono audio, truth pending a hand
transcript). The audio is not in git. `crates/manta-testkit/audio-corpus/manifest.json`
pins each file by SHA-256 and holds its score floors.

For: contributors with a local copy of the recordings, and the maintainer
who hosts them for the nightly `corpus.yml` workflow.

Corpus details and provenance:
[crates/manta-testkit/audio-corpus/README.md](../../crates/manta-testkit/audio-corpus/README.md).
Decision record:
[2026-10-14-man119-streaming-wav-and-real-audio-corpus](../DECISIONS/2026-10-14-man119-streaming-wav-and-real-audio-corpus.md).

## 1. Using a local copy

Put the recordings in one directory under the names `manifest.json` gives
them. B2 is the as-received file, `B2_20251129_000000_7080kHz.source.wav`,
not the older PCM16 conversion. `crates/manta-testkit/audio-corpus/` itself
works: `.gitignore` keeps the audio there out of git.

Check a pinned file is the one the manifest means:

```sh
sha256sum crates/manta-testkit/audio-corpus/B2_20251129_000000_7080kHz.source.wav
# 74eba71e00fede9cab8067f9ff5a043396066015bbf8c23232788279b9f1c5f2
```

Stream every IQ recording end to end through `WavIqSource`. The test checks
each file's channels, rate and bit depth against the manifest, that the
frame count equals the header's, and that the samples are non-zero and
within ±1.0. It prints one `<file>: <n> frames streamed` line per file:

```sh
MANTA_CORPUS_DIR="$PWD/crates/manta-testkit/audio-corpus" \
  cargo test --release --locked -p manta-testkit --test real_audio_corpus -- --nocapture
```

`MANTA_CORPUS_DIR` must be an absolute path. Cargo runs the test from
`crates/manta-testkit`, so a relative path resolves there and finds none
of the recordings.

With `MANTA_CORPUS_DIR` unset the test only parses the manifest and prints
`skipping: MANTA_CORPUS_DIR is not set`. Set to a directory holding none of
the recordings, it fails with `holds none of the manifest's recordings`.

Decode and score everything present:

```sh
cargo build --release --locked -p manta-cli
python3 scripts/corpus.py score \
  --corpus-dir crates/manta-testkit/audio-corpus \
  --manta target/release/manta \
  --json-out /tmp/corpus-results.json
```

`score` runs `manta decode <file> --json` for each IQ recording (with
`--center-freq-hz` when the manifest gives one) and `manta run --source
<file> --json` for each audio recording. It prints a Markdown table: exit
code, spot count and wall time per file, and for B2 the precision and recall
against all RBN spotters and against K5TR alone. Exit codes: 0 when every
decode succeeded and every floor held, 1 when a decode failed or a floor was
missed (the message names the floor), 2 when the directory holds no
manifest recording.

To decode B2 by hand, give its centre frequency; `WavIqSource` does not read
the frequency embedded in the WAV:

```sh
target/release/manta decode \
  crates/manta-testkit/audio-corpus/B2_20251129_000000_7080kHz.source.wav \
  --center-freq-hz 7080000 --json > b2.json
scripts/score-against-rbn.py b2.json \
  crates/manta-testkit/audio-corpus/ground-truth/B2_20251129_000000_7080kHz.rbn.csv \
  --capture-start 2025-11-29T00:00:00Z --sample-rate-hz 192000
```

`manta decode` streams the file, so a 1 GB recording decodes in under
20 MiB of memory. B2 takes several minutes in a release build.

## 2. Pinning a recording

`scripts/corpus.py fetch` downloads only entries with a `sha256`, so pin a
recording before uploading it. WPX and VP8GEO are not pinned yet.

1. Hash the exact file you will upload:
   ```sh
   sha256sum wpx_cw_iq_96khz.wav
   ```
2. In `manifest.json`, set the entry's `sha256` to that hash (64 lowercase
   hex characters). Check `sample_rate_hz` and, for `iq` entries,
   `bits_per_sample` (16, 24 or 32) against the file.
3. Run the manifest checks and the gated test on the file:
   ```sh
   python3 -m unittest scripts.tests.test_corpus -v
   MANTA_CORPUS_DIR="$(cd <dir> >/dev/null && pwd)" cargo test --release --locked -p manta-testkit --test real_audio_corpus -- --nocapture
   ```
   The gated test fails if the header disagrees with the manifest. The
   `cd <dir> >/dev/null && pwd` makes the path absolute, which the test
   needs; the redirect drops the path `cd` echoes when `CDPATH` is set.
4. Commit the manifest change. A pin changes only with the file: uploading a
   re-encoded or trimmed copy needs a new hash.

## 3. Hosting after rights are confirmed

Nothing is uploaded until George K5TR's redistribution permission is
recorded. Do the steps in order.

1. Record the permission in the corpus README's *Provenance and licensing*
   section, and merge that change. If the permission covers only some
   recordings, upload only those. Without it, stop here.
2. Create a prerelease that is not marked latest. The release workflows
   trigger only on `v[0-9]*` tags, so the `corpus-v1` tag builds nothing:
   ```sh
   gh release create corpus-v1 --prerelease --latest=false \
     --title "Test corpus (not a manta release)" \
     --notes "Real-audio test recordings for manta's nightly corpus.yml. Not a manta release. Provenance and licence: crates/manta-testkit/audio-corpus/README.md."
   ```
3. Upload the pinned files under their manifest names. Each release asset
   must be under 2 GiB:
   ```sh
   gh release upload corpus-v1 B2_20251129_000000_7080kHz.source.wav
   ```
4. Check the hosted copy matches its pin:
   ```sh
   python3 scripts/corpus.py fetch --dest "$(mktemp -d)" \
     --base-url https://github.com/HagaleTechnologies/manta/releases/download/corpus-v1
   ```
5. Set the repository variable that turns the workflow on:
   ```sh
   gh variable set MANTA_CORPUS_BASE_URL \
     --body https://github.com/HagaleTechnologies/manta/releases/download/corpus-v1
   ```
6. Run the workflow once instead of waiting for the night:
   ```sh
   gh workflow run corpus.yml
   gh run watch
   ```

Release assets have no bandwidth cap, which is why the corpus is not in Git
LFS: a nightly uncached 1.4 GB pull uses LFS's free 10 GiB a month in about
a week (decision D4).

## 4. Reading a nightly result and tightening floors

`corpus.yml` runs at 07:41 UTC and on `workflow_dispatch`. It fetches,
builds, runs the gated test, then `corpus.py score`. It is not a required
check; a failure shows as a red run on the Actions page.

- The run's summary page shows `score`'s Markdown table.
- The `corpus-results` artifact holds `corpus-results.json`, the full
  results:
  ```sh
  gh run download <run-id> -n corpus-results
  ```
- A `fetch` failure names the file and the expected and actual hashes: the
  hosted copy changed, or the pin is wrong.
- A gated-test failure means `WavIqSource` could not stream a real file to
  its end, or the header disagrees with the manifest. That is a bug to fix
  in the reader, not a reason to convert the file.
- A `score` failure names the decode that failed or the floor that was
  missed.

B2's floors (`min_spots` 50, `min_precision` 0.20, `min_recall` 0.02) and
WPX's (`min_spots` 10) are catastrophe floors, set well under the stale
2026-09 baseline. After the first hosted run (FU-1), set each floor to that
run's measured value minus a margin, say why in the commit, and record the
measured numbers in the corpus README's *Ground truth* section. A later
change that moves the numbers on purpose updates the floors in the same
pull request.

## 5. Transcribing VP8GEO

VP8GEO has no RBN reference, so its ground truth is a hand transcript. It
takes about a day of listening.

1. Open `vp8geo_cw_48khz.wav` in an editor with a spectrogram view, such as
   Audacity (track menu → *Spectrogram*). The spectrogram gives each
   signal's pitch and start and end times.
2. Transcribe at least 300 s of the 323 s file in the format in the corpus
   README's
   [*Transcript ground truth*](../../crates/manta-testkit/audio-corpus/README.md#transcript-ground-truth)
   section: one row per transmission, `start_s,end_s,pitch_hz,text`. Write
   only copy you are sure of and leave the rest out; do not use
   placeholders. Leave `pitch_hz` empty when you cannot tell.
3. Save it as
   `crates/manta-testkit/audio-corpus/ground-truth/vp8geo_cw_48khz.transcript.csv`.
4. Add the truth to the VP8GEO entry in `manifest.json`:
   ```json
   "truth": {"kind": "transcript", "path": "ground-truth/vp8geo_cw_48khz.transcript.csv"}
   ```
5. Decode and score:
   ```sh
   target/release/manta run --source vp8geo_cw_48khz.wav --json > vp8geo.jsonl
   scripts/score-transcript.py vp8geo.jsonl \
     crates/manta-testkit/audio-corpus/ground-truth/vp8geo_cw_48khz.transcript.csv \
     --sample-rate-hz 48000 --json
   ```
   It reports spot precision, call recall and word recall, matched within
   10 s and 100 Hz (`--time-tol-s`, `--pitch-tol-hz`). Without
   `--dial-freq-hz`, `run` reports each spot's audio pitch as its
   frequency, which is what the transcript's `pitch_hz` holds. Spot
   precision is a lower bound: a caller you could not hear but manta
   copied counts against it.
6. Record the numbers in the corpus README's *Ground truth* section and
   commit the transcript, the manifest change and the README together.
   From then on `scripts/corpus.py score` scores VP8GEO too. Add a
   `min_precision` floor to the entry only after a second run confirms the
   number.
