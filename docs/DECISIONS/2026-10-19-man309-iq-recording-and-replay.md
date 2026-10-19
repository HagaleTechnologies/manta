# MAN-309: record the decoded IQ to a file and replay it deterministically

## Decision

`manta run --record-iq PATH` records the IQ handed to the engine while it
decodes, and `manta decode` / `manta run --source PATH --source-iq` replay
the recording. Both commands also read headerless cf32 and ci16 files
described on the command line. Operator guide:
[docs/GUIDES/iq-recording.md](../GUIDES/iq-recording.md).

- **D1 — format.** The payload is headerless little-endian cf32 at `PATH`.
  The metadata is JSON Lines at `PATH.meta.jsonl` (suffix appended to the
  whole path): a `manta-iq.v1` header (rate, centre, start time as integer
  seconds plus nanoseconds, `rf_passband_hz`, `trusted_band_hz`,
  `analytic_guard_hz`), one `gap` record per discontinuity
  (`before_sample` counts stored frames; `missing_samples` may be 0), and
  an `end` record (`sample_count`, `reason` = `eof`/`stopped`/
  `backpressure`). Not WAV: hound 3.5.1 keeps a `u32` data length, which a
  live capture at 192 kS/s outgrows in under two hours, and a WAV header
  cannot hold a non-integer rate, gaps or source guards. JSON Lines streams
  gaps without an in-memory array; the reader validates in one bounded
  pass (64 KiB line cap) and streams gaps in a second. A recording may span
  at most 2^53 samples, stored plus missing, so a hostile gap length
  cannot overflow the engine's sample clock.
- **D2 — where it records.** `manta_input::RecordingSource` wraps the final
  source just before `listen_with_observers`, outside reconnect, decimation,
  the dial override and file-replay wrappers, so the file holds exactly the
  samples, gaps and source properties the engine saw. Every read, error,
  property and discontinuity passes through unchanged.
- **D3 — the decoder never waits.** A copy of each read goes in 4096-frame
  blocks through a bounded (16 MiB) `try_send` queue to one writer thread,
  which alone touches the files. A gap travels with the first block after
  it, so a full queue never leaves a gap describing unrecorded samples. A
  full queue stops the recording at the last accepted block
  (`reason: backpressure`); a write, flush or publication error stops it
  and leaves the `.partial` metadata. Either warns once, decoding
  continues, and `run` exits 1 at the end. Shutdown waits at most 5 s in
  total, one deadline shared by every recording input, before the client
  drain (`TimeoutStopSec=60` covers 5 + 50 + 2 s); a timeout cancels
  publication if it has not begun.
- **D3a — configuration.** `record_iq` is a shared `[input]` key (and
  `MANTA_INPUT_RECORD_IQ`), resolved against the config file's directory
  like `path` (the input path resolver now takes the key name). Each
  `[[input]]` records independently through `multi_run`'s opener; its
  handle lives beside the engine's source and is finished after
  `listen_inputs` returns. `--record-iq` overrides the single form and is
  refused with several inputs; duplicate destinations, however spelled,
  are refused at startup. `config check` reports it and creates nothing.
- **D4 — never overwrite.** Payload and partial metadata are created with
  `create_new`, after a startup check that fails `run` with exit 1 before
  any input or listener opens. Publication hard-links the partial file to
  its final name (failing on an existing target) and removes the partial.
