# 2026-10-15 — MAN-13: one daemon decodes several receivers into one spot output

**Status:** Implemented (branch `MAN-13`): the engine, spot validation,
config, `run`, health, alert and calibrate paths below. The items under
"Not yet" are open and named.

**Ticket:** MAN-13. Scope reading: the ticket's comment 2 (Catalyst Cloud,
2026-10-11): "one process with N input streams sharing validation, dedupe,
servers and metrics". The description asks for both topologies day one;
comment 2 moves "one HPSDR radio, one receiver per band" to MAN-307. Here,
several channels of one SDR means ka9q-radio SSRCs on one radiod and
channels of one KiwiSDR, whose transports are already independent; the
loader refuses two entries on one HPSDR radio until MAN-307.

## Problem

A manta process read one receiver. Operators with several SDRs, or several
receiver channels of one SDR, ran one process per receiver, and a station
in two receivers' overlap was spotted twice: on base `6596e3a`, two `run`
processes over two segments (96 kS/s at 14.000 MHz, 192 kS/s at
14.060 MHz) both spotted W1AW. The two receivers' W1AW tracks read
14012349.9 Hz and 14012352.7 Hz: 3 Hz apart, either side of the
repetition gate's 100 Hz bucket edge.

The rejected alternative, N processes plus an external merger, would have
to own cross-process dedupe, per-process calibration, identity and split
observability, and would be thrown away once real multi-input support
existed.

## Decisions

- **D1 One thread per input, one back end.** Each input opens and runs on
  its own OS thread (`IqSource` is not `Send`): source read, channelizer,
  `TrackManager`, decoder-pool drain, RTTY and static-crash observers
  (`listen::run_input_front`). Batches go to one back end on `run`'s
  thread, which owns the only `Validator` and every callback
  (`listen::Back`). `listen_with_observers` is unchanged, so a single
  input's output is unchanged by construction; the engine test
  `one_input_through_listen_inputs_matches_listen_with_observers` checks
  that the multi-input path gives the same events and spots for one input.
- **D2 Common clock.** The session rate F is the highest input rate. Every
  channelizer rate is 93.75 Hz × 2^k, so input *i*'s `sample_ts` maps
  exactly to `offset_i + sample_ts × F / fs_i` (`SourceClock`). Replay:
  every offset is 0. Live: each input's open-completion time after the
  earliest one. Spot and event timestamps from several inputs count
  samples at F.
- **D3 Interleaved track ids.** Input *i* of N numbers its tracks
  `1+i, 1+i+N, ...` (`TrackIdSpace`), so ids stay unique and the
  validator recovers an input as `(id − 1) mod N`. On overflow an input
  wraps to its own first id. One input keeps today's numbering.
- **D4 Per-input calibration.** Each front end calibrates its own
  `TrackMeta`/`TrackPromoted` frequencies with its own
  `freq_correction_ppm`; the shared validator is built with factor 1.0.
- **D5 Merge order.** Replay merges whole batches in `(batch end time,
  input index)` order, blocking only on inputs with nothing queued, so the
  order is a pure function of the files and replays are byte-identical.
  Live inputs merge in arrival order through one queue, so no input waits
  on another. Files and live receivers do not mix in one run.
- **D6 Cross-input dedupe** (`manta-spot::dedupe`). Within the 600 s
  window, a report from a different input than the stored spot's is a
  duplicate in the same 300 Hz bucket, or within one channel (93.75 Hz)
  across a bucket edge, with no SNR or spot-type override. Same-input
  behaviour is unchanged; the stored time never moves backwards. Honouring
  diversity (another input's much stronger report) belongs with per-
  receiver attribution (MAN-14) and the cross-source refinements MAN-16
  owns (MAN-53's open questions).
- **D7 Failure isolation.** An input whose read errors or panics ends only
  its own thread; the back end logs `input NAME: stopped (...)` and the
  other inputs keep spotting. A live input reconnects on its own thread
  (`ReconnectingSource`) and logs `input NAME: lost; retrying, k of n
  inputs decoding` / `back`. Startup opens every input in parallel and
  fails fast naming the input (`input "b" failed to open: ...`); the
  common rate and every input's RF reference depend on every input.
- **D8 Health.** With two or more inputs, `/healthz` and `manta_healthy`
  are `ok` while every listener is up, the decode watchdog passes and at
  least one input is healthy; each input keeps its own line and
  `manta_source_health{source="<name>"}` series. One input: unchanged.
- **D9 Metrics.** No new families. `manta_active_tracks` and
  `manta_static_crashes_total` are sums over inputs.
- **D10 Config.** `[[input]]` entries take `[input]`'s keys plus `name`;
  a one-entry array is the single form. Names are required, unique,
  `[A-Za-z0-9][A-Za-z0-9._-]{0,31}`; at most 16 entries; each needs a
  type; files and live receivers do not mix; `replay_epoch` only on the
  first entry; file entries agree on `realtime`/`loop`; an `audio` entry
  needs `center_freq_hz`; `MANTA_INPUT_*` and the per-receiver flags are
  refused with several entries. Every input needs an RF reference
  (sidecar or `center_freq_hz`), checked before any input opens.
- **D11 One-receiver commands.** `calibrate`, `doctor`, `soak` and `check`
  take `--input NAME`, required with several inputs. `calibrate --input
  NAME` saves `freq_correction_ppm` into that `[[input]]` entry
  (`config_edit::set_freq_correction_ppm`), keeping every other byte.
- **D12 Wire output unchanged.** One station identity, the same telnet,
  JSON and uplink formats.

- **D13 Operator lines.** One input: unchanged. Several: each input logs
  `input NAME: sample_rate_hz=… dial_frequency=…` after opening and its own
  `ready: decoding source=NAME` line on its first batch; health edges log
  `input NAME: lost; retrying, k of n inputs decoding`, `input NAME: back;
  …` and `input NAME: stopped (…); …`; the status line appends
  `inputs=k/n` only with two or more registered sources.
- **D14 Coverage.** `SKIMMER/SETT` advertises the union of every input's
  segments, each built with its own calibration, sorted and merged
  (`manta-server::sett::segments_for_inputs`).
- **D15 Alert.** `MantaInputDown` (`manta_source_health == 0` for 2 m,
  warning) reports a dead input, since `MantaNodeUnhealthy` no longer fires
  for one input of several.
- **D16 Topology B.** Several channels of one SDR come from ka9q-radio
  (SSRCs on one radiod) and KiwiSDR (channels of one receiver), whose
  transports are independent. HPSDR multi-DDC is MAN-307.

## Services with several inputs

The live-state stream, SIGHUP reload, the scheduled list refresh, RTTY
marks and per-input `manta_input_*` counters work with several inputs as
with one. Each input's front end takes its own `bandState` sample, named
in `source`. `manta_rtty_signals` counts the union of every input's marks
(two inputs' marks within 50 Hz count once), and a lost input clears only
its own. An input that ends while the daemon runs on reads
`manta_source_health{source} 0` from then on; a clean end at shutdown is
not counted as an outage.

## Not yet

These parts of the plan are not in this change and stay open:

- Single-input runs keep `listen_with_observers` unchanged rather than a
  front/back split of it; the multi-input path has its own front end
  (`listen::run_input_front`) and back end (`listen::Back`).
- The wiki pages (determinism, source reconnect, spot validation).

## Live clock drift

A live input's clock offset is fixed at open. Receiver oscillators drift
about 10 ppm, under 30 s a month, far inside the 600 s dedupe window; the
gate's 1 s cross-track filter can at worst grant an extra repetition
credit, never a duplicate spot.
