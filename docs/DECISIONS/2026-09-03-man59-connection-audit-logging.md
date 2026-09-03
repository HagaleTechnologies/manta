# MAN-59: connection/rejection audit logging

MAN-59 (found during MAN-23's threat-modeling review) — a Repudiation-
category STRIDE finding, distinct from the Tampering/DoS shape of most of
MAN-22/23's other fixes: `manta-server` had no `tracing`/`log` dependency
anywhere, and no production `eprintln!`/`println!` diagnostics either. The
daemon accepts connections on two internet-facing surfaces (telnet,
JSON/WS) plus an unauthenticated metrics endpoint, and there was no
record anywhere — not even ephemeral in-memory — of connecting IPs, login
callsigns, commands issued, or disconnect reasons. Live Prometheus
counters exist but don't retain history and reset on restart: after an
abuse incident (a command-flood attempt, a client probing for parser
bugs, unusual connection volume), an operator would have had no way to
reconstruct what happened or from where.

## Design decisions (per the ticket's own explicit ask — not assumed)

**Crate: `tracing` + `tracing-subscriber`.** The closest thing to a
Rust-ecosystem default for structured, leveled logging; both already
appear transitively in the dependency tree via `tokio`'s own
instrumentation hooks, so this adds no new supply-chain surface, only
promotes an existing transitive dependency to direct.

**Format: `tracing-subscriber`'s default `fmt` layer (plain text with
key=value fields), not JSON.** `tracing`'s event macros already produce
structured, grep-able key=value output (`peer=127.0.0.1:54321
ip=203.0.113.7 lost=42`) without a JSON serialization layer — enough
structure for `grep`/`journalctl`-style post-hoc analysis, matching this
project's own "small, auditable" ethos (see `telnet.rs`'s own doc comment
on skipping IAC for the same reason) without committing to a JSON
logging pipeline no current operator has asked for. Revisit if/when an
operator wants machine-parsed ingestion into a real log aggregator.

**Verbosity: `RUST_LOG` env var, default `info` when unset.** Connection
events (connect/disconnect/login) and rejections (rate/quota exceeded,
malformed/oversized input, WS handshake failures) are logged at
`info`/`warn` respectively, so an operator gets useful output with zero
configuration; `RUST_LOG=manta_server=debug` (or `trace`) is available
for deeper debugging without a code change.

**Scope: the three listeners the ticket names** (`telnet`, `json_stream`,
`metrics_http`) — not `uplink.rs` (manta's OUTBOUND connection to an RBN
collection target). The ticket's own framing is specifically about
Repudiation on manta's INBOUND, internet-facing, untrusted-client
surfaces; the outbound uplink already has its own accounting via
`Metrics::record_uplink_*` and isn't accepting connections from
unauthenticated peers.

**Every listener gets a per-connection `#[tracing::instrument]` span**
(`telnet_client`/`json_client`/`ws_client`, `fields(peer = %peer)`) so
every event inside that connection's handler automatically carries its
peer address without repeating it at each call site. `metrics_http`
deliberately does NOT log every successful request at `info` — Prometheus
scrapers hit this endpoint frequently (typically every 10-30s) and doing
so would be pure noise with no security value; only rejections (per-IP
quota, header-read timeout, oversized/malformed request) are logged
there.

**Every natural integration point the ticket named is now logged**:
`bounded_io` rejections (oversized/malformed line reads, both listeners'
handshake/command reads), every `RateLimiter`/`IpRateLimiter` disconnect,
every `IpQuota`-exhausted rejection, every malformed-WS-frame disconnect,
plus ordinary connect/login/disconnect events for full session
reconstruction. `ConnectionLimiter` itself has no discrete "rejected"
event to hook (a flood beyond it just waits in the OS accept backlog, per
its own doc comment) — consistent with what the code actually does, not
logged as a rejection that never occurs.
