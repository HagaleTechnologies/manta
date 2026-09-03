# 2026-09-02 — MAN-23 structured threat model: manta's network-facing surfaces

**Status:** accepted (investigation only — no application-code changes in this
doc's own PR; findings requiring code changes are filed as their own
tickets, per the invariant below).

## Decision

A structured, STRIDE-organized adversarial pass over every surface manta
exposes to a network it doesn't control, complementing MAN-22's baseline
input-hygiene fixes with the deeper "is this the right design" pass MAN-22's
own ticket explicitly deferred to this one. Every finding below is given
exactly one disposition: **fixed** (already landed, cites the PR), **filed**
(a new ticket, cited by number), or **accepted risk** (recorded here with
its rationale, no ticket). Nothing is left as unfiled prose.

Scope, per MAN-23's own technical notes: the UDP HPSDR/OpenHPSDR driver
(MAN-11), the telnet cluster server + JSON/WebSocket stream (MAN-12), and
their outbound sibling, the RBN uplink (shipped under MAN-32's scope, same
`manta-server` crate, same "accepts input from a source manta doesn't fully
control" property that motivates this whole ticket). **MAN-13** (multi-source
orchestration) is explicitly out of scope — it doesn't exist yet, so there's
nothing to adversarially review; this pass should be re-run once it lands,
the same caveat MAN-23's own ticket body already states about MAN-11/MAN-12.

Method: manual STRIDE walk over each surface's actual code (not the
`security-review` diff-review skill, which is built for a pending diff —
this is existing, already-shipped code), cross-checked against
`ARCHITECTURE.md` §3/§7/§8's stated design intent. Sources: direct reading
of `crates/manta-input/src/hpsdr.rs`, `crates/manta-server/src/{telnet,
json_stream, bounded_io, tasks, rate_limit, bus, uplink, rbn,
metrics_http, command}.rs`, plus a full-workspace `unsafe` grep (zero
hits in either crate).

## STRIDE findings by surface

### UDP HPSDR/Hermes input (MAN-11, `crates/manta-input/src/hpsdr.rs`)

