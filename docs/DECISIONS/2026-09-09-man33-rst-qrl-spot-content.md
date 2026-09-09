# MAN-33: RST extraction and QRL? flag on manta spots

`manta-spot/src/context.rs` classifies CQ/DE/Beacon but had no RST or QRL?
extraction anywhere in the crate — the capability matrix's original "covered"
claim was over-broad, caught by Codex review on PR #57
(`docs/DECISIONS/2026-09-01-legacy-capability-matrix.md`, row 50/111). CW
Skimmer's manual (Band map section) names both the "599" label ("the most
recent RST is shown") and "QRL?" ("allows the operator to notice new stations
on a crowded band") as explicit pileup aids manta's operators lacked.

## Measurements this decision is based on

Before writing any implementation code, the gap and the parser design were
reproduced and validated directly against `5b9e747` in this container:

- A throwaway integration test fed `["QRL?", "QRL?", "CQ", "TEST", "K5ARH",
  "K5ARH", "TU", "5NN", "CQ", "K5ARH"]` through `Validator`. The emitted spot
  carried neither the `5NN` in the window nor the fact that the station opened
  with `QRL?` — both gherkin scenarios failed exactly as the ticket states.
- A 27-case positive/negative table (RST forms, cut numbers, QRL variants,
  callsign/prosign false-positive checks) validated the exact regexes shipped
  in `manta_spot::message` before they were wired into the pipeline. All 27
  passed.
- `5NN` was measured to pass both the callsign grammar gate and the cty.dat
  gate (`5N` = Nigeria) — a structurally spottable "callsign" today. It is
  not a live bug (no CQ/DE/UP/beacon framing reaches it without further
  context), but it is the reason a stop-word fix is explicitly deferred
  (FU-4) rather than folded into this ticket.
- The full implementation was spiked end-to-end, then reverted, before being
  written for real: `cargo fmt --all --check`, `cargo clippy --workspace
  --all-targets -- -D warnings`, and `cargo test --workspace` were all green
  with the change applied, with every pre-existing test count unchanged.

## Decision 1: RST cut-number handling is `N` → 9 only

A valid RST has R ∈ 1–5, S ∈ 1–9, T ∈ 1–9 — the digit `0` never appears. CW's
two universal cut numbers are `T` = 0 and `N` = 9; only `N` can ever occur
inside an RST, since `T` = 0 is unreachable there. Restricting the
substitution to `N` collapses the cut-number problem to one mapping and
eliminates the false-positive surface a full `A`/`U`/`V`/`E` cut-letter
alphabet would open against callsign fragments (verified: `V3E` produces no
match, where an `A`/`U`/`V`/`E` mapping would have produced a spurious
"335"). Regex: `(?i)\b([1-5])([1-9N])([1-9N])\b`, normalized by uppercasing
and mapping `N` → `9`.

