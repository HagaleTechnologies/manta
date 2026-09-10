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

## Decision 2: QRL recognition requires the literal `?`

**Revised on the PR #159 review round.** The original decision matched bare
`QRL` too, on the grounds that `manta_decode::beam` emits `Glyph::Char('?')`
at confidence 0.0 as the glyphless-survivor placeholder (SPEC §4.4.4), so `?`
is overloaded between "a real question mark was sent" and "something failed to
decode" — keying on it makes detection depend on one low-confidence character.

That reasoning is still true, but it was outweighed: `QRL` and `QRL?` are
opposite halves of the same exchange, not two spellings of one cue. Bare `QRL`
**asserts** "the frequency is in use" (the response); `QRL?` **asks** (the
query). The field is named `qrl_query` and its wire name is `qrlQuery`, so
matching the response permanently reports to JSON consumers that the station
sent `QRL?` when it did not — and the ticket's own acceptance criterion is
written on the interrogative form. CW Skimmer's band-map label is `QRL?` for
the same reason. The regex is therefore `(?i)\bQRL\?`.

Accepted cost, symmetric and one character wide in each direction: a genuine
`QRL?` whose `?` did not survive the beam decodes as bare `QRL` and is not
flagged, and a bare `QRL` followed by an unresolvable character can read as a
query. Neither invents an interrogative the decoded text does not show, which
the previous rule did by construction.

Rejected: a third state distinguishing bare/uncertain `QRL` from `QRL?` (the
reviewer's other offered option) — it would widen the same unratified wire
contract Decision 5 is already trying to keep narrow, for a cue CW Skimmer
does not surface at all; and treating `QRZ?` as equivalent (different meaning,
out of scope).

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

**Revised on the PR #159 review round (round 1):** a `ClosureKind::Bookkeeping`
merge is not the end of the identity, so the loser's annotations migrate to the
survivor instead of dying with its `TrackState`
(`Validator::migrate_message_annotations`) — same reasoning that already
carried pending Beacon candidates across a merge.

**Revised again on round 2 (Codex, `crates/manta-spot/src/validator.rs:526`):**
the survivor a closure *names* is not necessarily the identity still being
tracked. `TrackManager::merge_converged`'s "already a loser" guard excludes
only losers, not survivors, so one batch can close a chain (`2 -> 1` together
with `1 -> 3`); `process_hops` then sorts `TrackClosed` by `track_id`, so
`1 -> 3` is delivered *before* `2 -> 1` and track 2's migration target is a
track the validator has already closed and removed. The same shape arises
whenever a survivor's own closure (a hang-expiry, an eviction) sorts ahead of
its loser's. Migrating as named both stranded the annotations on a dead
track_id and resurrected per-track_id state that no further `TrackClosed`
would ever free — the MAN-19 leak, reopened.

Resolution: the validator resolves the named survivor to the identity still
carrying it before migrating, via a bounded `track_id -> survivor` map of
closures it has already processed (`Validator::{resolve_migration_target,
note_closed}`, `MAX_CLOSED_SURVIVORS = 512`, above the shipped `track_cap`
default of 500 so a still-needed redirect is never the one dropped). The
stored value is itself already resolved, so chains stay flat. Chosen over the
reviewer's alternative of reordering closures leaves-to-roots in
`manta-engine`, because resolving in the consumer also covers the
survivor-closed-by-an-unrelated-reason ordering — which no merge-internal
ordering rule can reach — and it applies to the pending-Beacon migration on
the same line at no extra cost. A dropped redirect degrades to "no survivor"
(annotations discarded, pending beacons counted lost), never to resurrecting
removed state; `dropped_survivor_watermark` is what makes that one-directional.
Covered by `chained_merge_forwards_annotations_to_the_final_survivor`,
`a_survivor_closed_for_good_takes_no_migrated_annotations` and
`a_dropped_redirect_degrades_to_no_survivor`, all three of which fail against
the pre-fix migrate-as-named behaviour.

**Revised again on round 3 (Codex, `crates/manta-spot/src/validator.rs:1183`):**
that bounded map evicted the *lowest track_id* on overflow, justified as
"oldest-first" because `TrackManager` hands out strictly increasing ids. It is
not: ids are assigned at track *creation*, the map is keyed at track *closure*,
and a long-lived low-id track closes late — so its brand-new redirect is
simultaneously the newest entry and the smallest key, and the very next closure
evicted it. Once the map is full (512 historical closures), a merge loser
naming that track therefore lost the chain and its RST/QRL annotations were
discarded, with pending beacons miscounted as lost to eviction — the exact
failure the round-2 fix exists to prevent, and it contradicted the invariant
`MAX_CLOSED_SURVIVORS` claims ("a redirect is only ever consulted by a closure
in the same batch as the one that recorded it, so a still-needed redirect is
never the one dropped").

Resolution: `Validator::closed_survivor_order`, a `VecDeque<u32>` of the map's
keys in first-insertion order; `note_closed` evicts from its front, so the
oldest *closure* goes rather than the smallest id, and a batch's own fresh
redirects are the last things dropped. A repeated `TrackClosed` for the same
track_id refreshes the stored survivor without re-queueing the id, keeping the
deque and the map exactly in step. `dropped_survivor_watermark` stays
`max(dropped id) + 1`: dropped ids are no longer a prefix of the id space, so
the watermark can now sit above ids still present in the map, which only ever
makes an *unknown* target degrade to "no survivor" — still one-directional,
still never resurrecting removed state. Covered by
`a_fresh_low_id_redirect_outlives_a_full_map`, which fails against the
lowest-key eviction policy.

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

**Bounded further on the PR #159 review round:** both fields are
`#[serde(skip_serializing_if = ...)]`, so a spot carrying no RST and no QRL?
serializes byte-identically to the pre-MAN-33 wire. A strict consumer
therefore cannot reject the *stream*; at worst it rejects the individual
spots that actually carry the new information, which is the smallest exposure
available without dropping the feature's only operator-facing surface (the
CLI debug line is a developer aid, not a band map). The reviewer's preferred
remedy — ratify in dispensa first, or omit the keys entirely until then —
cannot be executed from this repo: dispensa is a separate repository this
container holds no checkout or credential for, and omitting the keys entirely
would leave the ticket's JSON deliverable unimplemented. FU-1 stays open and
this thread is parked for a human, not resolved.

Proposed dispensa schema fragment, ready to lift verbatim:

```jsonc
// dispensa contracts/spots/spots.v1.schema.json -- proposed addition (MAN-33)
"rst": {
  "type": ["string", "null"],
  "pattern": "^[1-5][1-9][1-9]$",
  "description": "Most recent RST signal report decoded on this track, normalized to three digits (CW cut number 'N' resolved to 9, so '5NN' is reported as '599'). Advisory band-map context, not a QSO record: a track can carry both sides of a QSO, so the report is not necessarily one the spotted station received. Absent (key omitted) when no report was decoded.",
  "examples": ["599", "579"]
},
"qrlQuery": {
  "type": "boolean",
  "description": "This track sent the interrogative 'QRL?' ('is this frequency in use?') during its lifetime. A bare 'QRL' response is deliberately NOT counted. Sticky once observed. Mirrors CW Skimmer's band-map QRL? label. Absent (key omitted) when false.",
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
  `qrlQuery` on the JSON Lines/WebSocket wire, both omitted from the wire
  when empty (Decision 5).
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
- A `QRL?` whose `?` was lost to the decoder reads as a bare `QRL` response
  and is not flagged; a bare `QRL` immediately followed by an unresolvable
  character can read as a query (Decision 2).
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
