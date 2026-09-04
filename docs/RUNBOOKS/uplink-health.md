# Reading RBN uplink health with `manta status`

MAN-44. For a daemon started with `--server-config`, run:

```console
$ manta status --server-config /etc/manta/manta.toml
manta status — daemon up 3h 12m

  spots published      18432
  telnet clients       2      json/ws clients  1
  active tracks  n/a

RBN uplink: DEGRADED — 1 of 2 enabled targets connected

  TARGET                         STATE       SENT    SUPPR   RECONN RECENT(5m)
  telnet.reversebeacon.net:7000  connected  18001        0        0          0
  backup.example.net:7000        flapping       0        0       37         14

$ echo $?
1
```

(`SUPPR` and `RECONN` are the suppressed and lifetime-reconnects counts,
abbreviated so the table stays inside 80 columns even with a long RBN
hostname.)

Or against an explicit address (e.g. from another host, or when you don't
have the daemon's config file to hand):

```sh
manta status --addr 10.0.0.5:7302
manta status --addr 10.0.0.5:7302 --json | jq .uplink
```

`--server-config`'s `bind_addr` is translated for you: a wildcard
(`0.0.0.0`/`::`) means "the daemon listens everywhere," which isn't itself
something `manta status` can dial, so it falls back to loopback. If the
daemon runs on a different host than the one you're checking from, use
`--addr <that host>:<metrics_port>` instead.

## Reading the output

- **Overall verdict** (`OK` / `DEGRADED` / `DOWN` / `DISABLED`): `OK` only
  when every *enabled* configured target is connected. `DISABLED` means no
  `[[rbn_uplink]]` table is enabled at all (including none configured) —
  that's a normal, healthy state for a node that doesn't forward to RBN,
  not an error.
- **Per-target `STATE`**:
  - `connected` — up, and not reconnecting more than the flapping
    threshold within the last 5 minutes.
  - `flapping` — reconnecting repeatedly (3 or more times in the last 5
    minutes, `manta_uplink_target_recent_reconnects`/`RECENT(5m)`
    column), **even if it happens to be connected at the instant you
    check**. A target stuck in a reconnect loop cycles through brief
    connected windows; `flapping` exists specifically so that doesn't
    read as healthy.
  - `down` — not connected, and not (yet) flapping by the above
    threshold. A target that has *never* connected still appears here,
    not silently omitted — "configured and stuck" must be distinguishable
    from "not configured at all."
  - `disabled` — `enabled = false` in that target's `[[rbn_uplink]]`
    entry. Still listed, so an operator can see it's intentionally off.
- **`RECENT(5m)`** is the reconnect count within a rolling 5-minute
  window (`RECONNECT_WINDOW`), not since daemon start — `RECONN` (the
  table's lifetime-reconnects column) is the cumulative count since the
  daemon started. 3 or more recent reconnects
  (`FLAPPING_RECONNECTS`) is what flips a target to `flapping`; this is
  sized against the uplink's own backoff cap (`uplink::MAX_BACKOFF` =
  60s), so a target that's simply, persistently down produces roughly 5
  reconnects per window (comfortably over the threshold) while one
  transient blip does not trip it.

## Exit codes (scripting)

| Code | Meaning |
| --- | --- |
| `0` | Every enabled target connected, or no uplink configured at all. |
| `1` | Reached the daemon; the uplink is unhealthy (`DEGRADED`/`DOWN`). |
| `2` | Could not reach or parse the daemon's status — check the daemon is running and the address/port. |

A cron/Nagios-style one-liner:

```sh
manta status --server-config /etc/manta/manta.toml >/dev/null || echo "uplink check failed (exit $?)"
```

## Also available: `GET /status` and Prometheus

`manta status` is a thin client over the same JSON document served on
`GET /status` by the metrics listener (default port 7302) —
`curl -s http://localhost:7302/status | jq` gets you the same data for
scripting or an existing monitoring stack. Per-target series are also on
`GET /metrics` (`manta_uplink_target_connected`, `_sent_total`,
`_suppressed_total`, `_reconnects_total`, `_recent_reconnects`, labeled
`target="host:port"`) for anyone who already scrapes Prometheus.

Both endpoints share `/metrics`'s exposure posture — unauthenticated,
bound to `[server].bind_addr` (`0.0.0.0` by default) — see
`docs/RUNBOOKS/network-exposure.md` if you need to restrict access.
Design rationale: `docs/DECISIONS/2026-09-04-man44-uplink-status-surface.md`.
