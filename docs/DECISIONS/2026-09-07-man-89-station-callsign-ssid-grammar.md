# MAN-89: `station_callsign` accepts RBN's `CALL-N-#` per-band SSID

Parent decision: `docs/DECISIONS/2026-09-06-broad-review-decisions.md` D4 — "MAN-89 is the
blocking callsign-grammar fix; MAN-13/14 carry the design decision itself." This record covers
only the validator-level decisions D4 left to MAN-89's implementation.

## Problem

`[server].station_callsign` was validated with `manta_spot::grammar::is_plausible`, a prefilter
built for garbled decoder output (ARCHITECTURE §6.2), not operator-supplied identity. It rejects
any `-`, so a multi-band node could not configure itself as `W5AU-1` — the exact convention a
live RBN capture showed 10 of 71 observed spotters already use (`61x -#, 7x -1-#, 2x -6-#, 1x
-2-#`). The same function also rejected real operator callsigns with DXCC prefix overrides or
portable suffixes (`JW/LB2PG`, `GB3LER/B`), a narrower but related problem this ticket's
technical notes asked to fix together.

## Decisions

1. **Replace `check_plausible` with a purpose-built `check_operator_callsign`** in
   `manta-server::config`, rather than continuing to reuse `manta_spot::grammar::is_plausible`.
   The new function accepts `CALL`, `CALL/SUFFIX` (DXCC prefix override or portable designator),
   and an optional trailing `-N` SSID. `manta_spot::grammar::is_plausible` is unchanged — its one
   remaining call site (`manta_spot::validator`, the decoder-output gate) still needs it narrow.

2. **The SSID grammar is exactly one `-` followed by 1–2 ASCII digits, no leading zero**: `-1`
   through `-99`. Rejected: `-0` (SSID 0 is the no-SSID case in the packet convention this
   descends from — `W5AU-0-#` and `W5AU-#` must not both be configurable for the same band),
   leading-zero forms like `-01` (one band index must have exactly one wire spelling), and
   anything beyond `-99` (unbounded digits would let an operator push the rendered identity
   arbitrarily wide into a wire format with a bounded identity field). 1–99 comfortably covers
   any real HF+6m band count and the AX.25/packet SSID range (0–15) the convention descends
   from. The digits-only rule is what keeps the server's own literal `-#` suffix unconfigurable,
   replacing the blanket ban on `-` that previously provided that guarantee.

3. **`login_callsign` (MAN-32) shares the same validator and therefore also accepts SSIDs.**
   `RbnUplinkConfig::effective_login_callsign` already defaults the login identity to
   `station_callsign` when omitted — once an SSID is legal there, rejecting the identical value
   when written explicitly would make the same string valid by omission and invalid by
   declaration. This is a config-surface consistency choice only; it adds no new uplink
   behaviour and is not part of MAN-13/14's multi-band-uplink design.

4. **Accepted values are normalized to ASCII uppercase** at the deserializer. `cty::Table::
   lookup` and the decoder-output allowlist already uppercase internally, and RBN lines are
   conventionally uppercase — without this, a lowercase-configured `station_callsign` would leak
   its casing onto the wire as `deCall: "w5au-1"`. This is the one operator-visible behaviour
   change: a config that previously produced lowercase output now produces uppercase.

5. **The SSID is stripped before the `cty.dat` geography lookup, never from the wire identity.**
   `spot_message.rs`'s `SpotMessage::from_spot` looks up `station_call` in `cty.dat` to populate
   the JSON stream's `deContinent`/`deLat`/`deLon`. `cty.dat`'s exact-call alias rows (e.g.
   `4U1UN`, `4U1VIC`) only match a full-string or `/`-portable-stripped callsign — never a
   `-`-suffixed one — so an operator whose own callsign carries such an override would silently
   lose it once an SSID is appended, falling back to a generic (and wrong) prefix entry. The fix
   strips a valid SSID before that one lookup only; `de_call` and the spot `id` keep the full
   `CALL-N` identity. `cty::exact_match_base_len` itself is deliberately not changed — that
   would extend shared `manta-spot` decoder-output behaviour to solve an operator-identity-only
   problem.

6. **No live-capture column re-measurement in this ticket.** MAN-88 (the AK1A column-layout
   rewrite of `rbn::format_line`) asked MAN-89 to re-measure its 16-column identity budget
   against a fresh capture once SSIDs exist. As of this ticket, MAN-88 is not merged — `rbn.rs`
   has no column anchoring at all, so there is no invariant for an SSID to violate yet — and no
   live-capture data is available in this environment. Capture-backed verification of RBN's own
   padding for long identities is tracked under MAN-86. If MAN-88 lands first, its own
   shift-right/never-truncate/guaranteed-separator rule (its Decision 3) already covers SSID
   identities without further change; this repo's `rbn.rs` and `spot_message.rs` tests assert
   that rule layout-independently rather than freezing absolute columns.

