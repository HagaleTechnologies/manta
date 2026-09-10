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
verified:
  commit: 01d1ea1
  date: 2026-09-07
links:
  - spot-validation
---
manta produces spots on two surfaces: a **telnet DX cluster server** (default :7300) emitting standard RBN-format `DX de` lines — the drop-in compatibility surface existing aggregators consume with zero changes — and a **JSON Lines stream** (TCP + WebSocket, default :7301) carrying full-fidelity spot objects for modern consumers like cqdx. This repo *produces* both; it consumes no external spot contract. Both are thin fan-out consumers of one broadcast channel — slow clients are dropped, never back-pressured. The formats and ports are described in ARCHITECTURE §7.

## Pointers

- RBN telnet format and the command grammar manta supports (`sh/dx`, filters): ARCHITECTURE §7. Ports and station-callsign spotter ID are TOML config keys (ARCHITECTURE §8).
- What an operator may spell in `station_callsign`/`login_callsign` — including RBN's per-band `CALL-N` SSID, which the de-side identity on both surfaces carries verbatim while the `cty.dat` geography lookup behind `deContinent`/`deLat`/`deLon` sees it stripped: `docs/DECISIONS/2026-09-07-man-89-station-callsign-ssid-grammar.md`. That record, not this page, is authoritative for the grammar; the server still appends its own `-#`.
- JSON spot schema: **the schema is an ecosystem contract that belongs in the `dispensa` repo** (JSON Schema, ADR pending — noted in CLAUDE.md and ARCHITECTURE §7), not solely in this repo. When it lands, this page should point at the corresponding ADR in plain text.
- cqdx is the intended first-class JSON ingest consumer (README "Relationship to sibling projects"); the boundary is referenced across repos, not linked from this wiki.

## Status caveat

A schema now exists in dispensa and rejects malformed batches — this is no longer design-phase. `dxDxcc`/`deDxcc`/`dxContinent`/`deContinent`/`dxCqZone` are required and non-nullable on it; every spot carries real or named-sentinel values for all five (never `null`), per `docs/DECISIONS/2026-09-07-man136-dxcc-and-unknown-geography-sentinels.md`, which is the authoritative record of what each field means when a callsign doesn't resolve — see that doc and ARCHITECTURE §7 rather than restating the field set here. Validated spots reaching these surfaces come from [[spot-validation]].
