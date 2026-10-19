# Recording IQ and replaying it

`manta run --record-iq PATH` writes the IQ it decodes to a file while it
decodes. `manta decode` and `manta run --source PATH --source-iq` replay
that recording later, producing the same output on every run. The same
commands also read headerless complex-float32 (`cf32`) and complex-int16
(`ci16`) files from other software, given their sample rate and centre
frequency on the command line (MAN-309).

## Record a session

```sh
manta run --dial-freq-hz 14000000 \
  --record-iq session.cf32 --json >spots.jsonl 2>session.log
```

Recording is off by default and is chosen when `run` starts; there is no
command to start or stop it mid-session. It works with every input:
rig audio, KiwiSDR, SoapySDR, HPSDR, ka9q-radio and file replay. The
recorder sits where samples enter the decoder, after Hilbert conversion,
resampling, `--capture-rate-hz` decimation, the dial override and
reconnect handling, so the recording holds exactly what was decoded.

`session.log` gains two lines; the spot output is unchanged:

```text
recording IQ to session.cf32; metadata: session.cf32.meta.jsonl
recording finished: session.cf32, 5760000 samples
```

The finished line appears only once the metadata is complete. 5 760 000
samples is 120 s at 48 kS/s.

### Where it writes

- The samples go to `PATH` exactly as given (`--record-iq` or `record_iq`).
- The metadata goes to `PATH.meta.jsonl`: the suffix is appended to the
  whole path, so `session.cf32` gets `session.cf32.meta.jsonl`. While the
  session runs it is `PATH.meta.jsonl.partial`, renamed only after the
  last record is flushed.
- Neither file is ever overwritten. If `PATH`, its metadata or the partial
  marker exists, `run` exits 1 before it opens the input:

  ```text
  error: IQ recording destination already exists: session.cf32
  ```

  A restarted service therefore needs a new filename, or the old pair moved
  away first.
- The directory must already exist and be writable by the service; manta
  creates no directories and does not rotate files. Under systemd, add the
  capture directory to the unit's writable paths, for example
  `ReadWritePaths=/var/lib/manta/captures`, and record to
  `/var/lib/manta/captures/session.cf32`.
- Copy or move the two files together:

  ```sh
  cp /var/lib/manta/captures/session.cf32 \
     /var/lib/manta/captures/session.cf32.meta.jsonl /srv/archive/
  ```

A service sets it in its config instead; changing it needs a restart:

```toml
[input]
type = "audio"
center_freq_hz = 14000000
record_iq = "captures/session.cf32"
```

A relative `record_iq` in a config file is relative to that file's
directory; from `--record-iq` or `MANTA_INPUT_RECORD_IQ` it is relative to
the working directory. The precedence is the usual command line >
environment > file. `manta config check` shows the resolved path and
creates nothing.

With several receivers, put `record_iq` in each `[[input]]` entry that
should be recorded, each with its own file; two entries cannot share one,
and `--record-iq` (one file) is refused. Each recording replays on its own
at its own start time; replaying several together uses the first input's
start time, as any multi-file replay does (MAN-13), not the original
staggered start of each receiver.

### Disk use

The payload is 8 bytes per complex sample: 1.38 GB per hour at 48 kS/s,
2.76 GB at 96 kS/s and 5.53 GB at 192 kS/s. The metadata is a few hundred
bytes plus one short line per input gap. No length cap applies; the free
disk space does.

### If storage cannot keep up

The decoder never waits for the disk. Each recorded input queues at most
16 MiB of samples for its writer thread. When that queue is full, the
recording stops, keeping everything written so far, and decoding carries
on:

```text
warning: IQ recording stopped: writer queue full; decoding continues
```

A write error stops the recording the same way:

```text
warning: IQ recording stopped: write failed for session.cf32: <cause>; decoding continues
```

Either way `run` exits 1 at the end, so a script recording a finite
session does not mistake it for a complete capture. A stopped-on-backlog
recording is finalized with `"reason":"backpressure"` and replays as the
contiguous part it holds. A failed write leaves the `.partial` metadata,
which replay refuses.

At shutdown, `run` waits at most 5 s for the writer to finish. If it does
not:

```text
warning: IQ recording finalization timed out: session.cf32; completion is unconfirmed
```

## Replay a recording

```sh
manta decode session.cf32 --json >first.json
manta decode session.cf32 --json >second.json
cmp first.json second.json

manta run --source session.cf32 --source-iq --json >events.jsonl 2>replay.log
```

The metadata is found next to the file, whatever its extension, and
supplies the sample rate, centre frequency, start time, the input's
passband and its spawn guards (an audio-derived recording keeps the
600 Hz DC/Nyquist guard of MAN-273). `--center-freq-hz` (`decode`) or
`--dial-freq-hz` (`run`) moves only the RF origin.

Each command gives the same bytes on every run of the same binary with
the same configuration and reference lists. `decode` and `run` batch
differently, so their outputs are not compared with each other.

Under `run --config` with a `[server]` table, spot timestamps start at
the recorded start time, not the file's modification time, so a copied
pair keeps its timestamps. `--replay-epoch` still overrides it. The spot
ids' session nonce covers the samples and the metadata, not the file's
name.

A recording whose metadata is missing, partial or malformed is refused:

```text
error: incomplete IQ recording: session.cf32.meta.jsonl.partial
```

A payload copied without its metadata is just headerless cf32: import it
with `--iq-format cf32`, below. Its timing, gaps and guards are lost.

