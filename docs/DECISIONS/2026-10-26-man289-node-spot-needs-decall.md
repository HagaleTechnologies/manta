# MAN-289: a node spot without a `deCall` is malformed

Date: 2026-10-26. Ticket: MAN-289, deferred from PR #219 (MAN-96) review
round 8, thread `PRRT_kwDOTQvU8M6qkqnn`, under
`docs/DECISIONS/2026-08-07-pr-review-convergence-policy.md`.

## Context

`load_node_spots` in `scripts/shadow-compare.py` read
`str(d.get("deCall") or "").upper()`, so a spot with no `deCall` loaded with
an empty identity, and `compare()` drops empty identities from the
self-exclusion set (`if s["de_call"]`). At `40d275a`, one node spot with no
`deCall` and one RBN row from the node's own `N0CALL-1` spotting the same
call gave 1/1 corroborated and 0.0% uncorroborated: the node's forwarded
echo passed the D-I gate for it. `field-node.py record-spots` archived such a
line, because `validate_spot_line` checked only `dxCall`, `frequency` and
`timestamp`.

## Decision

Both tools require a `deCall` that is a string with a non-whitespace
character. `record-spots` skips any other line as invalid (`missing deCall`
in its log, counted under `invalid`), after the existing checks, so a line
that already failed one keeps its old reason. `shadow-compare.py` treats it
as a malformed node spot record: exit 2 naming the file and line, or, as a
torn final line, one skipped malformed line. There is no escape hatch.
`--exclude-spotter` does not waive the rule; it still adds calls to the
`deCall`s found in the archive, for a node that reaches RBN under another
call.

## Consequences

- Real manta archives are unaffected: `deCall` is `[server].station_callsign`,
  which `docs/SPOT-SCHEMA.md` lists as never omitted and which config
  requires (`missing_station_callsign_is_a_parse_error`). A hand-edited or
  foreign archive missing `deCall` now fails the run.
- `manta grade` keeps accepting a spot with no `deCall` on purpose
  (`grade_summary_lists_own_calls_apart_when_a_spot_has_no_de_call` in
  `crates/manta-cli/tests/grade_cmd.rs`); it feeds no D-I number.
- `compare()`'s `if s["de_call"]` guard stays for direct callers.
- The tests are in `scripts/tests/test_shadow_compare.py` and
  `scripts/tests/test_field_node.py` (`RecordSpotsTests`). The MAN-96 record
  is not rewritten.
