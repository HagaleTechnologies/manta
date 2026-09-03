# MAN-61: per-source-IP connection quota

MAN-61 (found during MAN-23's threat-modeling review) explicitly asked for
a design decision, not an assumed fix: `ConnectionLimiter` bounds only the
TOTAL connection ceiling shared across every client combined (512 for
telnet/JSON, 64 for metrics) — it does not prevent one source from
permanently holding a large share (up to all) of that ceiling. A telnet
client retains its permit indefinitely once logged in; a raw JSON/WS
client is *designed* to be quiet forever after connecting (the whole
point of a push-only protocol); a metrics client's permit is held for up
to `HEADER_READ_TIMEOUT` even sending nothing. A single source opening up
to the ceiling and going quiet denies admission to every other client for
as long as it keeps those sockets open.

## Options considered (per the ticket's own technical notes)

1. **Per-source-IP connection quota.** Caps how many of the shared
   ceiling's permits one IP may hold concurrently.
2. **Idle-but-connected reaper.** Disconnect a client that's sent nothing
   verifiably alive for some generous window.
3. **Accepted-risk disposition**, deferring to a reverse-proxy/firewall
   layer already doing connection-rate limiting.

## Decision: per-source-IP quota

Chosen over the idle reaper because a reaper would directly conflict with
an already-established, deliberate design principle in this codebase:
`bounded_io::IDLE_READ_TIMEOUT` intentionally does NOT apply to an
established, quietly-listening client — `telnet.rs`'s round-5 review
finding specifically reverted an earlier version that disconnected a
logged-in client for staying quiet, because a read-mostly DX-cluster
client legitimately listens for minutes with nothing to say. A JSON/WS
client is *designed* to never send anything at all. Reintroducing an idle
timeout for this finding would re-break that already-fixed behavior for
every well-behaved long-running client, not just an attacker.

Chosen over the accepted-risk disposition because the fix is cheap,
self-contained, and doesn't depend on an operational assumption (a
reverse proxy in front of manta) that isn't guaranteed for every
deployment this driver targets.

A per-IP quota directly bounds what the threat model describes as the
actual attack shape (one source parking at the ceiling) without touching
the already-correct "quiet-but-healthy client" behavior at all: it only
ever affects a SECOND-OR-LATER connection attempt from a source already
at its own cap, never an existing connection's liveness.

## Implementation

`IpQuota`/`IpQuotaGuard` (`crates/manta-server/src/tasks.rs`): a
`std::sync::Mutex<HashMap<IpAddr, usize>>` reference-counting connections
per source IP, with an RAII guard releasing the slot on drop (the same
shape as `ConnectionLimiter`'s `OwnedSemaphorePermit`, just synchronous
since the critical section has no `.await` in it). Checked in each accept
loop BEFORE `ConnectionLimiter::acquire_owned` — a source already at its
own cap is declined without consuming a shared permit at all, so a quota
rejection never blocks the accept loop waiting on capacity it won't use.

Applied to all three listeners flagged in the ticket and its round-2
scope-expansion comment (telnet, JSON/WS, metrics HTTP), each with its
own cap sized proportionally to that listener's total ceiling:

| Listener | Total ceiling | Per-IP cap |
|---|---|---|
| Telnet | 512 | 16 |
| JSON/WS | 512 | 16 |
| Metrics HTTP | 64 | 8 |

16 leaves room for a handful of legitimate multi-connection uses behind
one IP (NAT, a monitoring tool opening more than one session) while still
requiring at least 32 distinct sources to exhaust the full telnet/JSON
ceiling. Metrics' smaller total ceiling gets a proportionally smaller
per-IP cap, matching `MAX_METRICS_CONNECTIONS`'s own reasoning that
legitimate traffic there is a low-cardinality set of Prometheus scrapers,
not end-user clients.

## What this does not fix

A single source behind many distinct IPs (e.g. a botnet, or IPv6 address
rotation) is not bounded by this change — that's inherently a
network/infrastructure-layer concern (firewall, reverse proxy rate
limiting) outside what a per-connection application-layer quota can
address. This closes the specific "one source parks at the ceiling"
shape the ticket describes, not distributed abuse.
