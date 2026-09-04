# MAN-64: metrics request-rate budget and `manta_source_health`'s failure transition

MAN-64 carries two findings deferred from PR #76's review round 7
(`chatgpt-codex-connector`), under the round-6-15 tier of
`docs/DECISIONS/2026-08-07-pr-review-convergence-policy.md` (a genuinely
new P2-or-lower finding is ticketed rather than triggering another live
review round). Both are re-verified against `main` at `5b9e747` below,
with one correction to Finding 1's own premise picked up along the way.

## Finding 1: `manta_source_health` is one-sided

> In every daemon session using metrics, the sole production call to
> `set_source_health` is `main.rs:1082`, which sets the selected source to
> `true`; a repo-wide search finds no path that ever sets it to `false`,
> and a fatal source read tears down the daemon instead. Listing
> `manta_source_health` as an implemented health metric therefore makes
> operators expect live failure reporting when it is only a startup-success
> marker. Document this limitation alongside `manta_active_tracks`, or wire
> failure transitions.

**Correction to the finding's own premise:** by the time this ticket was
worked, MAN-55 (HPSDR protocol hardening) had already added
`IqSource::confirmed_live_handle` (`crates/manta-input/src/lib.rs`), giving
`main.rs`'s `Command::Listen` handler a second call shape: a source whose
`open()` doesn't itself prove liveness (HPSDR's UDP `connect`/send needs no
peer response at all) starts health `false` and a spawned watcher flips it
`true` on the source's first genuinely valid packet. That is now three call
sites in one `match`, not the single always-`true` site the finding quotes
— and one of them does write `false`. This does not rescue the gauge: the
`false` is a one-time, startup-only "not yet confirmed" placeholder, not a
degradation signal. It fires once, the watcher task then exits, and nothing
anywhere re-arms it. The finding's substance — no live failure transition
exists, and a fatal read still tears the daemon down instead of recording
anything — stands unchanged.

### Split disposition

