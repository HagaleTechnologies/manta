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
  commit: 5b9e747
  date: 2026-09-04
links:
  - spot-validation
---
manta produces spots on two surfaces: a **telnet DX cluster server** (default :7300) emitting standard RBN-format `DX de` lines — the drop-in compatibility surface existing aggregators consume with zero changes — and a **JSON Lines stream** (TCP + WebSocket, default :7301) carrying full-fidelity spot objects for modern consumers like cqdx. This repo *produces* both; it consumes no external spot contract. Both are thin fan-out consumers of one broadcast channel — slow clients are dropped, never back-pressured, and at shutdown each client's queued backlog is drained best-effort under its own bounded deadline (ARCHITECTURE §7/§8). The formats and ports are described in ARCHITECTURE §7.

## Pointers

- RBN telnet format and the command grammar manta supports (`sh/dx`, filters): ARCHITECTURE §7. Ports and station-callsign spotter ID are TOML config keys (ARCHITECTURE §8).
- JSON spot schema: **the schema is an ecosystem contract that lives in the `dispensa` repo** (`contracts/spots/spots.v1.schema.json`, ADR-0011 — noted in CLAUDE.md and ARCHITECTURE §7), not solely in this repo. Do not restate fields here; the contract is authoritative.
- **Unresolvable geography for an allowlisted call**: MAN-28's Watch List allowlist can make the validator emit a spot for a callsign `cty.lookup` can't resolve. `dxContinent`/`dxCqZone` emit out-of-domain sentinels rather than the contract-forbidden `null` (those two fields are required/non-nullable on the wire); `dxLat`/`dxLon` are already nullable and are the contract-legal "unknown" signal. See `docs/DECISIONS/2026-09-04-man45-unresolved-geography-sentinels.md` for the full rationale and the cross-repo question proposed to dispensa.
- cqdx is the intended first-class JSON ingest consumer (README "Relationship to sibling projects"); the boundary is referenced across repos, not linked from this wiki.

Validated spots reaching these surfaces come from [[spot-validation]].
