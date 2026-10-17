# MAN-136: `dxDxcc`/`deDxcc` and the `UNKNOWN_*` geography sentinels

## Context

dispensa's `contracts/spots/spots.v1.schema.json` declares `dxDxcc`, `deDxcc`,
`dxContinent`, `deContinent`, and `dxCqZone` **required and non-nullable**.
manta's JSON Lines stream (`:7301`) emitted JSON `null` for `dxDxcc`/`deDxcc`
on **every** spot, regardless of whether the callsign resolved against
`cty.dat` — cqdx's own ingest would reject the batch on this field alone.
Separately, when a callsign genuinely couldn't be resolved (reachable in
production through MAN-28's Watch List allowlist, which lets an operator
allowlist a call that bypasses `cty.is_allocated()` entirely,
`crates/manta-spot/src/validator.rs:669-680`), the fallback values for
`dxContinent`/`dxCqZone` were a real-looking `""`/`0` — indistinguishable, on
the wire, from "we looked this up and got zone 0/empty continent" rather than
"we couldn't look this up at all". This is the geography-null finding already
on file in MAN-45.

This repo does not vendor dispensa's schema, so the required/non-nullable
reading here comes from three independent broad-review lenses that read the
actual schema directly (2026-09-05 broad review, lens 2 item #13, lens 4 item
#15, lens 5 item #14), not from `spot_message.rs`'s own prior code comment
(which claimed `dxDxcc`/`deDxcc` were nullable on the contract — that comment
was secondhand and wrong, and has been removed as part of this change). This
record supersedes that comment's claim.

## Decision

Vendor a small ADIF DXCC entity-number table (`crates/manta-spot/data/dxcc.tsv`,
346 rows) alongside `cty.dat`, derived from AD1C's own `cty.csv` (same
publisher, same license posture, same refresh cadence as `cty.dat`), joined
in `manta_spot::cty::Table` on the primary-prefix field already present in
`cty.dat`'s header. Per broad-review decision D10
(`docs/DECISIONS/2026-09-06-broad-review-decisions.md:144-148`), this ships
without waiting on a dispensa contract relaxation.

## Why the primary prefix, not the entity name

Measured directly (2026-09-07): joining this repo's vendored `cty.dat`
(retrieved 2026-07-25) against AD1C's `cty.csv` (fetched 2026-09-06) on the
primary-prefix field matched **346/346** entities with zero mismatches.
Joining on the entity **name** instead matched only 343/346 — three names had
drifted in a six-week gap: `Cape Verde` → `Cabo Verde`, `Juan de Nova, Europa`
→ `Juan de Nova & Europa`, and `Tristan da Cunha & Gough` → `... Gough
Islands`. ADIF's own 3.1.7 release notes independently corroborate that
entity names are not stable over time (they record renaming entities
207/462/468/502/518 "to realign with the ARRL DXCC list"). The primary
prefix is the stable key.

## The three sentinel values, and why each

- **`UNKNOWN_DXCC: i64 = -1`** (`crates/manta-server/src/spot_message.rs`).
  Not `0`: ADIF defines DXCC entity code 0 as **"None — the contacted station
  is known to NOT be within a DXCC entity"** (e.g. maritime mobile in
  international waters) — a specific, different, and false claim to make
  about a callsign manta merely failed to resolve. Reusing 0 here would be
  the same class of bug this ticket exists to remove. Not `null`: the field
  is required non-nullable per the Context section above. `-1` is outside
  ADIF's valid 1–522 range, so no consumer can mistake it for real data, and
  a JSON Schema `"type": "integer"` with no `minimum` still accepts it.
- **`UNKNOWN_CONTINENT: &str = ""`** and **`UNKNOWN_CQ_ZONE: u16 = 0`** — the
  same values these fields already fell back to before this change, but now
  **named, exported, documented, tested, and counted**. On inspection these
  values were not "fabricated": CQ zones are 1–40 and continents are one of
  seven two-letter codes, so both were already out of the field's real
  domain. What was actually wrong was that the fallback was *implicit* — a
  bare `unwrap_or_default()`/`unwrap_or(0)` with no name, no test, and no way
  for a consumer to know it meant "unresolved" rather than "resolved to
  this." Naming them fixes exactly that. Inventing new values (e.g. `"XX"` or
  `999`) was considered and rejected: it would change wire bytes an existing
  consumer has already seen, for no gain in distinguishability over what's
  documented here. This choice reuses MAN-45's already-designed names and
  values verbatim (see "Reconciliation with MAN-45" below), so the two don't
  diverge.

## Maritime and aeronautical mobile: ADIF 0, not the home entity

