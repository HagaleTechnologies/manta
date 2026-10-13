# MAN-324: a live-state stream for dashboards

**Status:** Implemented (branch `MAN-324`). Records why a node's live state
(band noise floor and activity, each track's copy and candidates, and every
validation decision) goes out on its own opt-in listener, what each record
holds, and the record rates it was sized against. The field reference is
[docs/LIVE-STATE.md](../LIVE-STATE.md).

## Context

Before this change a node published only finished output: validated spots
on telnet `:7300` and the JSON/WebSocket stream `:7301`, plus `spotGrade`
records when `[rbn_compare]` is on (MAN-301). A dashboard could not see what
the node hears between spots. The ticket asks for four things:

1. per band, the noise floor, how busy it is, and how many tracks are
   decoding;
2. per track, frequency, speed, SNR, the decoded text, and the candidate
   callsigns with their support;
3. why a candidate was not spotted;
4. a dashboard that stops reading is dropped, and decoding is unaffected.

**MAN-207 does not exist; this ticket builds its record.** The ticket says
it builds on MAN-207, a why-not-spotted record. `rg -n
"MAN-207|why.not.spotted|WhyNotSpotted"` over the checkout returns nothing,
and there is no trace of it in the planning notes or on GitHub. Every
rejection in `Validator::evaluate_candidate` returned a bare `None`. The
record is built here as an opt-in decision log on the validator (D4).

**MAN-118 D1 is not contradicted.** MAN-118 exports decode quality as
node-level `/metrics` families with no `track_id` label, because track ids
are never reused and a per-track series would stay in the scraper's TSDB
until retention expired
([MAN-118](2026-10-12-man118-decode-quality-metrics.md) D1). That reasoning
is about a pull endpoint whose samples are stored. This stream is push and
ephemeral: a record is gone once sent, and nothing indexes it by track. A
per-track record therefore costs no TSDB series. D1 still stands for
`/metrics`, and this stream adds no per-track metric.

## Decisions

- **D1 — its own listener, off by default.** `[server].live_port`, unset
  by default, starts a fourth listener; unset, the node behaves as before
  (no socket, no live metric families, no live work in the decode loop).
  MAN-301 put `spotGrade` records on `:7301` with a `kind` key. This stream
  cannot follow it: on V8w it carries about 290 times the spot rate (see
  "Measured volume"), which would land on every cqdx ingest connection and
  use up the spot stream's 1024-record lag allowance in under a second. The
  ticket's "subscribes to the live-state stream" implies opt-in, and
  `:7301` has no opt-in or subscription message. ARCHITECTURE §7's "thin
  fan-out consumers of one broadcast channel" still holds: the live stream
  is one more channel on `SpotBus`.
- **D2 — `live_bind_addr` defaults to `127.0.0.1`.** This is MAN-132's
  posture for an endpoint with no password
  ([MAN-132](2026-10-08-man132-metrics-loopback-bind.md)), and the stream
  carries every track's decoded text, which telnet and `:7301` never do.
  A dashboard on another machine needs `live_bind_addr = "0.0.0.0"`, a
  tunnel or a reverse proxy. `manta config check` prints a note when the
  address is a wildcard, and refuses a `live_port` that another listener
  uses on an overlapping address, by the existing duplicate-port rule.
- **D3 — the JSON stream's server code, a second channel.**
  `json_stream::serve` takes a `StreamKind` (`Spots` or `Live`). The live
  stream subscribes to its own `broadcast` channel
  (`LIVE_CHANNEL_CAPACITY = 4096`), has its own client cap
  (`MAX_LIVE_STREAM_CONNECTIONS = 32`), and its own per-IP connection
  quota and ping limiter built from `json_max_connections_per_ip` and
  `json_max_pings_per_ip`. Raw TCP and WebSocket detection, the handshake
  and write timeouts, lag-means-disconnect and the shutdown drain are the
  same code as `:7301`, which is what the ticket's "same fan-out rules as
  the JSON spot stream" asks for. There are no separate live per-IP keys:
  raising the `json_*` keys for a reverse proxy raises both listeners'
  quotas.
