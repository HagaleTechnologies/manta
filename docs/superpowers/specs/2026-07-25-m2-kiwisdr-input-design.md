# M2 remaining sub-project 3: KiwiSDR input

Status: approved
Date: 2026-07-25

## Purpose

The last of M2's remaining sub-projects (ROADMAP.md): ARCHITECTURE.md §3's
KiwiSDR client (`manta-input::kiwi`), the network-based IQ source that
"gives instant worldwide receiver access for development and lets
low-budget nodes contribute spots." Same scope pattern as the SoapySDR
sub-project (PR #30, not yet merged): crate-level `IqSource` + engine
generalization + CLI wiring.

## Environment finding: real, live protocol verification (not blind code)

No official Rust crate exists for the KiwiSDR protocol (unlike SoapySDR's
mature `soapysdr` binding) and no WebSocket crate exists anywhere in this
workspace yet. The protocol itself is community-reverse-engineered, with
`jks-prv/kiwiclient` (Python) as the de facto reference implementation.

Rather than implement blind against documentation, this was verified with a
real, live WebSocket connection to a real public receiver
(`greatlakesreceiver.hopto.me:8073`, confirmed reachable and consented to as
a brief, light test connection — KiwiSDR's whole public-directory model
exists for exactly this kind of use) using a scratch `tungstenite` client:

- **Connection URL**: `ws://<host>:<port>/<timestamp>/SND` — `<timestamp>`
  is an arbitrary client-chosen numeric ID (no server-side clock
  synchronization needed; used only to distinguish concurrent connections
  server-side).
- **Handshake**, sent as WebSocket **text** frames after connect:
  1. `SET auth t=kiwi p=<password>` (`p=` empty for anonymous/no-password
     receivers, which most public ones are).
  2. `SET mod=iq low_cut=-5000 high_cut=5000 freq=<khz>` — `freq` in kHz,
     `mod=iq` selects raw complex-IQ output (vs. demodulated audio).
  3. `SET agc=1 hang=0 thresh=-100 slope=6 decay=1000 manGain=50` (AGC
     params; exact values likely don't matter much for `mod=iq`, which
     bypasses audio AGC processing — untuned defaults used in the spike,
     worked fine).
  4. `SET compression=0` — IQ/stereo modes are unconditionally uncompressed
     regardless of this setting (confirmed against the reference client:
     `mod in ["iq","drm","sas","qam"]` are never ADPCM-compressed), but
     sending it explicitly matches the reference client's behavior.
  5. `SET keepalive`, sent repeatedly at ~1 Hz for the connection's entire
     lifetime — confirmed required: a spike without any keepalive received
     the initial `MSG` handshake batch but then received **zero** `SND`
     frames; adding `SET keepalive` immediately unblocked real IQ data
     within under 600ms.
- **Frame format**: contrary to this project's own initial assumption
  (worth recording as a real, caught mistake, the same spirit as the
  SoapySDR `type=null` correction) — server responses do **not** arrive as
  WebSocket text frames. Both `MSG` (text parameters) and `SND` (binary IQ
  payload) arrive as WebSocket **binary** frames, distinguished by a 3-byte
  ASCII prefix (`"MSG"` / `"SND"`) at the start of the frame body.
- **`MSG` frames**: `MSG` + ASCII key=value text (e.g.
  `sample_rate=11998.937786`). The real, live-measured IQ sample rate is
  **not exactly 12000 Hz** — it's a per-device hardware clock value, must be
  read from this `MSG` field at connect time, never hardcoded.
- **`SND` frames** (IQ mode): `SND` + 1-byte flags + 4-byte little-endian
  seq + 2-byte big-endian S-meter + a 10-byte block (GPS
  solution/timestamp fields per the reference client, structurally present
  in every captured frame in the spike) + big-endian int16 I/Q sample
  pairs. 8 real captured frames were all exactly 2068 bytes (3+1+4+2+10+2048
  header+payload), consistently decoding to exactly 512 complex sample
  pairs per frame (2048 / 4 bytes-per-pair) — internally consistent across
  every frame captured, both the flags=0xd first frame and the flags=0x8
  subsequent frames.
- **Byte order**: the reference client documents a `SND_FLAG_LITTLE_ENDIAN
  = 0x80` flag bit controlling whether I/Q sample data itself is
  little-endian; none of the captured frames had that bit set (all
  big-endian), so the implementation should honor this flag at runtime
  (branch on it) rather than hardcoding big-endian, even though that's what
  was observed.

**Residual risk, stated plainly**: the exact SND byte layout above is
inferred from a combination of live captures (authoritative) and an
AI-summarized reading of the reference client's source (not read
byte-for-byte by a human or verified against the raw file directly) — solid
enough to build against, consistent across 8 real frames, but Task 1 should
re-verify field-by-field against either the raw reference source directly or
additional live captures with more variety (e.g. a frame that visibly lacks
the GPS block, if one exists) before treating the layout as fully nailed
down. This is NOT the same category of "genuinely untestable without
hardware" gap the SoapySDR sub-project had — a real KiwiSDR connection is
available and should be used to validate byte-for-byte.

