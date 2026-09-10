# MAN-88: fixed-column AK1A telnet layout, plus a CW-Skimmer-native variant

MAN-88's gherkin asked for two things: manta's telnet spot line to match
RBN's fixed-column AK1A layout (not just field order), and an operator
toggle to a CW-Skimmer-native layout for operators running manta behind
W3OA's Aggregator. Both scenarios required decisions this ticket's own
text, the 2026-09-05 review reports, and
`docs/DECISIONS/2026-09-06-broad-review-decisions.md` (decision D1) left
open.

## The measured gap

`rbn::format_line`'s old format string
(`"DX de {spotter}-#:{freq:>9.1}  {call:<8} CW  ..."`) had fixed *widths*
on two fields but no fixed *columns* anywhere: the `DX de {spotter}-#:`
prefix was never padded, so every field after it drifted with the
spotter callsign's length. Measured directly against a live capture from
`telnet.reversebeacon.net:7000` quoted in the 2026-09-05 lens-2 review:

```
DX de S53A-#:   14011.90  N8II           CW    21 dB  25 WPM  CQ      0236Z
```

Indexed column-by-column, this gives a fixed absolute-column layout:

| Field | Columns | Rule |
|---|---|---|
| `DX de {spotter}-#:` | 1-… | literal, left-justified |
| padding | … | frequency's last char lands on column **24** |
| frequency, kHz | ends at **24** | `{:.2}` — 2 decimals, never truncated |
| separator | 25-26 | pads to the callsign column; minimum one space |
| callsign | **27**-41 | left-justified, minimum width **15** |
| mode | **42**-47 | `"CW"` in a 6-wide field; its own padding is the separator |
| SNR | 48-52 | `{:>2} dB` |
| separator | 53-54 | two spaces |
| WPM | 55-60 | `{:>2} WPM` |
| separator | 61-62 | two spaces |
| type | 63-68 | `{:<6}` — `CQ`/`DE`/`BEACON`/`` all fit; `BEACON` is exactly 6 |
| separator | 69-70 | two spaces |
| time | **71**-75 | `HHMMZ` |

Implemented in `crates/manta-server/src/rbn.rs::format_line`, reproducing
the capture byte-for-byte (`matches_the_live_rbn_capture_byte_for_byte`).

## Decision 1 — `[server].line_format` governs the inbound telnet server only

Both the inbound telnet server and the outbound `[[rbn_uplink]]` client
shared one rendering function before this ticket. Nothing in the ticket,
the reviews, or D1 settled whether a new format-selection key should
reach the uplink too.

**Decision: it does not.** `uplink::forward_loop` calls
`rbn::format_line(..., rbn::LineFormat::Rbn)` with a hardcoded constant,
independent of `[server].line_format`.

Rationale:
- The ticket names the key `[server].line_format`, and `[server]` is
  already the *inbound-listener* namespace; `RbnUplinkConfig` is a
  separate array-of-tables for outbound targets.
- The uplink's peer is an RBN spot-collection endpoint, not an Aggregator
  instance reading *from* manta — the ticket's Scenario 2 rationale
  ("running manta behind Aggregator") doesn't apply to it.
- D2 (MAN-90) has not yet verified what a real RBN ingest accepts from an
  uplink; letting an inbound-listener setting silently change the bytes
  on that unverified path would be exactly the coupling D2 warns against.

If a per-target format is ever wanted, `RbnUplinkConfig` gets its own key.

## Decision 2 — the `skimmer` layout is derived, not measured