- **D4 — the why-not-spotted record lives in `manta-spot`.** `Validator`
  gains `record_decisions(on)` and `take_decisions()`. Off by default;
  every record site is behind `if let Some(log) = self.decisions.as_mut()`,
  so a caller that never turns it on pays one branch and no allocation.
  Each real decision point in `evaluate_candidate`, the power-step guard
  and the beacon path records a `Decision` with a `Reason`. The repetition
  requirement becomes the named constant `REQUIRED_REPS = 2`, and
  `better_supported_rival`'s winner is kept for the record instead of
  being collapsed with `.is_some()`. Two non-decisions record nothing: a
  re-attempt the reclassification guard stops, and a track with no
  `TrackMeta` yet. `candidates_for(track_id, now)` lists a track's entries
  in the support ledger, the words variant arbitration weighs.
  `recording_decisions_never_changes_spots` runs six golden scenarios with
  the log off and on and requires the same spots.
- **D5 — one `trackState` per track per decode batch in which it changed,
  no time throttle.** A track changes when it decodes a character or a word
  boundary (the boundary changes its candidates). Measured, a 0.5 s
  per-track throttle saves 15% on V8w (80.8 to 68.7 records/s) and 1 s
  saves 41%, at the cost of a dashboard's copy lagging the decoder. A
  throttle can be added later without changing the record.
- **D6 — `bandState` once per second of stream time.** The band tick runs
  every `fs` samples, so it is deterministic on a file and paced with
  `--realtime`. `occupancy` is "the detector's rise test passed on at least
  one hop this second": a throwaway probe found 0 to 2 of V1's 512 channels
  above the threshold on any single hop, because CW keying flickers. The
  rise flags are kept in `TrackManager` behind an enable flag that no
  decode logic reads (`occupancy_tracking_never_changes_events`).
  `noiseFloorDb` is the median of `FloorBank::effective_floor_db` over the
  band's channels, uncalibrated dB: a probe read -62.2 dB on both halves of
  V1's passband at 5, 30 and 60 s, and -61.8/-62.2 dB on V8w.
- **D7 — records are built only while someone listens.** The decode thread
  checks `broadcast::Sender::receiver_count() > 0` per batch and serializes
  each record once into an `Arc<str>` that every client shares (MAN-301's
  grade-line precedent). Per-track text, frequency, speed and SNR are kept
  whenever `live_port` is set, so a client that connects mid-track gets the
  full text in that track's next `trackState`. Decisions are recorded
  whenever `live_port` is set and discarded while no one listens.
  Publishing is a non-blocking `broadcast::Sender::send`, as `SpotBus::publish`
  already is from the same thread. `live_tap_never_changes_spots` requires
  the same spots on V1 and V7 with and without a tap that always wants
  records.
- **D8 — layering follows the crate graph.** `manta-server` does not depend
  on `manta-engine`. Engine-side types and the tap are in
  `manta-engine::live`; the wire structs and the band rollup are in
  `manta-server::live_state`; the mapping and serialization are in
  `manta-cli::live_state`, the wiring layer, as MAN-118's
  `decode_quality_of` is.
- **D9 — fixed bounds.** `text` is the newest 128 characters, `candidates`
  at most 8 (strongest first: more repetitions, then the larger confidence
  sum, then the call), the lag allowance 4096 records and the client cap
  32. All are constants, documented in LIVE-STATE.md, not config keys.
  They bound what one record and one slow client can cost. 4096 records is
  about 50 s of V8w and about 3 s of the contest estimate below; memory is
  at most about 1.2 MB of retained lines (4096 records of about 300 bytes)
  plus 128 bytes of text per open track.
- **D10 — the dispensa fragment is proposed here, not filed.** The JSON
  contract for the ecosystem lives in dispensa, which this repo cannot
  write to. The `liveState.v1` fragment below is ready to lift, as
  MAN-301's `spotGrade` fragment was (its §13).
- **D11 — separate live metric families.** `json_stream` records loss with
  the spot-loss counters (`manta_spots_dropped_lagged_total` and the
  rest). Live loss goes to `manta_live_records_dropped_total{reason}`, so
  those counters still mean spots. `manta_live_clients_connected` and
  `manta_live_records_total{kind}` complete the set, and
  `manta_listener_up` gains a `live` series. All of it is registered only
  when `live_port` is set, as `[rbn_compare]` gates its families.

