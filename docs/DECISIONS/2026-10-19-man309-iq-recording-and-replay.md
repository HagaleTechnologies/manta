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
  pass (64 KiB line cap) and streams gaps in a second.
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
  continues, and `run` exits 1 at the end. Shutdown waits at most 5 s; a
  timeout cancels publication if it has not begun.
- **D3a — configuration.** `record_iq` is a shared `[input]` key (and
  `MANTA_INPUT_RECORD_IQ`), resolved against the config file's directory
  like `path` (the input path resolver now takes the key name). Each
  `[[input]]` records independently through `multi_run`'s opener; its
  handle lives beside the engine's source and is finished after
  `listen_inputs` returns. `--record-iq` overrides the single form and is
  refused with several inputs; duplicate destinations are refused at
  startup. `config check` reports it and creates nothing.
- **D4 — never overwrite.** Payload and partial metadata are created with
  `create_new`, after a startup check that fails `run` with exit 1 before
  any input or listener opens. Publication hard-links the partial file to
  its final name (failing on an existing target) and removes the partial.
- **D5 — replay.** `open_iq_file` dispatches on the metadata's presence
  (any extension), then `--iq-format`, then WAV. A recording restores its
  passband and guards; an explicit format or rate must match its
  metadata; a centre override moves only the RF origin. Under `run`, the
  server epoch is `--replay-epoch` > recorded start > file mtime, and the
  session nonce continues the payload's FNV-1a hash over the metadata
  bytes; a plain file's nonce is unchanged.
- **D6 — gaps.** `run --source` replays recorded gaps through
  `take_discontinuity`, so `listen` closes and re-opens segments exactly as
  it did live. `PacedSource` counts a gap's samples once in its pacing
  debt; `LoopingSource` carries a terminal gap into the next pass, added
  (checked) to that pass's first gap, and refuses a pass whose passband or
  guards differ. A gap-free loop still splices without a discontinuity. A recording with any gap is refused by `decode` (its
  single-segment batching would splice the outage) and by a rate-changing
  `--capture-rate-hz` (the decimator does not carry gaps).

## Not done here

These planned items are follow-ups:

- Gap-aware `decode` (segment the offline decode at each gap, with source
  guards).
- `iq_format` and `sample_rate_hz` config keys (the flags are CLI-only).
- Native-recording support in `oracle`, `check`, `soak` and `bench`.

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
- No golden vector or legacy WAV output changes.
