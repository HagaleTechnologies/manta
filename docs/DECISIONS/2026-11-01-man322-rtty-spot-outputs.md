# MAN-322: RTTY spots reach telnet, the JSON stream and the RBN uplink labelled RTTY

Date: 2026-11-01. Ticket: MAN-322. It is the output half of RTTY spotting.
The engine and validator half is the MAN-321 follow-up
([2026-10-30-man321-rtty-decoding.md](2026-10-30-man321-rtty-decoding.md),
"Follow-up scope").

## Problem

manta could not say a spot was anything but CW. `manta_spot::Spot` had no
mode and no baud, and `wpm: f32` was its only speed. Every output wrote CW
as a literal: `SpotMessage::from_spot` set `"mode":"CW"`, `rbn::format_line`
wrote `CW` in the mode column and `render` appended ` WPM`. Telnet had one
listener, and its `sh/dx RTTY` was hard-coded to match nothing.

Reproduced on `main` at `d144e36`: a test rendered a `Spot` at 14 080.17 kHz
with `wpm: 0.0` through each output.

```
RBN : DX de W3XYZ-#:  14080.17  W1AW           CW    27 dB   0 WPM  CQ      2213Z
SKIM: DX de W3XYZ-#:  14080.17  W1AW             27 dB   0 WPM  CQ      2213Z
JSON: {... "band":"20m","mode":"CW",... "wpm":0,"rst":null,...}   (29 keys)
CLI : {"dxCall":"W1AW","frequency":14080170.0,"snr":20.0,"wpm":0.0,...,"qrlQuery":false}
```

A spot in the RTTY sub-band was labelled CW with `0 WPM` on every output.
Parsing `[server]` with `rtty_telnet_port = 7304` failed with
`unknown field 'rtty_telnet_port'`, so no RTTY port existed either.

## Decision

`Spot` gains `mode: SpotMode` (`Cw` or `Rtty`) and `baud: Option<f32>`.
Every output reads them. The validator stamps `SpotMode::Cw` and
`baud: None` on every spot it builds, so no RTTY spot exists until the
MAN-321 follow-up sets them.

| Surface | What changes |
|---|---|
| JSON stream (`:7301`) | `mode` is `"CW"` or `"RTTY"` from the spot. A new key `baud` follows `wpm`: the RTTY rate to two decimals (`45.45`), `null` on CW. `wpm` is `null` on an RTTY spot. 30 keys instead of 29 |
| `run --json` / `decode --json` | `"mode"` and `"baud"` follow `qrlQuery`. `wpm` is `null` on an RTTY spot |
| `manta run` stdout | An RTTY spot prints `45 BPS RTTY` where a CW spot prints `20 WPM`. A CW line is byte-identical to before |
| Telnet `telnet_port` (`:7300`) | Nothing. It serves CW spots only, live and in `sh/dx` history |
| Telnet `rtty_telnet_port` (new, opt-in) | A second listener on `bind_addr` that serves RTTY spots only, with the same greeting, login, commands and `line_format` |
| Telnet and RBN uplink line | An RTTY spot has `RTTY` in the mode column and `45 BPS` as its speed. The columns do not move |
| Live comparison with RBN | An RTTY spot is skipped: no `spotGrade`, and it does not count as ungraded |
| `set dx filter unique` | Occurrence counts key on callsign and mode |
| `/metrics` | `manta_listener_up{listener="rtty_telnet"}` when the RTTY port is set |

The RTTY telnet port, with `rtty_telnet_port = 7304`:

```
DX de W3XYZ-#:  14080.17  W1AW           RTTY  27 dB  45 BPS  CQ      2213Z
```

With `line_format = "skimmer"` the same spot has no mode column:

```
DX de W3XYZ-#:  14080.17  W1AW             27 dB  45 BPS  CQ      2213Z
```

The JSON stream, for the same spot:

```json
{"id":"W3XYZ:1:12:2737440:W1AW","source":"skimmer","timestamp":1700000000,"ingestedAt":1700000000,"frequency":14080170,"band":"20m","mode":"RTTY","dxCall":"W1AW","dxGrid":null,"dxLat":37.6,"dxLon":-91.87,"dxDxcc":291,"dxContinent":"NA","dxCqZone":5,"deCall":"W3XYZ","deGrid":null,"deLat":null,"deLon":null,"deDxcc":291,"deContinent":"NA","snr":20,"snrRefHz":2500,"wpm":null,"baud":45.45,"rst":null,"qrlQuery":false,"decodeConfidence":0.71,"decoderVersion":"v","channelizerResolutionHz":null,"seq":1}
```

`sh/dx` mode words intersect with the listener's mode:

| Listener | `sh/dx` | `sh/dx CW` | `sh/dx RTTY` |
|---|---|---|---|
| `telnet_port` | CW rows | CW rows | no rows |
| `rtty_telnet_port` | RTTY rows | no rows | RTTY rows |