## Measured volume

Measured before implementation with a throwaway example (deleted
afterwards) that ran `listen_with_observers` in release mode over the
testkit vectors and counted `DecoderEvent`s. The proxy for `trackState`
records is text-change events per track (`CharDecoded` and
`WordBoundary`), at each candidate cadence:

| Vector (120 s, 96 kS/s) | Spots | `CharDecoded` | Text-change records/s, per event | Per track, 0.25 s | 0.5 s | 1 s | Peak tracks with text in 1 s |
|---|---|---|---|---|---|---|---|
| V1 (1 signal) | 1 | 139 | 1.2 | 1.2 | 1.1 | 1.0 | 1 |
| V8 (50, AWGN) | 53 | 8,082 | 67.3 | 66.2 | 62.2 | 45.7 | 49 |
| V8w (50, Watterson poor) | 34 | 9,690 | 80.8 | 77.7 | 68.7 | 47.7 | 51 |

V8w carries about 81 `trackState` records/s against 34 spots in 120 s
(0.28/s), roughly 290 times the spot rate, or about 1.6 records/s per
decoding track. Scaled to the 886 concurrent tracks MAN-166 measured on a
contest recording, that is about 1,400 records/s. These numbers set D1
(a separate port), D5 (no throttle) and D9 (4096 records).

### Implemented tap rates (man324_live_rates)

`cargo run --release -p manta-engine --example man324_live_rates` runs
`listen_with_observers` over V1, V8 and V8w with a counting sink that
always wants records, and prints records/s per kind and the wall time with
and without the tap:

```
$ cargo run --release --locked -p manta-engine --example man324_live_rates
V1: 120 s, 1 spots; records/s bandState=0.99 trackState=1.16 candidateDecision=0.15 trackClosed=0.01; wall without tap 4.32 s, with tap 4.25 s (-1.6%)
V8: 120 s, 53 spots; records/s bandState=0.99 trackState=67.10 candidateDecision=7.54 trackClosed=0.42; wall without tap 6.05 s, with tap 6.81 s (+12.5%)
V8w: 120 s, 34 spots; records/s bandState=0.99 trackState=80.12 candidateDecision=3.22 trackClosed=0.43; wall without tap 10.22 s, with tap 9.66 s (-5.4%)
```

Measured 2026-10-13 in the implementing container, one run each, while
other builds shared the CPU, so the wall-time deltas (-5.4% to +12.5%)
are within run-to-run noise rather than a measured cost. The tap's
record rates are deterministic per build. `trackState` runs at about
80 records/s on V8w because a snapshot goes out whenever a track's text
changes (once per decoded character batch or word boundary), not once
per second; the counting sink does not serialize, so serialization cost
is not in these wall times.

## Proposed dispensa `liveState.v1` schema fragment