- **D5 — replay.** `open_iq_file` dispatches on the metadata's presence
  (any extension), then `--iq-format`, then WAV. A recording restores its
  passband and guards; an explicit format or rate must match its
  metadata; a centre override moves only the RF origin. Under `run`, the
  server epoch is `--replay-epoch` > recorded start > file mtime, and the
  session nonce continues the payload's FNV-1a hash over the metadata's
  canonical header and gap records and its sample count (not the end
  reason or the file's formatting); a plain file's nonce is unchanged.
- **D6 — gaps.** `run --source` replays recorded gaps through
  `take_discontinuity`, so `listen` closes and re-opens segments exactly as
  it did live. `PacedSource` counts a gap's samples once in its pacing
  debt; `LoopingSource` carries a terminal gap into the next pass, added
  (checked) to that pass's first gap, and refuses a pass whose passband or
  guards differ. A gap-free loop still splices without a discontinuity. A
  recording with any gap is refused by a rate-changing
  `--capture-rate-hz` (the decimator does not carry gaps).
- **D7 — gap-aware `decode`.** `manta_engine::decode_source` checks
  `take_discontinuity` after every read and at EOF. At a gap it processes
  the pre-gap partial chunk, closes the segment with
  `finish_for_discontinuity` (a `Bookkeeping` closure, as `listen`), and
  starts a fresh channelizer and track manager whose clock is the old
  segment's base plus its consumed and missing samples and whose track ids
  continue; the post-gap samples open the new segment after its zero
  pre-roll. A gap pending at EOF closes the last tracks the same way
  (MAN-258). Each segment keeps the 4096-sample chunking of its own
  zero-padded stream, so `decode` keeps its batching and is not `listen`
  (no 2 s calibration). The validator, RTTY rows and bad-sample count span
  every segment. Every segment applies the source's passband, trusted
  band, guard and mirror veto as `listen::new_segment` does; for a plain
  complex source those setters are no-ops (`DetectorConfig::guard_hz` is 0;
  the loader rejects `[detector] guard_hz`), so a gap-free WAV decodes
  byte-for-byte as before.
- **D8 — config and diagnostics.** `[input] iq_format`/`sample_rate_hz`
  (file inputs with `iq = true`, together) describe headerless IQ; the
  centre is `center_freq_hz` or `--dial-freq-hz`, checked once merged, and
  the flags override them. `soak`, `doctor` and `check` open a configured
  file through the same `LiveSourceSpec::open` as `run`, so they read
  recordings and headerless files alike, and with `calibrate` they note
  that they ignore `record_iq`; `oracle` reads a gap-free
  recording through `open_iq_file` and refuses a gapped one (it decodes one
  continuous capture). `bench` reads no file.

## Verification

- `crates/manta-input/tests/{raw_iq,recording_format,recording}.rs` and the
  recorder unit tests: bit-exact cf32/ci16 decoding at read sizes
  1/7/4095/4096/4097, partial frames and bad rates refused at open,
  every malformed-metadata case, gap delivery under varied read sizes,
  a sparse 5 GiB payload with 20 000 gap lines, wrapped-versus-bare read
  traces, a held writer that fills the queue without blocking reads,
  injected write and publication failures, and the finish deadline.
- `crates/manta-cli/tests/iq_recording.rs`: `run --record-iq` on a 30 s V1
  produces the same stdout as without recording and a payload equal to the
  WAV's samples; `decode` and `run` of the recording give identical bytes
  on three runs with W1AW in them; `decode` of the recording equals
  `decode` of the WAV; a copied pair replays the same; a gapped recording
  replays deterministically and differently from its gap-free twin; cf32
  and ci16 decode exactly like the equivalent Float32 and PCM16 WAVs; a
  config `record_iq` resolves against the config file, `config check`
  creates nothing, a restart onto the same file fails untouched; each input
  of a two-input daemon records its own file.
- `crates/manta-engine/tests/decode_source.rs`: a gap inside a chunk and
  on a chunk boundary, at read sizes 1 to 65 536, gives identical reports,
  no event inside the outage, fresh track ids and a `Bookkeeping` close; a
  gap before the first sample only moves every timestamp; a terminal gap
  closes the last tracks as `Bookkeeping`; W1AW repeated across a 10 s gap
  is one spot and across a 700 s gap two, the second at its true time.
- `crates/manta-engine/tests/recording_replay.rs`: a gapped source decoded
  through a recorder gives the same report as without, and its recording
  replays to that report at every read size; rig-audio recordings (750 Hz
  at 48 kHz, 2600 Hz decimated to 6 kS/s) keep their passband, trusted band
  and guard and decode to one emitting track, as the live source does.
- `crates/manta-cli/tests/iq_recording.rs` also: a fake KiwiSDR that hangs
  up and comes back is recorded with its rate, centre, 10 kHz passband and
  the outage as a gap record, and SIGINT finalizes it as `stopped` (the
  recorder holds `run`'s stop flag, since a live source answers a stop
  with EOF); SIGINT finalizes a looping replay's recording; a write failure
  under `RLIMIT_FSIZE` leaves stdout identical, warns once and exits 1,
  and in a two-input daemon leaves the other input's recording to finish;
  `decode` of a gapped
  recording is identical on three runs and differs from its gap-free twin;
  a config-described headerless file runs like the flags; `oracle` reads a
  gap-free recording like its WAV and refuses a gapped one.
- Second round: a flush failure and a gap refused by a full queue (no gap
  record describes unrecorded samples); a gap inside `listen`'s
  calibration window, recording on and off; a copied pair with a moved
  mtime keeps its epoch and nonce, and replays identically with the spot
  servers up; `record_iq` precedence (CLI > environment > file, CLI and
  environment paths from the working directory); ci16 through `run`
  equals its PCM16 WAV; two recordings replay as a two-input daemon to
  the same bytes three times; a same-rate request on a gapped recording
  and decimation of a gap-free one still work; recording onto the
  replayed file and two spellings of one destination are refused.
- No golden vector or legacy WAV output changes.
