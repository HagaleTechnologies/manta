# manta JSON spot stream

`manta run` with a `[server]` table publishes every validated spot as a JSON
object on `[server].json_port` (default 7301). This stream is the ingest
surface for cqdx and other consumers that want more than the RBN telnet line
on port 7300. This page lists every field of that object.

## Transport

- Raw TCP and WebSocket share one listener on `json_port`. The server tells
  them apart by peeking at the first bytes a client sends.
- A raw TCP client gets one JSON object per line (JSON Lines), each ending in
  `\n`. It should send nothing. A client that sends nothing for 500 ms is
  classified as raw TCP. Any bytes it sends later close the connection.
- A client whose first bytes are an HTTP `GET` WebSocket upgrade request gets
  one WebSocket text frame per spot. It must complete the handshake within
  10 s. After that it may send Ping and Close frames only. Text, Binary and
  Pong frames close the connection, as does any inbound frame over 16 KiB.
- The server only pushes. There is no request, subscription or filter
  message, and no history: a client receives the spots published after it
  connects.
- Slow clients are disconnected, never back-pressured. A client is
  disconnected when a single write takes longer than 10 s, or when it falls
  more than 1024 spots behind the broadcast channel.
- Connection limits: at most 512 concurrent clients in total, and 16 per
  source IP by default. A connection over the per-IP quota is closed at once.
  `[server].json_max_connections_per_ip` overrides the per-IP quota (`0`
  means no per-IP cap). Behind a TLS-terminating reverse proxy every client
  shares the proxy's IP, so raise or disable it there (see
  [RUNBOOKS/network-exposure.md](RUNBOOKS/network-exposure.md)).
- Ping limits: a WebSocket client may send at most 10 Pings per 60 s, counted
  both per connection and per source IP. A client over either budget is
  disconnected. `[server].json_max_pings_per_ip` overrides the per-IP budget
  (`0` means no per-IP cap; the per-connection budget still applies).
