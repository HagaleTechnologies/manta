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
  commit: 01d1ea1
  date: 2026-09-07
links:
  - spot-validation
---
manta produces spots on two surfaces: a **telnet DX cluster server** (default :7300) emitting standard RBN-format `DX de` lines — the drop-in compatibility surface existing aggregators consume with zero changes — and a **JSON Lines stream** (TCP + WebSocket, default :7301) carrying full-fidelity spot objects for modern consumers like cqdx. This repo *produces* both; it consumes no external spot contract. Both are thin fan-out consumers of one broadcast channel — slow clients are dropped, never back-pressured, and at shutdown each client's queued backlog is drained best-effort under its own bounded deadline (ARCHITECTURE §7/§8). The formats and ports are described in ARCHITECTURE §7.

## Pointers

- RBN telnet format and the command grammar manta supports (`sh/dx`, filters, `SKIMMER/SETT`, `BYE`): ARCHITECTURE §7. Ports and station-callsign spotter ID are TOML config keys (ARCHITECTURE §8).
- **Aggregator compatibility is a gate, not a nicety** (MAN-86): RBN's Aggregator will not forward spots from a source that never answers `SKIMMER/SETT`, so the greeting banner, the `SETT` reply and the `BYE` close are an RBN *admission requirement* on this surface, not a convenience — that is the one thing worth knowing before you touch it. Every wire detail (what the banner lines contain, the `SETT` reply grammar, the close text) is normative in `docs/DECISIONS/2026-09-07-man86-aggregator-sett-handshake.md`, together with the four primary sources it was reconstructed from, and summarized in ARCHITECTURE §7 — read it there; this page deliberately does not restate it. The operator identity the banner reports comes from the `[server]` `operator_name`/`operator_qth`/`operator_grid` keys in the canonical config-key table (docs/SPEC-decode-core.md §9).
- Gotcha (MAN-87): IAC (telnet option negotiation) is `0xFF`, never valid UTF-8 — a strictly-UTF-8 line reader on the telnet listener rejects any real client that negotiates on connect (Windows `telnet.exe`, PuTTY telnet mode). manta strips and refuses negotiation before UTF-8 validation runs; see `docs/DECISIONS/2026-09-07-man87-telnet-iac-policy.md` for the normative design.
- What an operator may spell in `station_callsign`/`login_callsign` — including RBN's per-band `CALL-N` SSID, which the de-side identity on both surfaces carries verbatim while the `cty.dat` geography lookup behind `deContinent`/`deLat`/`deLon` sees it stripped: `docs/DECISIONS/2026-09-07-man-89-station-callsign-ssid-grammar.md`. That record, not this page, is authoritative for the grammar; the server still appends its own `-#`.
- JSON spot schema: **the schema is an ecosystem contract that lives in the `dispensa` repo** (`contracts/spots/spots.v1.schema.json`, ADR-0011 — noted in CLAUDE.md and ARCHITECTURE §7), not solely in this repo. Do not restate fields here; the contract is authoritative.
- **Unresolvable geography for an allowlisted call**: MAN-28's Watch List allowlist can make the validator emit a spot for a callsign `cty.lookup` can't resolve. `dxContinent`/`dxCqZone` emit out-of-domain sentinels rather than the contract-forbidden `null` (those two fields are required/non-nullable on the wire); `dxLat`/`dxLon` are already nullable and are the contract-legal "unknown" signal. See `docs/DECISIONS/2026-09-04-man45-unresolved-geography-sentinels.md` for the full rationale and the cross-repo question proposed to dispensa.
- `decoderVersion` is build identity, not decoder output: `manta-<crate version>+<commit>` (MAN-83), the commit as SemVer build metadata. A consumer comparing decoder behaviour across releases compares the part before `+`; the whole string names the exact binary. When decoder output may change between versions is normative in `docs/DECISIONS/2026-10-10-man83-build-identity-and-decoder-versioning.md` (summarised in `CHANGELOG.md`).
- cqdx is the intended first-class JSON ingest consumer (README "Relationship to sibling projects"); the boundary is referenced across repos, not linked from this wiki.

## Status caveat

A schema now exists in dispensa and rejects malformed batches — this is no longer design-phase. `dxDxcc`/`deDxcc`/`dxContinent`/`deContinent`/`dxCqZone` are required and non-nullable on it; every spot carries real or named-sentinel values for all five (never `null`), per `docs/DECISIONS/2026-09-07-man136-dxcc-and-unknown-geography-sentinels.md`, which is the authoritative record of what each field means when a callsign doesn't resolve — see that doc and ARCHITECTURE §7 rather than restating the field set here. `snrRefHz` (MAN-102 / decision D3) is not yet part of that frozen schema — see the SNR section below and `docs/DECISIONS/2026-09-07-man102-snr-reference-and-estimator.md` for the proposed fragment. Validated spots reaching these surfaces come from [[spot-validation]].

## SNR reference bandwidth differs by surface (MAN-102 / decision D3)

The telnet/RBN-uplink surface and the JSON surface quote SNR in two different reference bandwidths, on purpose — not restating the field set (see above), just flagging that the two numbers for the same spot are not directly comparable. Telnet/uplink render the 500 Hz bandwidth RBN and CW Skimmer use; JSON keeps the pipeline's native 2500 Hz measurement plus an explicit reference-bandwidth field. See `docs/DECISIONS/2026-09-07-man102-snr-reference-and-estimator.md` for the measurements and the dispensa schema-fragment proposal.

## Third surface: outbound RBN uplink

manta can also act as a telnet *client*, logging into an RBN spot-collection endpoint and forwarding its own spots there (`crates/manta-server/src/uplink.rs`, one task per `[[rbn_uplink]]` config entry) — the mirror direction of the telnet server above. It ships **dry-run by default** (MAN-159): an entry with no `dry_run` key connects and logs in but transmits nothing, and each target logs its mode at startup. See README's "Outbound RBN uplink" section for the config shape; the uplink itself is unverified against a real RBN ingest pending MAN-90.

## Audio-sourced spot frequencies (MAN-34)

A spot's `freq_hz` on either surface is only absolute (real RF) when its source has an RF reference. The rig-audio input mode (`listen`/`listen --device`, `AudioIqSource`) has none of its own — pass `--dial-freq-hz` (or call `AudioIqSource::with_center_freq_hz` from library code) or its reported frequencies are bare baseband offsets, not RBN-compatible. `--dial-freq-hz` is added to the decoded audio-tone offset as-is, so it must be the suppressed-carrier/USB dial reading — on a CW-mode dial display, subtract your sidetone pitch first, or spots read high by the pitch amount. See `docs/DECISIONS/2026-09-05-man-34-audio-rf-reference.md`.