Rejected: a bare `\d{3}` (misses the ticket's own `5NN` example); a full
cut-number alphabet (widens false positives for zero additional RST
coverage).

## Decision 2: QRL recognition matches bare `QRL`, with or without `?`

`manta_decode::beam` emits `Glyph::Char('?')` at confidence 0.0 as the
glyphless-survivor placeholder (SPEC §4.4.4) whenever beam survivors carry no
glyph — `?` is therefore overloaded between "a real question mark was sent"
and "something failed to decode and got a placeholder". Requiring the literal
`?` would make detection depend on whether one low-confidence character
happened to survive the beam. Bare `QRL` ("the frequency is in use") and
`QRL?` ("is it in use?") are the same operator cue CW Skimmer's band map
exists to surface, so both match: `(?i)\bQRL\b`.

Rejected: requiring the literal `?` (brittle against the overloaded glyph);
treating `QRZ?` as equivalent (different meaning, out of scope).

## Decision 3: RST is last-value-wins per track; QRL? is sticky per track

RST updates on every newly-completed word containing an RST-shaped token,
matching CW Skimmer's documented "the most recent RST is shown" — there is no
"better/worse" ordering between two RST reports the way there is between
`SpotType` classifications. QRL? is a discrete event with no natural un-set
signal; the track itself is the natural lifetime bound, so `TrackClosed`
clears both annotations (`Validator::ingest`'s existing `self.tracks.remove`
already does this — no new teardown code was needed).

The existing MAN-28 promotion-only reclassification machinery
(`Word::{seq, attempted, last_spot_type, classified_max_seq}`,
`crates/manta-spot/src/validator.rs`) was purpose-built over several review
rounds specifically so `SpotType` never downgrades. It is deliberately **not**
reused here: RST has no ordering for a never-downgrade guard to protect, and
reusing it would actively break last-value-wins. Both annotations live on
`TrackState`, scanned at `WordBoundary` push time against the single word that
just completed (not the joined 16-word window) — this is also what makes an
RST survive aging out of the window, matching "the most recently decoded RST",
not "whatever's currently in view".

Rejected: reusing the promotion-only guard; clearing QRL? after N words (an
arbitrary constant with nothing real to calibrate it against).

## Decision 4: JSON Lines stream only — never the RBN telnet line

`rbn::format_line` is the RBN/aggregator compatibility surface, shared with
the outbound RBN uplink (MAN-32) — anything added there is pushed into the
RBN network itself. Three reasons this stays off that line:

1. The `DX de` line is parsed positionally by clients with no slot for these
   fields.
2. MAN-88 is concurrently pinning this exact function to a live RBN capture
   byte-for-byte; adding fields would fight that work directly.
3. CW Skimmer itself only shows these in its band map GUI, never in what it
   uploads to RBN — matching this split, not contradicting it.

The operator-facing analogue of CW Skimmer's band map in this ecosystem is
cqdx, fed by the JSON Lines stream (`crates/manta-server/src/spot_message.rs`)
— that is where `rst`/`qrlQuery` go. A guard test
(`rst_and_qrl_never_reach_the_rbn_line`, `crates/manta-server/src/rbn.rs`)
pins this as a deliberate omission, not a gap a future reviewer should "fix".

## Decision 5: ship the JSON fields now; write the dispensa proposal rather than block on it

This repo vendors no copy of dispensa's `contracts/spots/spots.v1.schema.json`,
so whether it sets `"additionalProperties": false` could not be confirmed
from this checkout. `wiki/pages/spot-output-contract.md` already records the
schema as design-phase, not yet frozen — consistent with adding fields ahead
of the cross-repo contract. Residual risk (a strict-mode validator on the
cqdx side rejecting the two unknown keys) is bounded and tracked as FU-1
below.

Proposed dispensa schema fragment, ready to lift verbatim:

```jsonc
// dispensa contracts/spots/spots.v1.schema.json -- proposed addition (MAN-33)
"rst": {
  "type": ["string", "null"],
  "pattern": "^[1-5][1-9][1-9]$",
  "description": "Most recent RST signal report decoded from the spotted station's transmission, normalized to three digits (CW cut number 'N' resolved to 9, so '5NN' is reported as '599'). Null when no report was decoded. Advisory band-map context, not a QSO record.",
  "examples": ["599", "579"]
},
"qrlQuery": {
  "type": "boolean",
  "description": "The spotted station sent a QRL frequency query ('is this frequency in use?') during this track's lifetime. Sticky once observed. Mirrors CW Skimmer's band-map QRL? label.",
  "default": false
}
```

The dispensa PR itself must additionally confirm whether the schema sets
`"additionalProperties": false` — an open cross-repo question no manta
checkout can answer (FU-1).

## Decision 6: no dedupe override for an RST/QRL change

`Dedupe::should_emit` (`crates/manta-spot/src/dedupe.rs`) re-emits only on
window-elapse, an SNR jump ≥ 6 dB, or a `spot_type` change. Adding
"annotation changed" as a fourth trigger would raise spot volume on a surface
MAN-32 forwards straight into the RBN network, where duplicate spots are
exactly what node operators are judged on — and it is not required by the
acceptance criteria, which only require that *when* a spot is emitted it
carries the most recent RST, not that a new spot is forced.

**Accepted consequence:** an RST decoded strictly after a spot already fired
only reaches the operator on that track's next natural re-spot. In the
dominant real sequences this is not a practical gap — a station hunting a
clear frequency sends `QRL?` *before* calling CQ (so the flag is already set
when the first spot fires), and a running station repeats `5NN` between
callsigns inside the same 16-word window. Tracked as FU-2 below, whose real
fix is a live band-map/state surface, not more RBN spot volume.

## Decision 7: no callsign stop-word change, no config key

`5NN` passing grammar+cty (measured above) is a busted-spot concern that
belongs to the truncated/merged-variant-spotting tickets, not this
additive-metadata one, and mixing a validation-gate change into this ticket
would perturb existing golden-vector spot counts for no acceptance-criteria
benefit (FU-4). No config key was added: extraction is unconditional, gates
nothing, and costs two cheap regex scans per completed word — a knob would
invent operator-facing variability the feature doesn't have.

## What changed

- New module `crates/manta-spot/src/message.rs`: `parse_rst`/`is_qrl_query`,
  pure functions over a `&str`, no state, no RNG, no wall clock.
- `Spot` (`crates/manta-spot/src/validator.rs`) gains `rst: Option<String>`
  and `qrl_query: bool`. `TrackState` gains matching fields, updated at each
  completed `WordBoundary` and copied onto the emitted `Spot` in
  `evaluate_candidate`.
- Golden vectors V31 (rst-extraction) and V32 (qrl-query-flag),
  `crates/manta-spot/tests/golden_v31_v32.rs`; table entries in
  `docs/SPEC-decode-core.md` §7.1.
- `SpotMessage` (`crates/manta-server/src/spot_message.rs`) gains `rst`/
  `qrlQuery` on the JSON Lines/WebSocket wire.
- `manta-cli`'s debug line gains optional ` rst=599`/` QRL?` suffixes, empty
  (byte-identical output) for an ordinary spot; `--json` mode picks up both
  fields automatically since it serializes `Spot` directly.
- `rbn::format_line` is untouched; a guard test pins that RST/QRL never reach
  the telnet line.
- `ARCHITECTURE.md` §6 (new step 1a) and §7 updated to describe the
  annotations and to state explicitly that they gate nothing — the exact
  over-broad-prose failure mode Codex caught on PR #57 must not recur here.
- `docs/DECISIONS/2026-09-01-legacy-capability-matrix.md` row 50/111 flipped
  from Gap to Covered (JSON stream only).

## Accepted limitations

- A three-digit serial number (e.g. the "199" in "5NN 199") is RST-shaped and
  can win under last-value-wins. Pinned by a dedicated test
  (`a_three_digit_serial_can_be_mistaken_for_an_rst`) so a future change to
  this is a deliberate act, not an accidental regression.
- Bare `QRL` (no `?`) counts as the query (Decision 2).
- No on-air validation of either recognizer was possible — no transcribed,
  ground-truth CW corpus exists in this repo (the WPX/VP8GEO captures noted
  elsewhere in the project's review history are untranscribed and not
  committed). The false-positive/negative rates above are reasoned from the
  regexes and the validation table, not measured on air (FU-3).

## Follow-up register

No Linear credential exists in this environment, so these are recorded here
for filing at merge time:

- **FU-1** — dispensa: add `rst`/`qrlQuery` to `spots.v1.schema.json` /
  ADR-0011 (fragment above), and confirm `additionalProperties`. Blocks
  nothing in manta.
- **FU-2** — an RST decoded after a spot was already emitted only reaches the
  operator on the next natural re-spot (Decision 6). The right long-term fix
  is a live band-map/spot-update surface on the JSON stream, not an extra
  dedupe trigger that would push duplicates into RBN.
- **FU-3** — no on-air validation of either recognizer was possible. Fold
  RST/QRL precision into the RBN-parity benchmark once a transcribed corpus
  exists; the serial-vs-RST confusion rate is the specific number to measure.
- **FU-4** — `5NN` passes grammar and cty.dat (`5N` = Nigeria; measured
  above). Not reachable without CQ/DE/UP framing today, but
  `manta_spot::message::parse_rst` is now the principled stop-word oracle for
  whoever picks this up. Belongs to the busted-spot / truncated-variant
  tickets, not here.
- **FU-5** — `golden_v11_v15.rs`, `golden_v16_v17.rs`, and `golden_v31_v32.rs`
  each now carry their own copy of the `word_events`/`transmission_events`/
  `run`/`seed_meta` test helpers. A shared `manta-spot` test-support module
  would be a small, purely-mechanical cleanup.

## Migration notes

- **Wire compatibility**: two additive JSON keys. Consumers that ignore
  unknown fields are unaffected; a strict `"additionalProperties": false`
  schema would reject them — the risk Decision 5 accepts and FU-1 closes.
- **Telnet compatibility**: none — the `DX de` line and the outbound RBN
  uplink are byte-identical, guarded by a test.
- **Source compatibility**: `manta_spot::Spot` is a public struct gaining two
  public fields; any out-of-tree construction breaks at compile time with a
  clear `E0063`. There are no out-of-tree consumers today.
