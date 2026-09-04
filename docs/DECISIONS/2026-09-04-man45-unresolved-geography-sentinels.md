# MAN-45: unresolved-allowlisted-geography sentinels (spot_message.rs)

PR #63 round 6 (chatgpt-codex-connector) flagged `SpotMessage::from_spot`'s
fallback for a callsign `cty.lookup` cannot resolve: MAN-28's Watch List
allowlist lets an operator emit a spot for a deliberately unallocated or
otherwise cty-unresolvable call (the reviewer's example: `QQ9ZZZ`), and the
in-place comment claiming this path was "unreachable in practice" was
false — it's reachable by design, every time an allowlisted call doesn't
resolve. The fallback publishes CQ zone `0` and empty continent `""`
instead of an explicit "unknown" value, which the reviewer read as
fabricated geography a consumer could mistake for real data.

## The constraint that rules out the reviewer's suggested fix

The reviewer's specific suggestion — make `dxContinent`/`dxCqZone` (and
their `de*` counterparts) nullable, and emit JSON `null` for an
unresolvable call, matching how `dxDxcc`/`deDxcc` already work — conflicts
with dispensa's actual wire contract. `spot_message.rs`'s own module doc
(citing ADR-0011) states these four fields mirror
`contracts/spots/spots.v1.schema.json`, and per the in-place comment this
repo has always carried on that fallback: they are declared plain
`"type": "string"`/`"integer"` — REQUIRED and non-nullable on the wire,
unlike `dxDxcc`/`deDxcc` (declared nullable there specifically because
manta has no vendored ADIF DXCC-entity table to resolve them AT ALL, a
structurally different gap from continent/CQ-zone, which cty.dat *can*
usually resolve — just not for an allowlist-bypassed unallocated call).
Emitting `null` for `dxContinent`/`dxCqZone` would **fail cqdx's own
ingest validation**, not satisfy it. This repo has no vendored copy of the
schema to re-verify that claim independently; it is trusted as stated in
`spot_message.rs`'s comment and this PR's review thread.

## Options considered

1. **Make the four fields nullable in dispensa's contract, emit `null`.**
   The reviewer's suggestion. Requires a cross-repo dispensa change; until
   that lands, doing this manta-side would produce output that fails
   validation against the CURRENT contract — trading a documentation gap
   for a live ingest outage.
2. **Withhold JSON-stream spots for allowlisted-but-cty-unresolvable
   calls entirely.** Named in the ticket as the other manta-side option.
   Rejected: this would silently revoke exactly what MAN-28's Watch List
   allowlist promises operators — the feature exists *specifically* to let
   an operator force-emit a call that automatic validation (cty allocation,
   the repetition gate) would otherwise drop. Suppressing the JSON stream
   output for those same calls defeats the feature's own purpose from a
   different angle.
3. **Add a new `dxGeoResolved`-style boolean field to the wire object.**
   Considered and rejected (see below).
4. **Keep the wire values as-is, but name/document/count/test them as
   deliberate sentinels, and point consumers at the field that already
   carries a contract-legal "unknown" signal.** Chosen.

## Decision: named sentinels + point at the already-nullable lat/lon

The wire values for `dxContinent`/`dxCqZone` do not change — they cannot,
without either breaking cqdx ingest validation (option 1) or reneging on
MAN-28's own feature contract (option 2), and neither is a call this PR
gets to make unilaterally.

What changes:

- `spot_message.rs` gains two named constants,
  `spot_message::UNKNOWN_CONTINENT` (`""`) and
  `spot_message::UNKNOWN_CQ_ZONE` (`0`), used at both fallback sites
  instead of `unwrap_or_default()`/`unwrap_or(0)`. Same bytes on the wire,
  but now a named, documented, testable sentinel instead of an
  unlabeled default.
- Both are chosen **outside** their field's real domain — CQ zones are
  1–40, continents are the seven two-letter codes — so they read as
  "unknown," not as plausible-but-wrong data. This was already true before
  this change; it was simply unnamed, unexplained in one comment only, and
  untested.
- **`dxLat`/`dxLon` (and their `de*` counterparts) are already
  `Option<f64>`** and already serialize to JSON `null` for an unresolved
  call (`spot_message.rs:26-27` in the pre-MAN-45 code). This is a
  contract-legal "unknown" signal that exists on the wire **today**,
  requiring no schema change, and was not mentioned anywhere in the
  original ticket or the standing code comment — a consumer that wants to
  detect "manta could not resolve this call's geography" can already key
  on null coordinates rather than needing a new field.
- A new server-local Prometheus counter,
  `manta_spots_unresolved_geography_total`, is incremented once per spot
  (at publish time in `manta-cli`'s `on_spot` callback, via
  `cty.lookup(&spot.callsign).is_none()` — not inside `from_spot`, which
  runs once per connected client and would scale the count with client
  count instead of spot count) so an operator can see how often this
  happens without needing to correlate it from spot content.
- `spot_message.rs`'s standing comment is corrected: the prior text
  claiming this path "should be unreachable in practice" was already fixed
  as part of PR #63 round 6 (before this ticket); this decision record and
  the sentinel naming complete the fix the round-6 finding actually asked
  for.

## Option 3 in more detail: why no new field

A `dxGeoResolved: bool` (or similar) field was the obvious alternative to
leaning on the already-nullable lat/lon. Rejected because this repo has no
vendored copy of `spots.v1.schema.json` and therefore cannot confirm
whether the schema sets `"additionalProperties": false` — if it does,
adding an undeclared field would fail cqdx ingest validation exactly the
way option 1's `null` would, just via a different mechanism. Adding a
field that might break ingest trades a documentation gap for a possible
live outage, the same trade option 1 makes. The lat/lon-null signal
already exists and is already contract-legal; reusing it costs nothing and
risks nothing.

## Cross-repo follow-up (not blocking, proposed for dispensa)

The underlying gap — no contract-defined "unknown" sentinel or nullable
variant for `dxContinent`/`dxCqZone`/`deContinent` specifically — is real
and worth fixing at the contract layer eventually, so a consumer doesn't
have to know that "check for null lat/lon" is the actual signal for
"geography unknown" rather than a more discoverable field. This is a
clarity improvement, not an outage fix: manta's current output is already
contract-valid as-is, and nothing in this repo blocks on the dispensa
change landing.

Proposed text for a dispensa `questions/` entry (ready to lift verbatim):

> **Question:** `spots.v1.schema.json` declares `dxContinent`/`dxCqZone`/
> `deContinent` as required, non-nullable `string`/`integer`. A producer
> (manta) can legitimately have a validated spot for a callsign whose
> geography it cannot resolve (an operator-allowlisted call absent from
> the producer's DXCC-prefix table) and currently has no contract-defined
> way to signal "unknown" for these three fields other than an
> out-of-domain sentinel value (empty string / CQ zone 0) chosen
> unilaterally by the producer. Would dispensa accept either (a) a
> documented sentinel convention for these fields analogous to what
> `dxDxcc`/`deDxcc` already do via `null`, or (b) making these three
> fields nullable like `dxDxcc`/`deDxcc`? Either resolves the ambiguity
> for every producer independently choosing its own out-of-domain value
> today. Producer reference: manta,
> `crates/manta-server/src/spot_message.rs`,
> `docs/DECISIONS/2026-09-04-man45-unresolved-geography-sentinels.md`.
