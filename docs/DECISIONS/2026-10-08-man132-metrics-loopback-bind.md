# 2026-10-08 — MAN-132: the metrics listener binds loopback by default

**Status:** Implemented (branch `MAN-132`). Implements
`docs/DECISIONS/2026-09-06-broad-review-decisions.md` D14: telnet and
JSON/WebSocket stay public by default, as RBN nodes run; the metrics
endpoint (`GET /metrics`, `GET /healthz`) moves to loopback.

## Context

Before this change all three listeners bound one shared
`[server].bind_addr`, default `0.0.0.0` (`crates/manta-cli/src/main.rs`'s
daemon wiring). The metrics endpoint has no password, so a node with no
address keys served it on every interface. Reproduced on `main` at
`888eeba`:

| What | Result |
|---|---|
| `manta run` banner, no address keys, ports 0 | `telnet=0.0.0.0:… json=0.0.0.0:… metrics=0.0.0.0:…` |
| `curl http://<non-loopback IP>:<metrics_port>/healthz` | `200` |
| `metrics_bind_addr` in the file or `MANTA_SERVER_METRICS_BIND_ADDR` | `unknown field 'metrics_bind_addr'`, exit 1 |
| metrics port already taken | `Error: Address already in use (os error 98)`, naming no listener |
| Linux bind rules (`SO_REUSEADDR` listeners, one port) | `0.0.0.0` + `127.0.0.1`: `EADDRINUSE` in either order; dual-stack `[::]` + `127.0.0.1`: `EADDRINUSE`; `127.0.0.1` + `172.18.0.2`: both bind |

`docs/RUNBOOKS/network-exposure.md` warned operators not to set
`bind_addr = "127.0.0.1"` to hide metrics, because that also took telnet
and JSON offline. D14 noted that the fix needs a real per-listener bind
option, not a flip of the shared default.

## Decisions

