# MAN-94: an NCDXF/IARU beacon spot reads `NCDXF B` on the wire

Date: 2026-10-22. Ticket: MAN-94, found by the 2026-09-05 broad review,
lens 2, item 23.

## Context

The lens-2 review's live capture from `telnet.reversebeacon.net:7000`
held 3 `NCDXF B` spots and 6 generic `BEACON` spots side by side: RBN
labels the 18 stations of the NCDXF/IARU International Beacon Project
`NCDXF B` and every other beacon `BEACON`. manta's `rbn::format_line`
rendered every `SpotType::Beacon` spot as `BEACON`, so a 4U1UN spot on
14.100 MHz read differently from RBN's own spot of the same station. The
raw capture lines were a scratch artifact and are not preserved; the label
spelling rests on the dated review report.

## Decisions

1. **Exact full-call membership plus the Beacon classification.** A spot
   reads `NCDXF B` only when the validator typed it `Beacon` and its
   callsign is exactly one of the 18 roster calls (`rbn::NCDXF_BEACONS`).
   The roster is the one listed at
   <https://www.ncdxf.org/beacon/beaconlocations.html>:

   ```text
   4U1UN VE8AT W6WX KH6RS ZL6B VK6RBP
   JA2IGY RR9O VR2B 4S7B ZS6DN 5Z4B
   4X6TU OH2B CS3B LU4AA OA4B YV5B
   ```

   A member stays listed while it is off the air
   (<https://www.ncdxf.org/beacon/RBN.html> separates membership from
   outages). No prefix or suffix is stripped: `4U1UN/P`, `4U1UNA` and
   `KH6/W6WX` are not members and keep `BEACON`. Matching is on the
   uppercase calls the validator emits.
2. **The classification gates the label, the call does not.** A roster
   call typed `Cq`, `De` or `Unknown` keeps `CQ`, `DE` or a blank type
   field. A Watch List hit without beacon framing is `Unknown` and stays
   so. Frequency and the beacon's three-minute schedule are not checked:
   adding them would be a beacon-identification policy, not a label.
3. **The label belongs to the formatter, not to `SpotType`.** No enum
   variant is added. Validation, deduplication, the uplink's `spot_types`
   filter, `SpotType`'s serialization and the CLI `SPOT:` line (which uses
   `SpotType`'s `Display`, still `BEACON`) are unchanged. The JSON spot has
   no type field, so it is unchanged too.
4. **Both layouts, one column of slack.** Telnet (`line_format = "rbn"` or
   `"skimmer"`) and the outbound uplink share `format_line`, so both show
   the label. The type field is a 6-column minimum; the 7-character
   `NCDXF B` starts on the type column (63 RBN, 59 Skimmer), takes one
   column of the two-space gap, and leaves one space before the time,
   which stays on column 71 (RBN) or 67 (Skimmer):

   ```text
   DX de S53A-#:   14100.00  4U1UN          CW    21 dB  25 WPM  NCDXF B 0236Z
   DX de S53A-#:   14100.00  4U1UN            21 dB  25 WPM  NCDXF B 0236Z
   ```

   The Skimmer line is manta's derived layout
   (`docs/DECISIONS/2026-09-06-man88-ak1a-column-layout.md`, Decision 2),
   not one checked against a real Aggregator.

## Tests

- `crates/manta-server/src/rbn.rs`:
  `a_roster_beacon_renders_the_ncdxf_label_in_both_layouts` pins the two
  lines above; `every_ncdxf_roster_beacon_renders_ncdxf_b_on_the_type_column`
  checks all 18 literal calls in both layouts;
  `a_generic_or_lookalike_beacon_keeps_the_beacon_label` and
  `a_roster_call_with_any_other_spot_type_keeps_its_own_label` pin the
  negatives.
- `crates/manta-server/tests/uplink_acceptance.rs`:
  `an_ncdxf_beacon_is_forwarded_with_rbns_ncdxf_b_label` reads the literal
  `NCDXF B` and `BEACON` lines, CRLF included, off a mock RBN target.

## Not verified

No new live RBN capture was made, and RBN's ingest of a manta-uplinked
`NCDXF B` line is as unverified as the rest of the uplink (MAN-90).
