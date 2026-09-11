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
manta produces spots on two surfaces: a **telnet DX cluster server** (default :7300) emitting standard RBN-format `DX de` lines — the drop-in compatibility surface existing aggregators consume with zero changes — and a **JSON Lines stream** (TCP + WebSocket, default :7301) carrying full-fidelity spot objects for modern consumers like cqdx. This repo *produces* both; it consumes no external spot contract. Both are thin fan-out consumers of one broadcast channel — slow clients are dropped, never back-pressured, and at shutdown each client's queued backlog is drained best-effort under its own bounded deadline (ARCHITECTURE §7/§8). The formats and ports are described in ARCHITECTURE §7.

## Pointers

- RBN telnet format and the command grammar manta supports (`sh/dx`, filters): ARCHITECTURE §7. Ports and station-callsign spotter ID are TOML config keys (ARCHITECTURE §8).
- JSON spot schema: **the schema is an ecosystem contract that lives in the `dispensa` repo** (`contracts/spots/spots.v1.schema.json`, ADR-0011 — noted in CLAUDE.md and ARCHITECTURE §7), not solely in this repo. Do not restate fields here; the contract is authoritative.
- **Unresolvable geography for an allowlisted call**: MAN-28's Watch List allowlist can make the validator emit a spot for a callsign `cty.lookup` can't resolve. `dxContinent`/`dxCqZone` emit out-of-domain sentinels rather than the contract-forbidden `null` (those two fields are required/non-nullable on the wire); `dxLat`/`dxLon` are already nullable and are the contract-legal "unknown" signal. See `docs/DECISIONS/2026-09-04-man45-unresolved-geography-sentinels.md` for the full rationale and the cross-repo question proposed to dispensa.
- cqdx is the intended first-class JSON ingest consumer (README "Relationship to sibling projects"); the boundary is referenced across repos, not linked from this wiki.

## Status caveat

A schema now exists in dispensa and rejects malformed batches — this is no longer design-phase. `dxDxcc`/`deDxcc`/`dxContinent`/`deContinent`/`dxCqZone` are required and non-nullable on it; every spot carries real or named-sentinel values for all five (never `null`), per `docs/DECISIONS/2026-09-07-man136-dxcc-and-unknown-geography-sentinels.md`, which is the authoritative record of what each field means when a callsign doesn't resolve — see that doc and ARCHITECTURE §7 rather than restating the field set here. `snrRefHz` (MAN-102 / decision D3) is not yet part of that frozen schema — see the SNR section below and `docs/DECISIONS/2026-09-07-man102-snr-reference-and-estimator.md` for the proposed fragment. Validated spots reaching these surfaces come from [[spot-validation]].

## SNR reference bandwidth differs by surface (MAN-102 / decision D3)

The telnet/RBN-uplink surface and the JSON surface quote SNR in two different reference bandwidths, on purpose — not restating the field set (see above), just flagging that the two numbers for the same spot are not directly comparable. Telnet/uplink render the 500 Hz bandwidth RBN and CW Skimmer use; JSON keeps the pipeline's native 2500 Hz measurement plus an explicit reference-bandwidth field. See `docs/DECISIONS/2026-09-07-man102-snr-reference-and-estimator.md` for the measurements and the dispensa schema-fragment proposal.

## Third surface: outbound RBN uplink

manta can also act as a telnet *client*, logging into an RBN spot-collection endpoint and forwarding its own spots there (`crates/manta-server/src/uplink.rs`, one task per `[[rbn_uplink]]` config entry) — the mirror direction of the telnet server above. It ships **dry-run by default** (MAN-159): an entry with no `dry_run` key connects and logs in but transmits nothing, and each target logs its mode at startup. See README's "Outbound RBN uplink" section for the config shape; the uplink itself is unverified against a real RBN ingest pending MAN-90.
