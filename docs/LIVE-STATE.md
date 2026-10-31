# manta live-state stream

`manta run` with a `[server]` table and `[server].live_port` set publishes
what the node is hearing between spots on its own listener: each band's
noise floor and activity, each track's running copy and the callsigns the
validator is weighing, why each candidate was or was not spotted, and when
a track closes (MAN-324). It is meant for dashboards. Spots stay on telnet
`:7300` and the JSON spot stream `:7301`
([SPOT-SCHEMA.md](SPOT-SCHEMA.md)); neither carries these records.

There are four record kinds:

| Kind | Sent |
|---|---|
| `bandState` | Once per second of stream time. |
| `trackState` | Once per decode batch in which a track decoded a character or a word boundary. |
| `candidateDecision` | Every time validation decides about a candidate callsign. |
| `trackClosed` | When a track closes. |

Why the stream has its own port, its rates and its bounds are recorded in
[DECISIONS/2026-10-12-man324-live-state-stream.md](DECISIONS/2026-10-12-man324-live-state-stream.md).

## Turning it on

The listener is off unless `[server].live_port` is set:

```toml
[server]
station_callsign = "W1AW"
live_port = 7303                 # unset (the default) runs no live-state listener
#live_bind_addr = "127.0.0.1"    # the default; "0.0.0.0" lets another machine connect
```

- With `live_port` unset, `run` opens no fourth socket, `/metrics` renders
  none of the `manta_live_*` families, and the decode loop does no live
  work.
