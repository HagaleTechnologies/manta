# MAN-323: FT8 and FT4 spots from the band streams manta already receives

**Status:** implemented, including the MAN-485 wiring slice. An input's
`digital_modes` key enables the engine tap, the Publisher sink, the
JSON-only spot path and the `manta_ftx_` metrics in builds with `ftx`.
Single-input and multi-input `manta run` sessions report coverage before
listening. The decoder pin now follows PR #550's untagged merge commit.
A five-minute live receiver run and the clock warning with network time
off remain operator checks; neither has been verified in the cloud run.

## Context

The ticket asks manta to decode FT8 and FT4 on the IQ streams it already
skims for CW, and to spot the callers on the JSON stream. Its contract note
(dispensa Q-0085, unread from this repository) says to use `pancetta-ft8`
as a pinned git dependency on its stable surface and not to build a decoder
in manta.

On 2026-11-01 the decoder surface the contract names (`decode_cycle`,
`CycleDecoder`, `docs/decoder-crate.md`) exists only on pancetta PR #550
(PAN-176), which was open (`mergeCommit: null`) with head
`390d7860315d5086eb52b834605a0a7ff4b741c5`. pancetta's tags were `v0.9.5`
and `v0.9.6`; no `pancetta-ft8-v*` tag existed.

## Decisions

- **D1. Pin.** MAN-485 pins
  `pancetta-ft8` to PR #550's merge commit
  `7f8078b19d759e24f50adb1efd0b3fd75371a39d`. PR #550 merged without a
  `pancetta-ft8-v*` tag. The rule, in order: such a tag when one exists;
  PR #550's merge commit when it has merged untagged; its head otherwise.
  `deny.toml` allows the pancetta URL; the crate also fetches the
  `kgoba/ft8_lib` submodule at build time, which cargo-deny does not see.
  Rules out pancetta `main`, which had only the 12.64 s `decode_window`
  family when the decoder work began.
- **D2. Contract.** `docs/decoder-crate.md` at the pinned commit: default
  features, the `CycleDecoder` reuse form, 12 kHz mono `f32` with sample 0
  on the slot boundary (FT8 180 000 samples, FT4 90 000). "Don't build it
  from manta" is read as: manta writes and vendors no decoder; Cargo builds
  the pinned crate.
- **D3. Feature.** `ftx` on `manta-engine` (`dep:pancetta-ft8`) and
  `manta-cli` (`manta-engine/ftx`), off by default like `soapy`. Everything
  except the adapter (`manta_engine::ftx::new_decoder`) builds and is
  tested without it. CI runs the feature's clippy, unit and band-stream
  tests on Linux and macOS; MSVC cannot compile ft8_lib, so Windows skips
  them.
- **D4. Config.** `[input].digital_modes` defaults to `[]`, accepts exact
  `"FT8"` and `"FT4"` spellings once each, and is restart-only. A nonempty
  list needs a build with `ftx` and a `[server]` table. The shared loader
  refuses unsupported or repeated modes and unmet requirements for config
  check, run, doctor and reload. Band-plan expansion copies the key to
  every stream; `MANTA_INPUT_DIGITAL_MODES='["FT8"]'` overrides it.
- **D5. Coverage** (`manta_engine::ftx::coverage`). A window is the mode's
  dial from the MAN-311 table plus the audio span `[dial + 300, dial + 3000]`
  Hz. A stream attempts it only when the whole span lies inside
  `centre + rf_passband_hz()`, narrowed by `trusted_band_hz()`. A partly
  covered span is `Partial`; further skips are `NoRfReference` (centre 0)
  and `UnsupportedRate` (not a multiple of 24 000 S/s). For the KiwiSDR
  20 m plan (passband +/-5000 Hz), `kiwi-20m-10` (centre 14 079 167 Hz)
  holds both the FT8 and FT4 spans, `kiwi-20m-9` part of FT8 only and
  `kiwi-20m-11` part of FT4 only. The research's +/-4.526 kHz reading (the
  CW skimming fraction) does not apply: the tap does not use the
  channelizer.
- **D6. Demodulator** (`manta_engine::ftx::demod::UsbDemod`). Mix the span's
  centre (dial + 1650 Hz) to 0 Hz with an `f64` phase; stage A, Kaiser
  70 dB, pass 1950 Hz, stop 22 050 Hz, decimates to 24 kS/s (13/23/43/85
  taps at 48/96/192/384 kS/s); stage B, Kaiser 70 dB, pass 1350 Hz, stop
  1950 Hz, 175 taps, decimates by 2; mix up 1650 Hz and take the real part
  times sqrt(2). Stage B rejects the lower sideband (at least 60 dB,
  tested). Output does not depend on chunk size. Group delay is about
  3.74 ms. Rules out a whole-cycle FFT, which is not streaming and needs
  about 23 MB per 192 kS/s window.
- **D7. Slicing** (`manta_engine::ftx::slicer::CycleSlicer`). Audio sample
  `m` stands for `epoch + m / 12000 - group_delay`. Slots start on every
  UTC instant divisible by 15 s (FT8) or 7.5 s (FT4), in integer
  milliseconds. The slot in progress at startup is skipped and reported
  incomplete; a discontinuity drops the slot it breaks and advances the
  clock by the gap; missing samples are zero-filled.
- **D8. Where decoding runs** (`manta_engine::ftx::tap`). Replay decodes
  inline in `read()`, so decodes are a pure function of the file and its
  epoch. Live input posts complete slots to per-window mailboxes; a newer
  slot replaces an undecoded one, reported `dropped`; one `ftx-decode`
  thread takes the oldest pending slot. Every decode runs in one
  process-wide one-thread rayon pool, so the decoder never competes with
  the CW decoder pool.

