# MAN-272: a spot carries its track's latest RST and a QRL? flag

Date: 2026-10-16. Ticket: MAN-272, which replaces MAN-33 (PR #159, closed
by the 2026-10-05 audit triage; its branch survives as the tag
`archive/MAN-33-pr159` at `d8cc13e0e7`).

## Problem

CW Skimmer's band map shows two pileup cues next to a decoded station: the
most recent RST it sent (the "599" label) and a "QRL?" label when it asked
whether the frequency is in use (CW Skimmer manual, Band map). manta's
`manta-spot` recognised callsign context (CQ/DE/Beacon/Unknown) and nothing
else in decoded text. `Spot` had no RST or QRL field, so no manta surface
could tell an operator what report a station was sending or that it had
asked `QRL?`. The capability matrix
(`docs/DECISIONS/2026-09-01-legacy-capability-matrix.md`) listed this as a
gap.

Reproduced on `main` at `799379d`: `grep -rn "RST\|QRL" crates/manta-spot/src`
found only MAN-105's `NON_CALLSIGN_TOKENS` in `grammar.rs`, which already
rejects `5NN`/`599` as callsigns. A recording keyed
`QRL? CQ CQ DE K5ARH K5ARH TU 5NN K` printed
`SPOT: K5ARH (CQ) 14012.3 kHz 21 dB 20 WPM conf=0.53` and a JSON spot with
no report or query field.

## Decision

