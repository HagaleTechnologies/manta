# Dashboards and alerts (MAN-129)

manta ships a Grafana dashboard and a set of Prometheus alert rules for a
node's `/metrics` endpoint. Import the dashboard and load the rules, and you
get panels for spot rate, active tracks, source health and uplink status, plus
alerts for a node that is down, unhealthy, silent for 30 minutes, or cut off
from its RBN uplink. Nothing needs building by hand.

The kit is in [`monitoring/`](monitoring/):

| File | What it is |
|---|---|
| [`manta-dashboard.json`](monitoring/manta-dashboard.json) | Grafana dashboard **manta node**: 18 panels in six rows, including **Spot rate**, **Active tracks**, **Source health** and **Uplink status**. |
| [`manta-alerts.yml`](monitoring/manta-alerts.yml) | Prometheus alert rules: [MantaNodeDown](#mantanodedown), [MantaNodeUnhealthy](#mantanodeunhealthy), [MantaNoSpots](#mantanospots) and [MantaUplinkDisconnected](#mantauplinkdisconnected). |
| [`manta-alerts.test.yml`](monitoring/manta-alerts.test.yml) | `promtool test rules` cases: when each alert fires and when it stays quiet, and a check that the dashboard's uplink states match `GET /status`. |
| [`prometheus.yml`](monitoring/prometheus.yml) | Example Prometheus config: the `manta` scrape job and the `rule_files:` entry. |

Every metric the kit reads is described in [METRICS.md](../METRICS.md).

## Before you start

- Prometheus 2.53 or newer. The rules and their tests pass on promtool 2.53.5
  and 3.15.0.
- Grafana 10.4 or newer. The dashboard was imported and checked on 10.4.19 and
  13.2.3.
- Prometheus must reach the node's `/metrics`. If Prometheus runs on the node,
  nothing changes: the metrics listener binds `127.0.0.1:7302` by default. If
  it runs elsewhere, set `[server].metrics_bind_addr` and restrict the port
  first; [network-exposure.md](network-exposure.md) explains how.
- A 15 s scrape interval costs 4 requests a minute from the scraper's address,
  out of the listener's default budget of 60
  (`[server].metrics_max_requests_per_ip`).

## Get the files

In a source checkout they are in `docs/RUNBOOKS/monitoring/`. Release archives
do not carry them yet. Without a checkout, download them:

```sh
for f in manta-dashboard.json manta-alerts.yml manta-alerts.test.yml prometheus.yml; do curl -fsSLO "https://raw.githubusercontent.com/HagaleTechnologies/manta/main/docs/RUNBOOKS/monitoring/$f"; done
```

## Prometheus

Copy `manta-alerts.yml` next to your `prometheus.yml`. Then either start from
the example `prometheus.yml` or merge its `rule_files:` entry and its `manta`
scrape job into yours.

Keep the scrape job named `manta`. [MantaNodeDown](#mantanodedown) matches
`up{job="manta"}`; if your job has another name, edit that rule to match.

Check the files, then reload Prometheus (`kill -HUP <pid>`,
`systemctl reload prometheus`, or a restart):

```console
$ promtool check config prometheus.yml
Checking prometheus.yml
  SUCCESS: 1 rule files found
 SUCCESS: prometheus.yml is valid prometheus config file syntax

Checking manta-alerts.yml
  SUCCESS: 4 rules found

$ promtool test rules manta-alerts.test.yml
  SUCCESS
```

promtool 2.53 prints `Unit Testing:  manta-alerts.test.yml` before
`  SUCCESS`. Run `promtool test rules` from the directory that holds both
files, since the test file names `manta-alerts.yml` by relative path.

## Grafana

Go to *Dashboards → New → Import*, upload `manta-dashboard.json` and click
*Import*. The import screen asks only for Grafana's usual name and folder: the
file has no `__inputs`, so there is no data source to pick. The dashboard opens
with **Data source** set to your default Prometheus data source and **Node**
set to *All*.

To provision it instead, put the file in the `path:` of a dashboards provider.
It carries no `${DS_…}` import placeholders, so it loads unchanged.

The **Node** list is built from `manta_build_info`. If it is empty, Prometheus
is not scraping any manta node: check the scrape job and the bind address.

What each row shows:

- **Overview**: **Health** (`manta_healthy`, the same verdict as
  `GET /healthz`), **Uptime**, **Restarts** in the selected range,
  **Spots / min**, **Active tracks** and **Version** (`manta_build_info`).
  Except Restarts, these tiles show each node's value now, not the last one
  in the range. A node Prometheus can no longer scrape drops off them at its
  first failed scrape, or after 5 minutes if Prometheus recorded no stale
  marker for it (a Prometheus restart during the outage, say). Spots / min
  keeps it until its rate window, which widens with the time range, has
  passed. [MantaNodeDown](#mantanodedown) fires after 2 minutes down.
- **Spots**: **Spot rate** per node from `manta_spots_total`, and
  **Spot rate by band** from `manta_spots_by_band_total`.
- **Decoder**: **Active tracks** over time (`manta_active_tracks`), and
  **Decode latency p99** from `manta_decode_latency_seconds`. The p99 must stay
  below one chunk's duration (2048 samples / sample rate) to keep up with
  real time.
- **Input source**: **Source health** per source (`manta_source_health`,
  Healthy green or Down red), **Source outages** per 5 minutes
  (`manta_source_outages_total`) and **Input packets lost** per 5 minutes
  (`manta_input_dropped_packets_total`).
- **RBN uplink**: **Uplink status** per target, **Uplink spots sent / min**
  (`manta_uplink_target_sent_total`) and **Uplink reconnects (5 min)**
  (`manta_uplink_target_recent_reconnects`, with a line at 3).
- **Clients**: **Connected clients** (telnet, JSON Lines, WebSocket) and
  **Spots dropped for clients / min** (`manta_spots_dropped_lagged_total`,
  `manta_spots_dropped_write_failed_total`).

**Uplink status** shows the verdict `manta status` and `GET /status` give for
each `[[rbn_uplink]]` target: Connected (green), Flapping (orange), Down (red)
or Disabled (grey). It is computed from `manta_uplink_target_enabled`,
`manta_uplink_target_connected` and `manta_uplink_target_recent_reconnects`
with the daemon's own rule: 3 or more reconnect attempts in 5 minutes is
Flapping, even while connected. The connected gauge on its own would show a
flapping target as healthy ([uplink-health.md](uplink-health.md)).

When a panel has nothing to show, it says why:

| Panel | Text | Meaning |
|---|---|---|
| Health | `No data: is Prometheus scraping this node?` | No current `manta_healthy` sample for the selected nodes: Prometheus is not scraping them, or their last scrape failed. |
| Spot rate by band | `No spots yet` | A band has a series only after its first spot. |
| Input packets lost | `Not measured for this source` | Only HPSDR and KiwiSDR sources count lost packets. Other sources have no series, not a zero. |
| Uplink spots sent / min, Uplink reconnects (5 min) | `No [[rbn_uplink]] configured` | The node has no uplink target. |

Every other panel shows Grafana's own `No data`.

## Alerts

Each alert's `runbook_url` points at its section below. `manta doctor` on the
node checks the whole setup and names the failing part.

### MantaNodeDown

**Fires when** Prometheus has failed to scrape the node for 2 minutes
(`up{job="manta"} == 0`). Severity `critical`.

**Means** the daemon is not running, or its metrics port is unreachable from
Prometheus. A dead process serves no metrics, so only Prometheus's own `up`
series can see this. The 2 minutes ride out a service restart.

**Check** that the service is running (`systemctl status manta`, or the
container's state), that it is bound where Prometheus looks (the banner's
`metrics=` address), and that no firewall sits between them.

### MantaNodeUnhealthy

**Fires when** `manta_healthy` has been 0 for 2 minutes. Severity `critical`.

**Means** the node answers scrapes but `GET /healthz` fails: an input source is
unhealthy, a listener is down, or the decode loop has made no progress for
10 seconds. A dead RBN uplink does not count; see
[MantaUplinkDisconnected](#mantauplinkdisconnected).

**Check** the per-check lines of `curl -s http://127.0.0.1:7302/healthz` on the
node. [node-health.md](node-health.md) explains each line.

### MantaNoSpots

**Fires when** `manta_spots_total` is where it was 30 minutes ago and the
process has run for at least 30 minutes. Severity `warning`.

**Means** the node has published no spots for 30 minutes. A band that has
closed for the night fires it legitimately; so does a dead antenna or a source
delivering silence. The alert waits 30 minutes after a restart, and 30 minutes
after a scrape gap, before it can fire.

**Check** the **Source health** and **Active tracks** panels: a healthy source
with no tracks means nothing is being heard. `manta check` reports the
source's input power and noise floor.

### MantaUplinkDisconnected

**Fires when** an enabled `[[rbn_uplink]]` target has not been connected, as
`GET /status` defines it, for 10 minutes: it is down, or it is flapping (3 or
more reconnect attempts in the last 5 minutes, even while connected).
Severity `warning`.

**Means** spots are not reaching that collector. The alert waits 10 minutes,
longer than the 5-minute reconnect window, so a collector restart that
recovers after a short burst does not fire it. A disabled target never fires.

**Check** `manta status` on the node. [uplink-health.md](uplink-health.md)
explains each target state and what to do about it. The direct uplink is
experimental (MAN-90).

## Changing thresholds

Edit your copy of `manta-alerts.yml`, add or adjust a case in
`manta-alerts.test.yml`, and run `promtool test rules manta-alerts.test.yml`.

For example, to keep a node that is parked on a band that closes at night out
of MantaNoSpots, add `{instance!="night-node:7302"}` to both selectors:

```yaml
expr: |
  (manta_spots_total{instance!="night-node:7302"} - manta_spots_total{instance!="night-node:7302"} offset 30m) == 0
  and on(instance, job) manta_uptime_seconds >= 1800
```

To change the 30 minutes, change the `offset` and the uptime term together:
the uptime term keeps a restart inside the window from comparing two
processes' counts. The tests in `crates/manta-cli/tests/monitoring_kit.rs`
pin the shipped values; your own copy is yours to change.

## Notifications

Prometheus evaluates the rules. Sending a page or an email is the job of your
Alertmanager, configured in the `alerting:` block of `prometheus.yml`; see
[Prometheus's alerting documentation](https://prometheus.io/docs/alerting/latest/overview/).
No Alertmanager configuration ships.

## Design record

[MAN-129](../DECISIONS/2026-10-12-man129-dashboard-and-alerts.md): why the
alerts and panels are defined the way they are, and how they were checked.