Added in round-7 review (finding 2). `cty::Table::lookup` resolves
`K5ARH/MM` and `K5ARH/AM` through the base call's prefix, so the entity
mapping above would have reported the United States (291) for a station
whose designator says it is at sea or airborne. ADIF entity code 0 —
"None: the contacted station is known to NOT be within a DXCC entity" — is
defined for exactly this case, so `spot_message::NO_DXCC_ENTITY` (0) is
emitted instead, and the rest of that side's geography (`dxContinent`,
`dxCqZone`, `dxLat`/`dxLon`) falls back to the `UNKNOWN_*` sentinels/`null`
rather than the home entity's real values: the designator tells us where the
station is *not*, never where it *is*. This is the one place code 0 is
correct and `UNKNOWN_DXCC` would be wrong — the distinction the `-1` choice
above exists to preserve. Only `/MM` and `/AM` qualify; `/P`, `/QRP`, `/M`
and `/<digit>` mean "somewhere else *within* an entity", which the base
prefix still describes correctly at entity granularity. The check
(`spot_message::is_outside_any_dxcc_entity`) splits the callsign from the
right, because MAN-28's Watch List allowlist bypasses `grammar::is_plausible`
and its single-`/` restriction entirely, so `KP4/K5ARH/MM` can reach the
wire path. Because those spots go out carrying `UNKNOWN_CONTINENT`/
`UNKNOWN_CQ_ZONE`, `manta_spots_unresolved_geography_total` counts them too
(`main.rs`'s `geography_is_unresolved`), even though their entity number is
the resolved value 0 rather than `-1`.

## What consumers should key on

Two independent unknown-geography signals are emitted together whenever a
callsign doesn't resolve:

- **`dxLat`/`dxLon` (and `de*`) are `null`.** These fields stay
  `Option<f64>` and nullable on the contract, so this is the one
  **contract-legal** "unknown geography" signal available today, with no
  schema change needed.
- **`dxDxcc == UNKNOWN_DXCC` (`-1`).** This is manta-defined, not yet
  contract-ratified by dispensa.

Both are emitted on the same spot whenever `cty.dat` itself fails to resolve
the callsign, which is the ordinary case; a consumer that doesn't yet know
about `UNKNOWN_DXCC` can still detect "unknown" via the null lat/lon. The one
state where they diverge is `cty.dat`/`dxcc.tsv` drift — `cty.dat` refreshed
by hand (data/SOURCES.md) without regenerating the table — where geography
resolves (real `dxLat`/`dxLon`) but the entity number does not, so `dxDxcc`
is `-1` with non-null lat/lon. That state is a data-vendoring bug, not a
routing signal: `cty::tests::every_entity_in_the_vendored_cty_dat_resolves_an_adif_dxcc_number`
fails the build on it, and `manta_spots_unresolved_geography_total` counts it
at runtime (see below).

## Operator visibility

Every time a spot goes out carrying an `UNKNOWN_*` sentinel on either side,
manta increments `manta_spots_unresolved_geography_total` (Prometheus
counter, `crates/manta-server/src/metrics.rs`), incremented once per spot at
publish time (`crates/manta-cli/src/main.rs`), not once per connected JSON/WS
client. The counter's condition is deliberately keyed on the RESOLVED ADIF
entity number (`main.rs`'s `geography_is_unresolved`), not on whether
`cty::Table::lookup` returned an entry, so the drift state above — sentinel
emitted, geography still real — is counted rather than silently uncounted.
The de-side term is resolved once at daemon start: `station_callsign` is
config and cannot change for the life of the process.

## The open cross-repo item

dispensa `Q-0028` has been open since 2026-07-14, awaiting skimmer
confirmation before cqdx's implementation proceeds. This decision
deliberately does not wait on it — see D10. If dispensa later ratifies a
different "unknown DXCC" representation than `-1`, this record is what gets
superseded; track that as a follow-up ticket against this record rather than
a blocker on landing MAN-136.

## Reconciliation with MAN-45

MAN-45 (referenced above) documents this same continent/CQ-zone fallback bug
and a previously-designed fix (`UNKNOWN_CQ_ZONE`/`UNKNOWN_CONTINENT`
sentinels, a `manta_spots_unresolved_geography_total` counter). This ticket
reuses those names, values, and the counter's shape verbatim, and adds the
`UNKNOWN_DXCC` sentinel and the `dxcc.tsv` vendored table alongside it — the
half of the problem MAN-45 did not cover.

## Supersedes

The `spot_message.rs` module doc and inline comment that claimed
`dxDxcc`/`deDxcc` were nullable on dispensa's `spots.v1` contract. That claim
is wrong per the three independent broad-review lenses cited above, and has
been removed from the code.

## References

- Ticket: MAN-136
- Decision implemented: `docs/DECISIONS/2026-09-06-broad-review-decisions.md`
  D10
- Origin finding: MAN-45; 2026-09-05 broad review lens 2 #13, lens 4 #15,
  lens 5 #14
- Code: `crates/manta-server/src/spot_message.rs`,
  `crates/manta-spot/src/cty.rs`, `crates/manta-spot/data/dxcc.tsv`,
  `crates/manta-server/src/metrics.rs`, `crates/manta-cli/src/main.rs`
- Upstream data: `https://www.country-files.com/bigcty/cty.csv` (ADIF entity
  code = column 3); `https://www.adif.org/` DXCC Entity Code enumeration
  (code 0 = "None")
