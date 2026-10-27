# MAN-320: manta relays a local skimmer's own spots and WSJT-X's FT8/FT4 CQs through the uplink

**Status:** accepted. `[[skimmer_relay]]` and `[[wsjtx_relay]]` are implemented; the wire layout of a relayed spot rests on MAN-90's open claims C12 and C13.

## Context

MAN-320 has two Gherkin scenarios. In the first, a second CW skimmer serves
spots by telnet on the LAN, and once it is added to manta as a source its
spots go out through manta's uplink under that skimmer's own callsign. In
the second, WSJT-X sends decodes to manta by UDP, and a decode that arrives
goes out as a digital-mode spot.

The research for the ticket (2026-10-27, commit `5e0e497`) found:

- **Both scenarios were recorded non-goals.** The
  [legacy capability matrix](2026-09-01-legacy-capability-matrix.md)
  disposed "Combines spots from external CW Skimmer/SkimSrv/... processes
  over telnet" (row 86) and "FT4/FT8 UDP monitoring" (row 99) as
  **Non-goal**, and README's Non-goals said "Not a cluster network. `manta`
  is a spot source, not an aggregator." and "Not a general digital-mode
  skimmer. FT8 and RTTY are out of scope for 1.0". Neither is phrased as
  permanent: the FT8 line says "for 1.0", and the cluster line is about
  manta *being an aggregator*.
- **"Secondary skimmer" already means the opposite data flow.** In MAN-96
  ([field-node record](2026-10-07-man96-secondary-skimmer-field-node.md),
  [runbook](../RUNBOOKS/secondary-skimmer-field-node.md)) manta is the
  secondary skimmer *behind* an RBN Aggregator, which reads manta's telnet
  server. MAN-320 has manta connect *out* to another skimmer and forward
  that skimmer's spots.
- **One spotter identity per process.** Every wire surface renders the
  configured `station_callsign`, or a target's `login_callsign`; no spot
  carries its own spotter, and `rbn::format_line` hardcodes `CW` and
  `<n> WPM`.
- **No path onto the bus bypasses the validator.** `SpotBus::publish` is fed
  only by `Validator::ingest`, which is built for manta's own noisy Morse
  text. A spot from another decoder is already finished output.
- **No UDP or WSJT-X code existed.** The protocol (WSJT-X's `NetworkMessage`,
  `Status` and `Decode`) came from external sources listed under Evidence.

## Decisions

- **D1. Build both scenarios, and narrow the two non-goals rather than
  delete them.** The ticket asks for the capability, and with D3 and D6
  manta forwards only the operator's own local stations' spots and still
  decodes no FT8. README now reads "Not a cluster network. `manta` relays
  its own spots, and those of the operator's own local skimmers and WSJT-X
  (MAN-320), to RBN; it takes no spots from other stations and serves no
  relayed spots to clients." and "Not a general digital-mode skimmer. manta
  decodes no FT8 or RTTY ... It can forward WSJT-X's FT8/FT4 CQ decodes
  through its uplink (MAN-320)." The MAN-314 RTTY sentence stays. The
  capability matrix rows 86 and 99 become **Covered — MAN-320**, keeping
  their correction history. `readme_non_goals_name_the_relay_scope` pins
  both lines.
- **D2. The tables are `[[skimmer_relay]]` and `[[wsjtx_relay]]`, never
  "secondary".** "Secondary skimmer" is MAN-96's opposite direction. Two
  tables rather than one with a `kind` key: each has exactly its own keys
  under `deny_unknown_fields`, which keeps the scaffold-lists-the-loader's-keys
  test meaningful and needs no "this key does not apply to this kind"
  validation. There is no `name` key: a skimmer relay is labelled
  `host:port` and a WSJT-X relay by its `listen` address, as
  `[[rbn_uplink]]` targets are labelled `host:port` (MAN-128 D7). A
  duplicate block is a load error, so labels are unique without `#N`
  suffixes.
