# 2026-10-10 — MAN-269: paced and looping file replay

**Status:** Implemented (branch `MAN-269`). Replaces MAN-121 (PR #135,
closed unmerged by the 2026-10-05 audit triage); that branch, tagged
`archive/MAN-121-pr135`, is prior art for the pacing logic and its tests.

## Context

ROADMAP M3's acceptance gate ("a stock client connects and receives
well-formed spots") should be checkable by anyone without an SDR. MAN-169
already lets `run --source <wav> --source-iq` replay the README's
`manta gen v1` output, but replay ran as fast as the machine decodes.
Measured on `main` at `0cd6a30`:

| What | Result |
|---|---|
| `manta run --source v1.wav --source-iq --config manta.toml` (120 s vector, release build) | one W1AW spot, exit 0 in `real 0m3.149s` |
| the same, debug build | `real 0m4.527s` |
| a telnet client connecting at t = 5 s | `ConnectionRefusedError(111, 'Connection refused')` |
| when V1's spot is decoded | `sample_ts` 2 006 272 (20.90 s); 1.5, 3 and 15 s renders emit none, 30 and 40 s renders emit the same spot |
| a 76-byte IQ WAV header declaring 393 216 000 Hz (`run --source-iq`, `ulimit -v 2000000`) | exit 134, `memory allocation of 3355443200 bytes failed` in `FloorBank::new` (exit 137 under the container cgroup) |

## Decisions

- **D1 — pacing is opt-in.** The default stays unpaced: determinism,
  `soak`'s compressed time and the test suite's speed depend on it.
- **D2 — `--loop` implies `--realtime`.** `paced = realtime || loop`. An
  unpaced loop with no server logged W1AW at recording times 21, 621 and
  1221 s within 30 s of wall time, stamping spots up to 20 minutes into the
  future. MAN-121 instead required `--realtime` alongside `--loop` when a
  server was configured.
- **D3 — config keys.** `input.realtime` and `input.loop` are
  `type = "file"` keys next to `iq` (`MANTA_INPUT_REALTIME`,
  `MANTA_INPUT_LOOP`). Precedence follows MAN-261 D6 as `--source-iq` does:
  a CLI flag ORs onto a config file source; a CLI source selector discards
  the typed `[input]` table, its `realtime`/`loop` included. Only `run`
  has the flags.
- **D4 — validate after `resolve`.** The source may come from the config
  file, so clap's `requires = "source"` (MAN-121) would wrongly reject
  `run --config demo.toml --realtime`. `resolve_replay_mode` refuses either
  setting unless the resolved source is `LiveSourceSpec::File`, inside
  `prepare_live`, before any source I/O. `validate_input` already refuses
  the keys on a non-file `[input]`.
- **D5 — `soak` and `doctor` ignore the keys** with
  `note: <command> replays files as fast as it can; ignoring input.realtime/input.loop from <origin>`.
- **D6 — the decorators live in `manta-input`** (`replay.rs`, beside
  `DecimatingSource`). `PacedSource::new` refuses a non-finite or
  non-positive rate (`Duration::from_secs_f64` would panic), starts its
  clock lazily at the first read, and sleeps after the inner read against
  the cumulative delivered count including that read; once the consumer is
  behind it never sleeps. `LoopingSource` reopens at EOF, once per `read`
  (so an empty recording ends instead of spinning), and fails if a later
  pass changes sample rate or centre frequency. Both forward every
  `IqSource` method, `health_counters` included.
- **D7 — no discontinuity at the loop wrap.** `listen` discards a partial
  calibration fill on a discontinuity, so signalling one would leave a file
  shorter than 2 s calibrating forever. The wrap is a hard splice the
  channelizer hears as a click; acceptable for a demo, and documented in
  `--loop`'s help.
- **D8 — composition.** `PacedSource(LoopingSource(first, reopen))` in the
  `is_reconnectable()` `else` arm (`wrap_file_replay`), so one clock spans
  every pass. `reopen` calls `spec.open(capture_rate_hz, dial_freq_hz)`, so
  each pass gets a fresh decimator and the same dial override.
  `capture_rate_hz` is now bound once from `resolved.capture_rate_hz`.
  **Incidental MAN-73 fix:** the reconnect opener used to capture the CLI's
  raw `--capture-rate-hz`, so an `input.capture_rate_hz` or
  `MANTA_INPUT_CAPTURE_RATE_HZ` applied to the first open was dropped on
  every reconnect. It now uses the resolved value. No dedicated test: the
  closure is inline in `main()`.
