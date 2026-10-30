# MAN-90: the direct RBN uplink's protocol is unverified — known gaps and the capture that would close them

**Status:** accepted (interim). Verification open: no Aggregator→RBN capture exists yet.

## Context

`crates/manta-server/src/uplink.rs` (MAN-32, multi-target MAN-42) logs into a
configured collector as a client and writes RBN-layout `DX de` lines. Until
MAN-90 its module doc described the target as "RBN's own spot-collection
endpoint" receiving spots "in the same `DX de` wire format" as manta's telnet
server. That was a design assumption. The uplink has only ever run against
in-process mock collectors.

The 2026-09-05 broad review found no public source for a direct ingest a node
can log into (lens 2 #24, lens 4 #2). The only documented way into RBN is an
RBN Aggregator reading a CW Skimmer-compatible telnet server. Broad-review
[D1](2026-09-06-broad-review-decisions.md) therefore makes manta's
Aggregator-compatible telnet server (MAN-86..89) the primary route into RBN,
and D2 asks for the uplink to be checked against real Aggregator↔RBN traffic
captured on a consenting operator's node.

That capture needs a consenting human operator, their live Aggregator and a
vantage point on their network. This record is what can be settled without
it: what manta sends, what each part of that assumes, and what a capture has
to show to confirm or correct each assumption. The procedure is
[the capture runbook](../RUNBOOKS/aggregator-rbn-capture.md).

## Which traffic

The capture target is **Aggregator's outbound connection to RBN's server**:
the leg manta's uplink imitates. Read literally, the ticket's Gherkin
("Aggregator … talking to a real Skimmer/SkimSrv") describes the inbound leg,
Aggregator reading a Skimmer's telnet port. MAN-86..89 already cover that leg
(`SKIMMER/SETT`, banner, AK1A layout, `CALL-N-#`). Broad-review D2 and lens-4
#2 both name the outbound leg into RBN, so that is what this record and the
runbook target. The runbook also asks for Aggregator's Skimmer Traffic tab for
the same window, which pairs each spot Aggregator read with the bytes it sent.

## What public sources say (retrieved 2026-10-11)

RBN, "Using Aggregator with the Reverse Beacon Network v6.0"
(<https://cms.reversebeacon.net/sites/cms.reversebeacon.net/files/2019/12/21/Using%20Aggregator%20-%20v6.0.pdf>,
HTTP 200, 867,578 bytes, sha256
`bc4acdb4fead0d6a08c6f71533fc6fdbcaf0023a48366d5d0a92b6289cc44647`):

- §2.0: "Forwards selected spots to the RBN server via an Internet connection."
- §4.3: "see if Aggregator has successfully contacted the RBN by looking at
  the "RBN Connection Status" box".
- §4.3: "A check in the second check box will prevent Aggregator from sending
  any spots to the RBN."
- §6.2.11: "This shows the mode Aggregator sends to the RBN server for this
  spot."

RBN's Aggregator page (<https://www.reversebeacon.net/pages/Aggregator+34>,
current version 6.7) says only that Aggregator "receives spots from CW
Skimmer or Skimmer Server's telnet port, and send the spots to the Reverse
Beacon Network".

Nothing names a host, port, transport, login or line format for RBN's ingest.

§6.2.11, together with §6.2.3 (CQ/NCQ) and §6.2.6 (Bcn/NBcn), shows that
Aggregator computes per-spot fields itself, so what it submits may not be the
Skimmer's line passed through. That is a concrete reason C4 below may differ.

The AK1A layout manta uses (`rbn.rs`, [MAN-88](2026-09-06-man88-ak1a-column-layout.md))
was measured on RBN's *outbound* relay, `telnet.reversebeacon.net:7000`, the
side loggers connect to in order to read spots. It says nothing about ingest.

## What manta sends

Pinned by `the_uplink_session_bytes_match_the_man90_reference` in
`crates/manta-server/tests/uplink_acceptance.rs`, and checked against this
file by `man90_records_list_every_open_claim_and_link_each_other` in
`crates/manta-cli/tests/docs_consistency.rs`:

```text
collector → manta : any one newline-terminated line (content ignored), e.g. login: \r\n
manta → collector : W3XYZ\r\n
manta → collector : DX de W3XYZ-#:  14027.10  JA1ABC         CW    30 dB  28 WPM  CQ      2213Z\r\n
```