- `live_bind_addr` defaults to `127.0.0.1`, so only this machine can
  connect. The stream has no password and carries every track's decoded
  text. For a dashboard on another machine, set `live_bind_addr =
  "0.0.0.0"` (or one interface's address) and firewall `live_port` to the
  dashboards that read it. `manta config check` prints a note when
  `live_bind_addr` listens on every interface. See
  [RUNBOOKS/network-exposure.md](RUNBOOKS/network-exposure.md).
- `MANTA_SERVER_LIVE_PORT` and `MANTA_SERVER_LIVE_BIND_ADDR` override the
  file, like every other `[server]` key.
- Both keys need a restart; SIGHUP does not reload them.
- When `live_port` is set, `manta config check` adds `live_port=` and
  `live_bind_addr=` to its `server:` line and refuses a `live_port` that
  another listener already uses on an overlapping address. `manta doctor`
  checks that the address and port can bind. `run`'s startup line ends in
  `live=<address>:<port>`.

Both keys are described in [CONFIG.md](CONFIG.md#server).

## Transport

The live listener runs the JSON spot stream's own server code
([`crates/manta-server/src/json_stream.rs`](../crates/manta-server/src/json_stream.rs))
on its own broadcast channel, so the rules are those of `:7301`
([SPOT-SCHEMA.md "Transport"](SPOT-SCHEMA.md#transport)) with a smaller
client limit and a larger lag allowance:

- Raw TCP and WebSocket share one listener on `live_port`. The server tells
  them apart by peeking at the first bytes a client sends.
- A raw TCP client gets one JSON object per line (JSON Lines), each ending in
  `\n`. It should send nothing. A client that sends nothing for 500 ms is
  classified as raw TCP. Any bytes it sends later close the connection.
- A client whose first bytes are an HTTP `GET` WebSocket upgrade request gets
  one WebSocket text frame per record. It must complete the handshake within
  10 s. After that it may send Ping and Close frames only. Text, Binary and
  Pong frames close the connection, as does any inbound frame over 16 KiB.
- The server only pushes. There is no request, subscription or filter
  message: a dashboard filters on its side. There is no history either: a
  client receives the records published after it connects. Every
  `trackState` carries the track's whole current `text` (up to 128
  characters), so a client that connects mid-track gets the copy so far at
  the track's next character or word boundary.
- Slow clients are disconnected, never back-pressured. A client is
  disconnected when a single write takes longer than 10 s, or when it falls
  more than 4096 records behind the live channel. Records a disconnected
  client never received are counted in `manta_live_records_dropped_total`,
  not in the spot-loss counters. Publishing never blocks the decode loop,
  and spot output is the same with or without live clients.
- Connection limits: at most 32 concurrent clients in total. The per-IP
  quota is `[server].json_max_connections_per_ip` (default 16, `0` means no
  per-IP cap), counted apart from the spot stream's: connections to
  `json_port` do not use up an address's live quota, nor the other way
  round. A connection over the per-IP quota is closed at once. A
  connection over the 32-client total cap waits, unserved, until a slot
  frees; further connections wait in the operating system's backlog.
- Ping limits: a WebSocket client may send at most 10 Pings per 60 s,
  counted both per connection and per source IP. The per-IP budget is
  `[server].json_max_pings_per_ip`, again counted apart from the spot
  stream's. A client over either budget is disconnected.
- Records are built and serialized on the decode thread only while at least
  one live client is connected.

## Common rules

- Every record is one JSON object. `kind` is its first key and tells the
  four kinds apart.
- Every record carries all of its kind's keys, in the order the tables
  below list them. Keys are never omitted; a missing value is JSON `null`.
- `timestamp` is Unix seconds (UTC, whole seconds) on the same clock as a
  spot's `timestamp`: the session start time plus the stream offset. For a
  replayed file the start time is the file's modification time, or
  `[input].replay_epoch` when set. Stream time, not the wall clock, drives
  every record, so replaying a file gives the same timestamps each time;
  with `--realtime` the records are paced like live input.
- Frequencies are integer Hz, rounded, with `[input].freq_correction_ppm`
  applied. They are absolute RF frequencies only when the source has an RF
  reference; an audio input reports audio offsets.
- `band` values are the spot stream's labels (`160m`, `20m` and so on), or
  `"unknown"` outside every allocation.
- A `trackId` is never reused within one `run`, across source reconnects
  included.

## `bandState` records

Once per second of stream time, one record covering the whole passband.
With several `[[input]]` entries (MAN-13), one record per input per stream
second, each covering that input's passband and naming it in `source`:

```json
{"kind":"bandState","deCall":"W1AW","timestamp":1700000005,"bands":[{"band":"20m","loHz":14000000,"hiHz":14047906,"channels":512,"noiseFloorDb":-62.2,"occupancy":0.0059,"activeTracks":1},{"band":"unknown","loHz":13952000,"hiHz":13999906,"channels":512,"noiseFloorDb":-62.2,"occupancy":0.0,"activeTracks":0}]}
```

| Field | JSON type | Null? | Meaning |
|---|---|---|---|
| `kind` | string | never | Always `"bandState"`. |
| `deCall` | string | never | `[server].station_callsign`, uppercased, as in a spot's `deCall`. |
| `source` | string | absent with one input | The `[[input]]` `name` whose passband the record covers. Present only with several inputs. |
| `timestamp` | integer | never | The stream second this record describes. |
| `bands` | array | never | One entry per band the passband covers, in band-table order (lowest band first), with the `unknown` entry last. Bands the passband does not reach are absent. Empty only if the passband has no channels. |

Each `bands[]` entry:

| Field | JSON type | Null? | Meaning |
|---|---|---|---|
| `band` | string | never | The band label of the entry's channels, from each channel's calibrated centre frequency. `"unknown"` gathers every channel outside an allocation; those channels need not be contiguous (an IQ passband that straddles a band edge), and an audio input with no RF reference is one `unknown` entry. |
| `loHz` | integer | never | The lowest calibrated channel centre in the entry, Hz. |
| `hiHz` | integer | never | The highest calibrated channel centre in the entry, Hz. |
| `channels` | integer | never | Number of 93.75 Hz channelizer channels in the entry. |
| `noiseFloorDb` | number | never | Median, over the entry's channels, of the detector's effective noise floor at the tick: one 93.75 Hz channel's power (squared channelizer magnitude) in dB, rounded to 0.1 dB. **Not calibrated to dBm.** Comparable across bands and over time on one node, not across nodes, nor across a change of receiver gain. |
| `occupancy` | number | never | Share, 0 to 1, of the entry's channels where the detector's rise test (smoothed power at least `[detector].on_snr_db` above the effective floor) passed on at least one hop during the last second. Rounded to 4 decimals. CW keying flickers, so an instantaneous reading would understate a busy band. |
| `activeTracks` | integer | never | Tracks holding a decoder whose calibrated frequency is in this band: the count `manta_active_tracks` reports, split by band. |

Rows cover the source's trusted band on a decimated capture (`capture_rate_hz`):
the decimator's transition band beyond its <= 1 dB boundary is not usable, so
those channels are neither reported nor tracked. The record shape does not
change — `channels` and `loHz`/`hiHz` simply narrow (MAN-196).

## `trackState` records

One per track per decode batch in which the track decoded a character or a
word boundary, in `trackId` order within a batch. A word boundary updates
the candidates even though `text` gains its space only with the next
character. A track that is closing gets a last `trackState` before its
`trackClosed` if it changed since the previous one.

```json
{"kind":"trackState","deCall":"W1AW","timestamp":1700000020,"trackId":1,"frequency":14012362,"band":"20m","wpm":20,"snr":21,"snrRefHz":2500,"text":"CQ CQ DE W1AW W1AW","candidates":[{"call":"W1AW","reps":2,"confSum":1.1}]}
```

| Field | JSON type | Null? | Meaning |
|---|---|---|---|
| `kind` | string | never | Always `"trackState"`. |
| `deCall` | string | never | As in `bandState`. |
| `timestamp` | integer | never | Stream time of the track's latest character or word boundary in this batch. |
| `trackId` | integer | never | The track's number: the same number `run`'s decoded-text lines show, and the `trackId` of its `candidateDecision` and `trackClosed` records. |
| `frequency` | integer or null | until known | The track's calibrated frequency, Hz: set when the track is promoted, then updated about once a second. `null` only before either has happened. |
| `band` | string or null | when `frequency` is | The band of `frequency`. |
| `wpm` | integer or null | until known | Keying speed in words per minute, rounded, from the decoder's latest speed estimate. `null` before the first one. |
| `snr` | integer or null | until known | Signal-to-noise ratio in dB, rounded, in a 2500 Hz reference bandwidth (see `snrRefHz`), updated about once a second. `null` before the first update, up to about 1 s after the track is promoted. |
| `snrRefHz` | integer | never | Always `2500`, as on the spot stream. Telnet quotes spots in 500 Hz, about 7 dB higher. |
| `text` | string | never | The newest 128 characters (at most) of the track's decoded copy, uppercase, with one space at each word boundary and no leading or trailing space. Prosigns are left out, as in `run`'s decoded-text lines. |
| `candidates` | array | never | The callsign-shaped words decoded on this track that the validator is weighing, strongest first, at most 8. Empty when there are none. |

Each `candidates[]` entry. These are the words the validator's variant
arbitration compares: words decoded on this track that have a valid
callsign shape and a `cty.dat` prefix, with at least one repetition in the
last 90 s. Strongest first means more `reps`, then the larger `confSum`,
then the call in ascending order.

| Field | JSON type | Null? | Meaning |
|---|---|---|---|
| `call` | string | never | The candidate callsign. |
| `reps` | integer | never | Message-distinct repetitions of this word on this track within the last 90 s. A spot needs its pattern tier's requirement (see `requiredReps` on `candidateDecision`), but this count is per track, while the spot decision's count can include nearby tracks. |
| `confSum` | number | never | The sum, over those repetitions, of each repetition's mean character confidence (geometric mean, 0 to 1). Breaks ties between candidates with equal `reps`. |

## `candidateDecision` records

One per decision validation makes about a candidate callsign. Validation
runs at a track's word boundaries, once more at the track's first SNR
update, and for deferred beacons when the track ends. The possible
`decision`/`reason` pairs are under
[Decisions and reasons](#decisions-and-reasons). A decision is sent after
the same batch's `trackState` records, so the state naming its candidate
always arrives first.

```json
{"kind":"candidateDecision","deCall":"W1AW","timestamp":1700000044,"trackId":9,"call":"W4KTNL","frequency":14031200,"band":"20m","spotType":"de","decision":"withheld","reason":"repetition","reps":2,"requiredReps":5,"patternTier":"unlisted","rival":null,"spotId":null}
```

| Field | JSON type | Null? | Meaning |
|---|---|---|---|
| `kind` | string | never | Always `"candidateDecision"`. |
| `deCall` | string | never | As in `bandState`. |
| `timestamp` | integer | never | Stream time of the decode the decision was made on. For a spotted decision it equals the spot's `timestamp`. |
| `trackId` | integer | never | The track the candidate was decoded on. |
| `call` | string | never | The candidate callsign. On a spotted decision it is the spot's `dxCall`. |
| `frequency` | integer | never | The validator's calibrated frequency for the candidate, Hz. On a spotted decision it is the spot's `frequency`. |
| `band` | string | never | The band of `frequency`. |
| `spotType` | string | never | `cq`, `de`, `beacon` or `unknown`: the same labels as `manta_spots_by_band_total`'s `type`. |
| `decision` | string | never | `spotted`, `withheld` or `deferred`. |
| `reason` | string or null | when spotted | Why the candidate was withheld or deferred; `null` when `decision` is `spotted`. One of the fourteen strings below. |
| `reps` | integer or null | when the gate did not run | The repetition gate's count for this decode: message-distinct decodes of the call near this frequency within the last 90 s, this one included, which can span nearby tracks. `null` when the decision came before the gate ran: reasons `blocklist`, `notch`, `grammar`, `cty`, `powerStepGuard`, `beaconPending`, `beaconSpeed`, `beaconOverflow` and `beaconDiscarded`. Set for `lookupPending` and `lookupNotFound`, which come after the gate. |
| `requiredReps` | integer | never | Repetitions this candidate needs to spot: 1 for an allowlisted call or beacon; otherwise 2, 3 or 5 by pattern tier at `normal`, or 2 at `minimal`. |
| `patternTier` | string or null | for an allowlisted call or beacon | `"common"`, `"listed"` or `"unlisted"`: the candidate's callsign-pattern tier. |
| `rival` | object or null | unless variant | When `reason` is `variant`, the better-supported variant that wins on this track, as `{"call", "reps", "confSum"}`: its callsign, its message-distinct repetitions on this track within the last 90 s, and its `confSum`. `null` for every other reason. |
| `spotId` | string or null | unless spotted | When `decision` is `spotted`, the `id` of the spot published on the `:7301` stream, so a dashboard can join the two streams. `null` otherwise. |
| `confSum` | number | never | Only inside `rival`: the rival's sum of per-repetition mean character confidence, as a `trackState` candidate's `confSum`. Variant arbitration compares `reps` first and `confSum` on a tie. |

Each decoded word is decided once. It is decided again only when a newer
word changes its spot type (for example `K5ARH` followed by `UP` makes it
a `de` candidate); a re-evaluation that would change nothing produces no
record. A track with no SNR update yet is not evaluated, and produces no
record until its first update.

## `trackClosed` records

```json
{"kind":"trackClosed","deCall":"W1AW","timestamp":1700000120,"trackId":1,"closure":"signalEnded","survivorTrackId":null}
```

Sent after the track's last `trackState` and after any decision its
closure produced (deferred beacons are judged when their track ends). A
dashboard can drop the track's row on this record. A `lookupPending`
candidate's later decision can follow its track's closure.

| Field | JSON type | Null? | Meaning |
|---|---|---|---|
| `kind` | string | never | Always `"trackClosed"`. |
| `deCall` | string | never | As in `bandState`. |
| `timestamp` | integer | never | Stream time of the newest decoder event when the track closed. |
| `trackId` | integer | never | The track that closed. |
| `closure` | string | never | `signalEnded`: the signal ended (a sustained power drop, no character for the silence timeout, or the end of the input). `bookkeeping`: the track stopped being tracked with no evidence the signal ended: it merged into another track, was evicted, or was closed by a source outage (a reconnect, or a stop while the source was down). |
| `survivorTrackId` | integer or null | unless merged | For a `bookkeeping` closure that merged into another track, that track's `trackId`; the signal continues there. `null` otherwise. |

## Decisions and reasons

`decision` and `reason` take exactly these values:

| `decision` | `reason` | Meaning |
|---|---|---|
| `spotted` | `null` | Published as a spot. `spotId` names it. |
| `withheld` | `"blocklist"` | The call is on the `[spot]` blocklist. |
| `withheld` | `"notch"` | The frequency is inside a `[spot]` notch. |
| `withheld` | `"grammar"` | Not a valid callsign shape. |
| `withheld` | `"cty"` | No `cty.dat` prefix allocates the call. |
| `withheld` | `"repetition"` | Fewer than `requiredReps` repetitions so far. **Not final**: a later repetition produces a new decision, `spotted` if nothing else withholds it. |
| `withheld` | `"variant"` | A better-supported variant on the same track wins; `rival` names it. |
| `withheld` | `"dedupe"` | The same call was spotted recently on this frequency. |
| `withheld` | `"powerStepGuard"` | A CQ/DE framing candidate burned by the power-step guard (MAN-105). |
| `deferred` | `"beaconPending"` | A beacon candidate, judged when its track ends. A later decision settles it (see below). |
| `withheld` | `"beaconSpeed"` | A deferred beacon rejected when its track ended: its speed was never confirmed, or is above 45 WPM. |
| `withheld` | `"beaconOverflow"` | A deferred beacon evicted, oldest first, because a ninth was deferred on its track (at most 8 wait at once). |
| `withheld` | `"beaconDiscarded"` | A deferred beacon dropped because its track closed for bookkeeping with no survivor. |
| `withheld` | `"noEvidence"` | No character of the candidate carried finite positive decoder confidence, so no public emission regardless of database membership (MAN-106). |
| `deferred` | `"lookupPending"` | `[callsign_lookup]` is on and QRZ.com has not answered for this call yet. A later decision settles it: `spotted`, or `withheld`/`lookupNotFound`. That later decision keeps the decode's `timestamp` and can arrive after the track's `trackClosed`. |
| `withheld` | `"lookupNotFound"` | QRZ.com does not list the call (a fresh or a cached answer). |

The fifteen `reason` strings are `blocklist`, `notch`, `grammar`, `cty`,
`repetition`, `variant`, `dedupe`, `powerStepGuard`, `beaconPending`,
`beaconSpeed`, `beaconOverflow`, `beaconDiscarded`, `noEvidence`,
`lookupPending` and `lookupNotFound`.

With `[callsign_lookup]` on (MAN-318), `spotted` still means published: a
candidate held for its lookup first gets `deferred`/`lookupPending`, and its
`spotted` record is sent only when the spot is published.

A beacon candidate first gets `deferred`/`beaconPending` (or a `blocklist`,
`notch`, `grammar` or `cty` withhold). Its later decision is `spotted`, or
`withheld` for `beaconSpeed`, `dedupe`, `beaconOverflow` or
`beaconDiscarded`, or for `blocklist` or `notch` when a SIGHUP reload has
changed those lists since. When its track merges into another, the pending
beacon moves to the surviving track, and its later decision carries that
track's `trackId`.

## Example session

A paced V1 replay (`manta run --config c.toml --realtime` with
`replay_epoch = 1700000000`, `station_callsign = "W1AW"` and `live_port =
7303`). The spot's `id`, `frequency`, `snr` and `wpm` are those of the
example spot in [SPOT-SCHEMA.md](SPOT-SCHEMA.md#example), and
`noiseFloorDb` is the measured median floor on V1 at 5 s. The other
timestamps, `occupancy` and `confSum` are illustrative.

```
$ nc 127.0.0.1 7303
{"kind":"bandState","deCall":"W1AW","timestamp":1700000005,"bands":[{"band":"20m","loHz":14000000,"hiHz":14047906,"channels":512,"noiseFloorDb":-62.2,"occupancy":0.0059,"activeTracks":1},{"band":"unknown","loHz":13952000,"hiHz":13999906,"channels":512,"noiseFloorDb":-62.2,"occupancy":0.0,"activeTracks":0}]}
{"kind":"trackState","deCall":"W1AW","timestamp":1700000013,"trackId":1,"frequency":14012362,"band":"20m","wpm":20,"snr":21,"snrRefHz":2500,"text":"CQ CQ DE W1AW","candidates":[{"call":"W1AW","reps":1,"confSum":0.55}]}
{"kind":"candidateDecision","deCall":"W1AW","timestamp":1700000013,"trackId":1,"call":"W1AW","frequency":14012362,"band":"20m","spotType":"cq","decision":"withheld","reason":"repetition","reps":1,"requiredReps":2,"patternTier":"common","rival":null,"spotId":null}
{"kind":"trackState","deCall":"W1AW","timestamp":1700000020,"trackId":1,"frequency":14012362,"band":"20m","wpm":20,"snr":21,"snrRefHz":2500,"text":"CQ CQ DE W1AW W1AW","candidates":[{"call":"W1AW","reps":2,"confSum":1.1}]}
{"kind":"candidateDecision","deCall":"W1AW","timestamp":1700000020,"trackId":1,"call":"W1AW","frequency":14012362,"band":"20m","spotType":"cq","decision":"spotted","reason":null,"reps":2,"requiredReps":2,"patternTier":"common","rival":null,"spotId":"W1AW:12534119743695398054:1:2006272:W1AW"}
{"kind":"trackClosed","deCall":"W1AW","timestamp":1700000120,"trackId":1,"closure":"signalEnded","survivorTrackId":null}
```

A `bandState` line arrives every second in between; they are left out
above. The variant withhold in the `candidateDecision` section uses the
case from `crates/manta-spot/src/support.rs`'s tests: W4KTNL, 2 repetitions
with a confidence sum of 0.885, loses to W4KCL, 2 repetitions with 1.016.

## Contract and stability

- Consumers must skip records whose `kind` they do not recognise. Later
  tickets may add kinds.
- The stream is not decoder output. It never changes which spots manta
  produces, and the decoder-output rule in [CHANGELOG.md](../CHANGELOG.md)
  does not apply to it.
- There is no cross-repo contract for it yet. A proposed dispensa
  `liveState.v1` JSON Schema fragment is in the
  [decision record](DECISIONS/2026-10-12-man324-live-state-stream.md#proposed-dispensa-livestatev1-schema-fragment);
  it has not been filed in dispensa. Until it is, this page is the
  reference.
- The bounds (128 characters of `text`, 8 `candidates`, the 4096-record
  lag allowance and the 32-client limit) are constants, not config keys.

## Metrics

With `live_port` set, `/metrics` adds `manta_live_clients_connected`,
`manta_live_records_total{kind}`, `manta_live_records_dropped_total{reason}`
and a `live` series of `manta_listener_up`; `/healthz` then also requires
the live listener up. See [METRICS.md](METRICS.md).

## Where the values come from

- [`crates/manta-spot/src/decision.rs`](../crates/manta-spot/src/decision.rs):
  the decision, its verdict and reason, and the candidate's `required_reps`
  and `pattern_tier`. Thresholds come from
  [`crates/manta-spot/src/pattern.rs`](../crates/manta-spot/src/pattern.rs).
- [`crates/manta-spot/src/support.rs`](../crates/manta-spot/src/support.rs):
  candidate support (`reps`, `confSum`) and variant arbitration.
- [`crates/manta-engine/src/live.rs`](../crates/manta-engine/src/live.rs):
  the decode loop's tap, which keeps each track's text, speed, SNR and
  frequency, and sets the band tick and the text and candidate bounds.
- `crates/manta-server/src/live_state.rs`: the band rollup and the four
  wire records. `crates/manta-cli/src/live_state.rs`: the mapping from the
  engine's records to the wire records, and the `spotId`.
- [DECISIONS/2026-10-12-man324-live-state-stream.md](DECISIONS/2026-10-12-man324-live-state-stream.md)
  (MAN-324): why a separate port, the measured record rates, the bounds,
  and the proposed `liveState.v1` schema fragment.