- **D9 — IQ WAV rate ceiling.** `manta_input::MAX_IQ_WAV_RATE_HZ = 10 MS/s`
  (inclusive), the same as `MAX_HPSDR_RATE_HZ`, checked in
  `WavIqSource::open` right after the channel count and before any sample is
  read. Only the size is checked; the shape (`fs / 93.75` a power of two)
  stays `Channelizer::new`'s job and keeps its README-quoted message. At the
  largest table rate admitted (6.144 MS/s) a 4-frame file peaked at
  163 MiB RSS. ARCHITECTURE's 768 kS/s "supported ceiling" was the
  alternative; it would newly reject 1536–6144 kS/s recordings that
  construct today.
- **D10 — pacing changes nothing but timing.** Spot timestamps, the replay
  epoch and the session nonce are untouched, so spot logs stay
  byte-identical. A paced and an unpaced `--json` replay of the 30 s render
  hashed the same (77 lines each).
- **D11 — CI fixtures.** A 30 s V1 render for scenario 1 (spot at 20.90 s,
  ~9 s before EOF) and a 1.5 s render for scenario 2. Wall-clock assertions
  are lower bounds only; deadlines exist only to fail cleanly. The full
  120 s vector is the manual README check.

## Deltas from MAN-121

- No clap `requires = "source"` (D4).
- `--loop` implies pacing (D2).
- Reopening goes through `LiveSourceSpec::open` (MAN-169's `--source-iq`
  dispatch) instead of MAN-121's `open_replay_wav` sidecar/rate dispatch,
  which is not reintroduced.
- The rate guard sits in `WavIqSource::open`, so every command that opens
  an IQ WAV (`run`/`listen --source-iq`, `soak`, `doctor`, `decode`,
  `oracle`) is covered.

## PR #135's open findings

Both P2 threads ("Validate replay rates before allocating calibration
buffers", "Cap replay sample rates before allocation") asked for a bound
before any rate-sized allocation. D9 is that bound: the check runs before
`WavIqSource` reads a sample, so neither the channelizer, `FloorBank` nor
the 2 s calibration buffer is ever sized from a rejected rate.

## Verification

- `manta-input`: `iq_wav_declaring_a_rate_above_the_replay_ceiling_is_rejected`,
  `iq_wav_rate_ceiling_is_inclusive`,
  `iq_wav_at_the_largest_admitted_table_rate_opens`,
  `iq_wav_declaring_zero_hz_is_rejected`, and the `paced_source_*`,
  `pacing_delay_*`, `looping_source_*` and
  `paced_looping_source_keeps_one_clock_across_passes` tests in `replay.rs`.
- `manta-cli` unit tests: `realtime_flag_paces_a_cli_file_source`,
  `loop_flag_implies_pacing`,
  `input_realtime_and_loop_keys_apply_to_a_config_file_source`,
  `cli_realtime_adds_to_a_config_file_source`,
  `a_cli_source_selector_discards_the_files_realtime_and_loop`,
  `realtime_with_a_live_source_is_an_error`,
  `loop_with_the_default_audio_device_is_an_error`,
  `replay_mode_note_names_the_ignored_keys`,
  `wrap_file_replay_is_identity_for_the_default_mode`,
  `wrap_file_replay_loops_inside_pacing`,
  `input_realtime_and_loop_are_file_keys`,
  `manta_input_loop_env_sets_the_loop_key`.
- `tests/cli.rs`: `run_rejects_an_iq_wav_whose_declared_rate_would_exhaust_memory`,
  `decode_rejects_an_iq_wav_whose_declared_rate_would_exhaust_memory`,
  `realtime_replay_is_byte_identical_to_unpaced_replay`,
  `realtime_paces_a_decimated_replay_at_the_decimated_rate`,
  `realtime_with_a_live_source_fails_before_any_io`,
  `soak_ignores_input_realtime_with_a_note`.
- `tests/replay_acceptance.rs` (the ticket's two scenarios):
  `a_client_connecting_five_seconds_into_a_paced_replay_receives_a_well_formed_spot`,
  `a_recording_shorter_than_the_calibration_window_does_not_start_without_loop`,
  `a_looped_replay_shorter_than_the_connect_time_keeps_the_server_up_until_stopped`.

## Not done here

- No pacing or looping for `soak`/`doctor`.
- No live-feed history for late subscribers; `sh/dx` covers that.
- No change to `WavIqSource`'s eager whole-file load; `--loop` re-reads the
  file once per pass.
- ROADMAP M3's stock-client gate stays open: it is a human session with a
  real logger. This change only makes it runnable without an SDR.
