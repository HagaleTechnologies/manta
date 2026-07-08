---
id: spot-output-contract
title: What spot-output contracts does skimmer expose (telnet RBN + JSON)?
kind: interface
status: current
maintainer: agent
sources:
  - ARCHITECTURE.md
  - README.md
  - CLAUDE.md
verified:
  commit: e68b106
  date: 2026-07-07
links:
  - spot-validation
---
skimmer produces spots on two surfaces: a **telnet DX cluster server** (default :7300) emitting standard RBN-format `DX de` lines — the drop-in compatibility surface existing aggregators consume with zero changes — and a **JSON Lines stream** (TCP + WebSocket, default :7301) carrying full-fidelity spot objects for modern consumers like cqdx. This repo *produces* both; it consumes no external spot contract. Both are thin fan-out consumers of one broadcast channel — slow clients are dropped, never back-pressured. The formats and ports are described in ARCHITECTURE §7.

## Pointers

- RBN telnet format and the command grammar skimmer supports (`sh/dx`, filters): ARCHITECTURE §7. Ports and station-callsign spotter ID are TOML config keys (ARCHITECTURE §8).
- JSON spot schema: **the schema is an ecosystem contract that belongs in the `dispensa` repo** (JSON Schema, ADR pending — noted in CLAUDE.md and ARCHITECTURE §7), not solely in this repo. When it lands, this page should point at the dispensa Q-id in plain text.
- cqdx is the intended first-class JSON ingest consumer (README "Relationship to sibling projects"); the boundary is referenced across repos, not linked from this wiki.

## Status caveat

The JSON schema is **not yet frozen in dispensa** — treat the field set as design-phase until the ADR lands. Do not restate fields here; the contract, once written, is authoritative. Validated spots reaching these surfaces come from [[spot-validation]].