`manta_spot::message` (`parse_rst`, `is_qrl_query`) scans each completed
word. `Validator` keeps the result per track, and every spot it emits copies
its emitting track's values into two new `Spot` fields, `rst:
Option<String>` and `qrl_query: bool`.

| Surface | What changes |
|---|---|
| JSON stream (`:7301`) | Two keys, always present, between `wpm` and `decodeConfidence`: `"rst":"599"` or `"rst":null`, `"qrlQuery":true` or `false` |
| `manta run` stdout | ` rst=599` and then ` QRL?` after `conf=`, each only when present, so an unannotated spot prints the same line as before |
| `run --json` / `decode --json` | `rst`/`qrl_query` on the provisional debug `Spot` object (it serialises `Spot` directly) |
| Telnet (`:7300`) and the RBN uplink | Nothing. `rbn::format_line` is unchanged and a test pins it |

The decisions, each with what it rules out:

- **D1. A new module, not an extension of `context`.** `context` classifies
  callsign candidates and decides spot type. RST and QRL? gate nothing; they
  ride on a spot that already passed. The module is PR #159's
  `message.rs` with its doc references updated.
- **D2. RST is `(?i)\b([1-5])([1-9N])([1-9N])\b`, with `N` read as `9`.** A
  valid RST has R 1-5 and S, T 1-9, so the `T` = 0 cut cannot occur inside
  one. Accepting the `A`/`U`/`V`/`E` cut letters would turn callsign
  fragments such as `V3E` into reports. A three-digit contest serial without
  a `0` (`199`) is RST-shaped and wins under last-value-wins; that is an
  accepted limitation, pinned by `a_three_digit_serial_can_be_mistaken_for_an_rst`.
- **D3. QRL? needs the literal `?`: `(?i)\bQRL\?`.** A bare `QRL` is the
  answer ("the frequency is in use"), not the query; Codex review on PR #159
  reversed the original rule for this reason. The decoder's `?` is
  overloaded: `manta_decode::beam` emits `Glyph::Char('?')` at confidence
  0.0 for a glyphless beam survivor (SPEC §4.4.4). A placeholder `?` after
  `QRL` therefore counts. That is accepted: the flag agrees with the decoded
  text every other surface shows, and the placeholder after `QRL` is most
  often a mis-timed `..--..`. Rejected: discriminating on `confidence >
  0.0`. That couples `manta-spot` to the legacy beam's placeholder encoding,
  and the HSMM engine's margin confidence can legitimately be 0.0.
- **D4. Per-track state, scanned when a word completes.** `TrackState.rst`
  is last value wins and `TrackState.qrl_query` is sticky. Both are set in
  `ingest`'s `WordBoundary` arm before `try_spot`, so "most recent" means
  most recently decoded on the track, not "still in the 16-word window"
  (V32's aged-out case). A word wiped by the `<ERR>` prosign is never
  scanned. `TrackState: Default` and the unconditional `tracks.remove` on
  every `TrackClosed` reset both with no teardown code.
- **D5. No forwarding through a track merge.** A `Bookkeeping {
  survivor_track_id: Some(_) }` close drops the loser's annotations with
  the rest of its `TrackState`; a comment at the
  `migrate_or_discard_pending_beacons` call says so, and V33's merge case
  pins it. Reasons:
  1. It matches how the survivor's words and CQ/DE context already behave:
     a spot's message content comes only from its own track's decode.
  2. In the common merge case, a duplicate spawn on the same signal, the
     survivor decoded the same text and already holds the annotation.
  3. In a convergence of two different stations, forwarding would label one
     station with the other's `QRL?` or report.
  4. It removes the whole defect class behind PR #159's 41 repair rounds.

  Accepted cost: a station whose `QRL?` or report was decoded only by a
  track that later merged away shows `null`/`false` until its own track
  decodes it again.
- **D6. Read when the spot is built, from the emitting track, at both
  `Spot` construction sites.** One helper, `Validator::track_annotations`.
  `evaluate_candidate` calls it immediately before building the spot;
  `resolve_pending_beacons` calls it once per close. A migrated deferred
  beacon resolves under the survivor's `track_id` and therefore carries the
  survivor's annotations, consistent with the spot's own `track_id`.
  Rejected: snapshotting annotations into `PendingBeacon` at capture. That
  forwards them through merges by the back door, which contradicts D5.
- **D7. JSON stream and CLI only, never telnet or the uplink.** The AK1A
  line (MAN-88, byte-pinned against a live RBN capture) has no column for
  either value, and it also feeds the outbound RBN uplink. CW Skimmer shows
  both only in its band map, not in what it sends. `SpotMessage` uses no
  `skip_serializing_if`: SPOT-SCHEMA's "keys are never omitted" invariant
  holds.
- **D8. Ship ahead of dispensa.** dispensa's `spots.v1.schema.json` has no
  `additionalProperties: false`, so the two keys break no consumer, the
  same reasoning MAN-102 used for `snrRefHz`. The fragment below is ready to
  lift.
- **D9. No dedupe override.** A report decoded after a spot was sent
  reaches the operator on the next natural re-spot. The ticket requires the
  emitted spot to carry the latest value, not that a new spot be emitted,
  and extra spots would push duplicates toward RBN through the uplink.
- **D10. No config key.** Extraction is unconditional, gates nothing, and
  costs two regex scans per completed word.

`manta_spot::Spot` gains two public fields, so an out-of-tree struct literal
fails with `E0063`; none is known. `Spot` deliberately has no `Default`: a
default spot is a plausible-looking empty-callsign spot at 0 Hz.

## PR #159 and its open threads

PR #159 carried annotations through merges in `validator.rs` and
`manta-engine`'s `track.rs` (+1197 and +463 lines, nearly all of it
forwarding). It stopped on the repair-round cap with three threads open.
D5 makes each of them moot, because no annotation crosses a merge:

| PR #159 thread | Why it does not apply here |
|---|---|
| Forward annotations through silent merge intermediates (`validator.rs`) | Nothing is forwarded through any merge, silent or not |
| Order equal-hop redirects before lower-ID survivors (`track.rs`) | `manta-engine` is unchanged; closure ordering does not affect annotations |
| Raise the deferred-state cap to match the shipped track cap (`validator.rs`) | There is no deferred annotation state to cap; annotations live in `TrackState` and end with it |

## Evidence

- `crates/manta-spot/src/message.rs`: 7 unit tests, including the two
  pinned limitations (`a_three_digit_serial_can_be_mistaken_for_an_rst`,
  `a_bare_qrl_response_is_not_a_query`).
- `crates/manta-spot/tests/golden_v32_v33.rs`: SPEC §7.1 V32/V33, 11 tests
  through `Validator::ingest`, including both gherkin scenarios, last value
  wins, window aging, close reset, the D5 merge pin and the D3 placeholder
  pin.
- `a_deferred_beacon_spot_carries_its_tracks_annotations` in
  `validator.rs` covers the deferred-beacon construction site.
- `rst_and_qrl_reach_the_wire_under_their_contract_names` and
  `a_spot_without_message_content_sends_null_rst_and_false_qrl` in
  `spot_message.rs` pin the wire keys, their order and the never-omitted
  nulls; `every_wire_field_is_documented` keeps `docs/SPOT-SCHEMA.md` in
  step.
- `rst_and_qrl_never_reach_the_rbn_line` in `rbn.rs` pins D7 for both line
  formats.
- `spot_specimen_and_unchanged_enum_serialization` in `crates/manta-cli/src/fmt.rs`
  pins the CLI suffix; `output_style.rs`'s `SPOT:` regex accepts it.
- Measured after the change: `cargo test -p manta-spot` lib 254 passed
  (246 before, plus 7 parser tests and the beacon test), `golden_v11_v15`
  52 and `golden_v16_v17` 5 unchanged, `golden_v32_v33` 11;
  `cargo test -p manta-server --lib` 460 passed (457 before, plus 3).

- End to end, after the change: a 60 s recording keyed
  `QRL? CQ CQ DE K5ARH K5ARH TU 5NN K` in a loop (`manta gen v1`'s
  parameters, rendered through `manta_testkit::vectors::write_fixture_set`)
  printed `SPOT: K5ARH (CQ) 14012.3 kHz 21 dB 20 WPM conf=0.53 rst=599 QRL?`
  from `manta run --source-iq --source`. Under a `[server]` config with
  `realtime = true`, the raw-TCP JSON spot carried
  `"wpm":20,"rst":"599","qrlQuery":true,"decodeConfidence":0.53494245`,
  and the telnet line stayed
  `DX de W1AW-#:   14012.34  K5ARH          CW    28 dB  20 WPM  CQ      2213Z`.
  `manta decode --json` reported `"rst": "599", "qrl_query": true` on the
  spot object.