- Both keys are described in [CONFIG.md](CONFIG.md#server).

The behaviour above is implemented in
[`crates/manta-server/src/json_stream.rs`](../crates/manta-server/src/json_stream.rs).

## Example

This line was captured from `manta run --config` replaying the output of
`manta gen v1`, with `replay_epoch = 1700000000` and `station_callsign` set to
`W1AW`:

```json
{"id":"W1AW:12534119743695398054:1:2006272:W1AW","source":"skimmer","timestamp":1700000020,"ingestedAt":1700000020,"frequency":14012362,"band":"20m","mode":"CW","dxCall":"W1AW","dxGrid":null,"dxLat":37.6,"dxLon":-91.87,"dxDxcc":291,"dxContinent":"NA","dxCqZone":5,"deCall":"W1AW","deGrid":null,"deLat":37.6,"deLon":-91.87,"deDxcc":291,"deContinent":"NA","snr":21,"snrRefHz":2500,"wpm":20,"decodeConfidence":0.5541629,"decoderVersion":"manta-0.1.0+d3bfa7c0c47a","channelizerResolutionHz":null}
```

## Fields

Every object carries all 26 keys, in this order. Keys are never omitted; a
missing value is JSON `null`. In the "Null?" column, "never" means the field
cannot be `null`, "always" means manta sends `null` on every spot today, and
"not today" means the field's type allows `null` but manta always fills it.

The `dx*` fields describe the station heard. The `de*` fields describe the
reporting station, `[server].station_callsign`. Geography comes from the
vendored `cty.dat` (or the file given by `--cty` or `[spot].cty_path`) and
the vendored ADIF entity table `dxcc.tsv`.

| Field | JSON type | Null? | Meaning |
|---|---|---|---|
| `id` | string | never | Unique key for this spot. Built today as `station_callsign:session_nonce:track_id:sample_ts:dxCall`. `session_nonce` is a hash of the file's contents when replaying a recording, so replaying the same recording again gives the same ids; for live input it is the session start time in nanoseconds. Treat `id` as an opaque key: do not parse it, because the composition may change. |
| `source` | string | never | Always `"skimmer"`. |
| `timestamp` | integer | never | When the spot was heard, in Unix seconds (UTC, whole seconds). It is the session start time plus the spot's offset into the stream. For a replayed file the start time is the file's modification time, or `[input].replay_epoch` when set. |
| `ingestedAt` | integer | never | Unix seconds. manta sends the same value as `timestamp`; cqdx overwrites it with its own receipt time. |
| `frequency` | integer | never | Spot frequency in Hz, rounded to the nearest Hz. It is an absolute RF frequency only when the source has an RF reference (MAN-34). |
| `band` | string | never | Amateur band label derived from the rounded `frequency`: `2200m`, `630m`, `160m`, `80m`, `60m`, `40m`, `30m`, `20m`, `17m`, `15m`, `12m`, `10m` or `6m`. `"unknown"` outside those allocations. Band edges are inclusive. |
| `mode` | string | never | Always `"CW"`. |
| `dxCall` | string | never | The decoded callsign, as validated. It may carry a `/` designator such as `/P` or `/MM`. |
| `dxGrid` | null | always | Maidenhead locator. Reserved by the contract; manta does not fill it. |
| `dxLat` | number or null | when unresolved | Latitude in degrees, north positive, of the `cty.dat` entry the call resolves to (an entity reference point, not the station's position). `null` when the call does not resolve, or carries `/MM` or `/AM`. |
| `dxLon` | number or null | when unresolved | Longitude in degrees, east positive (manta flips `cty.dat`'s west-positive sign). `null` in the same cases as `dxLat`. |
| `dxDxcc` | integer | never | ADIF DXCC entity number, for example `291` for the United States. `-1` (`UNKNOWN_DXCC`) when the call does not resolve. `0` (ADIF "not in any DXCC entity") when the call carries `/MM` or `/AM` after its first segment (MAN-136). |
| `dxContinent` | string | never | Two-letter continent code from `cty.dat`: `AF`, `AN`, `AS`, `EU`, `NA`, `OC` or `SA`. Empty string `""` (`UNKNOWN_CONTINENT`) when the call does not resolve or carries `/MM` or `/AM`. |
| `dxCqZone` | integer | never | CQ zone, 1 to 40. `0` (`UNKNOWN_CQ_ZONE`) when the call does not resolve or carries `/MM` or `/AM`. |
| `deCall` | string | never | `[server].station_callsign` exactly as configured, including any RBN `-N` SSID (MAN-89). The SSID is stripped before the geography lookup for the `de*` fields below. |
| `deGrid` | null | always | Maidenhead locator of the reporting station. Reserved by the contract; manta does not fill it. |
| `deLat` | number or null | when unresolved | As `dxLat`, for the reporting station's callsign. |
| `deLon` | number or null | when unresolved | As `dxLon`, for the reporting station's callsign. |
| `deDxcc` | integer | never | As `dxDxcc`, for the reporting station's callsign: `-1` when it does not resolve, `0` for `/MM` or `/AM`. |
| `deContinent` | string | never | As `dxContinent`, for the reporting station's callsign. |
| `snr` | integer | not today | Signal-to-noise ratio in dB, rounded, referenced to a 2500 Hz noise bandwidth (see `snrRefHz`). Telnet and the RBN uplink quote the same spot in 500 Hz, about 7 dB higher (MAN-102, decision D3). |
| `snrRefHz` | integer | never | The reference bandwidth `snr` is quoted in, in Hz. Always `2500`. Not yet in the dispensa contract. |
| `wpm` | integer | not today | Keying speed in words per minute, rounded. |
| `decodeConfidence` | number | not today | Callsign confidence from 0 to 1 (SPEC-decode-core §4.6): the geometric mean of the per-character confidences times a repetition factor `1 - 0.5^r`, multiplied by 1.15 and capped at 1 when the call is in `master.scp`. |
| `decoderVersion` | string | not today | Build identity, `manta-<version>+<commit>`, for example `manta-0.1.0+d3bfa7c0c47a`. The commit is 12 hex digits, an override from `MANTA_GIT_SHA`, or `unknown`. Compare the part before `+` across releases; the whole string names the exact binary. It is not decoder output (MAN-83). |
| `channelizerResolutionHz` | null | always | Reserved by the contract; manta does not fill it. |

There is no `deCqZone` field. The contract has none, and manta does not send
one.

When `dxDxcc` is `-1`, `dxLat` and `dxLon` are normally `null` as well. The
exception is a `cty.dat` that has been replaced without regenerating
`dxcc.tsv`: the coordinates then resolve but the entity number does not. The
`manta_spots_unresolved_geography_total` counter counts every spot sent with
unresolved geography on either side, including `/MM` and `/AM`.

## Contract and stability

The cross-repo contract for this object is dispensa's
`contracts/spots/spots.v1.schema.json` (ADR-0011). The dispensa repository is
not publicly readable, so this page is the in-repo field reference. The one
divergence today is `snrRefHz`, which the contract does not have yet; the
proposed schema fragment is in the MAN-102 decision record below.

Any change to what a field means follows the decoder-output rule in
[CHANGELOG.md](../CHANGELOG.md): a change that alters decoder output is
listed under `### Decoder output`, and the next release bumps at least the
MINOR version.

## Not this schema

`manta decode --json` and `manta run --json` print a debugging format.
That format is not stable and is not described here.

## Where the values come from

- [`crates/manta-server/src/spot_message.rs`](../crates/manta-server/src/spot_message.rs):
  the `SpotMessage` struct and `SpotMessage::from_spot`, which builds every
  field.
- [DECISIONS/2026-09-06-broad-review-decisions.md](DECISIONS/2026-09-06-broad-review-decisions.md),
  decision D3, and
  [DECISIONS/2026-09-07-man102-snr-reference-and-estimator.md](DECISIONS/2026-09-07-man102-snr-reference-and-estimator.md)
  (MAN-102): why `snr` is 2500 Hz here and 500 Hz on telnet, and the
  proposed `snrRefHz` contract fragment.
- [DECISIONS/2026-09-07-man136-dxcc-and-unknown-geography-sentinels.md](DECISIONS/2026-09-07-man136-dxcc-and-unknown-geography-sentinels.md)
  (MAN-136): `dxDxcc`/`deDxcc`, the `UNKNOWN_*` sentinels, and the `/MM` and
  `/AM` rule.
- [DECISIONS/2026-10-10-man83-build-identity-and-decoder-versioning.md](DECISIONS/2026-10-10-man83-build-identity-and-decoder-versioning.md)
  (MAN-83): the `decoderVersion` format and the decoder-output rule.
