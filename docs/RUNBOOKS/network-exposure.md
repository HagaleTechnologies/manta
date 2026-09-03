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

- **Firewall the metrics port** (`[server].metrics_port`) separately from
  the telnet/JSON ports you intend to leave public — e.g. an
  iptables/nftables rule restricting it to your management network, or a
  reverse proxy that only forwards the telnet/JSON ports through.

**Do NOT set `bind_addr = "127.0.0.1"` expecting it to hide only
metrics** — `bind_addr` is shared across all three listeners
(`crates/manta-cli/src/main.rs`), so that would *also* silently move the
telnet and JSON listeners to loopback, taking the services you meant to
keep public offline instead. A per-listener bind option doesn't exist yet;
until it does, firewalling is the only mitigation that doesn't have this
side effect.

If you want transport integrity (not just access restriction) for
WebSocket consumers specifically, terminate TLS in a reverse proxy in
front of the JSON/WS port — manta itself has no TLS support, matching the
DX-cluster ecosystem's own long-standing plaintext convention (see
`docs/DECISIONS/2026-09-02-man23-threat-model.md`, finding 20).

This publicly-bound-by-default posture is deliberate and documented (see
`docs/DECISIONS/2026-09-02-man23-threat-model.md`, findings 11 and 20) —
not an oversight — but it's easy to misconfigure if you're only thinking
about the telnet/JSON ports as "the public ones."