## Scope

1. **`manta-input::kiwi::KiwiIqSource`** — the crate-level `IqSource` impl.
   No feature gate (unlike `soapy`): this is pure WebSocket + TCP, no native
   library dependency, so it's always available in a default build. This
   does mean `tungstenite` becomes an unconditional dependency of
   `manta-input` (small, no native/system library requirement, unlike
   SoapySDR) — not gated because there's nothing to gate against
   (ROADMAP.md's "no SoapySDR dependency in default features" constraint is
   specifically about SoapySDR's native C library, not about this).
2. **Resampling** — `rubato` (new dependency): the real, measured KiwiSDR
   rate (~12 kHz, but not exactly, and device-specific) rational-resampled
   to 96000 Hz (the nearest SPEC §1.1 table rate) before reaching the
   channelizer, per `docs/SPEC-decode-core.md`'s explicit "non-power-of-two
   input rates... are rational-resampled in `manta-input`" requirement.
3. **Engine generalization** — `manta_engine::listen`/`soak`: same
   `AudioIqSource` → `Box<dyn IqSource>` change as the (unmerged) SoapySDR
   PR #30, redone independently here per Tony's explicit choice (branch
   fresh from `origin/main`, accept the future small merge conflict when
   both PRs land, rather than depend on an unmerged branch or block on it).
4. **CLI wiring** — new `--kiwi-host`/`--kiwi-port`/`--kiwi-freq`/
   `--kiwi-password` flags on `listen`/`soak`, mutually exclusive with the
   existing `--device`/`--source` (and, if PR #30's flags happen to already
   exist by the time this merges, `--soapy-driver` too — Task 3 should
   check the real state of `main.rs` at implementation time and follow
   whatever the merged pattern looks like).

## `KiwiIqSource`

`crates/manta-input/src/kiwi.rs`, unconditional module (`pub mod kiwi;` in
`lib.rs`, no `#[cfg(feature = ...)]`).

```rust
pub struct KiwiIqSource {
    socket: tungstenite::WebSocket<std::net::TcpStream>,
    fs: f64,             // real, resampled-target rate (96_000.0)
    center_freq_hz: f64,
    resampler: rubato::Fft<f32>,     // FixedSync::Input, channels=2 (I/Q interleaved)
    raw: Vec<f32>,                   // un-resampled input accumulator (see below)
    pending: std::collections::VecDeque<Complex32>,  // resampled-output chunk-assembler
    last_keepalive: std::time::Instant,
}
```