`spots.v1` is unchanged: the live stream is a separate listener, and spot
lines are byte-identical. Field names are camelCase, as in `spots.v1`. A
record whose `kind` the schema does not list is valid, so a consumer
validating against `liveState.v1` can skip kinds a later version adds.

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "liveState.v1.schema.json",
  "title": "liveState",
  "description": "One record of a manta node's live-state stream. Tell kinds apart by kind; skip kinds this version does not list.",
  "type": "object",
  "required": ["kind"],
  "properties": { "kind": { "type": "string" } },
  "allOf": [
    { "if": { "properties": { "kind": { "const": "bandState" } } }, "then": { "$ref": "#/$defs/bandState" } },
    { "if": { "properties": { "kind": { "const": "trackState" } } }, "then": { "$ref": "#/$defs/trackState" } },
    { "if": { "properties": { "kind": { "const": "candidateDecision" } } }, "then": { "$ref": "#/$defs/candidateDecision" } },
    { "if": { "properties": { "kind": { "const": "trackClosed" } } }, "then": { "$ref": "#/$defs/trackClosed" } }
  ],
  "$defs": {
    "bandState": {
      "description": "Once per second of stream time: each band the passband covers.",
      "type": "object",
      "additionalProperties": false,
      "required": ["kind", "deCall", "timestamp", "bands"],
      "properties": {
        "kind": { "const": "bandState" },
        "deCall": { "type": "string", "description": "The reporting station's callsign, uppercased." },
        "timestamp": { "type": "integer", "description": "Unix seconds, the spot stream's clock." },
        "bands": {
          "type": "array",
          "description": "Band-table order, unknown last.",
          "items": {
            "type": "object",
            "additionalProperties": false,
            "required": ["band", "loHz", "hiHz", "channels", "noiseFloorDb", "occupancy", "activeTracks"],
            "properties": {
              "band": { "type": "string", "description": "Band label, or \"unknown\" outside every allocation." },
              "loHz": { "type": "integer", "description": "Lowest calibrated channel centre, Hz." },
              "hiHz": { "type": "integer", "description": "Highest calibrated channel centre, Hz." },
              "channels": { "type": "integer", "minimum": 0, "description": "93.75 Hz channels in the band." },
              "noiseFloorDb": { "type": "number", "description": "Median per-channel effective noise floor, uncalibrated dB." },
              "occupancy": { "type": "number", "minimum": 0, "maximum": 1, "description": "Share of channels whose rise test passed on at least one hop in the last second." },
              "activeTracks": { "type": "integer", "minimum": 0, "description": "Tracks holding a decoder in this band." }
            }
          }
        }
      }
    },
    "trackState": {
      "description": "A track's running state, once per decode batch in which it decoded a character or a word boundary.",
      "type": "object",
      "additionalProperties": false,
      "required": ["kind", "deCall", "timestamp", "trackId", "frequency", "band", "wpm", "snr", "snrRefHz", "text", "candidates"],
      "properties": {
        "kind": { "const": "trackState" },
        "deCall": { "type": "string" },
        "timestamp": { "type": "integer", "description": "Unix seconds of the track's latest text event." },
        "trackId": { "type": "integer", "minimum": 0 },
        "frequency": { "type": ["integer", "null"], "description": "Calibrated Hz; null before the track's frequency is known." },
        "band": { "type": ["string", "null"], "description": "Band of frequency; null when frequency is." },
        "wpm": { "type": ["integer", "null"], "description": "Rounded; null before the first speed estimate." },
        "snr": { "type": ["integer", "null"], "description": "dB in snrRefHz; null before the first SNR update." },
        "snrRefHz": { "const": 2500 },
        "text": { "type": "string", "maxLength": 128, "description": "The newest decoded characters, one space per word boundary, prosigns dropped." },
        "candidates": {
          "type": "array",
          "maxItems": 8,
          "description": "Strongest first.",
          "items": {
            "type": "object",
            "additionalProperties": false,
            "required": ["call", "reps", "confSum"],
            "properties": {
              "call": { "type": "string" },
              "reps": { "type": "integer", "minimum": 1, "description": "Message-distinct repetitions on this track in the last 90 s." },
              "confSum": { "type": "number", "minimum": 0, "description": "Sum of per-repetition mean character confidence." }
            }
          }
        }
      }
    },
    "candidateDecision": {
      "description": "One validation decision about a candidate callsign.",
      "type": "object",
      "additionalProperties": false,
      "required": ["kind", "deCall", "timestamp", "trackId", "call", "frequency", "band", "spotType", "decision", "reason", "reps", "requiredReps", "rival", "spotId"],
      "properties": {
        "kind": { "const": "candidateDecision" },
        "deCall": { "type": "string" },
        "timestamp": { "type": "integer" },
        "trackId": { "type": "integer", "minimum": 0 },
        "call": { "type": "string" },
        "frequency": { "type": "integer", "description": "The validator's calibrated frequency, Hz." },
        "band": { "type": "string" },
        "spotType": { "enum": ["cq", "de", "beacon", "unknown"] },
        "decision": { "enum": ["spotted", "withheld", "deferred"] },
        "reason": {
          "enum": [
            "blocklist", "notch", "grammar", "cty", "repetition", "variant",
            "dedupe", "powerStepGuard", "beaconPending", "beaconSpeed",
            "beaconOverflow", "beaconDiscarded", null
          ],
          "description": "null exactly when decision is spotted."
        },
        "reps": { "type": ["integer", "null"], "minimum": 0, "description": "The repetition gate's count; null when the decision came before the gate ran." },
        "requiredReps": { "const": 2 },
        "rival": {
          "type": ["object", "null"],
          "description": "The winning variant when reason is variant, else null.",
          "additionalProperties": false,
          "required": ["call", "reps", "confSum"],
          "properties": {
            "call": { "type": "string" },
            "reps": { "type": "integer", "minimum": 1 },
            "confSum": { "type": "number", "minimum": 0 }
          }
        },
        "spotId": { "type": ["string", "null"], "description": "The spots.v1 id when decision is spotted, else null." }
      }
    },
    "trackClosed": {
      "description": "A track closed.",
      "type": "object",
      "additionalProperties": false,
      "required": ["kind", "deCall", "timestamp", "trackId", "closure", "survivorTrackId"],
      "properties": {
        "kind": { "const": "trackClosed" },
        "deCall": { "type": "string" },
        "timestamp": { "type": "integer" },
        "trackId": { "type": "integer", "minimum": 0 },
        "closure": { "enum": ["signalEnded", "bookkeeping"] },
        "survivorTrackId": { "type": ["integer", "null"], "minimum": 0, "description": "The track a bookkeeping closure merged into, else null." }
      }
    }
  }
}
```

**Filing.** Filing this fragment in dispensa is a follow-up outside this
repo; it has not been filed. Consumers build against
[docs/LIVE-STATE.md](../LIVE-STATE.md) meanwhile. MAN-136 is the
precedent: it shipped without waiting on its open dispensa question
(broad-review decision D10).

## Not done

- No subscription or filter message. The stream is push-only like `:7301`;
  a dashboard filters on its side.
- No change to `:7301`, telnet, the uplink or spot output, and no `kind`
  added to the spot stream.
- No history or replay on connect.
- No calibrated (dBm) noise floor; `noiseFloorDb` is documented as
  uncalibrated.
- No prosigns in `text`, matching `run`'s decoded-text lines (MAN-123).
- No `trackState` throttle and no configurable bounds (D5, D9).
- No per-track `/metrics` series (MAN-118 D1).
- No Dockerfile `EXPOSE`: the listener is off by default.
  [network-exposure](../RUNBOOKS/network-exposure.md) shows `-p
  127.0.0.1:7303:7303`.
- No SIGHUP reload of `live_port` or `live_bind_addr`; like every other
  `[server]` key they need a restart.

## Evidence and tests

- Decision record and candidates (`crates/manta-spot`):
  `a_first_decode_is_withheld_for_repetition_then_spotted_on_the_second`,
  `variant_withhold_names_the_better_supported_rival`, one test per other
  reason, `a_reclassification_reattempt_records_no_new_decision`,
  `candidates_for_lists_a_tracks_plausible_words_strongest_first`, and
  `recording_decisions_never_changes_spots` in `tests/golden_v11_v15.rs`.
- Tap (`crates/manta-engine`): `occupancy_tracking_never_changes_events`
  (`track.rs`); `live_tap_sends_v1_band_state_every_stream_second`,
  `live_tap_reports_w1aw_text_candidates_and_its_repetition_decision`,
  `live_tap_never_changes_spots` (`listen.rs`).
- Server (`crates/manta-server`): `a_lagging_live_subscriber_is_told_to_disconnect_after_4096`
  (`bus.rs`); `every_live_record_field_is_documented` (`live_state.rs`)
  holds LIVE-STATE.md's field tables to the serialized records both ways;
  `tests/live_state_acceptance.rs` covers raw TCP and WebSocket clients,
  the two streams staying apart, and a client that stops reading being
  dropped while publishing never blocks (scenario 4).
- End to end (`crates/manta-cli/tests/live_state_acceptance.rs`): a paced
  V1 replay with a live client sees `bandState`, W1AW's text and
  candidates, the `repetition` withhold, then a `spotted` decision whose
  `spotId` equals the `:7301` spot's `id`; a live client that never reads
  does not stop the spot.