| # | Category | Finding | Disposition |
|---|---|---|---|
| 1 | Tampering / DoS | A single malformed UDP packet (correct length, bad USB sync bytes) propagated a fatal `Err` all the way out of `manta-engine::listen`'s read loop via unmatched `?` — one crafted packet killed the whole process | **Fixed** — MAN-22, PR #75. Malformed datagrams are now discarded and counted (`GapStats.malformed_packets`), regression-tested against a live loopback socket, and the test verified to fail against the pre-fix code |
| 2 | Spoofing | UDP has no source authentication. `HpsdrDevice::open` does call `UdpSocket::connect()`, which does kernel-level source-address filtering for `recv()` — but that only stops an *off-path, non-spoofing* attacker. A network position able to spoof the configured device's source IP (feasible on networks without egress/ingress filtering — a shared LAN, some cheap hosting) can inject fabricated-but-well-formed IQ, which the decode pipeline has no way to distinguish from a real signal | **Accepted risk** — this is inherent to OpenHPSDR/Hermes (Protocol 1) itself, which has no cryptographic framing of any kind; every implementation of this protocol (piHPSDR included) has the identical exposure. The implicit, and correct, mitigation is operational: HPSDR-class SDR hardware is LAN-local by design (module docs: "standard Metis broadcast, UDP port 1024" — a discovery convention that assumes a trusted local segment), not meant to be reached across the open internet the way the telnet/JSON-WS output surfaces are (ARCHITECTURE.md §7's "internet-reachable" framing is specific to *those* surfaces, not the SDR-facing input side). No code-level mitigation is available within the protocol's own constraints |
| 3 | DoS (residual) | A source-IP-spoofed flood of well-formed-shaped datagrams (matching length + sync bytes) would be processed as real IQ, consuming CPU per packet at whatever rate the flood arrives | **Accepted risk** — generic to any UDP receiver; `MAX_CONSECUTIVE_TIMEOUTS` bounds *silence*, not a flood, and there is no useful application-level mitigation short of the same LAN-trust boundary noted in finding 2. Rate-limiting a raw SDR data feed would also legitimately break real high-DDC-count bursts, so this is left to network-layer controls (the operator's LAN), not manta's own code |

### Telnet cluster server + JSON/WebSocket stream (MAN-12, `crates/manta-server/src/{telnet,json_stream,bounded_io,tasks,command,rate_limit,bus}.rs`)

| # | Category | Finding | Disposition |
|---|---|---|---|
| 4 | Tampering / DoS | Oversized/unterminated telnet lines | **Already covered** — `bounded_io::MAX_LINE_BYTES` (1024), now proven end-to-end over a real socket by MAN-22's new acceptance test (previously only unit-tested at the `bounded_io` level, not through the live server) |
| 5 | Tampering / DoS | Structurally invalid (not just oversized) WebSocket frames | **Already covered** — tungstenite's server-side parser rejects these; MAN-22 adds the first test that actually sends non-tungstenite-constructible garbage bytes post-handshake to prove it, rather than only exercising well-formed-but-hostile messages built through tungstenite's own client API |
| 6 | DoS | Connection-count flooding | **Already covered** — `ConnectionLimiter` (512 per surface), bounds the accept loop itself, not just post-accept |
| 7 | DoS | Command/ping-rate flooding *per connection* | **Already covered** — `RateLimiter` (30 commands/10s telnet, 10 pings/60s WS) |
| 8 | DoS | `RateLimiter` is instantiated per-connection with no IP-keyed or shared state — a client opening many connections from one source IP gets one full budget *per connection*, multiplying its effective aggregate rate up to `ConnectionLimiter`'s cap (512x) rather than being held to the single-connection budget the limiter's own design intends | **Filed — MAN-57**. Blast radius is bounded (never unlimited — `ConnectionLimiter` still caps total connections), so this is a real but lower-severity gap, not another MAN-22-class DoS |
| 9 | Repudiation | No logging/audit trail anywhere in `manta-server` or `manta-input` — confirmed via full-crate grep: zero `tracing`/`log` dependencies, and the only `eprintln!` in either crate is inside a `#[test]`. An operator investigating a suspected abuse incident (command flood, parser-probing, mass connection attempts) has no durable record of source IPs, login callsigns, or rejection events — only live-only Prometheus counters that reset on restart | **Filed — MAN-59**. Deliberately scoped as its own design decision (which logging crate, what to log, what verbosity) rather than improvised here |
| 10 | Spoofing | Telnet/DX-cluster login accepts any client-supplied callsign with no verification | **Accepted risk (Non-goal)** — matches the DX-cluster/RBN telnet protocol's own long-standing convention; CW Skimmer, SkimSrv, and Aggregator (the systems manta explicitly targets feature-parity with, per `docs/DECISIONS/2026-09-01-legacy-capability-matrix.md`) have the identical property. Adding manta-specific client authentication would make it *incompatible* with the ecosystem it's meant to interoperate with, not more secure in any way clients would actually benefit from |
| 11 | Information Disclosure | `manta-server`'s Prometheus `/metrics` endpoint (`metrics_http.rs`) is bound to the same interface as telnet/JSON (`0.0.0.0` by default, `config.rs`'s `default_bind_addr()`) with no authentication — any network peer that can reach the port sees connection counts, spot throughput, and uplink status | **Accepted risk, documented explicitly** — this already matches the exact "publicly bound, no auth" posture ARCHITECTURE.md §7 establishes for telnet/JSON themselves (the metrics module's own doc comment cites §7 for this reasoning), so it's a consistent design choice, not an undocumented deviation. Operational recommendation (not a code change) added to `docs/RUNBOOKS/` in this same PR: operators who don't want metrics reachable outside their LAN should set `bind_addr = "127.0.0.1"` or firewall the metrics port separately from the intentionally-public telnet/JSON ports |
| 12 | Tampering | `command.rs`'s integer parsing (`sh/dx/<n>`, `set dx filter unique > <n>`) | **Already covered** — both use `str::parse()` matched as `Result`, never `.unwrap()`; malformed/negative/overflowing input cleanly becomes `Command::Unknown`, explicitly regression-tested (`command.rs`) |
| 13 | DoS | Unbounded allocation via a client-supplied large `sh/dx/<n>` | **Already covered** — `SpotBus::recent(n)` only ever iterates its own `RECENT_HISTORY_CAP`-bounded (50) `VecDeque`; `n` cannot force allocation beyond that regardless of magnitude |
| 14 | DoS | Slow/lagging client causing unbounded server-side memory growth (backpressure) | **Already covered** — `tokio::sync::broadcast` with a fixed capacity; a lagging subscriber is disconnected on next `recv()`, never back-pressures the publisher (ARCHITECTURE.md §7: "slow clients are disconnected, never back-pressure the pipeline") |
| 15 | DoS | Per-client server-side state growth (filter state, history) | **Already covered** — verified `SpotBus` holds no per-client state at all; `occurrence_counts` is keyed by callsign (bounded by real-world callsign cardinality), not by client; per-connection filter state (`min_unique`) lives in the connection's own stack frame, bounded by `ConnectionLimiter` |

### Outbound RBN uplink (`crates/manta-server/src/uplink.rs`)

| # | Category | Finding | Disposition |
|---|---|---|---|
| 16 | DoS | Both of `uplink.rs`'s inbound reads (`connect_and_forward`'s login-prompt read, `forward_loop`'s post-login discard read) use plain unbounded `AsyncBufReadExt::read_line`, unlike every other network-facing read in this codebase (telnet, JSON/WS, metrics HTTP all route through `bounded_io`) — an unterminated long line from the configured target can grow a buffer without bound | **Filed — MAN-58** |
| 17 | DoS | The login-prompt read additionally has no timeout and isn't raced against the shutdown signal (unlike `forward_loop`'s main loop, which does use `tokio::select!` against `shutdown.changed()`) — a target that accepts the TCP connection but never sends a line hangs that connection attempt indefinitely, unresponsive to shutdown | **Filed — MAN-58** (same ticket, same root inconsistency) |
| 18 | Spoofing | No TLS, no server-identity verification of the configured RBN target; `target_host`/`target_port` are unvalidated operator config | **Accepted risk (Non-goal)** — plaintext telnet with no server auth is RBN's own protocol as used by Aggregator today (per the capability matrix); manta matches, doesn't regress, existing practice. `target_host`/`target_port` are operator-supplied config, not attacker-reachable at runtime, so this isn't a remotely-exploitable finding the way MAN-11/MAN-12's surfaces are — it's a config-trust assumption already implicit in running any daemon at all |
| 19 | Tampering | Non-UTF8 / binary garbage from the configured target | **Already covered** — `read_line` errors cleanly (`InvalidData`), mapped to a reconnect cycle with backoff, never a panic |

