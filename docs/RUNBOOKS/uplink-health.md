# Reading RBN uplink health with `manta status`

MAN-44. For a daemon started with `--config`, run:

```console
$ manta status --config /etc/manta/manta.toml
manta status — daemon up 3h 12m

  spots published      18432
  telnet clients       2      json/ws clients  1
  active tracks  37

RBN uplink: DEGRADED — 1 of 2 enabled targets connected

  TARGET                         STATE       SENT    SUPPR   RECONN RECENT(5m)
  collector-01.example.net:7000  connected  18001      431        0          0
  backup.example.net:7000        flapping       0        0       37         14

$ echo $?
1
```

(`SUPPR` and `RECONN` are the suppressed and lifetime-reconnects counts,
abbreviated so the table stays inside 80 columns even with a long RBN
hostname. `SUPPR` counts spots the target received but did not send: every
spot while `dry_run` is on, and otherwise each DE or untyped spot its
`spot_types` setting holds back (MAN-91). It is therefore non-zero on a
healthy live uplink too.)

Spots skipped by the target's `max_spots_per_minute` ceiling (MAN-248) are
NOT in `SUPPR`; they show only as `manta_uplink_ceiling_suppressed_total`
(and its per-target form) on `/metrics`. That counter staying at 0 forever
is the healthy state. A sustained rise has one of two causes:

- **Genuine traffic, such as a contest, is above the ceiling.** The
  ceiling is too low and is dropping real spots: raise
  `max_spots_per_minute`, or set `0` to disable the ceiling as a
  rollback, then restart. SIGHUP does not reload it.
- **A runaway input.** The ceiling is already capping the flood; lowering
  it only trims the volume of wrong reports. Fix the input, or stop
  transmitting with `dry_run = true`. On Unix, SIGHUP applies that
  without a restart only if `dry_run` is the sole `[[rbn_uplink]]` change
  in the edit; otherwise, and on Windows, restart.

Lower the ceiling only on a node whose spots forwarded per minute
(`increase(manta_uplink_target_sent_total[1m])`) stay well below it. See
[DECISIONS/2026-10-14-man248-uplink-forwarding-ceiling.md](../DECISIONS/2026-10-14-man248-uplink-forwarding-ceiling.md).

Or against an explicit address (e.g. from another host, or when you don't
have the daemon's config file to hand):

```sh
manta status --addr 10.0.0.5:7302
manta status --addr 10.0.0.5:7302 --json | jq .uplink
```

(`--server-config` still works everywhere `--config` does here — it is a
deprecated alias, kept so existing runbooks and cron jobs don't break, and
it prints a one-line deprecation notice on stderr.)

`--config` is read the way `manta run` reads it, including `MANTA_*`
overrides and the `MANTA_CONFIG` fallback, so `status` dials the port the
daemon actually bound. It dials `[server].metrics_bind_addr` (MAN-132:
the metrics listener's own address, `127.0.0.1` by default), not
`bind_addr`, translated for you: a wildcard
(`0.0.0.0`/`::`) means "the daemon listens everywhere," which isn't itself
something `manta status` can dial, so it falls back to loopback. If the
daemon runs on a different host than the one you're checking from, use
`--addr <that host>:<metrics_port>` instead. That fails with exit `2`
until the daemon's metrics listener is widened, because it is
loopback-only by default: on the daemon, set `[server].metrics_bind_addr`
to an address you can reach and restart it, then firewall
`[server].metrics_port` to the hosts that check it, since the listener has
no password. Prefer the management interface's IP to `0.0.0.0`; with a
specific IP, `localhost` stops answering on the daemon's host too, but
`manta status --config` there follows the new address.
`docs/RUNBOOKS/network-exposure.md` has both steps.

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
    threshold. This is a brief, transitional state: with the uplink's
    `INITIAL_BACKOFF` = 1s, an unreachable target's first three reconnect
    attempts land at roughly t=0s/1s/3s, crossing the flapping threshold
    within a few seconds — so a target that has *never* connected reads
    `down` only for that initial window, then `flapping` for as long as it
    stays unreachable (the same "reconnecting repeatedly" signal a target
    that was once connected and dropped also produces). Either way it
    stays visible, not silently omitted — "configured and stuck" must be
    distinguishable from "not configured at all." `SENT` = 0 alongside a
    climbing `RECONN` is what tells you the target has never once worked,
    as opposed to a target that dropped after sending plenty.
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
| `141` | stdout closed before the status document was written. |

Exit `2` also covers version skew: if the daemon serves a `schema_version`
this `manta` build doesn't understand, `manta status` refuses to interpret
the document rather than guessing at an exit code, and names both versions
so you know which side to upgrade. A degraded uplink is never reported this
way — that is always exit `1`.

A cron/Nagios-style one-liner:

```sh
manta status --config /etc/manta/manta.toml >/dev/null || echo "uplink check failed (exit $?)"
```

## Also available: `GET /status` and Prometheus

`manta status` is a thin client over the same JSON document served on
`GET /status` by the metrics listener (default port 7302) —
`curl -s http://localhost:7302/status | jq` gets you the same data for
scripting or an existing monitoring stack. Per-target series are also on
`GET /metrics` (`manta_uplink_target_connected`, `_sent_total`,
`_suppressed_total`, `_reconnects_total`, `_recent_reconnects`, labeled
`target="host:port"`) for anyone who already scrapes Prometheus.
MantaUplinkDisconnected in [monitoring.md](monitoring.md) alerts on the same
verdict `/status` gives.

Both endpoints share `/metrics`'s exposure posture — unauthenticated,
bound to `[server].metrics_bind_addr` (`127.0.0.1` by default, MAN-132) — see
`docs/RUNBOOKS/network-exposure.md` if you need to restrict access.
Design rationale: `docs/DECISIONS/2026-09-04-man44-uplink-status-surface.md`.

**`/metrics`'s `manta_uplink_connected`/`manta_uplink_target_connected` are
the raw, instantaneous connected bool — they can briefly read `1` for a
target that `manta status`/`GET /status` correctly reports as `flapping`,
because a target reconnecting every ~60s is momentarily connected any time
you happen to scrape it. Trust `/status`'s verdict, not `/metrics`'s raw
gauge, for "is this target actually healthy" — see the ADR above for why
the two surfaces are allowed to disagree here.