The exact byte layout of a genuine CW-Skimmer-native line is not
verifiable from anything in this repo or the read-only thoughts pool: the
2026-09-05 review's primary source (`lens2/cwskimmer.txt`, a pdftotext
dump of the CW Skimmer manual) was a scratch-directory artifact of that
session and was never preserved. The one thing every surviving source
agrees on is qualitative: "no mode field; CQ/DE/blank" (lens-2 report,
hit-list item #4).

**Decision:** `LineFormat::Skimmer` renders the identical AK1A layout
with the 6-wide mode field deleted and the layout's standard 2-space
separator put in its place. Everything after the callsign column shifts
left by 4; time lands at column **67**.

Rationale for shifting rather than blanking the field in place: the
ticket says "no mode column" — a blanked-but-reserved 6-column field is
still a mode column to a column-based parser (it would read
`mode = ""`, not absent). Under whitespace tokenization (what Aggregator
almost certainly uses, since it must consume Skimmer, SkimSrv, and RBN
variants), the two choices are indistinguishable anyway, so the risk is
confined to a column parser reading the Skimmer variant specifically —
not this ticket's stated consumer.

This is a documented placeholder, not a verified capture. MAN-86 (the
`SKIMMER/SETT` handshake ticket) is where a real Aggregator/CW Skimmer
capture will land and can correct this — a one-constant change plus a
test-vector update, since both surfaces route through the same function.

## Decision 3 — minimum-width identity/callsign columns, shift-right overflow

`manta_spot::grammar::is_plausible` caps callsign length, but MAN-28's
Watch List bypasses grammar validation entirely
(`crates/manta-spot/src/validator.rs:669-673`), so an operator allowlist
entry of arbitrary length can reach the renderer. The spotter identity
and callsign columns are therefore **minimum** widths with a guaranteed
one-space separator: when a value doesn't fit, the rest of the line
shifts right by exactly the overflow, rather than truncating (which would
forge a wrong callsign/identity) or abutting the next field (which would
corrupt the token for whitespace-splitting parsers).

Each field's anchor is computed from the column the **previous field
actually ended on**, not from a fixed separator width, so an overrun is
absorbed by the next separator instead of cascading down the line. Two
combinations exhaust the 24-column identity+frequency budget and so cost
the *frequency field* its own anchor (it ends at column 25):

- a 7-character base callsign at an 8-character frequency — identity 16 +
  freq 8 (e.g. `VE3ABCD` on 20 m);
- a 6-character base callsign at a 9-character 6-digit-MHz frequency —
  identity 15 + freq 9 (e.g. `DL8LAS` on 2 m, raised by the PR #114
  review as the cross-product the per-field tests missed).

In both, the callsign column re-anchors at 27 and the mode, SNR, WPM,
type and time fields stay exactly on their RBN columns — a column-based
parser reads every field after the frequency correctly. Only the extreme
of both at once (a 7-character spotter on 2 m: identity 16 + freq 9 +
two mandatory separators already reaches column 27) pushes the callsign
column to 28, and even there the mode column onwards re-anchors at 42.
Pinned by `a_seven_character_spotter_drifts_only_the_frequency_field`,
`a_six_character_spotter_on_two_metres_keeps_every_later_column`, and
`every_spotter_length_and_band_keeps_the_later_columns_anchored`.

Absorbing rather than cascading is manta's own documented choice, not a
reproduction of how RBN itself pads a longer identity — no multi-line
capture with a longer spotter identity was available to check against.
What it protects is the property the ticket asked for: a fixed-column
parser never reads a later field one character late.

The **SNR and WPM fields are minimum widths on the same rule** (raised by
the PR #114 review as the case the identity/callsign reasoning above did
not cover). `{:>2}` pads but never truncates, and neither
`manta_spot::validator` nor the renderer clamps the value it is given:
`Demod::snr_2500_db` reaches roughly -14 dB when a track's keying rails
converge, which renders three characters. So the SNR, WPM, type and time
anchors are expressed as offsets from the column the **mode field
actually ended on** (`SNR_END_OFFSET` 2, `WPM_END_OFFSET` 9,
`TYPE_START_OFFSET` 16, `TIME_START_OFFSET` 24 in `rbn.rs`) rather than as
absolute columns: a wide SNR or a three-digit WPM widens its own field and
gives up only its own anchor, while the type field stays at 63 and the
time field at 71. Expressing them as offsets is also what lets one set of
constants describe both layouts — `LineFormat::Skimmer`'s 2-wide mode
field ends 4 columns earlier, so its tail follows at 59 and 67. Pinned by
`a_wide_snr_widens_its_own_field_but_moves_no_later_column`,
`a_wide_wpm_widens_its_own_field_but_moves_no_later_column`, and
`the_skimmer_layout_keeps_its_time_column_under_a_wide_snr`.

**MAN-89** (`CALL-N-#` SSIDs) pushes many spotter identities past the
16-column budget (`DX de ` + 7-char base call + `-#:`) for base callsigns
of 5+ characters — its plan should re-measure against a fresh capture
once that ticket lands.

## Scope not covered here

- The `SKIMMER/SETT` handshake, CW Skimmer banner, login prompt text, and
  `BYE` → `CU AGN!` are MAN-86. `line_format = "skimmer"` works standalone
  today without it — nothing in `command.rs` ties the two together.
- `CALL-N-#` SSID support in `station_callsign` is MAN-89 / D4.
- D3's +7 dB SNR reference-bandwidth conversion (MAN-102) changes the SNR
  field's *value* in this same function; deliberately kept separate so
  the two changes stay independently reviewable and revertable.
- The JSON stream is unaffected — SPEC §1.4 keeps full Hz precision there;
  only the telnet-side rounding sentence changed (0.1 kHz → 0.01 kHz).

## Implementation

- `crates/manta-server/src/rbn.rs` — `LineFormat` enum, rewritten
  `format_line`.
- `crates/manta-server/src/config.rs` — `ServerConfig::line_format`
  (`#[serde(default)]`, `Deserialize` via `LineFormat`'s own
  `rename_all = "lowercase"`).
- `crates/manta-server/src/telnet.rs` — `line_format` threaded through
  `serve` → `handle_client` → `write_spot_line`.
- `crates/manta-server/src/uplink.rs` — `forward_loop` pins
  `LineFormat::Rbn`.
- `crates/manta-cli/src/main.rs` — `cfg.line_format` passed into
  `telnet::serve`; `uplink::serve` unchanged.