### Spot-line rendering (`crates/manta-server/src/rbn.rs`)

Output-only — formats an already-validated `Spot` into a string. No
untrusted-input parsing exists in this file. **Out of scope**, confirmed by
direct reading rather than assumed.

### `unsafe` usage

Zero occurrences in `crates/manta-server` or `crates/manta-input`
(full-crate grep, both directions). No memory-safety-adjacent surface to
threat-model beyond Rust's own guarantees.

## New follow-up tickets filed from this pass

| Ticket | Finding | Priority rationale |
|---|---|---|
| MAN-57 | Per-connection-only rate limiting lets one source IP multiply its effective command/ping budget by opening more connections (up to `ConnectionLimiter`'s 512x cap) | P3 — real gap, but bounded blast radius; not remotely as severe as MAN-22's crash bug |
| MAN-58 | Outbound RBN uplink's inbound reads are unbounded, and the login-prompt read additionally ignores shutdown and has no timeout | P2 — a genuine inconsistency with every other network read in this codebase already being bounded; realistic trigger is a misbehaving/compromised/MITM'd configured target, not an arbitrary internet client |
| MAN-59 | No logging/audit trail anywhere in `manta-server`/`manta-input` — no way to reconstruct an abuse incident after the fact | P3 — an observability gap, not an active vulnerability; matters specifically because MAN-22/MAN-23 establish manta as safe-to-expose, which makes "what happened after the fact" a real operational question |

## Accepted risks (recorded here, no ticket)

1. UDP source-IP spoofing against the HPSDR input (finding 2) — inherent to
   the OpenHPSDR/Hermes protocol; mitigated operationally by keeping SDR
   hardware on a trusted LAN segment, not by manta's own code.
2. A source-IP-spoofed flood of well-formed-shaped HPSDR datagrams (finding
   3) — generic to any UDP receiver; same LAN-trust boundary as above.
3. No client authentication on the telnet/DX-cluster login (finding 10) —
   matches CW Skimmer/SkimSrv/Aggregator's own long-standing convention;
   adding manta-specific auth would break ecosystem compatibility, manta's
   stated goal, for no real security benefit against a protocol nobody else
   authenticates either.
4. Unauthenticated, publicly-bound `/metrics` endpoint (finding 11) —
   consistent with the already-documented "publicly bound, no auth" design
   for telnet/JSON (ARCHITECTURE.md §7); operational mitigation (bind to
   loopback, or firewall separately) documented in `docs/RUNBOOKS/` in this
   PR, not treated as a code gap.
5. No TLS / server-identity verification on the outbound RBN uplink (finding
   18) — matches RBN's own plaintext-telnet protocol as used by Aggregator
   today; `target_host`/`target_port` are trusted operator config, not
   attacker-reachable input.

## Non-outcomes

- No application-code changes were made from this ticket directly — every
  finding requiring a code change is filed as its own ticket (MAN-57,
  MAN-58, MAN-59) per the ticket's own invariant against folding new
  findings into this ticket's scope.
- One operational documentation addition (metrics-endpoint bind-address
  recommendation, `docs/RUNBOOKS/`) is included in this same PR, since it's
  the same kind of artifact as this doc itself (no application code).
- MAN-13 (multi-source orchestration) does not exist yet and is therefore
  not covered by this pass — re-run this STRIDE walk once it lands, adding
  its own combination-specific surface (e.g. cross-source state shared
  between the orchestrated sources).
