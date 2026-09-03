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
Rust-ecosystem default for structured, leveled logging. **Correction**
(review round 1): this DOES add new supply-chain surface, not zero as
originally claimed here — `tokio`'s own optional instrumentation hooks
are gated behind a feature this workspace doesn't enable, so neither
crate was actually present in the dependency tree before this change.
`Cargo.lock` gained 12 new packages: `tracing`, `tracing-attributes`,
`tracing-core`, `tracing-log`, `tracing-subscriber`, plus their own
transitive deps `matchers`, `nu-ansi-term`, `sharded-slab`,
`thread_local`, `valuable`, `lazy_static`, `smallvec`. Accepted anyway:
still the closest thing to a Rust-ecosystem default, all from
well-maintained, widely-used crates, and the alternative (a hand-rolled
logger) would be a worse trade for a repo this size.

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

## Addendum (review round 4): consolidated per-connection log budget

Rounds 2-3 each found one more individually un-gated audit-log call site
(the quota-reject warning, then the 404-reject warning, then a missing
task-boundary catch-all in `metrics_http`) — the same gap recurring three
rounds running. Replaced the several one-off, event-specific
`IpRateLimiter`s with ONE budget per listener, decided once per admitted
connection (`telnet`/`json_stream` — every admitted connection there
always logs at least a connect event, so nothing is wasted) and threaded
through as `log_enabled`: every tracing call in that connection's entire
lifetime checks the same decision, closing the whole class of gap rather
than the one specific instance each round happened to find.

## Addendum (review round 5): two corrections

1. **`metrics_http`'s "decide once" budget was itself wrong.** Unlike
   telnet/json_stream, most admitted connections here are successful
   scrapes that log NOTHING — deciding the budget once per connection
   (the round-4 shape, copied uncritically from telnet/json_stream)
   silently spent a slot on connections that were never going to produce
   a log line, letting a source burn its whole window on harmless
   scrapes and then flood rejections for free. Fixed: `metrics_http`
   consults its `IpRateLimiter` LAZILY, right at each actual warning
   site, instead of pre-deciding at admission time. `telnet`/
   `json_stream` keep the decide-once shape — it's correct there because
   every admitted connection genuinely does log something.
2. **WS audit attribution is unavailable behind the documented
   reverse-proxy deployment** — `peer` there is the proxy's own address
   for every downstream client, not the real client's, so the audit
   trail can't reconstruct which real client originated an abusive WS
   session in that specific deployment shape. A correct fix (trusting a
   forwarded-address header only from a configured trusted-proxy
   allowlist) is real future work, not implemented here — documented as
   a known limitation instead, per `docs/RUNBOOKS/network-exposure.md`'s
   own MAN-59 addendum, which is the reviewer's own explicitly offered
   alternative to building that feature blind at review time.
