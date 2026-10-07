# Node health runbook (MAN-128)

How to tell whether a live manta node is healthy from its metrics endpoint
alone — the question MAN-128 exists to answer. Covers `/healthz` as a
liveness probe, the new `/metrics` families, and the known caveats.

Both endpoints are served on the metrics listener (`[server].metrics_port`,
default bind `[server].bind_addr`) — see `docs/RUNBOOKS/network-exposure.md`
for exposure posture; `/healthz` carries exactly the same posture as
`/metrics` today, since it's the same listener.

## `/healthz` as a liveness probe

`GET /healthz` returns:

- `200 OK`, body `ok\n` followed by one `<check>: <status>` line per check,
  while every registered source is healthy, every registered listener
  (telnet/JSON/metrics) is up, and the decode loop has made progress within
  the last 10 seconds.
- `503 Service Unavailable`, body `unhealthy\n` plus the same per-check
  lines, otherwise — e.g. `source file: unhealthy`, `listener json: down`,
  `decode: stalled 12s`, or `decode: stopped` (the daemon has exited the
  decode loop and is draining shutdown).

**The RBN uplink is deliberately not part of this verdict.** A dead uplink
target does not make `/healthz` unhealthy — restarting a node that is
decoding fine just because an upstream RBN collector is unreachable would
make things worse, not better. Watch uplink health via the per-target
`/metrics` families below instead.

### Kubernetes

```yaml
livenessProbe:
  httpGet:
    path: /healthz
    port: 7302 # metrics_port
  initialDelaySeconds: 10 # the decode loop's own ~2s calibration plus startup
  periodSeconds: 10
  failureThreshold: 3
```

### systemd

```ini
[Service]
ExecStartPost=/usr/bin/curl -f -s --retry 5 --retry-delay 2 \
  http://127.0.0.1:7302/healthz
```

(systemd has no native HTTP health-check unit type; pair this with a
`Restart=on-failure` watchdog script polling the same endpoint on an
interval if you need an ongoing liveness check, not just a post-start one.)

## `/metrics` families this ticket added

- `manta_spots_by_band_total{band,type}` — spot counts broken out by
  amateur band and spot type (`cq`/`de`/`beacon`/`unknown`). Sums to the
  pre-existing, still-unlabeled `manta_spots_total`.
  ```promql
  sum by (band) (rate(manta_spots_by_band_total[5m]))
  ```
- `manta_decode_latency_seconds` — a real histogram of per-chunk decode
  wall-clock time (channelize + track + decode + validate, excluding the
  source-read wait). Keeping up with real time requires the p99 to stay
  below the chunk's real-time duration, `2048 / fs` seconds:
  ```promql
  histogram_quantile(0.99, rate(manta_decode_latency_seconds_bucket[5m]))
  ```
  Its `_count` rate is also the node's decode throughput in chunks/second.
- `manta_start_time_seconds` / `manta_uptime_seconds` / `manta_build_info`
  — process start time, uptime, and build metadata. Detect restarts:
  ```promql
  changes(manta_start_time_seconds[1h]) > 0
  ```
  `manta_build_info{git_sha="unknown"}` means the binary was built from a
  context with no `.git` directory (e.g. a Docker build — `.dockerignore`
  excludes `.git` on purpose) rather than a build failure.
- `manta_uplink_target_*{target="host:port"}` — every uplink counter now
  has a per-target series (`enabled`, `connected`, `sent_total`,
  `suppressed_total`, `dropped_lagged_total`, `dropped_write_failed_total`,
  `dropped_disconnected_total`, `reconnects_total`), alongside the
  unchanged aggregate series (now derived sums). Find a stuck target:
  ```promql
  manta_uplink_target_connected == 0 and manta_uplink_target_enabled == 1
  ```
- `manta_healthy` / `manta_listener_up{listener}` — the exact same
  evaluation `/healthz` reports, for a Prometheus-only operator with no
  separate probe.
- `manta_input_*{source="kiwi"}` — KiwiSDR sources now also publish the
  packet-loss/malformed-frame counters HPSDR already had (MAN-56 generalized
  beyond HPSDR). Under normal conditions the gap/malformed counters should
  stay at 0; a steadily increasing `manta_input_gaps_detected_total` means
  the receiver's network path is dropping SND frames.
- `manta_source_outages_total{source}` / `manta_source_down_seconds_total{source}`
  (MAN-96) — how many times each input source went from healthy to unhealthy
  (a lost connection or failed read), and the seconds it spent in outages
  that have ended. Both start at 0 for every registered source; a source that
  starts unhealthy (HPSDR until confirmed live) is not an outage. They catch
  drops shorter than a scrape interval, which `manta_source_health` alone
  misses. Source drops per day:
  ```promql
  increase(manta_source_outages_total[1d])
  ```
  An outage still in progress shows only as `manta_source_health == 0`; its
  seconds are added to `manta_source_down_seconds_total` when it ends. The
  30-day field-node ledger reads both
  (`docs/RUNBOOKS/secondary-skimmer-field-node.md`).

## Known gaps

- Signal-to-spot (track onset → first spot) latency is not yet a metric —
  tracked as a follow-up to this ticket.
- SoapySDR does not yet publish `manta_input_*` counters (no stream-overflow
  wiring) — tracked as a follow-up; untestable in an environment without
  `libsoapysdr-dev`.
- Kiwi seq-gap counting is unit-tested against the documented frame layout
  and upstream `kiwiclient` semantics, not yet live-verified against a real
  receiver — tracked as a follow-up for the next live-hardware session (see
  `wiki/pages/live-hardware-field-testing.md`).
