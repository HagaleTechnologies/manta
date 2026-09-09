---
id: spot-output-contract
title: What spot-output contracts does manta expose (telnet RBN + JSON)?
kind: interface
status: current
maintainer: agent
sources:
  - ARCHITECTURE.md
  - README.md
  - CLAUDE.md
  - docs/DECISIONS/2026-09-07-man86-aggregator-sett-handshake.md
verified:
  commit: c57458a
  date: 2026-09-09
links:
  - spot-validation
---
manta produces spots on two surfaces: a **telnet DX cluster server** (default :7300) emitting standard RBN-format `DX de` lines — the drop-in compatibility surface existing aggregators consume with zero changes — and a **JSON Lines stream** (TCP + WebSocket, default :7301) carrying full-fidelity spot objects for modern consumers like cqdx. This repo *produces* both; it consumes no external spot contract. Both are thin fan-out consumers of one broadcast channel — slow clients are dropped, never back-pressured. The formats and ports are described in ARCHITECTURE §7.

## Pointers

- RBN telnet format and the command grammar manta supports (`sh/dx`, filters, `SKIMMER/SETT`, `BYE`): ARCHITECTURE §7. Ports and station-callsign spotter ID are TOML config keys (ARCHITECTURE §8).
- **Aggregator compatibility is a gate, not a nicety** (MAN-86): RBN's Aggregator will not forward spots from a source that never answers `SKIMMER/SETT`, so the greeting banner, the `SETT` reply and the `BYE` close are an RBN *admission requirement* on this surface, not a convenience — that is the one thing worth knowing before you touch it. Every wire detail (what the banner lines contain, the `SETT` reply grammar, the close text) is normative in `docs/DECISIONS/2026-09-07-man86-aggregator-sett-handshake.md`, together with the four primary sources it was reconstructed from, and summarized in ARCHITECTURE §7 — read it there; this page deliberately does not restate it. The operator identity the banner reports comes from the `[server]` `operator_name`/`operator_qth`/`operator_grid` keys in the canonical config-key table (docs/SPEC-decode-core.md §9).
- JSON spot schema: **the schema is an ecosystem contract that belongs in the `dispensa` repo** (JSON Schema, ADR pending — noted in CLAUDE.md and ARCHITECTURE §7), not solely in this repo. When it lands, this page should point at the corresponding ADR in plain text.
- cqdx is the intended first-class JSON ingest consumer (README "Relationship to sibling projects"); the boundary is referenced across repos, not linked from this wiki.

## Status caveat

The JSON schema is **not yet frozen in dispensa** — treat the field set as design-phase until the ADR lands. Do not restate fields here; the contract, once written, is authoritative. Validated spots reaching these surfaces come from [[spot-validation]].