Resampler construction and integration — verified directly against the real
installed `rubato = "4.0"` source
(`~/.cargo/registry/src/*/rubato-4.0.0/src/synchro.rs`) AND a real,
compiled, executed spike, not just a docs.rs summary (a summarized fetch of
this same API was tried first and got the rate-parameter type wrong — worth
recording as another real "verify against the actual crate/a real run, not
a summary" catch, same discipline used for `soapysdr` earlier in this plan):

```rust
rubato::Fft::<f32>::new(
    rate_in_hz,        // usize -- the real, MSG-reported sample_rate, ROUNDED
                        // to the nearest Hz (e.g. 11998.937786 -> 11999);
                        // Fft::new takes usize, not f64 -- the sub-Hz
                        // fractional part is lost, a few hundred ppm of
                        // harmless resampling-ratio error
    96_000,            // usize -- rate_out, the SPEC §1.1 table rate
    RESAMPLER_CHUNK,   // see below -- NOT KiwiSDR's raw 512-sample SND
                        // frame size
    2,                 // nbr_channels: I/Q interleaved
    rubato::FixedSync::Input,
)?;
```

`rubato` and `audioadapter`/`audioadapter_buffers` (the buffer-wrapper types
`process_into_buffer` needs — `rubato::audioadapter_buffers::direct::
InterleavedSlice::new`/`new_mut` wrap a plain `&[f32]`/`&mut [f32]`) do
**not** need separate `Cargo.toml` entries: `rubato`'s own `lib.rs` does
`pub use audioadapter; pub use audioadapter_buffers;`, so both are reachable
as `rubato::audioadapter::*` / `rubato::audioadapter_buffers::*` through the
one `rubato = "4.0"` dependency. Confirmed by a real compiled spike using
exactly this path.

**Real finding that changes the buffering design**: feeding the resampler
in KiwiSDR's native 512-sample SND-frame chunks was tried in the spike and
produces an `output_delay()` of 48,000 output samples (0.5s at 96kHz) — and
after 6 consecutive 512-sample chunks fed (all of KiwiSDR's natural
frame-delivery rate for ~0.26s of audio), **zero** output samples had been
produced yet. `Fft`'s internal FFT block size is chosen from the chunk size
and the rate ratio together, not from the chunk size alone, and a small
chunk size at an 8x ratio produces a large, latency-heavy internal block.
This means **`RESAMPLER_CHUNK` must be decoupled from the raw 512-sample
SND frame size**: accumulate several SND frames' worth of raw samples into
`raw: Vec<f32>` first, and only call `process_into_buffer` once `raw` holds
a full `RESAMPLER_CHUNK`-sized (a larger, TBD-during-implementation value —
tune empirically for a reasonable delay/throughput trade-off; the spike
didn't explore this) block. `resampler.output_delay()` should be checked at
construction time and logged/documented once a real value is picked, so a
future reader understands the real startup latency this introduces (a
real-world consequence: `listen()`'s startup calibration window may need to
account for it, similar in spirit to the existing `CALIBRATION_SECONDS`
constant).

impl KiwiIqSource {
    pub fn connect(
        host: &str,
        port: u16,
        center_freq_hz: f64,
        password: &str,
    ) -> anyhow::Result<Self> { ... }
}

impl IqSource for KiwiIqSource {
    fn sample_rate(&self) -> f64 { self.fs }        // 96_000.0, post-resample
    fn center_freq_hz(&self) -> f64 { self.center_freq_hz }
    fn read(&mut self, buf: &mut [Complex32]) -> anyhow::Result<usize> { ... }
}
```

`connect()`:
1. Open a plain `TcpStream` to `host:port`, set a read timeout (mirrors
   `SoapySdrIqSource`'s `TIMEOUT_US` responsiveness reasoning — a blocking
   `read()` with no timeout would break `listen()`'s Ctrl-C responsiveness).
2. `tungstenite::client(url, tcp_stream)` to perform the WS upgrade against
   `ws://host:port/<timestamp>/SND` (timestamp: any process-unique value,
   e.g. current time in ms).
3. Send the 4 setup `SET` commands (auth, mod=iq, agc, compression) listed
   above.
4. Read `MSG` frames until `sample_rate=...` is seen (this is the real,
   authoritative rate — construct the `rubato` resampler from this exact
   value → 96000.0, not an assumed 12000.0).
5. Send an initial `SET keepalive`, record `last_keepalive`.
6. Return `Ok(Self { ... })` — first real `SND` frame is consumed lazily on
   the first `read()` call, matching `SoapySdrIqSource`'s "activate happens
   in `open()`, first data flows on first `read()`" shape. Unlike
   `SoapySdrIqSource`, though, keepalive is a *per-call*, connection-lifetime
   responsibility, not a one-time setup step: `read()` itself must check
   `last_keepalive.elapsed()` on every call and send another `SET keepalive`
   whenever ~1s has passed, or the server will stop sending `SND` frames
   (confirmed directly — this is exactly the failure mode the spike hit
   before keepalive was added).

`read()`:
1. If `pending` (the chunk-assembler buffer) has enough resampled samples
   to fill `buf`, drain and return immediately.
2. Otherwise: check/send keepalive if due: loop reading WebSocket frames
   (`socket.read()`) until a real `SND` frame arrives (skip/log `MSG`
   frames, handle `Ping`/`Close` per normal WebSocket housekeeping),
   parse its I/Q samples per the byte layout above, feed them through
   `rubato` to get 96 kHz-rate `Complex32` samples, push onto `pending`,
   then drain into `buf`.
3. Any WebSocket-level error (connection closed, TCP error) propagates as
   a real `Err` — same principle as `SoapySdrIqSource`'s bounded-retry
   fix for transient conditions vs. hard failures for real ones. A stalled
   read (no `SND` within the TCP read timeout) is analogous to SoapySDR's
   `Timeout` case and should get the same bounded-retry treatment rather
   than either a false EOF (`Ok(0)`) or an immediately fatal error.

## Testing

Unlike SoapySDR (genuinely no real hardware ever reachable in this
environment), a real KiwiSDR connection IS available — this sub-project's
tests should include real, live integration coverage, not just error-path
coverage:

- A `#[ignore]`d test that connects to a real public receiver (pick one
  from the live-verified list, e.g. `greatlakesreceiver.hopto.me:8073`, or
  let Task 1 pick a fresh one if that node isn't up by then — public nodes
  come and go), completes the full handshake, and asserts real `SND` frames
  with the expected byte-count/sample-count arrive. `#[ignore]`d because
  it's network-dependent, third-party infrastructure, and shouldn't run in
  default `cargo test`/CI — same pattern as this repo's existing
  network/hardware-dependent ignored tests.
- Unit tests for the resampling math (given a known synthetic input at a
  specific rate, confirm the `rubato` pipeline produces the expected output
  rate/sample-count) — no network needed for this part.
- Error-path tests (connection refused / DNS failure) don't need real
  network access to construct — a connection to `127.0.0.1:1` (or similar,
  guaranteed-refused) is a fast, reliable, hardware/network-independent
  regression test for the "can't connect" path, real without depending on
  external infrastructure being reachable.

## CI

No new native-library CI job needed (unlike SoapySDR) — `tungstenite` and
`rubato` are both pure-Rust, no system dependency, so the *existing* default
`test` job already builds/lints/tests everything except the `#[ignore]`d
live-network test, with zero changes to `ci.yml` required. The live-network
test stays `#[ignore]`d and un-run by CI (network-dependent, third-party
service, not something CI should depend on being reachable).
