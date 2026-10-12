# 2026-10-12 — MAN-129: a shipped Grafana dashboard and Prometheus alert rules

**Status:** Implemented (branch `MAN-129`). The operator guide is
`docs/RUNBOOKS/monitoring.md`; the kit is `docs/RUNBOOKS/monitoring/`.

## Context

MAN-56, MAN-44, MAN-128, MAN-96 and MAN-132 built the `/metrics` families a
dashboard needs (`docs/METRICS.md`), but nothing in the repository showed an
operator how to look at them: no dashboard, no alert rules, no guide. The
ticket asks for a dashboard that renders spot rate, active tracks, source
health and uplink status with no further configuration, and alerts for a node
that stops producing spots for 30 minutes, loses its uplink, or goes down.

The kit:

| File | What it is |
|---|---|
| `manta-dashboard.json` | Grafana dashboard `manta node` (uid `manta-node`): 18 panels in six rows. |
| `manta-alerts.yml` | Prometheus rules MantaNodeDown, MantaNodeUnhealthy, MantaNoSpots, MantaUplinkDisconnected. |
| `manta-alerts.test.yml` | `promtool test rules` cases: each alert firing and quiet, and the dashboard's uplink-state formula against `GET /status`'s labels. |
| `prometheus.yml` | Example scrape job `manta` plus `rule_files:`. |

`crates/manta-cli/tests/monitoring_kit.rs` keeps the kit in step with the code.

## Decisions

