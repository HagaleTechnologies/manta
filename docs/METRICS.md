# manta metrics reference

The daemon (`manta run` with a `[server]` table) serves three read-only
HTTP endpoints on its metrics listener:

- `GET /metrics`: Prometheus text exposition format, version 0.0.4.
- `GET /healthz`: a plain-text liveness verdict.
- `GET /status`: a versioned JSON document. `manta status` reads it.

This page lists every metric family `/metrics` renders and describes both
other endpoints. The source of truth is `crates/manta-server/src/metrics.rs`
(families), `health.rs` (`/healthz`) and `status_doc.rs` (`/status`) in the
same crate. The bodies and sample lines below come from a local
`manta run --config` replay of the V1 test vector (`manta gen --out DIR v1`,
`[input]` `type = "file"`, `iq = true`).

## Scraping

The listener binds `[server].metrics_bind_addr` on `[server].metrics_port`.
The defaults are `127.0.0.1` and `7302`. The telnet and JSON listeners bind
`[server].bind_addr` (`0.0.0.0` by default), but the metrics listener does
not follow it: to scrape from another host, set `metrics_bind_addr`
explicitly. See
[MAN-132](DECISIONS/2026-10-08-man132-metrics-loopback-bind.md) for why.

The endpoint has no password and no TLS. Anyone who can reach the port can
read every value on this page. Restrict the port with a firewall or network
policy before you widen the bind address; see
[RUNBOOKS/network-exposure.md](RUNBOOKS/network-exposure.md).

Two per-source-IP limits apply to all three paths together:

- `metrics_max_connections_per_ip` caps concurrent connections from one
  address. The default is 8; `0` removes the per-IP cap, leaving only the
  listener's total of 64.
- `metrics_max_requests_per_ip` is a budget of 60 charges per address
  per fixed 60 s window by default. Each admitted connection is charged
  once. A connection that closes without sending a byte, times out
  reading its headers, or hits a read error (a reset, or a line that is
  over-long or not valid UTF-8) is charged but never refused. Every
  other connection counts as a request, valid HTTP or not, and is
  refused once over budget: it gets `429 Too Many Requests` with an
  empty body, whatever the path. So a TCP liveness probe or a broken
  client can spend the budget and earn the next request from that
  address a 429. A liveness probe polling `/healthz` from a scraper's
  address shares that scraper's budget. `0` disables this limit.