### Input gaps

When a live input drops out and reconnects, the engine closes the tracks
it had and starts again with its sample clock moved on by the outage
(MAN-73). The recording keeps a `gap` record at that point, and
`run --source` replays it the same way: the outage is not spliced out and
not filled with zeros. A loss the input reports without a sample count
(for example a SoapySDR overflow) reaches neither the engine nor the
recording as a gap.

`manta decode` handles them the same way: it closes its tracks at each
gap, as a discontinuity rather than an end of signal, and starts a fresh
segment whose timestamps continue after the missing samples and whose
track ids follow on. The spot validator spans every segment, so a call
repeated across a short outage is still a duplicate. A recording without
gaps decodes exactly as before.

Two limits apply to recordings with gaps:

- They replay only at their recorded rate, because the decimator does not
  carry gaps:

  ```text
  error: cannot decimate an IQ recording with gaps; replay at its recorded sample rate
  ```

- `manta oracle` reads one continuous capture and refuses them.

`--realtime` waits out each gap once, as the outage took that long live.
`--loop` keeps a gap at the end of the recording and reports it at the
start of the next pass.

## Headerless cf32 and ci16

```sh
manta decode external.cf32 --iq-format cf32 --sample-rate-hz 96000 \
  --center-freq-hz 14000000 --json >decoded.json
manta decode external.ci16 --iq-format ci16 --sample-rate-hz 96000 \
  --center-freq-hz 14000000 --json >decoded-int16.json
manta run --source external.cf32 --source-iq --iq-format cf32 \
  --sample-rate-hz 96000 --dial-freq-hz 14000000 --json >events.jsonl 2>replay.log
```

Both formats are little-endian, I then Q. `cf32` is two IEEE float32
values per sample; `ci16` is two signed 16-bit integers, each divided by
32768. The file extension does not matter and manta never guesses the
byte order. `--sample-rate-hz` is the stored rate (above 0, at most
10 MS/s); `--capture-rate-hz` still decimates after it. A file that ends
inside a sample is refused when it is opened. Without the rate or the
centre:

```text
error: headerless IQ requires --sample-rate-hz and --center-freq-hz
error: headerless IQ requires --sample-rate-hz and --dial-freq-hz
```

(the first from `decode`, the second from `run`).

A service describes a headerless file with config keys instead; the
centre is `center_freq_hz`:

```toml
[input]
type = "file"
path = "external.cf32"
iq = true
iq_format = "cf32"
sample_rate_hz = 96000
center_freq_hz = 14000000
```

`iq_format` and `sample_rate_hz` go together and need `iq = true`; the
command-line flags override them. `manta config check` shows
`iq_format=cf32 sample_rate_hz=96000` for such an input.

## The metadata format

`PATH.meta.jsonl` is UTF-8 JSON Lines: one header, any number of gaps,
one end record. A 96 kS/s recording with one two-second outage before
stored sample 192 000:

```jsonl
{"type":"header","schema":"manta-iq.v1","sample_format":"cf32_le","sample_rate_hz":96000.0,"center_freq_hz":14000000.0,"start_unix_seconds":1792407600,"start_nanoseconds":0,"rf_passband_hz":[-48000.0,48000.0],"trusted_band_hz":null,"analytic_guard_hz":0.0}
{"type":"gap","before_sample":192000,"missing_samples":192000}
{"type":"end","sample_count":11520000,"reason":"stopped"}
```

| Field | Meaning |
|---|---|
| `schema` | `manta-iq.v1`; any other value is refused. |
| `sample_format` | `cf32_le`: the payload's layout. |
| `sample_rate_hz` | Complex samples per second of the payload. |
| `center_freq_hz` | RF frequency at 0 Hz offset, Hz; `0.0` when unknown. |
| `start_unix_seconds`, `start_nanoseconds` | Wall-clock time of stored sample 0: integer seconds since 1970-01-01 UTC, then nanoseconds in [0, 10⁹). For a live input this is when the session started; for a file replay it is the replay's epoch. |
| `rf_passband_hz` | `[lo, hi]` Hz around the centre that the input really carried (one-sided for rig audio). |
| `trusted_band_hz` | `[lo, hi]` Hz the decimator kept flat, or `null`. |
| `analytic_guard_hz` | Spawn guard near DC and Nyquist, Hz; `600.0` for rig audio at 48 kHz, else `0.0`. |
| `before_sample` | Stored samples before the gap (missing ones not counted). A gap at `sample_count` is an outage at the end. |
| `missing_samples` | Samples lost, at `sample_rate_hz`. `0` is still a discontinuity. |
| `sample_count` | Stored samples; the payload is exactly 8 × this many bytes. |
| `reason` | `eof` (the input ended), `stopped` (the session stopped) or `backpressure` (the writer fell behind; a prefix). |

Gap positions strictly increase. A reader refuses lines over 64 KiB,
missing or repeated header or end records, records after the end, unknown
record types, gaps out of order or past the end, a recording spanning more
than 2^53 samples (stored plus missing), and a payload whose length
disagrees with `sample_count`. It ignores unknown fields, so later
versions can add some. The metadata is read in a streaming pass, so a
recording with many gaps needs no more memory than one with none.

See [docs/DECISIONS/2026-10-19-man309-iq-recording-and-replay.md](../DECISIONS/2026-10-19-man309-iq-recording-and-replay.md)
for the design choices.
