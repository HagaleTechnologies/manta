# Network exposure guide

manta's output surfaces (telnet DX-cluster, JSON/WebSocket stream) are
designed to be internet-reachable with no authentication, matching the DX
cluster/RBN ecosystem's own long-standing convention (ARCHITECTURE.md §7).
The Prometheus metrics endpoint is different: it's operationally useful, not
part of that public-facing contract, and carries no authentication either.
**`GET /healthz` (MAN-128) shares the metrics listener, its
`metrics_bind_addr` and its port** — it is not a separate port, so every
mitigation below that applies to `[server].metrics_port` covers
`/healthz` too. See `docs/RUNBOOKS/node-health.md` for its semantics.

The telnet and JSON listeners bind `[server].bind_addr`, which defaults to
`0.0.0.0`: both ports are reachable from any network that can route to
the host, as a public cluster node expects. The metrics listener (and
`/healthz` with it) binds its own `[server].metrics_bind_addr`, which
defaults to `127.0.0.1` (MAN-132,
`docs/DECISIONS/2026-10-08-man132-metrics-loopback-bind.md`): out of the
box, only this machine can reach it.

**`bind_addr` and `metrics_bind_addr` are independent**: moving either
one never moves the other. `bind_addr = "127.0.0.1"` takes telnet and
JSON off the network and leaves metrics where it is; `metrics_bind_addr =
"0.0.0.0"` exposes metrics and leaves telnet and JSON where they are.

**To keep `/metrics` private, do nothing**: that is the default. Run
`manta config check` to see the effective `metrics_bind_addr` in its
`server:` line.

**To let a Prometheus server or health probe on another machine scrape
it**, widen the metrics listener deliberately and restrict who can reach
it:

- Set `metrics_bind_addr = "0.0.0.0"` under `[server]` (or
  `MANTA_SERVER_METRICS_BIND_ADDR=0.0.0.0`), or better, the IP of a
  specific management interface, e.g. `metrics_bind_addr = "10.0.0.5"`.
  `manta config check` prints a note whenever `metrics_bind_addr` is a
  wildcard (`0.0.0.0` or `::`).
- **Firewall the metrics port** (`[server].metrics_port`) — e.g. an
  iptables/nftables rule, or a cloud security-group rule, restricting
  inbound access to it to the machines that scrape it. The endpoint has
  no password, so the firewall is the access control.

**Containers and Kubernetes.** Docker's `-p 7302:7302` forwards to the
container's own network interface, never to its loopback, and a
Kubernetes `httpGet` probe connects to the pod IP — so with the default
`metrics_bind_addr = "127.0.0.1"` neither can reach `/metrics` or
`/healthz`. A container that must serve them through a published port or
to the kubelet needs `MANTA_SERVER_METRICS_BIND_ADDR=0.0.0.0` (or the
config key). In Docker, publish it as `-p 127.0.0.1:7302:7302` to keep it
reachable from the host only, and firewall it like any other widened
metrics port otherwise. `docs/RUNBOOKS/node-health.md` shows the
Kubernetes probe.

**A reverse proxy alone is NOT access control for a widened listener —
it's a routing convenience, unless you separately block direct access to
the real port it's proxying.** manta itself always binds telnet and JSON
to `bind_addr`, and metrics to `metrics_bind_addr`, directly, regardless
of whether a proxy also exists in front of it — once `metrics_bind_addr`
is widened, a proxy configured to "only forward the telnet/JSON ports"
does nothing to stop a client from connecting straight to the metrics
port itself, since that port is still listening and reachable on its
own. The same applies to the TLS-proxy
suggestion below: fronting the JSON/WS port with a TLS-terminating proxy
does not stop a client from connecting directly to manta's own plaintext
port instead, bypassing TLS entirely, unless direct access to that
backend port is *also* blocked. A proxy only provides real access control
or transport-integrity enforcement when paired with one of:

- A firewall/security-group rule blocking external access to manta's own
  ports outright, so the proxy's frontend is the only reachable path in.
- A network topology where the manta host itself isn't otherwise
  reachable from the network you're defending against (e.g. it only has a
  private address, and the proxy is the sole thing with a public one).

If you want transport integrity (not just access restriction) for
WebSocket consumers specifically, terminate TLS in a reverse proxy in
front of the JSON/WS port — manta itself has no TLS support, matching the
DX-cluster ecosystem's own long-standing plaintext convention (see
`docs/DECISIONS/2026-09-02-man23-threat-model.md`, finding 20) — **and
block direct external access to manta's own plaintext port**, per the
bypass note above, or the TLS termination is purely cosmetic against
anyone who just connects to the real port instead.