- **D1 — key name `metrics_bind_addr`.** It follows the `metrics_` prefix
  of `metrics_port` and `metrics_max_connections_per_ip`, and is the name
  the 2026-09-05 broad review (lens 2, #20) proposed.
- **D2 — a `String` with default `"127.0.0.1"`, not `Option<String>`.**
  Same shape as `bind_addr`. The effective value always shows in
  `manta config check`'s `server:` line; MAN-76's scaffold
  default-equivalence guard covers it with no special case; and the
  default lives in one place (`default_metrics_bind_addr()` in
  `crates/manta-server/src/config.rs`).
- **D3 — fully independent of `bind_addr`.** Neither follows the other.
  Widening the public listeners (`bind_addr = "192.168.1.5"`) must not
  silently widen the password-less one, and narrowing metrics must not
  take telnet/JSON offline — the mirror image of the trap the runbook
  warned about.
- **D4 — IPv4 loopback `127.0.0.1`, not `localhost` or `::1`.** It needs
  no resolution, exists on every supported OS, and matches the systemd
  and README probes that already used `127.0.0.1`. IPv6-only operators
  set `"::1"`.
- **D5 — `manta config init` comments, it does not prompt.** The ticket
  allows "asks (or clearly comments)". MAN-76 deliberately has no
  prompts, and `init --out -` runs non-interactively in scripts and
  containers. The scaffold's `metrics_bind_addr` block says the endpoint
  has no password, that the default accepts connections from this
  machine only, and when to set `"0.0.0.0"` (with a firewall on
  `metrics_port`). `bind_addr`'s block no longer mentions metrics.
- **D6 — `config check` notes per listener.** A wildcard `bind_addr`
  gets a note that telnet and JSON are public, as a cluster node is; a
  wildcard `metrics_bind_addr` gets a note that the password-less
  endpoint is public and should be firewalled. The not-an-IP-address
  note covers either key. There is no "metrics is local" note for the
  default: the summary line shows it, and notes are for likely mistakes.
- **D7 — address-aware duplicate-port check.** Two listeners on one
  non-zero port are rejected unless their addresses are two different,
  specific (non-wildcard) IP literals. A host name counts as overlapping
  because `check` does no I/O (MAN-76 D4). When the two addresses differ
  the error names both (`…, and metrics_bind_addr "127.0.0.1" overlaps
  bind_addr "0.0.0.0"; …`). The rule is conservative on Windows and for
  `0.0.0.0` with `::1`, where it rejects configs that would bind; "each
  server needs its own port" is never harmful advice.
- **D8 — a failed bind names its listener.** Each bind in `run` carries
  context, e.g. `binding the metrics server (metrics_bind_addr =
  "127.0.0.1", metrics_port = 7302)`. With two configurable addresses
  "which one failed?" is a real question; a typo such as
  `metrics_bind_addr = "10.0.0.99"` would otherwise surface as a bare
  `Cannot assign requested address`.
- **D9 — historical decision docs are annotated, not rewritten.** The
  threat model's finding 11 and accepted risk 4, D14 and MAN-76's
  follow-up each get a dated "MAN-132" sentence. The runbooks, README,
  ARCHITECTURE and SPEC describe current behaviour, so they are
  rewritten.

`/healthz` (and any future endpoint on the metrics socket) moves with
the listener; it has no bind of its own.

## Consequences

- A node with no address keys serves telnet and JSON on `0.0.0.0` and
  metrics on `127.0.0.1`; `manta run`'s banner shows both, and a
  connection to the metrics port on a non-loopback address is refused
  (`crates/manta-cli/tests/metrics_bind.rs`).
- Remote Prometheus scraping, a Kubernetes `httpGet` liveness probe
  (which connects to the pod IP) and a Docker-published metrics port
  (`-p` forwards to the container's interface, not its loopback) now
  need `metrics_bind_addr = "0.0.0.0"` or
  `MANTA_SERVER_METRICS_BIND_ADDR=0.0.0.0`, plus a firewall on
  `metrics_port`. `docs/RUNBOOKS/network-exposure.md` and
  `docs/RUNBOOKS/node-health.md` say so.
- No tagged release exists (`git ls-remote --tags origin` lists only
  `archive/*` tags; README: "There is no tagged release yet"), so no
  released user is affected by the default change.
- `config check` now accepts same-port configs on two distinct specific
  IPs (e.g. `bind_addr = "192.168.1.5"`, `metrics_port = 7300`).
- Configs that already set `bind_addr = "127.0.0.1"` (every test fixture
  and the README example) behave exactly as before.

## Follow-ups (not in this change)

- PR #95 (MAN-44, `GET /status` and `manta status`): `/status` rides the
  metrics listener, so once rebased its `manta status` dial logic must
  resolve `metrics_bind_addr`, not `bind_addr`.
- PR #219 (MAN-96, field node): if it merges after this, drop its
  runbook sentences saying per-listener bind (MAN-132) has not landed
  and that `check` notes `bind_addr = "0.0.0.0"` exposes the metrics
  endpoint. Its firewall advice stays valid as defence in depth, and
  `--metrics-url http://127.0.0.1:7302` keeps working.

## References

- Ticket: MAN-132 (2026-09-05 broad review, lens 1 #19, lens 2 #20)
- `docs/DECISIONS/2026-09-06-broad-review-decisions.md` D14
- `docs/DECISIONS/2026-09-02-man23-threat-model.md` finding 11, accepted
  risk 4
- `docs/DECISIONS/2026-10-07-man76-config-check-init.md` (scaffold and
  `check` machinery)
- `docs/DECISIONS/2026-10-05-man128-node-health-metrics.md` (`/healthz`
  on the metrics socket)
- `docs/DECISIONS/2026-09-03-man61-per-ip-connection-quota.md`
  (per-listener `[server]` field precedent)
- Research and plan: the thoughts pool's
  `2026-10-08-MAN-132-the-metrics-endpoint-should-default-to-loopback-while`
