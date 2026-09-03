# Network exposure guide

manta's output surfaces (telnet DX-cluster, JSON/WebSocket stream) are
designed to be internet-reachable with no authentication, matching the DX
cluster/RBN ecosystem's own long-standing convention (ARCHITECTURE.md §7).
The Prometheus metrics endpoint is different: it's operationally useful, not
part of that public-facing contract, and carries no authentication either.

By default all three bind to `[server].bind_addr`, which defaults to
`0.0.0.0` — every configured port is reachable from any network that can
route to the host, metrics included.

**If you don't want `/metrics` reachable outside your own network**, the
only safe option today is:

- **Firewall the metrics port** (`[server].metrics_port`) directly — e.g.
  an iptables/nftables rule, or a cloud security-group rule, restricting
  inbound access to it to your management network.

**A reverse proxy alone does NOT achieve this — it's a routing
convenience, not access control, unless you separately block direct
access to the real port it's proxying.** manta itself always binds every
port to `bind_addr` directly (`0.0.0.0` by default), regardless of
whether a proxy also exists in front of it — a proxy configured to
"only forward the telnet/JSON ports" does nothing to stop a client from
connecting straight to the metrics port itself, since that port is still
listening and reachable on its own. The same applies to the TLS-proxy
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

**Do NOT set `bind_addr = "127.0.0.1"` expecting it to hide only
metrics** — `bind_addr` is shared across all three listeners
(`crates/manta-cli/src/main.rs`), so that would *also* silently move the
telnet and JSON listeners to loopback, taking the services you meant to
keep public offline instead. A per-listener bind option doesn't exist yet;
until it does, firewalling the specific port you want restricted is the
only mitigation that doesn't have this side effect.

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

This publicly-bound-by-default posture is deliberate and documented (see
`docs/DECISIONS/2026-09-02-man23-threat-model.md`, findings 11 and 20) —
not an oversight — but it's easy to misconfigure if you're only thinking
about the telnet/JSON ports as "the public ones."

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