| # | Decision | Why |
|---|---|---|
| D1 | The kit lives in `docs/RUNBOOKS/monitoring/`, not `packaging/`. | `packaging/` is the release-archive kit: files there need the owner-gated `scripts/package-release.py` `ASSETS` edit, and `packaging/README.md`'s relative links must resolve inside an extracted archive. MAN-96 set the docs-kit precedent. `packaging/README.md` links the runbook with an absolute URL instead. |
| D2 | No new dependency and no `Cargo.toml` edit. | `serde_json`, `serde_yaml_ng` and `regex` are already `manta-cli` dev-dependencies. |
| D3 | Four alerts: the ticket's three plus MantaNodeUnhealthy. Client lag is a panel, not an alert. | "Goes down" has two shapes: the process is gone (`up == 0`), or it answers scrapes but `/healthz` fails (`manta_healthy == 0`). Each alert sees one the other cannot. |
| D4 | MantaNoSpots is `(manta_spots_total - manta_spots_total offset 30m) == 0 and on(instance, job) manta_uptime_seconds >= 1800`, with no `for`. | `increase(…[30m]) == 0` false-fires on the few samples after a scrape gap; `offset 30m` finds no sample and stays quiet. Without the uptime term, a restarted process that reaches the count the old one had 30 minutes earlier fires it. With no `for`, it fires at the first evaluation after 30 silent minutes. A test ties `offset` to the uptime threshold. |
| D5 | MantaUplinkDisconnected fires on "enabled and not `/status`-connected", `for: 10m`. | The raw connected gauge reads 1 for a flapping target (`docs/RUNBOOKS/uplink-health.md`). The rule is the complement of `classify_uplink_health`'s `connected` branch, using `FLAPPING_RECONNECTS`. A burst of reconnects keeps `manta_uplink_target_recent_reconnects` at or above 3 for up to `RECONNECT_WINDOW` (300 s) after recovery, so `for` must exceed 300 s; a test asserts it. The gauge is used rather than `increase(manta_uplink_target_reconnects_total[5m])` because it is the daemon's own count over its own window, the number `/status` classifies from. |
| D6 | MantaNodeDown is `up{job="manta"} == 0`, `for: 2m`; the scrape job is `manta`. | Only Prometheus's `up` sees a dead process. The job name matches `docs/METRICS.md`'s scrape config and the example `prometheus.yml`, and a test pins them together. 2 minutes rides out a systemd restart (`RestartSec=10`). |
| D7 | `critical` for down and unhealthy; `warning` for no spots and the uplink. | A node that is down or not decoding needs action now. A quiet band or the experimental uplink (MAN-90) usually does not. |
| D8 | The dashboard picks its data source with a `datasource` template variable (`uid: "${datasource}"` everywhere) and has no `__inputs`. | It imports through the UI with no extra prompt and loads unchanged through file provisioning, which does not resolve `${DS_…}` placeholders. A test rejects `__inputs` and any fixed Prometheus uid. |
| D9 | State timelines use `color.mode: "fixed"` and set no `noValue`. Mapped stats use thresholds whose colour for each mapped value equals the mapping colour. | On Grafana 13.2.3 a state timeline in thresholds colour mode ignored value-mapping colours: **Uplink status** drew connected, flapping and disabled targets all green, and **Source health** drew an unhealthy HPSDR source green. Only fixed mode drew the mapping colours. A state timeline's `noValue` appeared as a legend entry beside real states. A test enforces both. |
| D10 | The **Node** variable is `label_values(manta_build_info, instance)`, multi-select, default All. | `manta_build_info` exists only in the daemon, so the list holds manta nodes whatever their job is called. |
| D11 | No recording rules. | The dashboard must work whether or not the operator loads the rules. |
| D12 | `schemaVersion: 39`; the runbook says Grafana 10.4 or newer. | 10.4 is the oldest version checked; newer versions migrate it on load. |
| D13 | The runbook's download URL points at `main`. | No release tag carries the kit yet. Every family it reads is on `main`, and the tests fail if one disappears. |
| D14 | The Rust suite runs promtool only when it is installed (`PROMTOOL` or `PATH`) and prints `skipping: …` otherwise. | CI has no promtool, and adding it means an owner-gated `.github/` edit. The structural tests always run. |
| D15 | `CLAUDE.md` is not edited. | It is owner-gated; `docs/README.md` and this index make the kit discoverable. |
| D16 | No panels for MAN-118 or MAN-316 families. | Their PRs are unmerged. |
| D17 | `uplink-health.md`'s stale "bound to `[server].bind_addr`" now says `[server].metrics_bind_addr` (`127.0.0.1`). | It contradicted `docs/METRICS.md` since MAN-132. |
| D18 | The Overview stat tiles are instant queries, and Spots / min and Active tracks draw no sparkline. | Validate killed a node and its Health tile still read green "Healthy", with Uptime, Spots / min and Active tracks frozen: a range query reduced to its last non-null value keeps a dead node's last sample for the whole 6-hour range. Gating on `up` would not change that, since `lastNotNull` still finds the sample from before the node died. An instant query returns nothing for a series Prometheus marked stale at a failed scrape. The **Spot rate** and **Active tracks** time series keep the history. A test enforces the instant queries. |

## What the prototype found

The plan prototyped every file against Prometheus 3.15.0, Grafana 13.2.3 and
10.4.19 and two live `manta run` daemons before it was written. Three defects
a plan written from the docs alone would have shipped:

1. **State-timeline colours (D9).** In Grafana 13.2.3's default thresholds
   colour mode, value-mapping colours were ignored and every state drew in the
   base threshold colour, with a legend of `< 80` and `80+`. Of six variants
   compared, only `color.mode = "fixed"` drew Connected green, Flapping orange
   and Disabled grey.
2. **`increase()` after a scrape gap (D4).** In a promtool case where a
   40-minute gap ends at minute 50, `increase(manta_spots_total[30m]) == 0`
   fired at minute 55. The `offset 30m` form stays quiet.
3. **Restart inside the window (D4).** Without the uptime term, the `node4`
   promtool case (restarted at minute 30, back at the old count by minute 39)
   fired at minute 39.

