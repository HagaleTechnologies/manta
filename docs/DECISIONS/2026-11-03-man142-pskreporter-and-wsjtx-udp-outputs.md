# MAN-142: manta reports spots to PSKReporter and sends them to LAN loggers as WSJT-X UDP

**Status:** accepted. `[pskreporter]` and `[[wsjtx_output]]` are implemented and tested against fake local listeners written from the published formats. A live PSKReporter report and a real logger's rendering are external steps (below).

## Context

MAN-142 has two Gherkin scenarios. In the first, an operator enables
PSKReporter reporting, and each spot manta validates is also reported to
pskreporter.info in its expected format. In the second, an operator enables
a UDP output on the local network, and each validated spot is sent in the
WSJT-X UDP packet format, which loggers already listen for. The ticket notes
that CW Skimmer offers a PSKReporter checkbox. It came from the 2026-09-05
broad review, lens 2.

The 2026-10-31 scope note narrows acceptance. Both outputs are tested
against fake local listeners, and CI sends no live reports. Nothing public
happens until an operator enables it. Steps only the owner can do are
external, not acceptance criteria.

- **PSKReporter's format.** PSKReporter's developer page
  (<https://pskreporter.info/pskdev.html>, read 2026-11-02) documents a
  simplified IPFIX (RFC 5101) over UDP to `report.pskreporter.info:4739`,
  with a test listener on 14739. It sets the pacing rules: a call at most
  once per five minutes, a datagram at most once per five minutes, templates
  in the first few datagrams and then about hourly, and a fixed source port
  for the session. WSJT-X's `Network/PSKReporter.cpp` (read 2026-11-02)
  shows the seven-field sender template the production server accepts.
- **WSJT-X's format.** WSJT-X's `Network/NetworkMessage.hpp` (schema 3,
  read 2026-11-02) defines the `Heartbeat`, `Status`, `Decode` and `Close`
  messages, and the `Off air` flag on decodes "that result from playing back
  a .WAV file".
- **What already existed.** `SpotBus::subscribe()` hands every validated
  spot to any number of readers, as the RBN uplink reads it. `[server]`
  already resolves the station's grid (`ServerConfig::station_location`,
  MAN-138). MAN-320's `[[wsjtx_relay]]` already reads WSJT-X datagrams, and
  MAN-141's N1MM+ feed is a finished UDP sender with fake-listener tests.

## Decisions

