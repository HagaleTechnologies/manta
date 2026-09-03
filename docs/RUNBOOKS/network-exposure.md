# Network exposure guide

manta's output surfaces (telnet DX-cluster, JSON/WebSocket stream) are
designed to be internet-reachable with no authentication, matching the DX
cluster/RBN ecosystem's own long-standing convention (ARCHITECTURE.md §7).
The Prometheus metrics endpoint is different: it's operationally useful, not
part of that public-facing contract, and carries no authentication either.

By default all three bind to `[server].bind_addr`, which defaults to
`0.0.0.0` — every configured port is reachable from any network that can
route to the host, metrics included.

**If you don't want `/metrics` reachable outside your own network**, do one
of:

- Set a separate, loopback-only bind for it if/when a per-listener bind
  option exists (currently `bind_addr` is shared across all three servers —
  check the current config surface before assuming this is available).
- Firewall the metrics port (`[server].metrics_port`) separately from the
  telnet/JSON ports you intend to leave public — e.g. an iptables/nftables
  rule restricting it to your management network, or a reverse proxy that
  only forwards the telnet/JSON ports through.

This is a deliberate, documented posture (see
`docs/DECISIONS/2026-09-02-man23-threat-model.md`, finding 11) — not an
oversight — but it's easy to miss if you're only thinking about the
telnet/JSON ports as "the public ones."
