# MAN-44: uplink health at a glance — `manta status` + `GET /status`

MAN-32/MAN-42 already collect every counter MAN-44's gherkin asks for
(`uplink_sent_total`, `uplink_suppressed_total`, `uplink_reconnects_total`,
a connected count) as `Metrics` atomics, rendered on the existing
`GET /metrics` Prometheus endpoint. Nothing *presents* them: an operator
had to read raw counter deltas over time themselves to answer "is my
uplink healthy," and with more than one configured `[[rbn_uplink]]` target
(MAN-42), the aggregate-only counters couldn't even say *which* target was
the problem. MAN-42's own plan named MAN-44 as the ticket responsible for
closing that per-target gap.

## Options considered

1. **A local control socket** (Unix domain socket / named pipe), matching
   the sketch ARCHITECTURE §8 has carried since before MAN-32/MAN-42
   existed ("`manta --status` hits a local control socket for live
   stats").
2. **`GET /status` on the existing metrics HTTP listener** + a `manta
   status` CLI subcommand that fetches and renders it.
3. **Prometheus-only**: leave the surface as raw counters and rely on an
   external tool (Grafana, alertmanager) to compute health from them.

## Decision: option 2 — reuse the metrics listener, no control socket

**Cross-platform reach.** manta's own first line is "cross-platform." A
Unix domain socket has no equivalent on Windows without an entirely
separate code path (a named pipe), and shipping an operator health check
that silently doesn't exist on one of the project's three target
platforms is worse than reusing a listener that already works
identically everywhere `manta listen` runs. ARCHITECTURE §8's
control-socket line was always a design sketch, not a commitment — the
ticket's own technical notes say so explicitly.

**Free hardening reuse.** `metrics_http.rs` already carries MAN-59
(connection audit logging), MAN-61 (per-IP connection quota), and MAN-62-
adjacent bounded-header-read discipline, all proven in production against
this exact listener. A new Unix socket would need to either re-derive
all of that (permission model, backlog bounds, audit logging) or ship
without it — plus a genuinely new class of operational concern this
codebase has no precedent for at all: socket-file lifecycle (stale
sockets from an unclean shutdown, directory permissions, umask). Reusing
the listener means `/status` inherits every one of those protections for
free and adds zero new attack surface beyond one more route on an
already-hardened server.

**Rejected: Prometheus-only.** Still requires an external tool to turn
counters into a verdict, which does not satisfy the ticket's second
scenario ("they don't need to read raw logs" — reading raw counters
through `curl`/Grafana is the same class of problem). Per-target
Prometheus series are still added (see below) as a superset, not instead
of, a human-facing surface.

**Accepted trade-off:** `/status` inherits `/metrics`'s unauthenticated,
`0.0.0.0`-by-default exposure posture (`docs/RUNBOOKS/network-exposure.md`),
and additionally discloses configured RBN uplink target `host:port` pairs
— public RBN infrastructure by nature, but newly enumerable by anyone who
can reach the port. The runbook's existing firewall guidance is the
mitigation, unchanged in kind from what `/metrics`'s `manta_source_health`
labels already expose.

## What was built

- **`crates/manta-server/src/metrics.rs`**: a per-target `UplinkTarget`
  registry (`Metrics::register_uplink_target`), replacing the old shared
  `uplink_connected_count: AtomicI64` with one `AtomicBool` per target.
  This is a structural fix, not just a testing discipline, for the
  MAN-42 round-1 review finding (a last-writer-wins shared boolean let
  one target's failed reconnect clear the gauge while another was
  genuinely connected) — with a dedicated `AtomicBool` per target, that
  bug class cannot recur regardless of call-site discipline. Every
  aggregate uplink counter (`uplink_sent_total` etc.) is now **derived**
  by summing over the registry rather than maintained as a second,
  independently-incremented counter — two parallel sources of the same
  number is exactly how the round-1 bug happened in the first place.
- **Windowed health classification** (`UplinkHealth`): `disabled` →
  `flapping` (recent reconnects, within a rolling `RECONNECT_WINDOW` =
  300s, at or above `FLAPPING_RECONNECTS` = 3) → `connected` → `down`, in
  that priority order. `flapping` deliberately outranks `connected`: a
  target reconnecting every 60s (`uplink::MAX_BACKOFF`) is momentarily
  "connected" any time you happen to look, and reporting that as healthy
  is exactly the failure MAN-44 exists to prevent. The threshold is sized
  against the uplink's own backoff cap: a target that's simply down
  produces roughly 5 reconnects per 5-minute window (comfortably over 3),
  while one transient blip — whose backoff resets to `INITIAL_BACKOFF` on
  any connection that reached login — does not trip it.
- **`crates/manta-server/src/status.rs`**: `StatusDoc`/`UplinkStatus` (a
  `schema_version`-tagged JSON document; `UplinkTargetSnapshot` from
  `metrics.rs` is reused directly as the per-target shape rather than a
  second, parallel struct, so there is exactly one Rust definition of the
  wire shape) and `render_human` (the one-screen text `manta status`
  prints). `active_tracks` is `Option<u64>`, hardcoded `None` in
  `StatusDoc::from_metrics` — `Metrics::set_active_tracks` has no
  production call site (ARCHITECTURE §8's existing "served but not
  populated" caution), and wrapping the getter's always-real `0` here
  would repeat that exact mistake instead of avoiding it. Never includes
  `login_callsign`: knowing *which* target is broken is the point;
  which callsign it logs in as isn't, and the type it's built from
  (`UplinkTargetSnapshot`) has no such field to leak even by accident.
- **`crates/manta-server/src/metrics_http.rs`**: routing extracted into a
  pure `route(request_line, metrics) -> Response` function (unit-testable
  without a socket) with two matched paths, `/metrics` and `/status`;
  everything else still 404s. The path match now tolerates a trailing
  query string (`GET /metrics?x=1`) — the old `starts_with("GET /metrics
  ")` check rejected one, and some scrapers append it.
- **`crates/manta-cli/src/main.rs`**: `manta status` — `Command::Status`,
  a subcommand (not a top-level `--status` flag: the CLI's `Command` enum
  is already a required subcommand, matching `listen`/`soak`). Resolves
  the daemon's metrics address from `--addr`, or a `--server-config`'s
  `[server]` table (`0.0.0.0`/`::` `bind_addr` collapse to loopback for
  DIALING purposes — a client can't connect *to* a wildcard bind
  address), defaulting to `127.0.0.1:7302`. Fetches `GET /status` with a
  small hand-rolled HTTP/1.1 client (`fetch_status`, matching
  `metrics_http`'s own hand-rolled-over-a-framework precedent, bounded by
  both an overall timeout and a 256 KiB response-body cap against a wrong
  or hostile endpoint), then either prints the JSON (`--json`) or
  `status::render_human`'s summary. Exit codes: `0` every enabled uplink
  target connected (or none configured), `1` reached the daemon but the
  uplink is unhealthy, `2` couldn't reach or parse the daemon's status at
  all — a cron/Nagios check can tell "broken uplink" from "daemon down"
  without parsing output.
- **New Prometheus series**: `manta_uplink_target_connected`,
  `_sent_total`, `_suppressed_total`, `_reconnects_total`,
  `_recent_reconnects`, one series per configured target, labeled
  `target="host:port"` (a second-or-later duplicate `host:port` gets a
  `#N` suffix, assigned deterministically from config order). New metric
  NAMES, not labels bolted onto the existing aggregate series — adding a
  label to an already-shipped series changes its shape and breaks
  existing scrape configs; the pre-MAN-44 `manta_uplink_*` series are
  byte-identical in name and value. Target labels (and the existing
  `source` label on `manta_source_health`, fixed here while already
  touching this code) are now escaped per the Prometheus text-exposition
  spec — both come from operator-supplied config strings, not manta's own
  validated callsign grammar.

## What this does not do

No control socket, no daemon control verbs (reload/stop/reconnect-now) —
read-only status only. No authentication or TLS for `/status` — it
inherits the metrics listener's posture; firewalling remains the
mitigation, same as `/metrics` today. No dashboard, Grafana JSON, or
alert rules — MAN-32's plan scoped those out and nothing here changes
that; the per-target series make them possible downstream. No history —
every counter resets on daemon restart, same as before. `manta status`
has no watch/follow mode: one shot, print, exit.
