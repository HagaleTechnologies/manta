# MAN-198: a stage-attributed loss funnel for `manta oracle`

Date: 2026-11-03. Ticket: MAN-198. Source: docs/DECISIONS/2026-09-09-decoder-recall-research.md,
investigation 1. This record covers the first slice; the rest of the plan is
listed under "Follow-ups".

## What it does

`manta oracle --funnel PATH` runs the recording once more through the
production pipeline (`listen_with_observers`) with a write-only track trace
attached, and writes JSON Lines to PATH: a `lossFunnelHeader`, one
`lossFunnel` record per RBN episode, and a `lossFunnelSummary`. The oracle's
stdout and `--jsonl` are unchanged; one `note: loss funnel: ...` line goes to
stderr. No exit code reads the funnel. Nothing in it is a recall score.

An opportunity is one RBN episode: the chosen spotter's rows for one call on
one 0.1 kHz bin, split where two rows are more than 900 s apart. Rows outside
the recording are kept and charged to input coverage.

Each opportunity is charged to exactly one earliest failed stage. `reached` is
the furthest stage with evidence; `lostAt` is the stage after it, or `null`
when the call was delivered. Later evidence implies the earlier stages, so an
opportunity is never counted twice.

| Stage (`lostAt`) | Evidence that it passed | Failure `detail` |
|---|---|---|
| `inputCoverage` | the acoustic window [first - W/2, last + W/2] overlaps the recording, and the frequency is inside the RF passband less half a channel | `outsideRecording`, `outsidePassband` |
| `detectableEnergy` | a candidate track was born within `trackTolHz` plus half a channel and was alive in the acoustic window | `noCandidate` |
| `admittedTrack` | one of those candidates leased a decoder (`TrackPromoted`) | `closed:<closeReason>` or `openAtEnd`, for the most advanced candidate (confirmed first, then the latest) |
| `usableTiming` | a leased candidate's speed came within max(3 WPM, 20%) of the RBN speed, or RBN gave none and the track has any estimate | `noSpeedUpdate`, `speedOff` |
| `textContainsCall` | a leased track within `spotTolHz` and the spot window decoded the call as a whole word, or held it as a reading | `substringOnly`, `noCall` |
| `selfIdRecognized` | a validator decision for the call that grammar, cty and the power-step guard did not reject | `rejected:<reason>`, `noDecision` |
| `sufficientEvidence` | a decision that cleared the repetition, variant, zero-evidence and beacon gates | `<reason> <reps>/<required>` |
| `emitted` | a spot for the call within the spot tolerances, or a `dedupe` withhold | `policy:blocklist`, `policy:notch`, `lookupPending`, `lookupNotFound`, `notEmitted` |
| `delivered` | as `emitted`: offline, `SpotBus::publish` drops nothing (`deliveredMeans` in the header) | none offline |

The `Reason` mapping is an exhaustive `match` (`funnel/attribute.rs`
`reason_effect`): a new `Reason` does not compile until someone places it.
Grammar, cty and power-step guard lose at `selfIdRecognized`; repetition,
variant, no-evidence and the four beacon reasons at `sufficientEvidence`;
blocklist, notch and the two lookup reasons at `emitted`. `dedupe` is
evidence of delivery: the validator withholds a duplicate only after
delivering the same call within 600 s, shorter than the episode gap.

The summary keeps candidate births, confirmations and leased (active) track
births apart in `births`, as the ticket asks, and counts closes by reason plus
`openAtEnd`.

## Decisions

- D1. The funnel lives in `manta-engine::funnel`; RBN episode parsing lives in
  `manta-testkit::oracle` (`parse_rbn_episodes`); the CLI wires them in
  `manta-cli/src/funnel_cmd.rs`. Attribution needs engine and spot types, and
  testkit cannot depend on the engine.
- D2. One production pass. Detection and admission depend on band-wide state
  (floor, track cap, cooldown), which a per-window rerun does not reproduce.
- D3. Several opportunities may share one track (both calls of a QSO).
  Call-keyed evidence never credits the wrong call.
- D4. Tolerances are `manta grade`'s 500 Hz and 90 s for call-keyed evidence,
  150 Hz plus half a channel for track evidence, and the oracle's
  `--window-s`. They are flags (`--funnel-track-tol-hz`,
  `--funnel-spot-tol-hz`, `--funnel-spot-window-s`) and are echoed in the
  header.
- D5. The trace is opt-in. `TrackManager::set_funnel_trace(None)` (the
  default) records nothing; every close still goes through the same
  `close_counts`. A test pins that events and spots are equal with and
  without the trace.
- D6. The `Reason` labels moved into `manta_spot::Reason::label`;
  `manta_server::live_state::reason_label` delegates, so the live-state wire
  is unchanged.

- D7. The header names the run: `truthLayer`, `spotter`, `captureStart`
  (ISO-8601 UTC), `recordingFile`, `recordingSha256`, `sidecarSha256` (the
  `<name>.json` or `PATH.meta.jsonl` beside the recording, or `null`),
  `referenceFile`, `referenceSha256`, `configSha256`, `build`
  (`DECODER_VERSION`), `engine`, `prior`, the geometry, the tolerances, the
  stage order, `deliveredMeans` and `bypasses`. It carries no wall-clock
  field, so two runs on the same inputs write the same bytes.

## Gherkin mapping

- "A truth opportunity lost at detection": `attribute.rs`
  `gherkin_no_track_opened_is_detectable_energy`, and end to end
  `crates/manta-engine/tests/loss_funnel.rs`
  `funnel_charges_one_stage_per_opportunity_on_v1` (OH2XYZ: no candidate,
  nothing counted past `detectableEnergy`).
- "A truth opportunity decoded correctly counts as a full pass":
  `attribute.rs` `gherkin_validated_spot_is_delivered`, and end to end the
  same `loss_funnel.rs` test (W1AW on vector V1).
- Through the binary: `crates/manta-cli/tests/oracle_funnel.rs` pins the
  file shape, the stderr note, two-run byte identity, and that stdout and
  `--jsonl` are byte-identical with and without `--funnel`.

## Follow-ups

- The spawn-veto census (`refusedRise:<veto>` and `sourceMask:<veto>` details).
- The validator role log (`role:<addressee|...>` details at `selfIdRecognized`).
- The oracle-bypass experiments (known frequency, transitions, timing,
  transcript), always segregated as `oracleAssisted` lines after the
  production summary.
- Running the funnel with the config's `[spot]` and `freq_correction_ppm`, as
  `manta decode` does; the nine-row stage-scene golden test.
- When this branch is replayed onto a main that carries MAN-201's
  `Reason::BeaconNoEvidence`, `Reason::label` gains `beaconNoEvidence` and
  `reason_effect` charges it to `selfIdRecognized` (with grammar and cty),
  as the plan's stage table places it.
- The plain oracle's out-of-range rows (outside the recording or passband are
  still counted in its `n`).