- **D9. Replay order.** FTx results from several replayed inputs publish
  in `(slot start, input index, mode, decode order)` order, independent of
  worker interleaving. Each input's end releases its remaining results.
  Their position among CW JSON records is not fixed.
- **D10. Spot rule.** Only CQ callers become spots, using MAN-320's CQ
  parsing rule. The grid comes from the CQ; `RR73` is not a grid. Repeats
  are held for 600 s per callsign, mode and band. Every decoded message
  still contributes to the decode counter. Frequency includes the input's
  `freq_correction_ppm` calibration.
- **D11. Wire.** An FT8/FT4 spot is its own JSON-only bus record,
  `JsonItem::Ftx`, holding `FtxSpot { mode, callsign, grid, freq_hz,
  snr_db, slot_start_ms }`. `SpotBus::publish_ftx` gives it the next `seq`
  and a place in the resume history, aged by its slot start, and sends it
  on the JSON channel only: telnet, the RBN uplink and `recent()` never see
  it. `SpotMessage::from_ftx` renders the same 30 keys as a CW spot, with
  `mode` `"FT8"`/`"FT4"`, `dxGrid` from the `CQ`, `timestamp` the slot start
  rounded down to whole seconds, the decoder's SNR on the 2500 Hz
  reference, and `wpm`, `rst`, `decodeConfidence` and
  `channelizerResolutionHz` null. The `id` is
  `station_callsign:session_nonce:MODE:slot_start:dxCall`. Rejected: folding
  FTx spots into `manta_spot::Spot`, which would put them on telnet and the
  uplink and is MAN-322's `Spot.mode` work.
- **Metrics.** `manta_ftx_cycles_total{input,mode,outcome}`,
  `manta_ftx_decodes_total{mode}`, `manta_ftx_spots_total{mode}` and
  `manta_ftx_decode_seconds{input,mode}` render only once a window is
  registered with `Metrics::register_ftx_window`, so a run without FT8/FT4
  shows no `manta_ftx_` line, not even a header.

- **D12. Clock.** A live session attempting a window checks the kernel's
  network-clock sync state once. An unsynchronised clock warns that UTC
  slot decoding needs about 1 s accuracy and names network time/chrony;
  an unreadable state asks the operator to check `manta doctor`. Replay
  uses its own epoch and prints no clock warning. The slot in progress at
  startup is skipped, so the first FT8 spot can take up to 30 s.
- **D13. Cost.** The MAN-323 prototype measured roughly 4% of one core for
  one FT8 window at 96 kS/s. FT4 doubles the decode rate (7.5 s slots),
  costing about 2 s of a Ryzen 9 5950X core per slot; a Pi 4 may drop
  slots. These are prototype measurements, not a new platform guarantee.
  With the key unset, no tap, decode thread or FTx metric series is built.

## Evidence

- `cargo test -p manta-engine --features ftx --test ftx_band_stream`
  decodes `CQ W1AW FN31` synthesised 1234 Hz above the 14 074 kHz dial at
  -10 dB through the tap at 48, 96, 192 and 384 kS/s, within 3.125 Hz and
  inside -13 to -7 dB; a +60 dB FT8 signal 1050 Hz below the dial does not
  fold in beside a +40 dB target; the FT4 CQ decodes on the 14 080 kHz
  dial; two replay runs give identical results.
- The test signals are GFSK synthesised from tone strings that ft8_lib's
  own encoder printed (`gen_ft8` at the vendored `9fec6ca`), in
  `manta_testkit::ftx`. No binary fixture is committed.
- `cargo test -p manta-server --test json_stream_acceptance
  an_ft8_spot_reaches_the_json_stream_labelled_ft8`: a TCP and a WebSocket
  client each receive the published FT8 spot as a 30-key line with
  `"mode":"FT8"`, `"dxGrid":"FN31"` and `"wpm":null`, and the telnet
  channel receives nothing. The FT8 example in `docs/SPOT-SCHEMA.md` is
  pinned to `from_ftx`'s output by
  `the_spot_schema_ft8_example_is_what_from_ftx_renders`.
- `the_ft8_lib_c_decoder_is_linked_not_stubbed` asserts
  `pancetta_ft8::ft8lib_is_available()`: pancetta's build falls back to a
  stub with reduced recall when the submodule is missing.

## Known limits

- FT4 costs about 2 s of one desktop core per 7.5 s slot (measured in the
  plan, Ryzen 9 5950X); a Raspberry Pi 4 may drop FT4 slots.
- pancetta's FT4 SNR reads low (-15.2 dB for -8 dB injected in the plan's
  prototype); FT4 SNR is not asserted.
- The `kgoba/ft8_lib` submodule fetch is outside cargo-deny's view.
- The slicer anchors sample 0 to the session epoch once and then counts
  samples (D7, as planned). A live source whose sample clock runs off
  nominal drifts against UTC: 50 ppm is 2.5 s after about 14 h, past the
  decoder's DT range. The decode-seconds and spot-rate metrics can show
  that drift; re-anchoring to the wall clock is a follow-up.
- Decoders registered with the live decode service stay for the session:
  inputs are restart-only, so no input re-attaches with a new window set.

## Follow-ups

- Move the pin to a `pancetta-ft8-v*` tag when one exists.
- Operator verification: five-minute live 20 m receiver run and a clock
  warning on a host with network time disabled.
- Release builds with `ftx`, once ft8_lib builds on every release target.
- A diff of dispensa Q-0085 against `docs/decoder-crate.md`.
- Re-anchor live slot boundaries to the wall clock if sample-clock drift
  shows in the field.
