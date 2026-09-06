# MAN-57: per-source-IP aggregate command/Ping rate budget

MAN-57 (found during MAN-23's threat-modeling review) flagged that
`RateLimiter` (telnet's command budget, JSON/WS's Ping budget) is
instantiated fresh per connection with zero cross-connection state. A
source opening N connections gets N independent full budgets, so the
effective per-IP rate is `N x max_per_window`, not the intended
single-connection budget. MAN-61's per-IP connection quota
(`docs/DECISIONS/2026-09-03-man61-per-ip-connection-quota.md`) bounds N
itself (16 for telnet/JSON), so the practical worst case is `16x`, not
unbounded — a real gap, but a bounded one.

## Options considered (per the ticket's own technical notes)

1. **IP-keyed shared rate-limiter map.** A second, shared limiter checked
   in addition to each connection's own, aggregating events by source IP
   across every connection that source holds.
2. **Defer to a reverse-proxy/firewall layer**, documenting it as the
   intended operational mitigation instead of an in-process fix.

## Decision: IP-keyed shared rate-limiter map

Chosen for the same reason MAN-61 chose an in-process quota over
proxy-only mitigation: this codebase's default posture is directly
publicly bound (`docs/DECISIONS/2026-09-02-man23-threat-model.md`,
findings 11/20) — a real deployment has no proxy in front of it unless an
operator specifically sets one up (`docs/RUNBOOKS/network-exposure.md`).
An in-process fix protects that default deployment; a proxy-only answer
would leave it exactly as exposed as today.

`rate_limit::IpRateLimiter` (`crates/manta-server/src/rate_limit.rs`) is a
`Clone`-able, `Arc`-backed sibling to the existing per-connection
`RateLimiter`: `allow(ip)` records one event against that source's
aggregate window and returns whether it's still in budget, checked in
addition to (never instead of) each connection's own `RateLimiter::allow()`
call. Wired into `telnet::serve`/`json_stream::serve` as a shared instance
constructed once and cloned into every connection handler.

**Same budget as the single-connection limiter**, not a separate
configurable value: `MAX_TELNET_COMMANDS`/`COMMAND_RATE_WINDOW` and
`MAX_INBOUND_PINGS`/`PING_RATE_WINDOW`'s own stated intent was always
"this many events per source in this window" — opening more connections
was never meant to multiply it. Reusing the existing constants avoids a
second, independently-tunable budget with no clear reason to diverge from
the first.

## Follow-on: the exact reverse-proxy override gap MAN-61 already hit

MAN-61 round 1 shipped a single shared connection-quota override, then
round 3 corrected it to per-listener fields after realizing a shared
knob would also disable protection on listeners never actually behind
the proxy. `IpRateLimiter` would have shipped with the identical
un-overridable gap if left as constructed above: behind the documented
reverse-proxy JSON/WS deployment, every downstream client shares the
proxy's own IP as far as `peer.ip()` is concerned, so the shared aggregate
budget would throttle every legitimate client behind the proxy combined
into ONE 30-commands-per-10s (or 10-Pings-per-60s) budget — a sharper
false positive than the connection quota's, given how much tighter these
rate windows are.

Added `IpRateLimiter::new_with_override(default, window, override_val)`,
same override shape as `tasks::IpQuota::new_with_override`
(`Some(0)` = no per-IP aggregate cap, `Some(n)` = explicit override,
`None` = default), wired through two new independent `ServerConfig`
fields (`telnet_max_commands_per_ip`, `json_max_pings_per_ip` — no
`metrics_*` field: the metrics listener has no rate-limited client input
at all). `docs/RUNBOOKS/network-exposure.md` updated with the same
"only override the listener(s) actually behind the proxy" guidance
MAN-61's runbook section already gives for the connection quota.

## Follow-on: unbounded per-IP map growth

`IpQuota`'s entries self-remove when a connection's guard drops to zero
holders. `IpRateLimiter`'s entries have no such release event — a source
that sent one event and never again would otherwise sit in the map for
the life of the process, growing it without bound across ordinary
connection churn from many distinct real client IPs over a long uptime
(the same class of gap MAN-62 raised for `SpotBus::occurrence_counts`).
Added `rate_limit::spawn_stale_entry_reaper`, mirroring
`tasks::spawn_reaper`'s periodic-drain pattern: every 60s, evicts entries
whose window hasn't reset in over `2 * window`, i.e. genuinely idle, not
just mid-window.