The decisions, each with what it rules out:

- **D1. Output surfaces only.** This ticket changes how spots are written,
  not which spots exist. The validator stamps `SpotMode::Cw` and
  `baud: None` at both `Spot` construction sites. The ticket's
  "Given manta is spotting both CW and RTTY" is shown with RTTY spots
  published on the bus. A daemon test shows the RTTY port exists and never
  sends a CW spot. The end-to-end RTTY test belongs to the MAN-321 follow-up.
- **D2. Two new fields at the end of `Spot`; `wpm` stays `f32`.** `wpm` now
  means CW keying speed, and outputs read it only when `mode` is `Cw`.
  Rejected: carrying baud in `wpm`. That was the MAN-321 plan's default, the
  Q-0085 note reversed it, and 45.45 would trip the validator's
  `MAX_PLAUSIBLE_WPM = 45.0` gate once the engine feeds it. Rejected:
  `wpm: Option<f32>`. It changes more files for no difference on any wire.
  Rejected: an enum that carries baud. It needs `#[serde(flatten)]` and
  omits `baud` on CW, which breaks SPOT-SCHEMA's "keys are never omitted"
  rule.
- **D3. JSON stream.** `mode` comes from `SpotMode::label()`. `wpm` is `null`
  on a non-CW spot. `baud` follows `wpm`, is `null` on CW, and is the RTTY
  rate rounded to two decimals as a JSON number. The fragment below is
  ready to lift into dispensa.
- **D4. CLI JSON and the `SPOT:` line.** CLI JSON appends `"mode"` and
  `"baud"` after `qrlQuery` and sends `wpm` as `null` on an RTTY spot.
  `Spot` serializes through a private `SpotJson` with an exhaustive
  destructure, so a future `Spot` field fails to compile until it is
  serialized. The `SPOT:` line prints `<baud> BPS RTTY` for an RTTY spot,
  and `unknown BPS RTTY` when the baud is missing. A CW line is unchanged.
- **D5. One mode per telnet listener.** `telnet_port` serves CW only, as
  before. The opt-in `[server].rtty_telnet_port` (unset by default) binds
  `bind_addr` and serves RTTY only. Both listeners have the same greeting,
  login, command grammar and `line_format`. Each has its own connection cap
  and per-IP quotas, built from the existing `telnet_*` keys. A `sh/dx` mode
  word intersects with the listener's mode. The MAN-92 rule that mode words
  never filter the live stream still holds. Rejected: one port plus a
  command after connecting. A logger that only connects, as the ticket's
  scenario says, would still get every spot. Rejected: one port carrying
  both modes. A CW-only logger, an Aggregator reading the Skimmer layout
  (which has no mode column) and a downstream manta relay would all receive
  RTTY rows. Rejected: an RTTY port on by default. It would open a new
  public port on every node for a mode no manta decodes yet.
- **D6. Line format.** The mode column says `RTTY`. The speed is the baud
  rounded to an integer, with the unit `BPS`. ` BPS` and ` WPM` are the same
  width, so the type still starts at column 63 and the time at column 71.
  An RTTY spot with no baud gets a blank speed field, like a relayed FT8
  spot (MAN-320).
- **D7. Uplink.** RTTY spots go to the uplink like CW spots, through the
  same `spot_types` and `dry_run` gates. There is no new `[[rbn_uplink]]`
  key. The layout is unverified against RBN's ingest, like every uplink
  line, and joins MAN-90's open claims as C14
  ([2026-10-11-man90-uplink-protocol-verification.md](2026-10-11-man90-uplink-protocol-verification.md)).
- **D8. The live comparator skips non-CW spots.** RBN's reference feed is
  CW-only (`[rbn_compare]`), so grading an RTTY spot against it would
  always read `none`. The comparator skips the spot before grading: no
  `spotGrade`, and it does not count as ungraded.
- **D9. `set dx filter unique` counts per mode.** Occurrence counts key on
  `(callsign, mode)`, so each listener's threshold counts its own mode. A
  call heard twice on CW does not pass the unique filter on the RTTY port.
- **D10. One name.** The listener is `rtty_telnet` everywhere it is named:
  the `manta_listener_up` label, the startup banner field `rtty_telnet=`,
  and the `manta config check` listener list. The key is
  `rtty_telnet_port`, and `main.rs` names it `LISTENER_RTTY_TELNET`.
- **D11. A 40-file budget.** The MAN-321 implement attempt stopped at the
  dispatcher's 40-file limit with 63 files. A new `Spot` field touches 26
  files by itself. This change leaves README.md, ARCHITECTURE.md and the
  wiki alone; docs/CONFIG.md is the key reference and this record is the
  normative one. The `config init` scaffold and `manta.example.toml` each
  carry a commented `rtty_telnet_port` line, because the docs tests require
  every loader key in both, so the change is 41 files.