Both keys are under `[server]`; see [CONFIG.md](CONFIG.md#server).

A minimal Prometheus scrape config for a scraper on the same host:

```yaml
scrape_configs:
  - job_name: manta
    scrape_interval: 15s
    static_configs:
      - targets: ["127.0.0.1:7302"]
```

The default `metrics_path` (`/metrics`) is correct. Each request is served
on its own connection (`Connection: close`); a query string on the path is
ignored. Any other path or any method other than `GET` gets
`404 Not Found` with an empty body.

A Grafana dashboard and Prometheus alert rules built on these families ship
in `RUNBOOKS/monitoring/`; see
[RUNBOOKS/monitoring.md](RUNBOOKS/monitoring.md).

## Metric families

All 60 families, in the order `/metrics` renders them. Every family's
`# HELP` and `# TYPE` lines are always rendered, except two groups at the
end of the table: the ten RBN comparison families (MAN-301), rendered only
when `[rbn_compare]` is enabled, and the three live-state families
(MAN-324), rendered only when `[server].live_port` is set. "Present when" says when
the family also carries samples. Where it says a family has no samples, the
value is absent, not zero: the node does not measure it, so do not read the
missing series as "no loss" or "no traffic".

| Metric | Type | Labels | What it measures | Present when |
|---|---|---|---|---|
| `manta_spots_total` | counter | — | Total spots published to the broadcast bus. | Always; starts at 0. |
| `manta_spots_by_band_total` | counter | `band`, `type` | Spots published to the broadcast bus, by amateur band and spot type; sums to `manta_spots_total`. `band` is a band name such as `20m`, or `unknown` outside every band. `type` is `cq`, `de`, `beacon` or `unknown`. | One series per (band, type) pair after its first spot. No samples before the first spot. |
| `manta_spots_dropped_lagged_total` | counter | — | Spots dropped because a slow client fell behind and was disconnected. | Always; starts at 0. |
| `manta_spots_suppressed_by_filter_total` | counter | — | Spots suppressed by a client's own filter (for example `set dx filter unique`), not by a lag disconnect. | Always; starts at 0. |
| `manta_spots_dropped_write_failed_total` | counter | — | Spots dropped because a client's socket write timed out or failed before a lag or close error was observed. | Always; starts at 0. |
| `manta_spots_dropped_shutdown_total` | counter | — | Spots abandoned because the daemon shut down while a client was still in its pre-login or handshake phase, before any socket write timed out or failed. | Always; starts at 0. |
| `manta_spots_replay_abandoned_total` | counter | — | `sh/dx` history entries abandoned without being replayed, because the replay write failed or the daemon shut down mid-replay. These are re-sends of spots already counted in `manta_spots_total`, so they are counted apart from live-delivery loss. | Always; starts at 0. |
| `manta_spots_history_truncated_total` | counter | — | Spots evicted from `sh/dx` history at its 10,000-spot ceiling while still inside `[server].history_minutes`. Normal ageing does not increment it; a nonzero value means the ceiling has shortened the retained window. See [MAN-93](DECISIONS/2026-10-11-man93-sh-dx-history-window.md). | Always; starts at 0. |
| `manta_spots_unresolved_geography_total` | counter | — | Spots emitted with an `UNKNOWN_DXCC`, `UNKNOWN_CONTINENT` or `UNKNOWN_CQ_ZONE` sentinel on the dx or de side. The callsign did not resolve against `cty.dat`, or its entity has no row in the vendored `dxcc.tsv`, or it carries a `/MM` or `/AM` designator that places it outside any DXCC entity. | Always; starts at 0. |
| `manta_telnet_clients_connected` | gauge | — | Currently connected telnet clients. | Always. |
| `manta_json_clients_connected` | gauge | — | Currently connected JSON Lines clients. | Always. |
| `manta_ws_clients_connected` | gauge | — | Currently connected WebSocket clients. | Always. |
| `manta_active_tracks` | gauge | — | Currently active decoder tracks. Copied from the decode loop every 250 ms; set to 0 while the source is in an outage. | Always. |
| `manta_static_crashes_total` | counter | — | Static crashes detected: times at least three quarters of the passband's channels rose 3 dB or more together for 8 ms or more, counted once per crash. A sudden band-wide level step (a gain change, or the input returning from digital silence) also counts once. Counted with any `[detector].crash_filter` value, including `"off"`. Stays 0 when the passband holds fewer than 96 channels. See [SPEC §2.7](SPEC-decode-core.md#27-band-wide-static-crash-detection-man-312) and [MAN-312](DECISIONS/2026-10-13-man312-static-crash-filter.md). | Always; starts at 0. Summed across source reconnects. |
| `manta_rtty_signals` | gauge | `band` | RTTY (two-tone FSK) signals the detector currently marks, by amateur band. CW tracks are not started on a marked signal ([MAN-314](DECISIONS/2026-10-11-man314-rtty-recognition.md)). `band` is a band name such as `20m`, or `unknown` outside every band. Copied from the decode loop on every change; set to 0 while the source is in an outage and at shutdown. | One series per band after the first RTTY mark in that band; the series stays (at 0) after its signals clear. No samples before the first mark. |
| `manta_source_health` | gauge | `source` | Health of each input source: 1 healthy, 0 unhealthy. `source` is the input type: `file`, `audio`, `kiwi`, `soapy`, `hpsdr` or `ka9q`. | One series per registered source, from the first health report. A file replay reports 1 at startup. Reconnecting sources report on each connect and disconnect; HPSDR reads 0 until the radio is confirmed live, and ka9q-radio until the first RTP packet of its channel arrives. |
| `manta_source_outages_total` | counter | `source` | Times each input source went from healthy to unhealthy (a lost connection or failed read). A source that starts unhealthy has not had an outage. | Same `source` set as `manta_source_health`; starts at 0. |
| `manta_source_down_seconds_total` | counter | `source` | Seconds each input source spent in outages that have ended. An outage still in progress is added when it ends; until then it shows only as `manta_source_health` 0. | Same `source` set as `manta_source_health`; starts at 0. |
| `manta_input_dropped_packets_total` | counter | `source` | Input packets estimated lost in transit before reaching the decoder. | HPSDR, KiwiSDR and ka9q-radio sources only: from startup at 0, sampled every second, summed across reconnects. Other sources (file, audio, SoapySDR) show the `# TYPE` line but no samples: absent, not zero. |
| `manta_input_gaps_detected_total` | counter | `source` | Distinct input-stream gap events detected. One gap may account for several dropped packets. | HPSDR, KiwiSDR and ka9q-radio sources only, as for `manta_input_dropped_packets_total`. Other sources: absent, not zero. |
| `manta_input_malformed_packets_total` | counter | `source` | Input datagrams discarded as malformed: wrong length, or correct length but failed framing or sync validation (MAN-22). | HPSDR, KiwiSDR and ka9q-radio sources only, as for `manta_input_dropped_packets_total`. Other sources: absent, not zero. |
| `manta_uplink_sent_total` | counter | — | Spots forwarded to RBN uplink targets; the sum over every configured target. | Always, one series. Stays 0 with no `[[rbn_uplink]]` configured. |
| `manta_uplink_suppressed_total` | counter | — | Spots not sent to an RBN uplink target because of `dry_run` or its `spot_types` setting; the sum over every configured target. | Always, one series. Stays 0 with no `[[rbn_uplink]]` configured. |
| `manta_uplink_dropped_lagged_total` | counter | — | Spots the uplink fell behind on and lost before its next reconnect; the sum over every configured target. | Always, one series. Stays 0 with no `[[rbn_uplink]]` configured. |
| `manta_uplink_dropped_write_failed_total` | counter | — | Spots dropped because a write to an RBN uplink target's socket timed out or failed; the sum over every configured target. | Always, one series. Stays 0 with no `[[rbn_uplink]]` configured. |
| `manta_uplink_dropped_disconnected_total` | counter | — | Spots dropped when an RBN uplink connection was torn down for a reason other than a failed write (rate limit, protocol violation, stalled login, shutdown); the sum over every configured target. | Always, one series. Stays 0 with no `[[rbn_uplink]]` configured. |
| `manta_uplink_reconnects_total` | counter | — | Reconnect attempts by the RBN uplink: one per failed or dropped connection, including attempts that never connected (an unreachable target counts on every retry); the sum over every configured target. | Always, one series. Stays 0 with no `[[rbn_uplink]]` configured. |
| `manta_uplink_connected` | gauge | — | Count of configured RBN uplink targets currently connected. A raw instantaneous count: a flapping target can read as connected here. | Always, one series. 0 with no `[[rbn_uplink]]` configured. |
| `manta_uplink_target_enabled` | gauge | `target` | Whether this configured RBN uplink target is enabled (1) or configured but disabled (0). | One series per `[[rbn_uplink]]` entry, disabled entries included. `target` is `host:port`; the second and later exact duplicates get `#N` appended. No `[[rbn_uplink]]`: no samples. |
| `manta_uplink_target_connected` | gauge | `target` | Whether this RBN uplink target is currently connected (1) or not (0). | One series per `[[rbn_uplink]]` entry, as for `manta_uplink_target_enabled`. |
| `manta_uplink_target_sent_total` | counter | `target` | Spots forwarded to this RBN uplink target. | One series per `[[rbn_uplink]]` entry, starting at 0. |
| `manta_uplink_target_suppressed_total` | counter | `target` | Spots not sent to this RBN uplink target because of `dry_run` or its `spot_types` setting. | One series per `[[rbn_uplink]]` entry, starting at 0. |
| `manta_uplink_target_dropped_lagged_total` | counter | `target` | Spots this target fell behind on and lost before its next reconnect. | One series per `[[rbn_uplink]]` entry, starting at 0. |
| `manta_uplink_target_dropped_write_failed_total` | counter | `target` | Spots dropped because a write to this target's socket timed out or failed. | One series per `[[rbn_uplink]]` entry, starting at 0. |
| `manta_uplink_target_dropped_disconnected_total` | counter | `target` | Spots dropped when this target's connection was torn down for a reason other than a failed write. | One series per `[[rbn_uplink]]` entry, starting at 0. |
| `manta_uplink_target_reconnects_total` | counter | `target` | Reconnect attempts for this RBN uplink target: one per failed or dropped connection, including attempts that never connected. The `# HELP` text says "reestablished", but a never-reachable target counts on every retry. | One series per `[[rbn_uplink]]` entry, starting at 0. |
| `manta_uplink_target_recent_reconnects` | gauge | `target` | Reconnect attempts (failed or dropped connections, as counted by `manta_uplink_target_reconnects_total`) for this RBN uplink target within the last 300 s (the MAN-44 flapping window). 3 or more makes `/status` report the target `flapping`, so an unreachable target reads `flapping` rather than `down` once it has retried 3 times. | One series per `[[rbn_uplink]]` entry, starting at 0. |
| `manta_start_time_seconds` | gauge | — | Unix time at which this daemon's spot server and metrics endpoint started. A change means the daemon restarted. | Always. |
| `manta_uptime_seconds` | gauge | — | Seconds since this daemon's spot server and metrics endpoint started, from a monotonic clock. | Always. |
| `manta_build_info` | gauge | `version`, `git_sha`, `features` | Build metadata; the value is always 1. Labels carry the crate version, the git commit (`unknown` for a build without `.git`, such as the Docker image; a build-time `MANTA_GIT_SHA` override is used verbatim if it is SemVer build metadata, so it need not be a hex SHA — see `decoderVersion` in [SPOT-SCHEMA.md](SPOT-SCHEMA.md)) and the compiled-in optional features (`none` if there are none). | Always in the daemon, one series. |
| `manta_decode_latency_seconds` | histogram | `le` | Wall-clock time the decode thread spent processing one input chunk (channelize, track, decode, validate), excluding the wait for source data. To keep up with real time each observation must stay below that chunk's own duration, n samples / sample rate. A full chunk is 2048 samples, but KiwiSDR, HPSDR, ka9q-radio and the last read of a file can return shorter ones, so 2048 / sample rate is only the full-chunk upper bound, not a threshold every observation can be compared with. Buckets: 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5 s and `+Inf`, plus `_sum` and `_count`. | From startup: the daemon publishes an all-zero histogram just before the decode loop starts, then refreshes it every 250 ms. A scrape in the instant before that sees the `# TYPE` line only. |
| `manta_decode_chars_total` | counter | — | Characters decoded across every track (`CharDecoded` events). The denominator for the garble rate below. | From startup at 0: the daemon publishes it just before the decode loop starts, then refreshes it every 250 ms with `manta_decode_latency_seconds`. |
| `manta_decode_garbles_total` | counter | — | Characters the decoder could not match to any Morse character (every beam branch rejected the marks), across every track. Engines `legacy` and `edge-legacy` produce garbles; `hsmm` never does. Rare on a healthy node: the beam usually commits a wrong character instead (V8w: 3 garbles in 9,690 characters), so read a sudden rise as a fault. See [MAN-118](DECISIONS/2026-10-12-man118-decode-quality-metrics.md). | As `manta_decode_chars_total`. |
| `manta_track_rail_ratio` | histogram | `le` | Keying-rail ratio (mark rail / space rail amplitude) of each decoding track, sampled once per second per track at the same hop `TrackMeta` reports on. Deep, clean keying reads high (V1: median 22); fading or QRM eating the keying depth pulls it toward 2, the floor the rails collapse to (V5's CCIR-poor fading: median 3.3). `le="2"` counts samples at that floor. Buckets: 2, 2.5, 3, 4, 6, 10, 16, 25, 40, 63 and `+Inf`, plus `_sum` and `_count`. `legacy` engine only. No `track_id` label: each observation is one track's value. | As `manta_decode_chars_total`. Buckets stay at 0 until a track's rails initialise, and always under `[decode] engine = "edge-legacy"` or `"hsmm"`, which have no keying rails. |
| `manta_candidate_repetitions` | histogram | `le` | One observation per callsign candidate the repetition gate scored: how many message-distinct decodes of that call near that frequency it had seen within the trailing 90 s, this one included. A candidate needs 2, 3 or 5 to spot by its callsign-pattern tier (2 for every call at validation_level minimal), so the `le="1"` share is the share of candidates nobody repeated (a busted-call proxy). Buckets: 1, 2, 3, 4, 5, 6, 8, 10, 15, 20 and `+Inf`, plus `_sum` and `_count`. | As `manta_decode_chars_total`. |
| `manta_healthy` | gauge | — | Whether `/healthz` currently reports this node healthy (1) or not (0). Same evaluation as `/healthz`. | Always. |
| `manta_listener_up` | gauge | `listener` | Whether each listener is currently up (1) or down (0). `listener` is `telnet`, `json`, `metrics`, or `live` for the live-state stream. | One series per listener once the listeners have started; a listener whose task ends reads 0. The `live` series is registered only when `[server].live_port` is set. |
| `manta_rbn_compare_grades_total` | counter | `band`, `grade` | Spots graded against the RBN feed since start, by the spot's band and grade (`match`, `disagree`, `elsewhere`, `none`, `unknown`). | Only with `[rbn_compare]` enabled: the family's `# HELP`/`# TYPE` lines are absent otherwise. One series per (band, grade) pair after its first grade. |
| `manta_rbn_compare_grades_last_hour` | gauge | `band`, `grade` | Spots graded in the last hour, by band and grade. | Only with `[rbn_compare]` enabled, as for `manta_rbn_compare_grades_total`. |
| `manta_rbn_compare_disagreements_last_hour` | gauge | `band`, `kind` | Spots graded `disagree` in the last hour, by band and the tolerance that failed (`frequency` or `speed`); a spot failing both counts under each. | Only with `[rbn_compare]` enabled. |
| `manta_rbn_compare_freq_offset_median_hz` | gauge | `band` | Median of manta's frequency minus RBN's modal frequency, in Hz, over spots graded `match` or `disagree` in the last hour. | Only with `[rbn_compare]` enabled, per band with at least one such spot. |
| `manta_rbn_compare_freq_offset_samples` | gauge | `band` | Spots the median frequency offset is computed over. | As for `manta_rbn_compare_freq_offset_median_hz`. |
| `manta_rbn_compare_feed_connected` | gauge | — | Whether the RBN feed is logged in and delivering lines (1) or not (0). | Only with `[rbn_compare]` enabled. |
| `manta_rbn_compare_feed_reports_total` | counter | — | CW reports accepted from the RBN feed. | Only with `[rbn_compare]` enabled; starts at 0. |
| `manta_rbn_compare_feed_reconnects_total` | counter | — | RBN feed connections lost or failed. | Only with `[rbn_compare]` enabled; starts at 0. |
| `manta_rbn_compare_pending` | gauge | — | Spots not yet graded: waiting for their comparison window to close, or held past the close while the feed is silent. | Only with `[rbn_compare]` enabled. |
| `manta_rbn_compare_ungraded_total` | counter | — | Spots never graded: the pending queue was full, or the comparator lagged the spot bus. | Only with `[rbn_compare]` enabled; starts at 0. |
| `manta_live_clients_connected` | gauge | — | Live-state stream clients connected now, raw TCP and WebSocket together. Live clients are not counted in `manta_json_clients_connected` or `manta_ws_clients_connected`. See [LIVE-STATE.md](LIVE-STATE.md). | Only with `[server].live_port` set: the family's `# HELP`/`# TYPE` lines are absent otherwise. |
| `manta_live_records_total` | counter | `kind` | Live-state records published to at least one connected client, by `kind`: `bandState`, `trackState`, `candidateDecision` or `trackClosed`. Records are built only while a live client is connected, so nothing is counted while none is. | Only with `[server].live_port` set, as for `manta_live_clients_connected`. |
| `manta_live_records_dropped_total` | counter | `reason` | Live-state records a disconnected client never received, by `reason`: `lagged` (the client fell more than 4096 records behind), `write_failed` (a write, or the client's WebSocket handshake, timed out or failed) or `shutdown` (unsent when the daemon stopped). The spot-loss counters (`manta_spots_dropped_lagged_total`, `manta_spots_dropped_write_failed_total`, `manta_spots_dropped_shutdown_total`) never count live records, so they still mean spots. | Only with `[server].live_port` set, as for `manta_live_clients_connected`. |

### Sample

Lines from `/metrics` during the V1 replay. `# HELP` lines, most families
and eight of the histogram's bucket lines are omitted:

```text
# TYPE manta_spots_by_band_total counter
manta_spots_by_band_total{band="20m",type="cq"} 1
# TYPE manta_source_health gauge
manta_source_health{source="file"} 1
# TYPE manta_input_dropped_packets_total counter
# TYPE manta_uplink_target_connected gauge
# TYPE manta_build_info gauge
manta_build_info{version="0.1.0",git_sha="d3bfa7c0c47a",features="none"} 1
# TYPE manta_decode_latency_seconds histogram
manta_decode_latency_seconds_bucket{le="0.0005"} 1476
manta_decode_latency_seconds_bucket{le="0.001"} 4245
manta_decode_latency_seconds_bucket{le="0.0025"} 5066
manta_decode_latency_seconds_bucket{le="0.005"} 5071
manta_decode_latency_seconds_bucket{le="+Inf"} 5071
manta_decode_latency_seconds_sum 3.670230484
manta_decode_latency_seconds_count 5071
# TYPE manta_listener_up gauge
manta_listener_up{listener="json"} 1
manta_listener_up{listener="metrics"} 1
manta_listener_up{listener="telnet"} 1
```

The file source has no packet-loss model and the config has no
`[[rbn_uplink]]`, so the input and per-target families show only their
`# TYPE` lines.

## /healthz

`GET /healthz` returns `Content-Type: text/plain; charset=utf-8`:

- `200 OK` when the node is healthy. The first body line is `ok`.
- `503 Service Unavailable` otherwise. The first body line is `unhealthy`.

One line per check follows, in this order:

1. Sources, by name: `source <name>: healthy` or `source <name>: unhealthy`.
   If no source has reported yet, the single line is
   `source: none registered`, which fails the check.
2. Listeners, by name (`json`, `metrics`, `telnet`, and `live` when
   `[server].live_port` is set):
   `listener <name>: up` or `listener <name>: down`. If none has started,
   the line is `listener: none registered`, which fails the check.
3. The decode loop, one of:
   - `decode: ok`: the loop has processed a chunk within the last 10 s.
   - `decode: stalled <N>s`: no chunk for more than 10 s.
   - `decode: stopped`: the loop has exited and the daemon is draining for
     shutdown.
   - `decode: not monitored`: the watchdog was never armed. The daemon
     always arms it, so a running node does not show this line.

The node is healthy only when every line passes. RBN uplink state is not
part of the verdict; use the uplink families or `/status` for that.
`manta_healthy` and `manta_listener_up` come from the same evaluation.

Captured during the V1 replay:

```text
HTTP/1.1 200 OK
Content-Type: text/plain; charset=utf-8
Content-Length: 94
Connection: close

ok
source file: healthy
listener json: up
listener metrics: up
listener telnet: up
decode: ok
```

For probe setup (Kubernetes, systemd) and what to do about each failing
line, see [RUNBOOKS/node-health.md](RUNBOOKS/node-health.md).

## /status

`GET /status` returns `200 OK`, `Content-Type: application/json`, a
pretty-printed JSON object. `manta status` fetches and renders the same
document.

| Field | Meaning |
|---|---|
| `schema_version` | Document version, currently `1`. |
| `version` | Daemon crate version. |
| `uptime_seconds` | Whole seconds since the spot server and metrics endpoint started. |
| `spots_total` | Same value as `manta_spots_total`. |
| `telnet_clients`, `json_clients`, `ws_clients` | Same values as the three `*_clients_connected` gauges. |
| `active_tracks` | Same value as `manta_active_tracks`. |
| `uplink.health` | `ok` when every enabled target is `connected`, `degraded` when some are, `down` when none are, `disabled` when no target is enabled (including no `[[rbn_uplink]]` at all). |
| `uplink.connected_targets` | Targets whose `health` is `connected`. A flapping target is not counted. |
| `uplink.enabled_targets` | Enabled targets. |
| `uplink.sent_total`, `uplink.suppressed_total`, `uplink.reconnects_total` | Sums over all targets, matching the aggregate uplink counters. |
| `uplink.reconnect_window_seconds` | The flapping window, 300. |
| `uplink.flapping_threshold` | Recent reconnect attempts that mark a target flapping, 3. |
| `uplink.targets` | One object per `[[rbn_uplink]]` entry in config order, disabled entries included. |

Each `uplink.targets` object has `target` (the same `host:port` label as
the per-target metrics), `enabled`, `connected`, `sent`, `suppressed`,
`lagged`, `write_failed`, `disconnected`, `reconnects`, `recent_reconnects`
and `health`. `reconnects` and `recent_reconnects` count reconnect
attempts, failed ones included, not successful reconnections. A target's `health` is `disabled` if it is not enabled,
otherwise `flapping` if `recent_reconnects` is 3 or more, otherwise
`connected` or `down`. `flapping` takes precedence over `connected`, so
`/status` can report a target as flapping while
`manta_uplink_target_connected` reads 1.

Captured during the V1 replay (no `[[rbn_uplink]]` configured):

```text
HTTP/1.1 200 OK
Content-Type: application/json
Content-Length: 433
Connection: close

{
  "schema_version": 1,
  "version": "0.1.0",
  "uptime_seconds": 3,
  "spots_total": 1,
  "telnet_clients": 0,
  "json_clients": 0,
  "ws_clients": 0,
  "active_tracks": 1,
  "uplink": {
    "health": "disabled",
    "connected_targets": 0,
    "enabled_targets": 0,
    "sent_total": 0,
    "suppressed_total": 0,
    "reconnects_total": 0,
    "reconnect_window_seconds": 300,
    "flapping_threshold": 3,
    "targets": []
  }
}
```

For what each uplink state means and what to do about it, see
[RUNBOOKS/uplink-health.md](RUNBOOKS/uplink-health.md).

## Design records

- [MAN-128](DECISIONS/2026-10-05-man128-node-health-metrics.md): `/healthz`,
  build info, uptime, decode latency, per-target uplink families.
- [MAN-44](DECISIONS/2026-09-04-man44-uplink-status-surface.md): `/status`
  and the flapping classification.
- [MAN-56](DECISIONS/2026-09-04-man56-input-health-metrics.md): the input
  packet counters and why they are absent rather than zero.
- [MAN-96](DECISIONS/2026-10-07-man96-secondary-skimmer-field-node.md): the
  source outage counters.
- [MAN-132](DECISIONS/2026-10-08-man132-metrics-loopback-bind.md): the
  loopback default for `metrics_bind_addr`.
- [MAN-301](DECISIONS/2026-10-11-man301-live-rbn-comparison.md): the
  RBN comparison families.
- [MAN-324](DECISIONS/2026-10-12-man324-live-state-stream.md): the
  live-state families and the `live` listener.
- [MAN-129](DECISIONS/2026-10-12-man129-dashboard-and-alerts.md): the shipped
  Grafana dashboard and Prometheus alert rules.