- **D3. A skimmer relay forwards only its own skimmer's spots.** A line is
  forwarded only if its `DX de` spotter (uppercased, `-#` stripped, the
  `ak1a.rs` `normalize_spotter` rule) equals the relay's `callsign`; other
  lines count as `other_spotter`, and the first one in a session logs a
  warning. Without this, a relay pointed at a DX cluster or at RBN's own
  relay would re-submit other stations' spots. The relay's `callsign` may
  not be `[server].station_callsign` or any `[[rbn_uplink]].login_callsign`;
  otherwise a relay pointed at manta's own telnet port would forward
  manta's spots twice.
- **D4. Relayed spots reach the uplink only.** They travel on a fourth
  `SpotBus` channel (`publish_relay`/`subscribe_relay`, capacity 1024, no
  history, no occurrence count), which only the uplink subscribes to.
  Telnet, the JSON/WebSocket stream, `--json` stdout, the live-state stream,
  `[rbn_compare]` and `record-spots`/`shadow-compare` never see one. The
  spots.v1 contract fixes `source: "skimmer"`, `mode: "CW"` and `deCall` =
  the station (`docs/SPOT-SCHEMA.md`), the field-node scripts count JSON
  spots as the node's own (MAN-289), and serving relayed spots on telnet
  would make manta the cluster node README rules out. Determinism is
  untouched: the relay never feeds the decode pipeline or the spot
  channels.
- **D5. Relayed spots skip manta's validator.** They are another decoder's
  finished output. The relay applies structural checks only: a finite,
  positive frequency, and a spotted call of 3–20 ASCII letters, digits and
  `/` with at least one letter and one digit, so nothing can inject spaces
  or CR/LF into the uplink line (`bad_call`). D3's spotter match and, for
  WSJT-X, D7's gates follow. A skimmer's lines are forwarded as that
  skimmer deduplicated them; manta adds no CW dedupe, and none across
  relays or between a relay and manta's own spots, since different
  spotters are different RBN reporters.
- **D6. The uplink treats a relayed spot like its own, apart from the
  spotter.** `forward_loop` takes both channels through one gate sequence:
  `spot_types`, then `dry_run`, then the per-target `max_spots_per_minute`
  ceiling, which bounds the total sent to a target. A relayed spot is
  rendered by `rbn::format_relay_line` with its own spotter
  (`DX de W1ABC-2-#:`), never the target's login call; the login line is
  unchanged. The loss accounting drains both receivers on every exit.
  Spots published while a target is disconnected are lost to it, as
  manta's own are.
