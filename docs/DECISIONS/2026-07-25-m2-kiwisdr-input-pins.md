# M2 KiwiSDR input implementation pins

This is the M2 KiwiSDR-input sub-project's (`docs/superpowers/plans/2026-07-25-m2-kiwisdr-input.md`,
design: `docs/superpowers/specs/2026-07-25-m2-kiwisdr-input-design.md`)
implementation's pinned-decision record. Treat every numbered item below as
decided; SPEC and docs/ still win on anything not listed here.

## Deviations and pinned decisions

1. **Protocol handshake: the design spec was missing a required command,
   found only by live debugging.** `KiwiIqSource::connect`
   (`crates/manta-input/src/kiwi.rs`) opens `ws://<host>:<port>/<timestamp>/SND`
   (`timestamp` any process-unique value), then exchanges `SET ...` **text**
   frames while `MSG ...` parameter frames and `SND ...` IQ frames arrive as
   WebSocket **binary** frames carrying a 3-byte ASCII tag. Beyond what the
   design spec documented, the server also sends `MSG audio_rate=<int>`
   partway through its initial parameter batch, and the client **must**
   reply `SET AR OK in=<audio_rate> out=<desired_rate>` or the server
   silently stops streaming `SND` frames a few seconds after an otherwise-
   correct setup — found only after live debugging against 6+ different
   public receivers and cross-checking wire traffic directly against the
   `jks-prv/kiwiclient` Python reference client's `_process_aud` source.
   `SET keepalive` must additionally be resent roughly once a second for the
   life of the connection (confirmed live: one initial send is not enough)
   — enforced by `KEEPALIVE_INTERVAL = 1000ms` in the read loop.
   - **Follow-up robustness fix**: the initial implementation only watched
     for `sample_rate=` during the handshake and could silently drop the
     `audio_rate` ack if `MSG audio_rate=...` arrived before `MSG
     sample_rate=...` in a given node's initial batch (ordering is not
     guaranteed across real nodes). Fixed by routing every `MSG` frame seen
     during `connect()` through the same ack-handling logic used by the
     main read loop, not just pattern-matching `sample_rate=`. Also added a
     previously-missing `SET compression=0` and gave the handshake loop the
     same bounded-retry timeout handling (`MAX_CONSECUTIVE_TIMEOUTS = 40` at
     `READ_TIMEOUT = 250ms`, ~10s) as the steady-state read loop.
   - **`SND` frame layout**: confirmed byte-for-byte against both real
     captures and `jks-prv/kiwiclient`'s source — after the 3-byte `"SND"`
     tag: 1-byte flags, 4-byte little-endian seq, 2-byte big-endian
     S-meter, then (IQ/stereo mode) a 10-byte GPS block, then interleaved
     `I,Q,I,Q,...` 16-bit samples, big-endian unless flags bit `0x80` is set
     (little-endian). Real captures were a constant 2068 bytes (20-byte
     header + 512 complex pairs) per frame. This matched the design spec's
     prediction — no deviation here, unlike the handshake (above).

2. **Real device sample rate observed during live testing: ~11998.86–
   11998.96 Hz across 3 different live public receivers** (`MSG
   sample_rate=<float>`, never a round number, never hardcoded). The final
   live-integration test connects to `kiwisdr.inf.dhbw-ravensburg.de:8073`
   and asserts the *resampled* output rate is ~96000 Hz — the raw
   ~11998.9 Hz device rate itself is not asserted in the shipped test
   (device-specific and non-deterministic across nodes/time), only that
   `sample_rate()` reports 96000 post-resample.