7. **The "at least one letter and one digit" rule is per-segment, not whole-string** (PR #131
   review). Applied to the whole base it accepts identities where the letters and the digits
   live in *different* `/`-delimited segments — `ABC/123`, `A/1/B` — so no segment can be the
   actual callsign, and the malformed value then goes out as the station identity on every
   telnet and JSON spot. The rule now requires that **at least one** segment carry both a letter
   and a digit, which is exactly the segment that is the call; prefix segments (`JW/`) and
   portable/beacon suffixes (`/P`, `/B`, `/3`) legitimately carry letters or digits alone and
   stay accepted. This is still a strict superset of `grammar::is_plausible`, which requires the
   *first* segment to carry both.

   The qualifying segment must ALSO be at least `MIN_CALLSIGN_LEN` (3) characters -- the minimum
   ITU call structure of prefix + digit + suffix (`W1A`) -- not only the slash-composed base (PR
   #131 review, round 3). Applying the length bound to the base alone accepted `A1/B`, `W1/P` and
   `W1/P-1`: the base clears 3 characters while a two-character segment supplies the letter and
   the digit, so no segment is a complete call and the operator's typo becomes the station
   identity on every telnet and JSON spot.

   Finally, the qualifying segment is tested for that STRUCTURE directly, not for a length plus
   one of each character class (PR #131 review, round 4). Length-plus-classes still accepted
   `W12` -- and therefore `W12-1` and `W12/P`, whose only other segment is a portable
   designator -- because it never required the letter SUFFIX that follows a call's separating
   digit. `config::is_complete_callsign` now requires some digit with at least one letter before
   it AND at least one letter after it, which is exactly prefix + digit + suffix and subsumes the
   round-3 length bound (letter-digit-letter is already 3 characters). Verified to keep every
   real form the earlier rounds accepted: `W1A`, `W5AU`, `4U1UN`, `3DA0RS`, `GB3LER/B`,
   `JW/LB2PG`, `VP2E/K5ARH/M`, and multi-digit special-event calls such as `LZ130LO`.

   That letter suffix must run to the END of the segment (PR #131 review, round 5). "A letter
   somewhere to the right of the separating digit" is weaker than the structure it was meant to
   express: `W1A2` (and therefore `W1A2-1` and `W1A2/P`) satisfied it with a trailing digit
   sitting OUTSIDE the suffix, and that typo would become the station identity on every telnet
   and JSON spot. ITU RR 19.68A puts a letter last in every amateur callsign, so
   `config::is_complete_callsign` now requires the segment's LAST character to be a letter, with
   a digit somewhere between it and the segment's first letter. The accepted forms above are
   unchanged -- every one of them already ends in a letter.

   A DIGIT-LED segment qualifies too (PR #131 review, round 6). "Some digit with at least one
   letter BEFORE it" excluded the special-event calls whose only digit is the leading prefix
   numeral: `4AFARU`, `4GRID` and `5NNHR` are all in the vendored `crates/manta-spot/data/
   master.scp`, and `grammar::is_plausible` -- the validator this one replaced -- accepted every
   one of them, so rejecting them broke the strict-superset property this ticket claims and would
   have stopped those operators' daemons from starting at all. `config::is_complete_callsign`
   now also accepts a segment whose FIRST character is a digit and whose first letter is at index
   1 -- one leading numeral, then letters, then a letter last. The `first_letter == 1` bound
   keeps `12A` out (no ITU prefix carries two leading numerals), and the trailing-letter rule
   from round 5 is unchanged, so `W12`, `W1A2`, `4GRID2` and `5NNHR9` all stay rejected. Because
   the separating-digit shape no longer implies the round-3 length bound for this new shape, that
   bound is now restated explicitly in the predicate (`4G` is rejected). Re-measured over the
   whole vendored `master.scp`: 50,001 entries, and the only line the predicate rejects is the
   file's own `!!Order,1,1` header.

   The leading numeral is `1`-`9`, not "any digit" (round 7). No ITU prefix begins with `0`, and
   no `master.scp` entry does either, so `0AB` is a mistyped identity rather than a digit-led
   special-event call; restricting the shortcut keeps it from becoming the hole round 6 was
   guarding against. The separating-digit rule is untouched, so `0A1B` still qualifies through it
   exactly as before.

   That re-measurement is no longer a one-off: `config::tests::
   accepts_every_master_scp_call_the_replaced_grammar_accepted` asserts the strict-superset
   property over the whole vendored snapshot on every run, with and without an SSID. Rounds 4,
   5 and 6 each narrowed this predicate and each time the hand-picked fixture lists still
   reported the superset property as holding -- fixtures cannot catch that class of regression,
   the snapshot can.

8. **The unresolved-geography metric classifies the SSID-STRIPPED station callsign** (PR #131
   review, round 6). `SpotMessage::from_spot` resolves the de side through
   `config::strip_ssid(station_call)`, but `manta-cli::start_spot_server` precomputed
   `station_geography_unresolved` from the raw `cfg.station_callsign`. For a mobile node
   identity such as `K5ARH/MM-1`, `is_outside_any_dxcc_entity` then sees `MM-1` instead of `MM`
   (so the /MM test misses) while `cty.lookup` still resolves through the allocated `K` prefix --
   the spot goes out with the de-side `NO_DXCC_ENTITY` sentinel and unknown geography while
   `manta_spots_unresolved_geography_total` stays at zero for every spot the node emits. The
   precomputation now applies the same `strip_ssid` the emit path applies, so the counter's
   condition and the sentinel's condition are derived from one string.

   The classification is CENTRALIZED rather than fixed at the call site (round 7). The predicate
   used to live in `manta-cli` while the sentinel emission it has to agree with lives in
   `manta-server::spot_message` -- one crate away, which is how the two drifted in the first
   place. `geography_is_unresolved` now lives beside `SpotMessage::from_spot`, and the
   station-side entry point `spot_message::station_geography_unresolved` applies `strip_ssid`
   itself, so no caller can forget it; the dx-side entry point deliberately takes the callsign
   verbatim, since decoder output never carries an SSID.

## Non-goals

- Auto-generating the `N` band index. `manta-cli` still hardcodes a single DDC; a real per-band
  `N` requires the multi-band daemon architecture (MAN-13/14). Today an operator runs one daemon
  per band and configures each with its own `W5AU-<N>` — this ticket makes that configuration
  legal, not automatic.
- Any change to the telnet *login* callsign check (client-supplied, MAN-86's scope) or to
  `manta_spot::grammar::is_plausible`'s behaviour.