Not measured: recogniser precision on real recordings (no transcribed
corpus exists in this repo), and the Pi 4 CPU cost (no hardware here; the
CPU-budget bench is per hop and does not run the validator).

## Determinism

Both recognisers are pure functions of the word text over compiled regexes:
no RNG, no clock, no hash-map iteration. File input still gives
byte-identical spot logs.

## dispensa fragment

Proposed addition to dispensa `contracts/spots/spots.v1.schema.json`
(remove the comment line first):

```jsonc
// dispensa contracts/spots/spots.v1.schema.json -- proposed addition (MAN-272)
"rst": {
  "type": ["string", "null"],
  "pattern": "^[1-5][1-9][1-9]$",
  "description": "Most recent RST signal report decoded on the spotting track before the spot was sent, three digits (CW cut number N read as 9, so 5NN is 599). Null when none was decoded. Advisory band-map context, not a QSO record.",
  "examples": ["599", "579"]
},
"qrlQuery": {
  "type": "boolean",
  "description": "The spotting track decoded a QRL? frequency query before the spot was sent. Mirrors CW Skimmer's band-map QRL? label."
}
```

## Follow-ups (for the owner to file)

- **FU-1** dispensa: add `rst`/`qrlQuery` to `spots.v1` and ADR-0011, as a
  question alongside Q-0065. This session holds no dispensa credential.
- **FU-2** A live surface for annotations decoded after a spot, for example
  `rst`/`qrlQuery` on the live-state `trackState` record. Not a dedupe
  override (D9).
- **FU-3** Measure RST/QRL? precision, including the serial-vs-RST
  confusion rate, once a transcribed real-audio corpus exists.
- **FU-4** A shared `manta-spot` golden-test helper module;
  `golden_v32_v33.rs` is the fourth copy of the helper block.
- **FU-5** Forward annotations through merges only if field data shows
  operators losing `QRL?`/RST to merges. The extension point is the
  `migrate_or_discard_pending_beacons` call in `Validator::ingest`'s
  `Bookkeeping` arm, using MAN-174's loser-before-survivor ordering, with a
  per-annotation `sample_ts` to decide which value is most recent.