3. **`RESAMPLER_CHUNK = 16_384`, chosen empirically, not by the naive guess
   of matching KiwiSDR's native 512-sample SND frame size.** With
   `FixedSync::Input`, `rubato::Fft`'s internal input-side FFT block size
   for a KiwiSDR rate is (since `gcd(rate_in, 96000) == 1` for essentially
   every real device — a crystal-derived rate near 12000 Hz shares no
   factors with `96000 = 2^8*3*5^3`) equal to the rounded input rate itself
   (~12000). `rubato::Fft::new`'s `chunk_size` must be >= that internal
   block size for output to start on the very first `process_into_buffer`
   call; smaller sizes (512, 4096, 8192 were measured) don't break
   anything but delay first real output — chunk=512 measured 23 empty calls
   before any output. 16384 comfortably clears any real device's rate with
   headroom, while keeping the per-call working set trivial.
   - **`output_delay()` is a fixed 48,000 output samples (0.5s at 96kHz),
     independent of `RESAMPLER_CHUNK`** — confirmed by direct testing across
     `{512, 4096, 8192, 11999, 12000, 16384, 32768}`, all producing the same
     48,000. This is a structural property of exact rational resampling
     between two coprime rates (the smallest valid FFT block pair for a
     gcd-1 rate pair is `(rate_in, rate_out)` itself), not something any
     chunk-size choice can reduce. Callers needing to account for KiwiSDR
     startup latency should budget for this ~0.5s regardless of chunk size,
     analogous to `CALIBRATION_SECONDS` elsewhere in the pipeline.

4. **`center_freq_hz` bug: same root cause independently found and fixed
   twice, on two different branches.** `manta_engine::listen()` hardcoded
   `center_freq_hz = 0.0` when constructing `Channelizer::new` and
   `TrackManager::new`, instead of reading `src.center_freq_hz()`. This is
   the same bug already found and fixed once before on the separate,
   still-unmerged `feat/m2-soapysdr-input` branch (PR #30). This branch
   started fresh from `origin/main` per repo hygiene and so needed its own
   independent copy of the same fix — proven via a new regression test
   (`listen_uses_the_sources_center_freq_hz_not_a_hardcoded_zero` in
   `crates/manta-engine/src/listen.rs`) using a test double with a
   genuinely nonzero `center_freq_hz()`. Whichever of PR #30 or this
   sub-project's PR merges second will need to resolve this as a trivial
   merge conflict (both branches touch the same two call sites the same
   way) — not a real design disagreement, just duplicate independent work
   on a real, well-isolated bug.

5. **Real live-network integration test coverage achieved — qualitatively
   better than SoapySDR's situation, not hedged as equivalent.** Unlike the
   SoapySDR sub-project (no RF hardware ever reachable in that environment,
   so only error-path coverage was possible), KiwiSDR needs no local
   hardware — the shipped `#[ignore]`d test
   `connects_to_a_real_public_receiver_and_streams_iq`
   (`crates/manta-input/src/kiwi.rs`) performs a genuine live connection
   to a real public receiver (`kiwisdr.inf.dhbw-ravensburg.de:8073`),
   completes the real handshake, and reads real streamed IQ. Measured
   result from an independently-run fresh pass: `sample_rate=96000`
   (correctly resampled), 153,344 total real samples drained across
   multiple reads, steady-state signal amplitude `~0.47` (range 0.45–0.49
   across separate runs) once past the resampler's cold-start transient —
   genuine non-trivial RF, not silence or noise-floor residue. 5 additional
   non-network tests cover byte-layout parsing (both endiannesses), `MSG`
   key-value parsing, short-frame boundary safety, connection-refused error
   path, and resampler ratio math. This test is `#[ignore]`d (network-
   dependent, third-party infrastructure) but was actually run, not merely
   written.

### CLI wiring note (Task 3)

`--kiwi-host`/`--kiwi-port`/`--kiwi-freq`/`--kiwi-password` need no feature
gate (pure-Rust websocket client, no native library dependency, unlike the
separate SoapySDR sub-project's `--feature soapysdr` gating). `clap`'s
`requires` attribute was verified live to cover **both** directions here:
`kiwi_host` carries `requires = "kiwi_freq"`, and `kiwi_port`/`kiwi_freq`/
`kiwi_password` each carry `requires = "kiwi_host"` — confirmed against the
real built binary that both `--kiwi-host` alone and `--kiwi-freq` alone
produce a clean clap-level error (exit code 2) before the source-opening
helper's runtime `ok_or_else` check ever runs. That runtime check is
therefore defensive belt-and-suspenders, not load-bearing — unlike the
analogous SoapySDR case, where clap only covers one direction and the
runtime check is load-bearing.

### coppa dependency pin

Unchanged from M2 sub-project 2's pin: `coppa-dsp`/`coppa-audio`/
`coppa-channel` remain pinned in the workspace `Cargo.toml` to git rev
`f8a4d16df7e5776a0756943c05712038774e6c70` of
`https://github.com/HagaleTechnologies/coppa.git`. This sub-project made no
coppa API changes and needed no bump.