Three defaults stand until the owner says otherwise:

1. `telnet_port` stays CW-only, and RTTY spots reach telnet only through
   the opt-in `rtty_telnet_port`. RBN's own port 7000 carries both modes;
   D5 gives the reasons manta does not.
2. The speed unit on the telnet and uplink line is `BPS`, the spelling of
   every RTTY line fixture in this repo (`rbn_compare/ak1a.rs`,
   `relay/skimmer_line.rs`).
3. The JSON `baud` is a two-decimal number (`45.45`), not a rounded
   integer. If Q-0085's Answer says integer, only `wire_baud` and its tests
   change.

## dispensa fragment

The `baud` field is agreed for `spots.v1` in dispensa Q-0085. Its type is
not confirmed from this repo: this session could not read Q-0085's Answer.
Proposed change to dispensa `contracts/spots/spots.v1.schema.json` (remove
the comment line first):

```jsonc
// dispensa contracts/spots/spots.v1.schema.json -- proposed change (MAN-322; agreed in Q-0085, type unconfirmed here)
"mode": {
  "enum": ["CW", "RTTY"]
},
"wpm": {
  "type": ["integer", "null"],
  "description": "CW keying speed in words per minute, rounded. Null on an RTTY spot, which carries baud."
},
"baud": {
  "type": ["number", "null"],
  "exclusiveMinimum": 0,
  "description": "RTTY symbol rate in baud, to two decimals (45.45 for standard amateur RTTY). Null on a CW spot.",
  "examples": [45.45, 50]
}
```

## cqdx

cqdx's source enum lacks `skimmer`. manta already sends `"source":"skimmer"`
on every CW spot, so the gap is not new and no manta code or test depends
on cqdx ingest. Check Q-0085's Answer before relying on cqdx to ingest
manta spots of either mode.

## Known limits

- **Loss counters on a scoped listener.** When a telnet client's write
  fails or it lags, the listener counts every queued spot as lost,
  including spots of the other mode it would have skipped. No RTTY spot
  exists yet, so today's numbers do not change.
- **No mode label on spot metrics.** `manta_spots_total` and
  `manta_spots_by_band_total{band,type}` keep their labels. The engine
  follow-up should add a `mode` label when it produces RTTY spots.
- **Cross-input dedupe ignores mode.** `crates/manta-spot/src/dedupe.rs`
  is part of spot production. CW and RTTY sit in different sub-bands, and
  the engine follow-up decides whether dedupe needs the mode.
- **Telnet client metrics count both listeners together.** The telnet
  client and loss metrics have no listener label.
- **The RTTY port's handshake is port 7300's.** The greeting and the
  `SKIMMER/SETT` reply are the MAN-86 Aggregator handshake. Changing them
  without a capture risks admission, so the RTTY port sends what port 7300
  sends.

## Evidence

- `json_stream_acceptance.rs`:
  `an_rtty_spot_carries_mode_rtty_and_baud_not_wpm` (the ticket's JSON
  scenario).
- `telnet_acceptance.rs`:
  `rtty_port_receives_only_rtty_spots_with_speed_in_baud`,
  `sh_dx_on_each_port_returns_only_its_own_mode` and
  `the_unique_filter_counts_each_mode_separately` (the ticket's telnet
  scenario, D5 and D9).
- `replay_acceptance.rs`: `the_rtty_telnet_port_serves_no_cw_spot` (the
  daemon binds the RTTY port, names it in the banner and never sends it a
  CW spot).
- `uplink_acceptance.rs`:
  `an_rtty_spot_reaches_the_uplink_labelled_rtty_in_baud` and
  `dry_run_withholds_an_rtty_spot_too` (D7).
- `rbn_compare_acceptance.rs`:
  `an_rtty_spot_is_neither_graded_nor_counted_ungraded` (D8).
- `rbn.rs`: `an_rtty_spot_renders_rtty_and_bps_on_the_rbn_columns` and its
  Skimmer-layout and no-baud siblings (D6).
- `validator.rs`: `cli_json_appends_mode_and_baud_and_nulls_wpm_on_rtty`;
  `fmt.rs`: `an_rtty_spot_line_quotes_baud_and_names_the_mode` (D4).

## Determinism

The validator still emits CW spots only, so `decode` on any file gives the
same spots as before, with `"mode":"CW","baud":null` appended in CLI JSON.
No golden vector changes.

## Follow-up handoff

The MAN-321 engine slice sets `Spot.mode = SpotMode::Rtty` and `baud` on
the spots its RTTY decode path produces. The outputs above need no further
change. That slice should also add the `mode` label to the spot metrics and
decide whether cross-input dedupe keys on mode.
