# manta JSON spot stream

`manta run` with a `[server]` table publishes every validated spot as a JSON
object on `[server].json_port` (default 7301). This stream is the ingest
surface for cqdx and other consumers that want more than the RBN telnet line
on port 7300. This page lists every field of that object. With
`[rbn_compare]` enabled the same stream also carries one `spotGrade` record
per graded spot (MAN-301); see [`spotGrade` records](#spotgrade-records).

## Transport

- Raw TCP and WebSocket share one listener on `json_port`. The server tells
  them apart by peeking at the first bytes a client sends.
- A raw TCP client gets one JSON object per line (JSON Lines), each ending in
  `\n`. It should send nothing. A client that sends nothing for 500 ms is
  classified as raw TCP. Any bytes it sends later close the connection.
- A client whose first bytes are an HTTP `GET` WebSocket upgrade request gets
  one WebSocket text frame per record (`hello`, a spot, or `spotGrade`).
  It must complete the handshake within
  10 s. After that it may send Ping and Close frames only. Text, Binary and
  Pong frames close the connection, as does any inbound frame over 16 KiB.
- The first record on every connection is a [`hello`](#hello-record).
  Raw TCP receives it once the 500 ms classification ends, then live records only.
- Every spot and `spotGrade` carries `seq`, starting at 1 and increasing by 1
  per record in the session. A plain WebSocket connection also receives only live records.
- WebSocket clients can resume with `GET /?session=<session>&since=<last seq processed>`.
  The server sends the hello, then retained records after `since`, then live records.
  A same-session cursor from `historyFrom - 1` through `hello.seq` resumes fully.
  An older cursor gets the retained suffix with `resumed: false`. A missing or different
  session, or a cursor ahead of `hello.seq`, gets all retained records with `resumed: false`.
  In each false case the consumer has a gap. A session without `since` is a plain connection.
  Empty, negative, nonnumeric, overflowing or repeated `since` values get HTTP 400;
  repeated `session` also gets 400. Query values are not percent-decoded; unknown keys
  and request paths are accepted. Use the session string exactly as received.
- At most 30 resume requests per source IP per 10 s are allowed; excess requests
  get HTTP 429. Behind a reverse proxy clients share this budget and must retry.
- Replay retains `[server].history_minutes` of heard time (default 30 minutes),
  at least the newest 50 records and at most 10,000, spots and grades together.
  Grades age from the newest spot's heard time when published. A zero-minute window
  keeps only the newest 50 records. Replay uses the original seq and record contents.
- Apart from the resume query, the server only pushes. There is no subscription
  or filter message. The live-state port is a separate protocol and does not resume.
- Framing (`hello`, `seq`, resume) follows dispensa's
  `contracts/spots/stream.v1.md` (ADR-0015, Q-0070). Manta's implementation decisions
  are recorded in [MAN-137](DECISIONS/2026-10-14-man137-json-stream-resume.md).
- Slow clients are disconnected, never back-pressured. A client is
  disconnected when a single write takes longer than 10 s, or when it falls
  more than 1024 records (spots and `spotGrade` records together) behind
  the broadcast channel.
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

## `hello` record

Every spot-stream connection opens with this record, before spots or grades:

```json
{"kind":"hello","schema":"stream.v1","session":"1791719990000000000","seq":41,"historyFrom":1,"resumed":false}
```

Every hello carries all 6 keys, in this order:

- `kind` (string, never null): always `"hello"`.
- `schema` (string, never null): `"stream.v1"`, the framing contract.
- `session` (string, never null): opaque session identity, the decimal nonce used in spot IDs. A live server restart changes it. File replays reuse their content-derived identity for deterministic output.
- `seq` (integer, never null): newest sequence published before this connection subscribed, or 0 on an empty session. Live records follow this position.
- `historyFrom` (integer, never null): oldest retained position at the resume decision, clamped to `seq + 1`. If no records remain through this snapshot, it is `seq + 1`.
- `resumed` (boolean, never null): true only for a same-session resume with `historyFrom - 1 <= since <= seq`. False for plain connections, an expired cursor, a missing or different session, or a cursor ahead of the snapshot. A resume returning false signals a gap. Separate replays of the same file reuse a session while numbering starts over; an ahead-of-session cursor resets. Consumers must check `resumed` as well as `session`.

## Example

This line was captured from `manta run --config` replaying the output of
`manta gen v1`, with `replay_epoch = 1700000000`, `station_callsign` set to
`W1AW` and no station location configured, so `deGrid`/`deLat`/`deLon` are
`null`:

```json
{"id":"W1AW:12534119743695398054:1:2006272:W1AW","source":"skimmer","timestamp":1700000020,"ingestedAt":1700000020,"frequency":14012362,"band":"20m","mode":"CW","dxCall":"W1AW","dxGrid":null,"dxLat":37.6,"dxLon":-91.87,"dxDxcc":291,"dxContinent":"NA","dxCqZone":5,"deCall":"W1AW","deGrid":null,"deLat":null,"deLon":null,"deDxcc":291,"deContinent":"NA","snr":21,"snrRefHz":2500,"wpm":20,"decodeConfidence":0.5541629,"decoderVersion":"manta-0.1.0+d3bfa7c0c47a","channelizerResolutionHz":null,"seq":1}
```

## Fields

Every spot object carries all 27 keys, in this order. Keys are never omitted; a
missing value is JSON `null`. In the "Null?" column, "never" means the field
cannot be `null`, "always" means manta sends `null` on every spot today, and
"not today" means the field's type allows `null` but manta always fills it.

The `dx*` fields describe the station heard. The `de*` fields describe the
reporting station, `[server].station_callsign`. Geography comes from the
vendored `cty.dat` (or the file given by `--cty` or `[spot].cty_path`) and
the vendored ADIF entity table `dxcc.tsv`, except the reporting station's
position: `deGrid`/`deLat`/`deLon` come only from the `[server]` location
keys (MAN-138), never `cty.dat`.

| Field | JSON type | Null? | Meaning |
|---|---|---|---|
| `id` | string | never | Unique key for this spot. Built today as `station_callsign:session_nonce:track_id:sample_ts:dxCall`, with `station_callsign` uppercased, including any RBN `-N` SSID. `session_nonce` is a hash of the file's contents when replaying a recording, so replaying the same recording again gives the same ids; for live input it is the session start time in nanoseconds. Treat `id` as an opaque key: do not parse it, because the composition may change. |
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
| `deCall` | string | never | `[server].station_callsign`, uppercased, including any RBN `-N` SSID (MAN-89). The SSID is stripped before the geography lookup for `deDxcc` and `deContinent`. |
| `deGrid` | string or null | when unconfigured | Maidenhead locator of the reporting station: `[server].operator_grid`, or the 6-character square containing `operator_lat`/`operator_lon` when only those are set. Field and square uppercase, subsquare lowercase (`FN31pr`). `null` when none of the three keys is set (MAN-138). |
| `deLat` | number or null | when unconfigured | Latitude in degrees, north positive, of the reporting station: `[server].operator_lat`, or the centre of `operator_grid`'s square when only the grid is set. Never a `cty.dat` centroid. `null` when no location key is set (MAN-138). |
| `deLon` | number or null | when unconfigured | Longitude in degrees, east positive: `[server].operator_lon`, or the centre of `operator_grid`'s square. `null` in the same cases as `deLat`. |
| `deDxcc` | integer | never | As `dxDxcc`, for the reporting station's callsign: `-1` when it does not resolve, `0` for `/MM` or `/AM`. |
| `deContinent` | string | never | As `dxContinent`, for the reporting station's callsign. |
| `snr` | integer | not today | Signal-to-noise ratio in dB, rounded, referenced to a 2500 Hz noise bandwidth (see `snrRefHz`). Telnet and the RBN uplink quote the same spot in 500 Hz, about 7 dB higher (MAN-102, decision D3). |
| `snrRefHz` | integer | never | The reference bandwidth `snr` is quoted in, in Hz. Always `2500`. Not yet in the dispensa contract. |
| `wpm` | integer | not today | Keying speed in words per minute, rounded. |
| `decodeConfidence` | number | not today | Callsign confidence from 0 to 1 (SPEC-decode-core §4.6): the geometric mean of the per-character confidences times a repetition factor `1 - 0.5^r`, multiplied by 1.15 and capped at 1 when the call is in `master.scp`. |
| `decoderVersion` | string | not today | Build identity, `manta-<version>+<commit>`, for example `manta-0.1.0+d3bfa7c0c47a`. The part after `+` is the commit as 12 lowercase hex digits, `unknown` (no `.git`), or a build-time `MANTA_GIT_SHA` override: any SemVer build-metadata string (dot-separated `[0-9A-Za-z-]` identifiers, at most 64 characters) kept verbatim, except that a full 40- or 64-hex commit is shortened to 12 digits. So it can be, for example, `20261010123045` or `release.1`; don't validate it as a SHA. Compare the part before `+` across releases; the whole string names the exact binary. It is not decoder output (MAN-83). |
| `channelizerResolutionHz` | null | always | Reserved by the contract; manta does not fill it. |
| `seq` | integer | never | This record's position in the session named by `hello`: 1 for the first spot or `spotGrade`, +1 per record in send order. See [Transport](#transport). Optional in dispensa's `spots.v1`; manta always sends it. |

There is no `deCqZone` field. The contract has none, and manta does not send
one.

When `dxDxcc` is `-1`, `dxLat` and `dxLon` are normally `null` as well. The
exception is a `cty.dat` that has been replaced without regenerating
`dxcc.tsv`: the coordinates then resolve but the entity number does not. The
`manta_spots_unresolved_geography_total` counter counts every spot sent with
unresolved geography on either side, including `/MM` and `/AM`.

## `spotGrade` records

Only a node with `[rbn_compare]` enabled sends these; without it the stream
carries the hello and spots only. A
graded spot is followed, once its comparison window has closed
(`window_secs` after the spot plus up to 5 s; up to 75 s while the RBN
feed is silent), by one grade line on the same stream, raw TCP and
WebSocket alike. Telnet and the RBN uplink never carry grades.

Not every spot gets a grade. Spots refused because the comparator's pending
queue was full, or lost because it fell behind the spot bus, are counted in
`manta_rbn_compare_ungraded_total`. Spots still pending when the daemon
stops are never graded. Do not wait for a grade that may not come.

Tell the record shapes apart by the `kind` key. A spot has no `kind`
key; a grade always has `"kind":"spotGrade"`. The opening `hello` has
`"kind":"hello"`. A consumer that wants spots
only skips every object that has a `kind` key. A grade has no `dxCall`,
`frequency` or `timestamp` key, so a consumer that requires those keys
rejects it rather than mistaking it for a spot.

```json
{"kind":"spotGrade","seq":2,"spotId":"W3XYZ-1:1791719990000000000:12:173760000:W1AW","call":"W1AW","band":"20m","spotTime":1791721800,"windowClose":1791721980,"windowS":180,"grade":"disagree","disagreements":["frequency","speed"],"mantaFreqHz":14026400,"mantaWpm":30,"rbnModalFreqHz":14025000,"rbnMedianWpm":20.0,"rbnSkimmers":2,"rbnReports":2,"freqOffsetHz":1400,"rbnBands":["20m"],"spotterFilter":[]}
```

Every grade on the stream carries all 19 keys, in this order. Keys are never omitted; a
missing value is JSON `null`. The "same-band" values describe the RBN
reports of the call on the spot's band within the window, and are `null`
or `0` unless `grade` is `match` or `disagree`.

- `kind` (string, never null): always `"spotGrade"`.
- `seq` (integer, never null): this grade's own stream position, always larger than the graded spot's. Offline `manta grade` output omits it.
- `spotId` (string, never null): the graded spot's `id`.
- `call` (string, never null): the graded spot's `dxCall`.
- `band` (string, never null): the band of `mantaFreqHz`, labelled as the
  spot's `band`; `"unknown"` outside every band.
- `spotTime` (integer, never null): the spot's `timestamp`, Unix seconds.
- `windowClose` (integer, never null): `spotTime + windowS`, Unix seconds.
  It is not when the line was sent.
- `windowS` (integer, never null): the comparison window,
  `[rbn_compare].window_secs`, 30 to 3600.
- `grade` (string, never null): `match`, `disagree`, `elsewhere`, `none`
  or `unknown`. See
  [RUNBOOKS/rbn-comparison.md](RUNBOOKS/rbn-comparison.md#reading-a-grade)
  for what each means.
- `disagreements` (array of strings, never null): which tolerances failed,
  `frequency`, `speed` or both. Empty unless `grade` is `disagree`.
- `mantaFreqHz` (integer, never null): the spot's `frequency`, Hz.
- `mantaWpm` (integer or null): the spot's `wpm`.
- `rbnModalFreqHz` (integer or null): the most common same-band RBN
  frequency, Hz, at 100 Hz resolution.
- `rbnMedianWpm` (number or null): the median same-band RBN speed; also
  `null` when no same-band report carried a speed.
- `rbnSkimmers` (integer, never null): distinct RBN skimmers among the
  same-band reports.
- `rbnReports` (integer, never null): same-band RBN reports.
- `freqOffsetHz` (integer or null): `mantaFreqHz` minus `rbnModalFreqHz`.
- `rbnBands` (array of strings, never null): every band RBN reported the
  call on in the window, in band-table order, including other bands.
- `spotterFilter` (array of strings, never null): the
  `[rbn_compare].spotters` list the grade was made with; `[]` means every
  RBN skimmer.

`spots.v1` does not describe this record. The proposed dispensa
`spotGrade` schema fragment is in the MAN-301 decision record below.

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

- `manta decode --json` and `manta run --json` print a debugging format.
  That format is not stable and is not described here.
- The live-state stream on `[server].live_port` (band noise floor, each
  track's text and candidates, and every validation decision) is a
  separate listener with its own record kinds; this stream never carries
  them. See [LIVE-STATE.md](LIVE-STATE.md).

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
- [DECISIONS/2026-10-11-man138-station-location.md](DECISIONS/2026-10-11-man138-station-location.md)
  (MAN-138): `deGrid`/`deLat`/`deLon` from the configured station location.
- [`crates/manta-server/src/rbn_compare/grade.rs`](../crates/manta-server/src/rbn_compare/grade.rs)
  and
  [DECISIONS/2026-10-11-man301-live-rbn-comparison.md](DECISIONS/2026-10-11-man301-live-rbn-comparison.md)
  (MAN-301): the `SpotGrade` struct that builds every `spotGrade` field, the
  grading rules, and the proposed `spotGrade` contract fragment.
