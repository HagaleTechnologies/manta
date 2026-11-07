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
| `inputCoverage` | the acoustic window [first - W/2, last + W/2] overlaps the recording, the frequency is inside the RF passband less half a channel, and not every rise near it was refused by a source mask | `outsideRecording`, `outsidePassband`, `sourceMask:<trustedBand\|analyticGuard\|digitalWindow\|mirrorImage>` |
| `detectableEnergy` | a candidate track was born within `trackTolHz` plus half a channel and was alive in the acoustic window | `noRise`, `refusedRise:<warmup\|staticCrash\|ownedByTrack\|sidelobePosition\|coolingDown\|notLocalPeak\|rtty\|coModulated>` |
| `admittedTrack` | one of those candidates leased a decoder (`TrackPromoted`) | `closed:<closeReason>` or `openAtEnd`, for the most advanced candidate (confirmed first, then the latest) |
| `usableTiming` | a leased candidate's speed came within max(3 WPM, 20%) of the RBN speed, or RBN gave none and the track has any estimate | `noSpeedUpdate`, `speedOff` |
| `textContainsCall` | a leased track within `spotTolHz` decoded the call as a whole word, or held it as a reading, inside the spot window (the word's first-to-last character, or the reading's run of consecutive holdings, overlaps it; text from elsewhere in the track's life does not count) | `substringOnly` (the words decoded in the spot window, spaces removed, contain the call), `noCall` |
| `selfIdRecognized` | a validator decision for the call that grammar, cty, the power-step guard and the beacon-evidence check did not reject, other than a blocklist or notch withhold (checked before every gate, so it proves none) | `rejected:<reason>`, `policy:blocklist`, `policy:notch`, `noDecision` |
| `sufficientEvidence` | a decision that cleared the repetition, variant, zero-evidence and beacon gates | `<reason> <reps>/<required>` |
| `emitted` | a spot for the call within the spot tolerances | `policy:blocklist`, `policy:notch` (lists replaced after a held capture), `lookupPending`, `lookupNotFound`, `dedupeOutsideWindow`, `notEmitted` |
| `delivered` | as `emitted`: offline, `SpotBus::publish` drops nothing (`deliveredMeans` in the header) | none offline |

The `Reason` mapping is an exhaustive `match` (`funnel/attribute.rs`
`reason_effect`): a new `Reason` does not compile until someone places it.
Grammar, cty, power-step guard and MAN-201's `beaconNoEvidence` lose at
`selfIdRecognized`; repetition,
variant, no-evidence and the four beacon reasons at `sufficientEvidence`;
the two lookup reasons at `emitted`. Blocklist and notch prove only that the
call reached the validator: it checks them ahead of grammar, cty, acoustic
evidence, repetition and variant (MAN-31), so they credit none of those
gates and a policy-only call loses at `selfIdRecognized` as
`policy:<blocklist|notch>`. `dedupe` passes `sufficientEvidence` but is not
evidence of emission: its 600 s window outlasts the spot window, so the
original spot can sit outside the opportunity (a spot at 10 s suppresses a
copy at 400 s, whose spot window is 310-490 s). Only a spot that itself
matches the opportunity credits emission; otherwise the loss is
`emitted` / `dedupeOutsideWindow`.

The summary keeps candidate births, confirmations and leased (active) track
births apart in `births`, as the ticket asks, and counts closes by reason plus
`openAtEnd`.

## Oracle-bypass experiments

`--funnel-bypass` (requires `--funnel`) names the two RBN-fed bypasses:
`known-frequency` and, since MAN-493, `known-timing`. Each bypass, for
every opportunity with audio, leases a fresh decoder on
the truth frequency over the acoustic window clipped to the recording, fed
the channel with the highest mean power of the three around it (the rule
`manta oracle` uses, applied over the episode's whole window), and passes
its events through a fresh production validator. `known-timing` seeds that
decoder at the truth speed first (`TrackDecoder::seed_speed_wpm`), forcing
`usableTiming` as well, and reports `noRbnWpm` or `hsmmEngine` when the
truth carries no speed or the engine's speed never passes through the
seedable tracker. Call-keyed evidence is
still judged over the wider spot window, which the bypass does not decode,
so a bypass line can lose where production passed; `upperBound` counts
production's deliveries too.
The same `attribute` function then charges the result. `detectableEnergy`
and `admittedTrack` are forced (`forcedStages`); opportunities lost at
`inputCoverage` as `outsideRecording` or `outsidePassband` are
`available: false`. A source-mask loss is available: the bypass skips the
mask with everything else before the decoder.

The header's `bypasses` lists what ran. After the production summary come
one `lossFunnelBypass` line per opportunity and one
`lossFunnelBypassSummary`, each with `oracleAssisted: true`. The summary's
`upperBound` counts opportunities delivered by production or by the bypass;
`recoveredOverProduction` counts those only the bypass delivered. Neither
is a recall score, no exit code reads them, and no bypass line has a
`dxCall`, `frequency` or `spots` key. Apart from the header's `bypasses`,
the production lines are the same bytes with or without bypasses
(`oracle_funnel.rs`
`funnel_bypass_known_frequency_appends_after_summary`,
`funnel_bypass_known_timing_appends_after_summary`). Unknown names,
including the two synthetic-only ones below, exit 2.

MAN-493 added the two keying-truth experiments as engine API only:
`run_synthetic_bypass` takes `known-transitions` (the exact on/off keying
timeline fed hop by hop to a fresh seeded decoder) and `known-transcript`
(decoder events synthesized from the known words), built as plain data by
`manta-testkit::funnel_truth` and run by
`cargo run -p manta-engine --example loss_funnel_synthetic`. They force
detection, admission and timing by construction; the text and every
validator gate stay honest. Their names are not CLI flag values.

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
- D5a. The spawn-veto census. With a trace set, each hop, after its spawn
  scan, the tracker records for every rising channel the scan refused the
  condition it fails, counted per (stream second, channel). A rising channel
  that a track born this hop now owns (its owner differs from the snapshot
  taken before the scan) was spawned, not refused, and is skipped. Source masks
  are tested first: a channel inside the input's trusted band, analytic
  guard, digital window or mirror image can never spawn, whatever else
  holds. The scan itself is untouched. A truth frequency with no candidate
  is charged to `inputCoverage` (`sourceMask:<veto>`) when every refused
  rise near it in the window was a source mask and no later stage has
  evidence; otherwise to `detectableEnergy`, as `refusedRise:<veto>` (the
  most-counted detector veto, ties to the first in the order above) or
  `noRise`. A refused rise stays at `detectableEnergy`, not
  `admittedTrack`: no track opened, which is Gherkin scenario 1.
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
  `funnel_charges_one_stage_per_opportunity_on_v1` (OH2XYZ: `noRise`,
  nothing counted past `detectableEnergy`); `source_mask_is_input_coverage`
  pins the analytic-guard case.
- "A truth opportunity decoded correctly counts as a full pass":
  `attribute.rs` `gherkin_validated_spot_is_delivered`, and end to end the
  same `loss_funnel.rs` test (W1AW on vector V1).
- Through the binary: `crates/manta-cli/tests/oracle_funnel.rs` pins the
  file shape, the stderr note, two-run byte identity, and that stdout and
  `--jsonl` are byte-identical with and without `--funnel`.

## Follow-ups

- The validator role log (`role:<addressee|...>` details at `selfIdRecognized`).
- Done by MAN-493: the other three oracle-bypass experiments -- known
  timing (the `SpeedTracker` seed in `manta-decode`,
  `SpeedTracker::seed_wpm`/`TrackDecoder::seed_speed_wpm`) and known key
  transitions and known transcript (synthetic keying truth,
  `manta-testkit::funnel_truth` + `run_synthetic_bypass`) -- all built and
  segregated the same way.
- Running the funnel with the config's `[spot]` and `freq_correction_ppm`, as
  `manta decode` does; the nine-row stage-scene golden test.
- The plain oracle's out-of-range rows (outside the recording or passband are
  still counted in its `n`).