- **D7. WSJT-X: forward new FT8/FT4 `CQ` decodes only.** RBN takes only CQ
  spots from FT8 forwarders and culls duplicates (CWSL_DIGI's README), and
  RBN's digital feed carries FT8/FT4. The gates run in this order, each
  with its `manta_relay_dropped_total` reason: `bad_datagram`, `replay`
  (`New` false), `off_air` (`.wav` playback), `low_confidence`, `no_dial`
  (no `Status` from that instance `Id` yet, or dial 0), `mode` (the
  instance's latest `Status` mode is not `FT8` or `FT4`), `not_cq`,
  `no_call` (no caller, or a hashed `<...>` call), `duplicate` (same call,
  mode and band within 600 s, the window manta's own dedupe uses). The mode
  comes from `Status`, not from `Decode`'s mode field, whose wire value
  this ticket's research could not confirm. WSJT-X sends a `Status` when
  decoding starts or stops, so an instance is known within one cycle.
  Several instances can share one listener; state is per `Id`, at most 64
  instances, the oldest-heard evicted first. The frequency is the dial
  frequency plus the decode's audio offset.
- **D8. SNR is forwarded verbatim.** A skimmer's `dB` figure is already in
  the 500 Hz convention of manta's own telnet line (broad-review D3).
  WSJT-X's SNR is in WSJT-X's own 2500 Hz reference, the number FT8
  operators know; converting it would mislabel it. CLAUDE.md's SNR
  constraint names this exception so a later change does not "fix" it.
- **D9. Time is arrival time.** As `rbn_compare::feed` does for RBN's
  `HHMMZ`, a relayed spot is stamped with the wall clock when manta
  received it, through the injectable `rbn_compare::Clock`. A WSJT-X
  decode arrives within about 15 s of its period start, inside the
  `HHMMZ` minute in most cases.
- **D10. The skimmer relay's telnet behaviour.** It uses the uplink's and
  the RBN feed's bounded connect, resolver slot and 1 s→60 s backoff. The
  tolerant login read moves from `rbn_compare/feed.rs` into a shared
  `telnet_client.rs`, so CW Skimmer's newline-less
  `Please enter your callsign: ` prompt (MAN-86 Source 1) gets the login.
  The relay sends `callsign` plus CRLF, then reads bounded lines with no
  idle timeout, since a skimmer on a dead band can be silent for hours. To
  notice a vanished peer it writes a blank CRLF line every 300 s, as N1MM's
  cluster keepalive does (MAN-99 pinned manta's own server answering blank
  lines harmlessly); a failed or timed-out (10 s) keepalive write ends the
  session. It sends no `SKIMMER/SETT` and no password. The parser reads
  both RBN's layout (with the mode column) and CW Skimmer's and manta's
  `skimmer` layout (without it); only `CW`/`WPM` lines are forwarded, and a
  well-formed line in another mode counts as `not_cw`.
- **D11. No new dependency edge.** `manta-cli`, which already depends on
  `socket2`, binds each `[[wsjtx_relay]]` socket as `manta-input`'s ka9q
  source does: `SO_REUSEADDR` (and `SO_REUSEPORT` on unix) plus
  `join_multicast_v4` for an IPv4 group bound on `0.0.0.0:port`, and a plain
  bind for unicast, so a taken port is a clear startup error. The bound
  `std::net::UdpSocket` is handed to the `manta-server` worker. No
  `Cargo.toml` or `Cargo.lock` changes, so no CODEOWNERS path is touched.
  IPv6 multicast is rejected at load.
- **D12. Config rules.** Both tables are arrays; a bare `[skimmer_relay]`
  is an error. `enabled` defaults to `true`, since adding the block is the
  opt-in. Both are restart-only, which the generic restart-only diff gives
  (a SIGHUP reloads only `spot.*` lists and `rbn_uplink[N].dry_run`). An
  enabled relay needs at least one enabled `[[rbn_uplink]]`, which in turn
  needs `[server]`; the rule lives in `manta-cli`'s `load()`, so `check`,
  `run`, `doctor`, `config check` and a SIGHUP reload all enforce it. The
  environment cannot set these keys. `callsign` follows the MAN-89 grammar
  (`check_operator_callsign`), is uppercased, and rejects the `N0CALL`
  example as `login_callsign` does. A `[[wsjtx_relay]]` may use the
  station's own call, since FT8 spots are a different mode from manta's
  CW. `config check` prints `skimmer_relay:` and `wsjtx_relay:` lines
  (`none` without blocks).
- **D13. Two new open claims in MAN-90's table.** C12: a collector accepts,
  on one connection, `DX de` spotters other than the login call. C13: a
  digital spot is an RBN-layout line with `FT8`/`FT4` in the mode column,
  WSJT-X's SNR and a blank speed field, type at column 63 and time at
  column 71. Both are `open`, like C1–C11, because nothing has captured
  RBN's ingest ([MAN-90](2026-10-11-man90-uplink-protocol-verification.md),
  [MAN-319](2026-10-19-man319-uplink-registration-exchange.md)).
  `man90_claims_table_carries_the_man320_claims` pins the rows.
- **D14. Observability is metrics plus logs.** Three `/metrics` families
  (`manta_relay_up`, `manta_relay_spots_total`, `manta_relay_dropped_total`,
  labelled `relay`, `kind` and, for drops, `reason`; see
  [METRICS.md](../METRICS.md)), the relay log lines, and the `config check`
  lines. `/status`, the status line, `/healthz` and `manta doctor` do not
  change.

## What the collector receives

The uplink's AK1A RBN layout, with the relay's spotter:

```
DX de W1ABC-2-#: 14027.10 JA1ABC         CW    30 dB  28 WPM  CQ      0312Z
DX de W1ABC-#:  14075.52  K2DEF          FT8   -12 dB         CQ      1234Z
DX de W1ABC-#:   7075.75  JA1XYZ         FT4    3 dB          CQ      0007Z
```

Every field after the identity is anchored to the previous field's real
end, so the blank speed field keeps the type at column 63 and the time at
column 71 with no new constants.

## Evidence

Run in the plan session at `5e0e497` on 2026-10-27:

1. `rbn_compare::ak1a::parse_dx_line` reads manta's RBN-layout line but
   returns `None` for its `skimmer` layout
   (`DX de W1XYZ-2-#: 14027.10 JA1ABC           30 dB  28 WPM  CQ      0312Z`),
   and `DxLine` carries no SNR or spot type, so the relay needed its own
   tolerant parser (D10).
2. `manta config check` on a config with a relay table failed with
   `unrecognized table [relay_source] ... manta reads [server], [[rbn_uplink]], [rbn_compare], [input], [spot], [detector], [decode]`
   (exit 1): the config surface was closed.
3. A Python transcription of `rbn::format_line`'s column rules reproduced
   `LIVE_RBN_CAPTURE` byte for byte and produced the three lines above,
   type at column 63 and time at 71. The `rbn.rs` tests pin the real bytes.
4. `cargo test -p manta-server --locked` and the `scaffold*`,
   `metrics_reference_lists_every_rendered_family` and
   `man90_claims_table_carries_the_man319_claims` tests were green before
   the change.
5. Not available: a real CW Skimmer, a real WSJT-X, and any capture of
   RBN's ingest. The WSJT-X protocol came from the n4af/TR4W wiki
   ["WSJT-X UDP Interface API"](https://github.com/n4af/TR4W/wiki/WSJT-X-UDP-Interface-API),
   [k0swe/wsjtx-go](https://pkg.go.dev/github.com/k0swe/wsjtx-go), a
   [JS8Call `NetworkMessage.h` mirror](https://js8call.com/JS8Call-improved/d3/d3a/NetworkMessage_8h_source.html)
   and [GridTracker's multicast setup](https://docs.gridtracker.org/latest/Appendices/Appendix-B-Configuring-WSJT-X-and-JTDX-for-GridTracker.html).
   RBN's FT8 practice came from the
   [RBN FT8 announcement](https://reversebeacon.net/pages/FT8+Announcement+40)
   and [CWSL_DIGI's README](https://github.com/alexranaldi/CWSL_DIGI)
   ("Only CQ spots are submitted to RBN, and duplicates are culled"). Not
   confirmed externally, and designed around: `Decode`'s mode wire value,
   the heartbeat interval, JTDX field differences and a captured RBN FT8
   telnet line.

## Follow-ups

1. A `/status` `relays` array and a status-line segment (D14).
2. A `manta doctor` TCP probe for each `[[skimmer_relay]]`, as `doctor`
   already probes each `[[rbn_uplink]]` target.
3. `SKIMMER/SETT` and password login, for a CW Skimmer that requires them
   (D10).
4. RTTY relays from RTTY Skimmer Server; WSPR (`WSPRDecode`) and other
   digital modes; non-CQ FT8 messages (D7).
5. Settle MAN-90 C12 and C13 from the Aggregator→RBN capture
   ([runbook](../RUNBOOKS/aggregator-rbn-capture.md)), then correct the
   FT8 line layout if the capture differs: one function in `rbn.rs` plus
   its test vectors.

## Related

- [MAN-90 uplink protocol verification](2026-10-11-man90-uplink-protocol-verification.md):
  the claims table C12 and C13 extend.
- [MAN-96 secondary-skimmer field node](2026-10-07-man96-secondary-skimmer-field-node.md):
  the opposite direction (manta behind an Aggregator).
- [MAN-86 Aggregator SETT handshake](2026-09-07-man86-aggregator-sett-handshake.md):
  CW Skimmer's telnet greeting and prompt.
- [Legacy capability matrix](2026-09-01-legacy-capability-matrix.md):
  rows 86 and 99.
- [Broad-review decisions](2026-09-06-broad-review-decisions.md): D1 (the
  Aggregator path stays the supported route into RBN), D3 (SNR reference).
- [`docs/CONFIG.md`](../CONFIG.md) for both tables and
  [network exposure](../RUNBOOKS/network-exposure.md) for the WSJT-X
  listener.