The finding names two dispositions ("document... or wire failure
transitions"). Applied separately, because the problem itself splits into
two different capabilities:

1. **Wired**: the one failure transition this daemon can actually observe.
   `manta_engine::listen` returning `Err` means the source read (or the
   pipeline behind it) failed fatally and the process is about to exit.
   `record_terminal_source_health` (`crates/manta-cli/src/main.rs`) writes
   `manta_source_health{source=...} 0` on that `Err`, called *before*
   `shutdown_tx.send(true)` in the `Command::Listen` shutdown sequence —
   ordering matters here: `metrics_http::serve` is spawned bare (no
   `ClientTasks`, no shutdown watch), so it keeps answering scrapes for the
   whole `SHUTDOWN_DRAIN_DEADLINE` (25s) drain window. Writing the `false`
   before the drain signal, rather than after or not at all, is what makes
   it observable to a scraper reaching the daemon during that window rather
   than purely cosmetic. Silent on the `Ok(())` path — a clean end of
   stream (file replay finishing, an operator's Ctrl-C) is normal
   termination, not a source failure, and flipping the gauge there would
   make it lie in the opposite direction.

2. **Documented as a known limitation, not fixed**: a source that degrades
   *while `listen` keeps running* still cannot be detected. `listen`
   exposes only `on_event`/`on_spot` callbacks
   (`crates/manta-engine/src/listen.rs`) — no per-read progress hook a
   staleness watchdog could watch. Building one is a cross-crate API change
   to `manta-engine`, and it belongs with the tickets that already need the
   identical hook: MAN-56 (input-layer overrun/gap metrics, which needs to
   observe reads at the same granularity) and MAN-13 (multi-source
   orchestration, the point at which a degraded source stops being
   architecturally fatal-to-the-daemon in the first place, making a live
   health signal for it actually meaningful). MAN-64 is filed P3 as an
   *observability accuracy* gap in the threat-model's own ticket-table
   entry — inventing a new engine-wide progress-hook API here would be a
   substantially larger and riskier change than what the finding asks for.
   **No separate ticket is filed for this deferral; this document, plus the
   `ARCHITECTURE.md` §8 note it corrects, is the record.** MAN-56/MAN-13
   are the named owners of the day this becomes buildable, not new work
   items created by this ticket.

`ARCHITECTURE.md` §8 already carried a "document this limitation" note for
this finding (dated 2026-09-03, filed as MAN-64) before this doc was
written — its conclusion was correct, but its supporting detail (one call
site, `main.rs:1082`, always `true`) had gone stale relative to MAN-55's
code. That note is updated in this same change to describe three call
sites, the new terminal-`false` transition, and what still isn't covered.

### What this does not fix

A source can still read `manta_source_health{...} 1` while silently
producing garbage or nothing at all, right up until either a fatal read
error or process exit — the same "opened, confirmed once, hasn't failed
fatally" reading as before this ticket, just now honest at the one moment
(fatal exit) it previously wasn't. Live staleness detection remains
entirely future work under MAN-56/MAN-13.

## Finding 2: metrics endpoint has no bound on completed-request rate

> When the metrics listener remains publicly reachable, a peer can rapidly
> reopen connections and send complete `GET /metrics` requests:
> `metrics_http::serve` spawns a task for every admitted socket and
> `handle_request` renders and writes the full metrics response, but the
> permit is released immediately when that response closes. The
> 64-connection cap and 30-second header deadline... bound simultaneous
> slow requests only, not aggregate completed-request rate, so an attacker
> can continuously drive task, formatting, TCP, and bandwidth work without
> occupying all permits.

Confirmed open with no partial mitigation: `ConnectionLimiter` (64 total)
and `IpQuota` (8/IP, MAN-61) both bound *simultaneous* admitted
connections; `HEADER_READ_TIMEOUT` (30s) bounds how long an *incomplete*
request may hold one of those permits. None of the three bounds a fast,
cooperative peer that opens, completes, and closes a request faster than
any of those deadlines are ever approached — `handle_request` releases its
permit the instant the response closes, so such a peer never accumulates
enough concurrent holds to be declined, while still costing one full
`render_prometheus_text()` render plus a TCP write per round trip. This is
the opposite failure mode from MAN-61's *quiet*, permit-holding client on
this same listener, and needs its own disposition.

### Options considered

1. **Per-source-IP budget on completed requests** — a third admission tier,
   the same shape MAN-57 already built and wired for telnet/JSON.
2. **A global (all-IP) request-rate ceiling** instead of per-IP.
3. **Accepted risk**, deferred to an operator's reverse proxy or firewall.

### Decision: option 1

Same reasoning MAN-57 and MAN-61 both recorded for this exact endpoint:
manta's default posture binds all three listeners publicly with no proxy
assumed (`ARCHITECTURE.md` §7, `docs/RUNBOOKS/network-exposure.md`), so an
accepted-risk-only disposition (option 3) would leave a default deployment
exactly as exposed as before this finding was raised — not an improvement,
just a note. Option 2 (a global ceiling) was set aside because it creates
exactly the failure mode MAN-61 exists to prevent: one abusive source could
exhaust a shared budget and deny every *other*, legitimate scraper, which
is a worse availability story than the one this finding describes. A
per-IP budget bounds the abusive source's own request rate without letting
it affect anyone else's.

The mechanism, its override shape, its LRU entry cap, and its stale-entry
reaper are not new: `rate_limit::IpRateLimiter` already exists and is
already reviewed (MAN-57, MAN-68). This is a wiring job onto a third
listener, not a new primitive.

### Implementation

`crates/manta-server/src/metrics_http.rs`:

- `MAX_METRICS_REQUESTS_PER_IP = 60` per `METRICS_REQUEST_RATE_WINDOW =
  60s`. Sized against the tightest realistic scrape load: a 5s-interval
  Prometheus scraper is 12 requests/min, so 60/min leaves roughly 5x
  headroom for several independent scrapers or a federation setup sharing
  one source IP, while still cutting a flood from "whatever the network
  allows" to one request/second sustained. A 60s window (rather than
  telnet's 10s) because scrape traffic is periodic at minute granularity —
  a short window with a small count would false-positive whenever two
  independent scrapers' intervals happened to align.
- `serve` and `handle_request` both take a new `ip_request_limiter:
  IpRateLimiter` parameter, cloned into each connection's spawned task the
  same way `connection_log_limiter` already is.
- The check runs in `handle_request` on every **completed** request —
  including one that goes on to get a `404` — immediately after the header
  block is confirmed non-EOF, before rendering or matching the path. The
  finding names "task, formatting, TCP, and bandwidth work" as the cost
  being driven; the unit of that cost is a request, not a successful
  scrape, so metering only `GET /metrics` would hand a prober the identical
  task/socket/write cost for free. This matches `telnet.rs`'s command
  budget, which charges a line whether or not it parses into a known
  command. Header-read failures are deliberately **not** charged: they
  never reach this point at all, and `IpQuota` + `HEADER_READ_TIMEOUT`
  (MAN-61) already bound them.
- An over-budget request gets a complete `429 Too Many Requests` response
  with a `Retry-After: 60` header, `Content-Length: 0`, and `Connection:
  close` — not a bare socket close, which an operator's scraper would read
  as a network fault rather than "you are over budget, retry later." The
  three response shapes (200/404/429) share one `write_response` helper so
  they can't drift apart.
- The rejection is logged once, lazily, through the same
  `connection_log_limiter` every other warn site in this file already uses
  (MAN-59 rounds 4/5's "one budget, no second limiter to forget to wire"
  rule) — **not** a new, second `IpRateLimiter` instance. The handler
  returns `Ok(())` after logging and writing the `429`, not `Err`:
  `serve`'s task-boundary catch-all logs every `Err` it sees, and the
  double-logging that produces on a rejection path is exactly what
  `5b9e747` (this branch's own parent commit) removed from telnet/WS —
  returning `Err` here would reintroduce the identical bug on a third
  listener in the same PR that fixed it on the other two.
- **Deliberately no per-connection `RateLimiter` tier**, unlike telnet/JSON
  (MAN-57): this endpoint answers exactly one request per connection
  (`Connection: close` on every response), so a per-connection budget would
  be a budget of one and bound nothing the shared per-IP tier doesn't
  already bound on its own. The shared, IP-keyed `IpRateLimiter` is the
  entire mechanism here.
- `ServerConfig.metrics_max_requests_per_ip: Option<u32>`
  (`crates/manta-server/src/config.rs`), same `None`-default /
  `Some(0)`-disables / `Some(n)`-overrides shape as every other per-listener
  override in this file, threaded through `IpRateLimiter::new_with_override`
  and wired in `crates/manta-cli/src/main.rs` beside a
  `spawn_stale_entry_reaper` call — required, not optional, since
  `IpRateLimiter` entries have no release event the way `IpQuota`'s guard
  does; without the reaper a long-uptime daemon accumulates one entry per
  distinct scraper IP for the life of the process.

### What this does not fix

A single source behind many distinct IPs is not bounded by a per-IP budget
— the same inherent limitation MAN-61's own decision doc already names for
the sibling connection-quota fix on this listener. This closes the
"one source, one IP, fast and cooperative" shape the finding describes, not
distributed request floods.

## References

- Threat model: `docs/DECISIONS/2026-09-02-man23-threat-model.md`, row 11a
  and the MAN-64 ticket-table row
- Nearest precedents: `docs/DECISIONS/2026-09-03-man57-per-ip-rate-limit.md`,
  `docs/DECISIONS/2026-09-03-man61-per-ip-connection-quota.md`
- Code: `crates/manta-server/src/metrics_http.rs`,
  `crates/manta-server/src/rate_limit.rs`,
  `crates/manta-server/src/config.rs`,
  `crates/manta-server/src/metrics.rs`, `crates/manta-cli/src/main.rs`
- Operator docs: `docs/RUNBOOKS/network-exposure.md`, `ARCHITECTURE.md` §8