Each now has a regression test in `manta-alerts.test.yml` or
`monitoring_kit.rs`, and each was re-checked on the branch by mutation:
`< 3` → `< 4` in the uplink rule fails `promtool test rules` at 13m (and
`uplink_alert_is_the_complement_of_status_connected`); dropping the uptime term
fires MantaNoSpots for `node4` at 39m; the `increase()` form fires early at
39m and falsely at 55m; Source health in thresholds mode fails
`mapped_panels_colour_each_state_one_way`; `manta_spots_totl` fails
`dashboard_queries_only_rendered_metrics_for_the_selected_nodes`; and an
`__inputs` key fails `dashboard_imports_without_placeholders`.

## Verification

On the committed files, branch `MAN-129`:

- `promtool` 3.15.0: `check rules manta-alerts.yml` prints
  `SUCCESS: 4 rules found`; `test rules manta-alerts.test.yml` prints
  `SUCCESS`; `check config prometheus.yml` prints `SUCCESS: 1 rule files found`.
- `PROMTOOL=…/promtool cargo test -p manta-cli --locked --test monitoring_kit`:
  18 passed, the promtool test running rather than skipping.

Live run, 2026-10-12 19:42–20:18 UTC, release build of `f6a41cc` plus this
branch's kit files. Node 1 replays V1 in a loop with three uplink targets: a
fake collector that accepts the login (connected), a closed port, and
`enabled = false`. Nodes 2 and 3 are HPSDR sources at a TEST-NET address, so
they stay unhealthy and silent. Prometheus 3.15.0 scraped all three every 5 s
with job `manta` and loaded the committed `manta-alerts.yml`.

**Dashboard.** The committed JSON went in through `POST /api/dashboards/import`
with `inputs: []` on Grafana 13.2.3 and on 10.4.19, and both returned
`"imported": true`, uid `manta-node`. Every panel's queries went through
`POST /api/ds/query`. All 18 panels returned series, except that **Input
packets lost** had none for the file source, which is correct because that
family is absent there. Screenshots on both versions showed all six rows
rendering:

- **Health**: node 1 green "Healthy", nodes 2 and 3 red "Unhealthy".
- **Source health**: `file` green, both `hpsdr` sources red.
- **Uplink status**: Connected green (value 3), Flapping orange (2) and
  Disabled grey (0) for node 1's three targets. `GET /status` said
  `connected`, `flapping` and `disabled` for the same targets.
- **Decode latency p99**: about 2.5 ms on node 1.
- **Spot rate by band**: `20m`.

**Alerts**, from `GET /api/v1/alerts` and the `ALERTS` series:

| Alert | Condition | Active | Firing |
|---|---|---|---|
| MantaNodeUnhealthy | node 3, HPSDR with no radio | 19:43:18 | 19:45:22 |
| MantaNodeUnhealthy | node 2 after its restart | 19:47:33 | 19:49:37 |
| MantaNodeDown | node 2 stopped with `kill` at 19:45:01 | 19:45:03 | 19:47:07, resolved when node 2 restarted at 19:47:33 |
| MantaUplinkDisconnected | node 1's closed-port target (`/status`: `flapping`) | 19:43:03 | 19:53:07 |
| MantaNoSpots | node 3, 0 spots since it started at about 19:43:19 | — (no `for`) | 20:13:18, uptime 1801 s |
| MantaNoSpots | node 2, 0 spots since its 19:47:33 restart | — (no `for`) | 20:17:33 |

Nothing fired for node 1's connected or disabled targets. MantaNoSpots never
fired for node 1, whose count rose from 0 to 3.

## Follow-ups

1. Ship `docs/RUNBOOKS/monitoring/*` in release archives
   (`scripts/package-release.py` `ASSETS`; owner-gated), then make
   `packaging/README.md`'s link relative.
2. Install promtool in CI so `promtool_checks_and_tests_the_kit_when_installed`
   runs on every PR (`.github/workflows/ci-full.yml`; owner-gated).
3. Add decode-quality panels when MAN-118 merges, and reference-list freshness
   panels when MAN-316 merges.
4. Optionally publish the dashboard to grafana.com (needs the owner's consent).