| # | Decision | Reason |
|---|---|---|
| D1 | Two new tables, both file-only and restart-only. `[pskreporter]` is a single table. `[[wsjtx_output]]` is an array of tables. Table order: `server, rbn_uplink, rbn_compare, skimmer_relay, wsjtx_relay, pskreporter, wsjtx_output, callsign_lookup, input, spot, detector, decode`. The environment overlay rejects `MANTA_PSKREPORTER_*` and `MANTA_WSJTX_OUTPUT_*` by name. | A station has one public report stream, but one daemon can feed a unicast logger and a multicast group at once. A switch that publishes the station's spots belongs in the reviewed file, like `[[rbn_uplink]]`. |
| D2 | `[pskreporter].enabled` defaults to `false`. `[[wsjtx_output]].enabled` defaults to `true` once the block exists, and `destination` has no default. | Reports are public, as `[rbn_compare]`'s feed is (MAN-301), so they need an explicit switch. Writing a WSJT-X block with a LAN address is the opt-in, as for the sibling `[[wsjtx_relay]]` (MAN-320), and that output is not public. "Nothing public until an operator enables it" holds either way. |
| D3 | An enabled table needs `[server]`. Both outputs read `SpotBus::subscribe()`. `start_spot_server` subscribes each receiver before spawning anything, and spawns each worker into the tracked `ClientTasks` set. | `[server]` is where validated spots and the session epoch live, and MAN-34's dial guard then guarantees RF frequencies. Subscribing first means no spot published as decoding starts is missed. The shutdown drain awaits tracked tasks, so each worker can send its last datagram. |
| D4 | `[pskreporter].spot_types` reuses `UplinkSpotTypes` (default `cq_beacon`, or `all`). `[[wsjtx_output]]` sends every spot type. | One policy decides what leaves the box for public collectors. The WSJT-X `Decode` text says which type a spot is (D12). |
| D5 | PSKReporter pacing. A per-(call, band) gate of 300 s, keyed on the upper-cased call and `band_for_freq_hz`, timed by the spot's own Unix time, admits the first spot and drops later ones as `rate_gate`. The gate map holds at most 20000 entries and drops entries older than 300 s at each flush. The flush timer starts with the worker: the first flush at 300 s plus `session_nonce % 30` seconds, then every 300 s. A datagram goes out early when the next report would take it past 1400 bytes. When the hourly templates come due after the pending reports were sized without them, a datagram carries only the reports that fit beside the templates: at a flush the rest go in a second datagram at once, and after an early send they wait for the next one. Nothing is sent when nothing is pending. Templates ride in the first three datagrams and in the first datagram 3600 s or more after the last one that carried them. At shutdown the worker drains its receiver and sends what is pending. | These are the developer page's own rules. The gate also absorbs manta's dedupe re-emissions (a 6 dB SNR jump or a type change). The offset spreads many manta nodes over half a minute. WSJT-X also sends what is pending when it stops. |
| D6 | The receiver options template is the page's four-field descriptor (`receiverCallsign`, `receiverLocator`, `decodingSoftware`, `antennaInformation`, scope count 1) with template ID `0x6D72`. The antenna field is always present, empty when unset. The sender template is WSJT-X's seven-field layout (`senderCallsign`, 5-byte `frequency`, 1-byte `sNR`, `mode`, `senderLocator`, 1-byte `informationSource`, 4-byte `flowStartSeconds`) with template ID `0x6D73`. Strings are cut to 254 bytes on a character boundary. | A fixed layout never needs a template change mid-session. WSJT-X's layout carries SNR without inventing an `iMD`, and the production server demonstrably accepts it. manta's own template IDs keep its templates apart from any other program's cached ones, as the page advises. |
| D7 | The IPFIX header carries version 10, the export time from the system clock, a sequence number counting the reports sent in earlier datagrams, and an observation domain ID folded from the session nonce, constant for the session. | RFC 5101 section 3.1 counts records, not packets, and the page says the same. A constant domain ID ties one session's datagrams together. |
| D8 | `receiverCallsign` is `[server].station_callsign` without its `-N` SSID, upper-cased. `receiverLocator` is `station_location().grid`, required for an enabled table (from `operator_grid`, or `operator_lat`/`operator_lon`). `decodingSoftware` is `manta <version>` without the git SHA. `informationSource` is 1. `senderLocator` is empty. | PSKReporter places every report at the receiver's locator. Leaving out the SHA lets PSKReporter's statistics page group one release. Value 1 ("automatically extracted") is the only one the page says counts. By MAN-138, manta never invents a grid for another station. |
| D9 | `host` defaults to `report.pskreporter.info` and `port` to 4739. IPv4 only: an IPv6 host is a load error. `manta-cli` binds the socket once in `start_spot_server`, before anything is spawned: loopback for a loopback host, the IPv4 wildcard otherwise. The worker resolves `host` when its first datagram is due and again with each hourly template resend, and keeps the first IPv4 address. Each lookup may take at most 5 s. A failed re-lookup keeps the last address that worked. A failed lookup with no address yet drops that datagram's reports as `resolve_error` and warns once. A templated datagram that does not reach the OS leaves the templates due for the next one. | PSKReporter publishes no AAAA record. Startup then needs no DNS, and the source port stays fixed for the session, as the page requires. |
| D10 | On a file replay, `[pskreporter]` starts only when `host` is loopback (`127.0.0.0/8` or `localhost`). Otherwise `manta run` prints a note and does not start it. A run with several inputs counts as a replay when any input is a file. `[[wsjtx_output]]` runs on a replay and sets `Off air` on every `Decode`. | A replay's spots carry the recording's times, and PSKReporter has no dry-run to protect it. `[callsign_lookup]` (MAN-318) is the precedent for a public service staying quiet on replay. A loopback host keeps the end-to-end test possible. WSJT-X defines `Off air` for exactly this case. |
| D11 | Both outputs send the pipeline's 2500 Hz SNR, rounded, with no 6.99 dB shift; PSKReporter's is clamped to -128..127. | Each ecosystem these outputs enter (PSKReporter's FT8 and WSPR reporters, WSJT-X) uses 2500 Hz. The 500 Hz figure is an RBN convention (MAN-102). Reviewer question 1. |
| D12 | WSJT-X messages: `Heartbeat` (0) the first time an `Id` is used and every 15 s for every `Id` used so far; `Status` (1) before a `Decode` whose dial or mode differs from that `Id`'s last `Status`, and after every heartbeat; `Decode` (2) once per spot; `Close` (6) per `Id` at shutdown. `Id` is `manta-<band>` (`manta-unknown` outside the bands). `Status` is the complete master-schema layout. The dial is the spot's frequency rounded down to a kHz, and the `Decode` delta frequency is the rest. `Decode.mode` is `CW` or `RTTY`. The message is `CQ <call>` for a CQ spot and `DE <call>` otherwise. `Low confidence` is a tentative spot. There is no writer for `QSO Logged` (5) or `Logged ADIF` (12). | One instance per band means a `Status` and its `Decode` never interleave with another band's. Readers of any schema-3 version find every `Status` field. Dial plus delta is how manta's own relay reads the RF frequency. Loggers highlight `CQ` callers as callable (reviewer question 3). Types 5 and 12 make a logger write a contact into the log. |
| D13 | `destination` is a required socket address. Port 0, the unspecified addresses and IPv6 multicast are load errors. `manta-cli` binds each socket with `bind_udp_for(destination.ip())` before anything is spawned, and sets `SO_BROADCAST` on every IPv4 socket. Multicast TTL and loopback stay at the OS defaults (1 and on). Sends are unconnected `send_to`. | `SO_BROADCAST` makes a LAN broadcast address work; without it Linux refuses the send. TTL 1 keeps multicast on the local segment. An unconnected send to a closed port is never an error (MAN-141's finding). |
| D14 | `[[wsjtx_relay]]` ignores every datagram whose `Id` starts with `manta-`. It counts them as `manta_relay_dropped_total{reason="own_output"}`, creates no instance and does not mark the relay up. `wsjtx_proto::Message::Other` keeps its `Id`. | Operators share one multicast group between WSJT-X, a logger and `[[wsjtx_relay]]`, as `docs/CONFIG.md` recommends, and will point `[[wsjtx_output]]` at it too. Without the guard manta's own heartbeat would hold the relay "up". |
| D15 | Metrics `manta_pskreporter_reports_total`, `manta_pskreporter_datagrams_total`, `manta_pskreporter_dropped_total{reason}` (`spot_type`, `rate_gate`, `lagged`, `resolve_error`, `send_error`), `manta_wsjtx_output_spots_total{destination}`, `manta_wsjtx_output_datagrams_total{destination}` and `manta_wsjtx_output_dropped_total{destination,reason}` (`lagged`, `send_error`). They are registered before the metrics listener starts, at 0 for a configured table (enabled or not, and a PSKReporter table a replay turned off), and absent without the table. No `/healthz` impact. | MAN-44's reason for registering early. UDP gives no delivery signal, so "handed to the OS" is the strongest claim either output can make. Every loss is counted by reason. |
| D16 | Tests use fake local listeners only. Every test socket binds loopback (MAN-467). The replay-guard test uses `host = "192.0.2.1"` (TEST-NET-1, never routed), and the resolve test uses `pskreporter.invalid` (RFC 6761, never resolves). | The scope note. Even a broken replay guard could not reach PSKReporter from a test. |
| D17 | Docs in the same change: this record, `docs/CONFIG.md`, `docs/METRICS.md`, [GUIDES/pskreporter-and-lan-loggers.md](../GUIDES/pskreporter-and-lan-loggers.md) (checked by `operator_guides.rs`), `docs/RUNBOOKS/network-exposure.md`, README, `ARCHITECTURE.md`, `CHANGELOG.md` under Added, and one `wiki/INDEX.md` row. No capability-matrix row and no CLAUDE.md change. | The doc-consistency tests require the CONFIG and METRICS rows. The matrix cites only each program's published documentation, and the CW Skimmer checkbox comes from the ticket, not from CW Skimmer's manual. CLAUDE.md's Status section sends a ticket's outcome to its decision record. |

Spots are unchanged with and without the outputs. Both are readers of the
spot bus, so telnet, the JSON stream, the RBN uplink and `decode --json` stay
byte-identical, and the CHANGELOG entry is under Added.

## The datagrams

The first PSKReporter datagram of a session for receiver W5AU at EM10dh with
one report (W1AW, 14012340 Hz, 21 dB, CW, at Unix time 1793650021) is 176
bytes:

```
header (16 B):          00 0A 00 B0 6A E8 EE 9A 00 00 00 00 12 34 AB CD
receiver template (44): 00 03 00 2C 6D 72 00 04 00 01 80 02 FF FF 00 00 76 8F 80 04 FF FF 00 00 76 8F
                        80 08 FF FF 00 00 76 8F 80 09 FF FF 00 00 76 8F 00 00
sender template (60):   00 02 00 3C 6D 73 00 07 80 01 FF FF 00 00 76 8F 80 05 00 05 00 00 76 8F
                        80 06 00 01 00 00 76 8F 80 0A FF FF 00 00 76 8F 80 03 FF FF 00 00 76 8F
                        80 0B 00 01 00 00 76 8F 00 96 00 04
receiver record (32):   6D 72 00 20 04 57 35 41 55 06 45 4D 31 30 64 68 0B 6D 61 6E 74 61 20 30
                        2E 31 2E 30 00 00 00 00
sender record (24):     6D 73 00 18 04 57 31 41 57 00 00 D5 CF B4 15 02 43 57 00 01 6A E8 ED 65
```

Without templates the same datagram is 72 bytes. About 57 reports of
6-character calls fit in 1400 bytes with templates.

The same spot as WSJT-X datagrams (here from a replay, so `Off air` is set):

```
Heartbeat (54 B): ad bc cb da 00 00 00 03 00 00 00 00 00 00 00 09 6d 61 6e 74 61 2d 32 30 6d 00 00 00 03
                  00 00 00 05 30 2e 31 2e 30 00 00 00 0c 64 31 64 64 66 39 39 61 33 36 63 65
Decode (65 B):    ad bc cb da 00 00 00 03 00 00 00 02 00 00 00 09 6d 61 6e 74 61 2d 32 30 6d 01 04 51 0e 88
                  00 00 00 15 00 00 00 00 00 00 00 00 00 00 01 54 00 00 00 02 43 57 00 00 00 07 43 51 20
                  57 31 41 57 00 01
```

The `Decode` ends `00 01`: low confidence false, off air true. A live spot
ends `00 00`. The full-layout `Status` for this spot is 114 bytes.

## Evidence

The tests:

- `crates/manta-server/src/pskreporter/ipfix.rs`:
  `rebuilds_the_developer_pages_example_datagram_byte_for_byte` rebuilds the
  page's first complete example (172 bytes) from the module's general
  template code. Further tests pin manta's 44-byte receiver template, its
  60-byte sender template, the 176-byte first datagram above, the 72-byte
  templateless datagram, string truncation, the 5-byte frequency, the SNR
  clamp and set padding.
- `crates/manta-server/src/pskreporter/batch.rs`: the 300 s gate per call
  and band, a new band as a new report, an empty flush, templates in the
  first three datagrams and then hourly, the early flush before 1400 bytes,
  the split flush and early send when the hourly templates come due, the
  sequence number, `spot_types`, and the 20000-entry bound.
- `crates/manta-server/src/pskreporter/mod.rs`: the worker on tokio's paused
  clock against a loopback fake collector. Nothing goes out before the first
  flush, templates ride in datagrams 1 to 3 and not 4, a re-emitted call is
  reported once, shutdown sends what is pending, a spot published before the
  task first runs is reported, a closed port is not an error,
  `pskreporter.invalid` counts `resolve_error`, and the counters match.
- `crates/manta-server/src/wsjtx_output.rs`: the 54-byte `Heartbeat` and
  65-byte `Decode` above, every message read back by the relay's own parser,
  the 114-byte `Status`, band, dial and offset mapping, message text, low
  confidence and the time of day. Worker tests against a loopback fake
  logger cover the message order, a `Status` only when the kHz changes, one
  `Id` per band, the 15 s heartbeat, `Close` at shutdown, only types 0, 1, 2
  and 6, lag and send-error counting, and the relay ignoring this output.
  Those that check what the logger reads run in real time and pause the
  clock only across the 15 s wait, when nothing reads the socket: on a
  paused clock macOS could time out before it delivered a datagram
  (MAN-478).
- `crates/manta-server/src/relay/wsjtx.rs`:
  `mantas_own_output_is_counted_and_never_marks_the_relay_up`,
  `a_real_wsjtx_instance_still_marks_the_relay_up` and
  `own_output_applies_to_wsjtx_relays_only`.
- `crates/manta-cli/src/spot_outputs.rs`: the replay guard, the bind
  addresses, `SO_BROADCAST` and the startup lines.
- `crates/manta-cli/src/config.rs`, `config_cmd.rs` and
  `crates/manta-cli/tests/config_command.rs`: the load errors, the
  environment rejection, the `config check` lines and the scaffold.
- `crates/manta-cli/tests/pskreporter.rs` and
  `crates/manta-cli/tests/wsjtx_output.rs`: `manta run` on a 30 s V1 replay
  with a loopback fake collector and a loopback fake logger, each with a
  reader written from the published format and sharing no code with
  manta's encoders.

From the plan phase, in a cloud container on 2026-11-02:

- A probe written from the developer page rebuilt its first example byte for
  byte (172 bytes) and gave the bytes above. The page's descriptor-free
  example spells the mode bytes `50 53 4C` ("PSL"), a typo beside the first
  example's `50 53 4B`, so only the first example is a fixture.
- A throwaway spike fed manta-built WSJT-X datagrams to the real
  `relay::wsjtx::run`. Before D14, manta's heartbeat marked the relay up.
- Without `SO_BROADCAST`, `send_to` 127.255.255.255 returned
  `PermissionDenied` on Linux; with it the send succeeded and a listener on
  `0.0.0.0` received it. The default multicast TTL was 1.
- `report.pskreporter.info` resolved to one IPv4 address and no IPv6
  address. Nothing was sent to it.

## Limits

- No report has been sent to PSKReporter, including its test listener on
  14739. Whether the production server accepts manta's templates and
  records is unverified.
- No real logger has been tested. How GridTracker, JTAlert and Log4OM show
  a `CW` instance, `CW` in `Decode.mode` (WSJT-X itself sends a one-character
  mode code there), `DE <call>` text and `Off air` decodes is unverified.
- manta sends nothing when nothing is pending, with no receiver-only
  keep-alive datagram. A quiet manta may drop off PSKReporter's list of
  active monitors.
- The 2500 Hz SNR reference for PSKReporter is an assumption (D11).
- The CW Skimmer PSKReporter checkbox comes from the ticket and a
  second-hand review, not from CW Skimmer's published documentation.
- Only CW (and RTTY) spots go out. FT8 and FT4 spots (MAN-323 left
  `SpotBus::publish_ftx` without a production caller) and relayed spots do
  not. A later ticket that wires FT8/FT4 decides whether they join, and must
  avoid double-reporting with WSJT-X.
- macOS and Windows are covered only by CI.

## Reviewer questions, with the defaults built

1. Which SNR reference should PSKReporter reports carry? Built: manta's
   native 2500 Hz figure. The other choice is RBN's 500 Hz convention,
   6.99 dB higher. It is one constant (D11).
2. Is 300 s per call per band the right PSKReporter gate? Built: 300 s, the
   page's hard rule. The page's ideal is once an hour per call unless it
   changed band, which would cut reports about twelvefold for an hour of CQs
   but hide a second CQ session within the hour.
3. What message text should a non-CQ spot carry in `Decode`? Built: `CQ
   <call>` only for a CQ spot, and `DE <call>` for DE, beacon and untyped
   spots, so loggers highlight as callable only stations manta heard calling
   CQ.

## External steps

These need a station the owner controls. They are not acceptance criteria
(scope note, 2026-10-31).

1. Send one report to PSKReporter's test listener (`[pskreporter] port =
   14739`) and read PSKReporter's packet-analysis page, to confirm the
   server parses manta's templates and records.
2. Enable live reporting on one node for a session, and confirm the reports
   appear on the map under the station call.
3. Point `[[wsjtx_output]]` at GridTracker or JTAlert, and see how they show
   a `CW` instance, a `CW` `Decode`, `DE <call>` text and `Off air` decodes.
4. Decide reviewer questions 1 to 3 if the defaults turn out wrong on air.

## Related

- [GUIDES/pskreporter-and-lan-loggers.md](../GUIDES/pskreporter-and-lan-loggers.md):
  the operator guide.
- [MAN-320 relays](2026-10-27-man320-relay-local-skimmers-and-wsjtx.md): the
  WSJT-X reader, the `[[wsjtx_relay]]` table and sockets bound in
  `manta-cli`.
- [MAN-141 N1MM+ spectrum feed](2026-11-01-man141-n1mm-spectrum-feed.md): a
  UDP sender and the fake-listener rule.
- [MAN-323 FT8/FT4](2026-11-01-man323-ft8-ft4-from-band-streams.md): why no
  FT8/FT4 spots go out yet.
- [MAN-138 station location](2026-10-11-man138-station-location.md): the
  grid PSKReporter reports at, and no invented grids for other stations.
- [MAN-102 SNR reference](2026-09-07-man102-snr-reference-and-estimator.md):
  the 2500 Hz and 500 Hz references.
- [MAN-318 callsign lookup](2026-10-27-man318-callsign-lookup.md) and
  [MAN-301 RBN comparison](2026-10-11-man301-live-rbn-comparison.md): a public
  service quiet on replay, and off by default.
- MAN-467: test sockets bind loopback.