Station W3XYZ; one CQ spot: JA1ABC, 14027.10 kHz, SNR 23 dB at 2500 Hz
rendered as 30 dB at 500 Hz, 28 WPM, 2023-11-14 22:13Z. Nothing else: no
password, no keepalive, and no reply to anything the collector sends after
login (those lines are discarded, at most 30 per 60 s).

The line is always `LineFormat::Rbn`, whatever `[server].line_format` says.
`spot_types` (MAN-91) decides which spots are written; `dry_run` (default
`true`, MAN-159) holds all of them back.

## Known gaps

Each row is one claim the uplink makes about RBN's ingest. IDs are stable:
a capture updates the Status cell, it never renumbers.

| ID | manta assumes | Evidence today | A capture settles it by showing | Status |
|---|---|---|---|---|
| C1 | RBN runs an ingest a node can log into directly | None. RBN documents Aggregator as the only submitter. | The remote endpoint(s) Aggregator holds open to RBN, and whether any is a login service. | open |
| C2 | Plain TCP carrying CRLF-terminated ASCII | None | Transport (TCP or UDP), whether it is TLS, and whether it is text or binary. | open |
| C3 | Read one line, then send the login callsign and CRLF; no password | None; this mirrors manta's own telnet server | The first bytes in each direction after connect. | open |
| C4 | Spots in the RBN relay's AK1A layout (`LineFormat::Rbn`) | Layout measured on RBN's outbound relay (MAN-88), not on ingest. Aggregator computes its own mode and CQ fields (§6.2.3, §6.2.11). | The bytes of one forwarded spot, paired with its Skimmer Traffic row. | open |
| C5 | The spotter field is the station call plus `-#` (`CALL-N-#` with an SSID, MAN-89) | Relay convention only | How a submitted spot names its spotter, or whether identity comes from the login or the `.ini`. | open |
| C6 | The collector sends nothing meaningful after login, and manta never replies | None | Any collector→Aggregator traffic after login, and Aggregator's answers to it. | open |
| C7 | The collector's prompt ends with a newline | **Reproduced 2026-10-11.** When the collector sends `login: ` or `Please enter your callsign: ` with no newline, manta writes nothing, drops the connection after 30 s (`IDLE_READ_TIMEOUT`) and retries indefinitely; `/status` shows the target `down`. | Whether the ingest sends a prompt at all, and how the prompt ends. | open |
| C8 | Registration carries no locator (MAN-319 would need one to; manta assumes nothing about a registration phase at all) | None. No claim here — an assumption MAN-319 would need but does not make. | Whether any locator bytes leave Aggregator before spots, and in which field, spelling and phase. | open |
| C9 | RBN returns no spot settings a submitter must apply (MAN-319's Scenario 1 would need such a payload) | None. An assumption MAN-319 would need but does not make. | Any collector→Aggregator payload after login that changes what Aggregator sends. | open |
| C10 | RBN cannot direct a submitter to hold spots (MAN-319's Scenario 2 would need such a directive) | None. An assumption MAN-319 would need but does not make. | Any such line, and Aggregator's reaction to it. | open |
| C11 | RBN serves no reference files whose versions a submitter must match (MAN-319's Scenario 3 would need such an exchange) | None. An assumption MAN-319 would need but does not make. The third-party `cty.dat`/`MASTER.SCP` refresh (MAN-316) is unrelated to the uplink connection. | Any reference-file identity, version or transfer on the connection. | open |
| C12 | One connection can carry spots whose `DX de` spotter is not the login call (MAN-320 relays a local skimmer's spots under that skimmer's own call, e.g. `DX de W1ABC-2-#:` after logging in as `W1ABC`) | None. MAN-320 assumes it; RBN Aggregator forwards secondary skimmers, but no capture shows whether it names them per spot or per connection. | Whether Aggregator's forwarded secondary-skimmer and FT8 spots name per-skimmer spotters on one connection, or open a connection (or login) per spotter. | open |
| C13 | A digital spot is an RBN-layout line with `FT8`/`FT4` in the mode column, WSJT-X's own SNR in dB and a blank speed field, type at column 63 and time at column 71 (MAN-320's `rbn::format_relay_line`) | None. RBN's outbound relay carries FT8/FT4 spots, but no line from it or from ingest is captured in this repo. | The bytes of one forwarded FT8 spot, paired with the WSJT-X decode it came from. | open |

### C7 reproduction

`connect_and_forward` reads exactly one newline-terminated line before it
logs in (`read_line_bounded_with_timeout`, `IDLE_READ_TIMEOUT` = 30 s in
`crates/manta-server/src/bounded_io.rs`). A mock collector that accepts the
uplink's connection, sends one prompt, then waits 40 s and records what manta
wrote and what `/status` reported, gave on 2026-10-11 (base `44d5ed3`):

```text
MAN90-PROBE newline-terminated `login: \r\n`: after 0.0s target read 7 bytes: "W3XYZ\r\n"   … "health":"ok","connected_targets":1
MAN90-PROBE bare `login: ` (no newline): after 30.0s target read 0 bytes: ""               … "health":"down","connected_targets":0,…,"reconnects_total":1
MAN90-PROBE bare `Please enter your callsign: `: after 30.0s target read 0 bytes: ""       … "health":"down",…,"reconnects_total":1
```

Against a collector that ends its prompt without a newline, the classic
DX-cluster `login: ` convention, the uplink never logs in. The probe was a
temporary test and is not in the tree: each case takes 30 s, which is too
slow for CI, and the behaviour it pins is a gap, not a contract. To rerun it,
bind a `TcpListener`, point a `spawn_uplink` harness (as in
`uplink_acceptance.rs`) at it, write the prompt bytes without a newline, and
read with a 40 s timeout.

## Decisions

- **D-1. The direct uplink is experimental until C1–C7 are settled.** That is
  said in README (Outputs, the `### Outbound RBN uplink` subsection, "Run it
  as a node" and Status), ARCHITECTURE §7, the node guide
  (`docs/GUIDES/rbn-node.md`), `packaging/README.md`, the `manta config init`
  scaffold and `manta.example.toml`, a `manta config check` note for any
  enabled `[[rbn_uplink]]`, and the two "transmitting real spots" WARN lines
  (startup in `uplink.rs`, SIGHUP reload in `reload.rs`), which share
  `manta_server::uplink::EXPERIMENTAL_NOTICE`. The dry-run INFO line is
  unchanged: the node guide quotes it verbatim, and nothing is transmitted in
  dry-run. The supported route into RBN is manta's telnet server behind an RBN
  Aggregator (broad-review D1), as the
  [secondary-skimmer field-node runbook](../RUNBOOKS/secondary-skimmer-field-node.md)
  sets up.
- **D-2. No protocol change on assumptions, C7 included.** A tolerant prompt
  read (accept a prompt with no newline, or log in after a short silence)
  would be another guess about a server nobody has observed, and could send
  the callsign into the wrong phase of a handshake that turns out to differ.
  The reproduction above stays on record so a follow-up can act on real data.
  The same goes for a password field, a keepalive, or a Skimmer-layout option
  for the uplink.
- **D-3. `dry_run` stays `true` by default** (MAN-159). Nothing here changes
  the wire bytes; the reference test above proves it.
- **D-4. How a raw capture is handled.** The `.pcapng` and the Skimmer Traffic
  text stay private with the developer and are deleted when the operator
  asks. The repo receives only a redacted transcript excerpt the operator has
  approved, with any password replaced by `<redacted>`. The operator is named
  only with their consent.
- **D-5. What a result changes.** Each Status cell becomes `matches` or
  `differs: <what>`, with the capture date. If C1 or C2 shows there is no
  line-oriented ingest a node can log into, open a ticket to rework or remove
  the uplink. Otherwise, fix each difference in a follow-up PR that
  references MAN-90 and pins the captured bytes as a test fixture alongside
  the reference test above.

## Related

- [Broad-review decisions](2026-09-06-broad-review-decisions.md), D1 (the
  Aggregator-compatible path is primary) and D2 (friendly-node capture).
- [MAN-86 `SKIMMER/SETT` handshake](2026-09-07-man86-aggregator-sett-handshake.md),
  "Residual risk".
- [MAN-96 secondary-skimmer field node](2026-10-07-man96-secondary-skimmer-field-node.md),
  D-L (the field config has no `[[rbn_uplink]]`).
- [MAN-91 uplink spot-type filter](2026-10-10-man91-uplink-spot-type-filter.md).
- [MAN-88 AK1A column layout](2026-09-06-man88-ak1a-column-layout.md).
- [Capturing Aggregator→RBN traffic](../RUNBOOKS/aggregator-rbn-capture.md):
  the consent, capture and comparison procedure, and the log of captures.
