---
id: uplink-health-observability
title: How does an operator check RBN uplink health without reading logs?
kind: interface
status: current
maintainer: agent
sources:
  - ARCHITECTURE.md
  - docs/DECISIONS/2026-09-04-man44-uplink-status-surface.md
  - docs/RUNBOOKS/uplink-health.md
verified:
  commit: 5b9e747
  date: 2026-09-04
links:
  - spot-output-contract
---
MAN-44 added `manta status`, a CLI subcommand that fetches `GET /status` (a JSON `StatusDoc`) from the same metrics HTTP listener that already serves `GET /metrics` — deliberately *not* the local-control-socket ARCHITECTURE §8 used to sketch as a someday design, because that's Unix-only and this project targets Windows too. `/status` reports per-`[[rbn_uplink]]`-target connection state, sent/suppressed/reconnect counts, and a derived health verdict (`connected`/`flapping`/`down`/`disabled`), plus daemon uptime and client counts. See the runbook (`docs/RUNBOOKS/uplink-health.md`) for how to read the output, and the ADR for why this surface over the alternatives.

## Two gotchas worth remembering

**A monotonic counter is not a health signal on its own — window it.** `uplink_reconnects_total` climbing tells you nothing about whether a target is *currently* unhealthy: 40 reconnects over 30 days is fine, 40 in 5 minutes is a stuck loop. `Metrics`/`UplinkTarget` (`crates/manta-server/src/metrics.rs`) keep the cumulative counter but ALSO track a rolling window (`RECONNECT_WINDOW` = 300s) of recent reconnect timestamps, and classify health from the windowed count, not the lifetime one. If you're ever tempted to add a new "is X ok" signal from a counter that only ever goes up, this is the pattern: cumulative for the record, windowed for the verdict.

**Derived aggregates, not parallel ones, once you have per-entity state.** MAN-42 originally kept one shared `uplink_connected_count: AtomicI64` alongside independent per-target connect/disconnect calls — a last-writer-wins bug (round-1 review finding) where one target's failed reconnect could clear the gauge while another was genuinely connected. MAN-44's fix wasn't "be more careful with the shared counter," it was structural: give each target its own `AtomicBool`, and make every *aggregate* (`uplink_sent_total`, `uplink_connected_count`, etc.) a live sum over the per-target registry instead of a second, independently-incremented number. Two parallel sources of the same fact will eventually disagree; deriving one from the other removes the class of bug rather than making it rarer.

## Pointers

- Design record and alternatives considered: `docs/DECISIONS/2026-09-04-man44-uplink-status-surface.md`.
- Operator-facing usage: `docs/RUNBOOKS/uplink-health.md`.
- Exposure posture (`/status` shares `/metrics`'s unauthenticated, `0.0.0.0`-by-default bind): `docs/RUNBOOKS/network-exposure.md`.
- Per-target registry, windowed health classification: `crates/manta-server/src/metrics.rs`. JSON document + human renderer: `crates/manta-server/src/status.rs`. CLI fetch/render: `crates/manta-cli/src/main.rs` (`Command::Status`).