**If you front the JSON/WS port with a reverse proxy, raise or disable
`[server].json_max_connections_per_ip`** (MAN-61,
`docs/DECISIONS/2026-09-03-man61-per-ip-connection-quota.md`): every
listener caps how many concurrent connections a single source IP may
hold (16 for telnet/JSON, 8 for metrics) to stop one quiet client from
parking at the connection ceiling. Behind a proxy, every downstream
client shares the proxy's own IP as far as manta can tell, so the
default cap would deny admission after only that many real users despite
the listener having room for far more. Set `json_max_connections_per_ip
= 0` under `[server]` to disable the JSON/WS listener's per-IP cap
entirely (only its total connection ceiling still applies), or set it to
a higher number. **Only override the listener(s) actually behind the
proxy** — `telnet_max_connections_per_ip` and
`metrics_max_connections_per_ip` are separate fields for exactly this
reason: the setup above fronts JSON/WS only, so telnet and metrics stay
directly exposed and should keep their own per-IP protection.

Disabling the per-IP cap shifts responsibility for bounding one client's
share of capacity to the proxy — and **rate limiting alone is not
sufficient there**. These are long-lived streams that may legitimately
stay open and silent forever (the whole point of a push protocol), so a
client opening connections slowly enough to stay under any new-connection
rate budget can still retain every one of them and eventually occupy all
512 backend permits. Configure a genuine **per-client concurrent-connection
limit** at the proxy (most reverse proxies support this directly), not
just a connection-rate limit, before disabling the backend's own per-IP
quota.

The publicly-bound-by-default posture of the telnet and JSON listeners is
deliberate and documented (see
`docs/DECISIONS/2026-09-02-man23-threat-model.md`, findings 10 and 20, and
`docs/DECISIONS/2026-09-06-broad-review-decisions.md` D14) — not an
oversight. It does not apply to metrics: finding 11's metrics posture
changed with MAN-132, which moved the metrics listener to loopback by
default. A widened `metrics_bind_addr` puts it back in the public
posture, so treat that setting as the deliberate exposure it is.

**Same caveat applies to `json_max_pings_per_ip`/`telnet_max_commands_per_ip`
(MAN-57)** — separate from the connection quota above: each listener also
caps the AGGREGATE command/Ping rate a single source IP may generate
across every connection it holds (matching what a lone connection's own
per-connection budget already allows), to stop one source from
multiplying its effective rate by opening more connections. Behind the
same reverse proxy setup, every downstream client's commands/Pings would
be aggregated into that ONE shared budget too — a sharper false positive
than the connection quota, since this window is much tighter (e.g. 30
telnet commands per 10s, TOTAL, for every client behind the proxy
combined). If you disable or raise `json_max_connections_per_ip` for a
proxied JSON/WS deployment, raise or disable `json_max_pings_per_ip` the
same way (`0` disables it; only each connection's own per-connection
Ping budget still applies). Same reasoning for telnet's
`telnet_max_commands_per_ip` if telnet is ever put behind a proxy too.

**Known limitation (MAN-59, review round 5): per-client audit-log
attribution is unavailable for WS traffic behind this same reverse-proxy
deployment.** `json_stream.rs`'s connect/disconnect/rejection logging
(`docs/DECISIONS/2026-09-03-man59-connection-audit-logging.md`) records
the TCP socket's own remote address as `peer` — behind the proxy, that's
the PROXY's backend-facing address for every downstream client, not the
real client's, since the proxy terminates the actual client connection
and opens its own to manta. Every WS audit event in this deployment will
show the proxy's IP, indistinguishable from any other client behind it —
the audit trail cannot reconstruct which real client originated an
abusive session, only that "some client behind the proxy" did.
`accept_async_with_config` (`tokio_tungstenite`) has no built-in support
for a trusted-proxy forwarded-address header (`X-Forwarded-For`/
`Forwarded`), and adding one correctly needs a genuine trust-boundary
design (a configured trusted-proxy allowlist, so an arbitrary client
can't simply set its own `X-Forwarded-For` and spoof a different logged
identity) — real future work if per-client attribution behind this
deployment becomes operationally necessary, not assumed here. Until
then: the proxy's OWN access log (most reverse proxies log the real
client IP per request/connection by default) is the only place to find
per-client attribution in this specific deployment shape.
