# Capturing Aggregator→RBN traffic to check manta's uplink (MAN-90)

Not part of CI. manta's direct `[[rbn_uplink]]` is experimental: its login and
spot format are manta's assumption, never observed against RBN's real ingest.
This runbook records what a real RBN Aggregator sends to RBN's server, so the
uplink's open claims C1–C7 in
[the MAN-90 decision record](../DECISIONS/2026-10-11-man90-uplink-protocol-verification.md)
can each be marked `matches` or `differs`. Until a capture is logged at the
end of this page, the supported route into RBN is manta's telnet server behind
an Aggregator ([secondary-skimmer field-node runbook](secondary-skimmer-field-node.md)).

## Who

- **An RBN Aggregator operator** who has agreed to it in writing. They run the
  capture on their own Windows host.
- **A manta developer**, who receives the files privately and does the
  comparison.

First candidate: George, K5TR, who runs SkimSrv and supplied manta's
real-signal audio corpus (`crates/manta-testkit/audio-corpus/README.md`). He
had not been asked as of 2026-10-11.

## Asking for consent

Send something like this, and keep the written reply:

> Would you record about 15 minutes of your Aggregator's traffic with the RBN
> server, so manta can check its RBN uplink against what Aggregator really
> sends?
>
> - **What is recorded:** your Aggregator host's traffic with RBN's servers.
>   That includes Aggregator's RBN login and the spots it sends. Wireshark
>   records everything on that network card while it runs, so before sending
>   you keep only the packets of the connections Aggregator itself made, and
>   delete the rest.
> - **What happens to it:** the raw file stays private with me and is deleted
>   whenever you ask.
> - **What gets published:** only a short excerpt that you approve first, with
>   any password removed, goes into manta's repository. You are named only if
>   you agree.
> - **What it costs you:** restarting Aggregator once so the login is
>   recorded, which briefly disconnects its Skimmers.

## Operator steps (Windows host running Aggregator)

1. **Find Aggregator's connection to RBN.** In PowerShell:

   ```powershell
   $p = Get-Process | Where-Object ProcessName -like 'Aggregator*'
   Get-NetTCPConnection -OwningProcess $p.Id | Select-Object LocalPort,RemoteAddress,RemotePort,State
   Get-NetUDPEndpoint -OwningProcess $p.Id | Select-Object LocalAddress,LocalPort
   ```

   The established connection whose RemoteAddress is not `127.0.0.1` or a LAN
   address is RBN's server. Note its LocalPort, RemoteAddress and RemotePort.
   If there is none, open Resource Monitor (`resmon.exe`), go to Network, tick
   Aggregator, and note the remote addresses under Network Activity. That view
   shows UDP as well (this answers part of C2). For UDP, also note the
   LocalPort values the third line lists. If the first line finds no process,
   Aggregator runs under another name: change `'Aggregator*'`. Once it finds
   the process, a line that prints an error means Aggregator has no
   connection of that kind.
2. **Start recording.** Open Resource Monitor (`resmon.exe`) on the Network
   tab and leave it open. Open Wireshark on the internet-facing interface with
   no capture filter, and start the capture. After its restart Aggregator may
   reach RBN at a different address, open a second endpoint, or send over
   UDP, and a filter on the step 1 address would drop all of that. Step 5
   removes everything else before the file leaves your host.
3. **Restart Aggregator** (close it, then start it again) so its RBN login is
   recorded. In Resource Monitor tick the restarted Aggregator (it has a new
   PID) and watch Network Activity while it reconnects. Confirm that "RBN
   Connection Status" turns green, then run the PowerShell lines from step 1
   again (the first finds the restarted process). Note every connection
   Aggregator holds after the restart, TCP or UDP, whose remote address is not
   `127.0.0.1`, a LAN address, or one of the Skimmers Aggregator reads. For
   TCP note its LocalPort, RemoteAddress and RemotePort, from the second line
   or from Resource Monitor's TCP Connections pane. Watch that pane while
   Aggregator reconnects, so a connection that closes within seconds is noted
   too. For UDP note every LocalPort the third line lists. Also note every
   such remote address Network Activity lists for Aggregator. Leave it
   running for at least 15 minutes while the band is active, until the
   Skimmer Traffic tab shows at least 20 spots marked `+` (sent to RBN, manual
   §6.2.1). Check Network Activity and TCP Connections once more just before
   you stop, and note anything new.
4. **Copy the Skimmer Traffic tab** for the same window ("Copy to Clipboard",
   manual §6.3) into a text file. Note the Aggregator version shown in its
   title bar.
5. **Stop and keep only Aggregator's traffic.** Stop Wireshark. In the display
   filter bar enter one term per TCP connection from steps 1 and 3,
   `(ip.addr == <RemoteAddress> && tcp.port == <LocalPort> && tcp.port == <RemotePort>)`
   (`ipv6.addr` for an IPv6 address), and one per UDP LocalPort from step 3,
   `udp.port == <LocalPort>`, all joined with `||`. Apply it. A TCP term on
   the address alone would also keep any other program on this host that
   talks to the same address, such as a browser open on an RBN page. The UDP
   term names no address, so it keeps what Aggregator sent from that port
   even to an address Resource Monitor showed too briefly to note. It uses
   only the restarted Aggregator's ports: a step 1 port is freed at the
   restart, and another program may then take its number. Open Statistics →
   Conversations and tick "Limit to display filter". The TCP tab must list
   only the connections you noted. On the UDP tab, the port next to this
   host's own address must be a noted UDP LocalPort, and the other address
   must not be a LAN address or one of your Skimmers. Right-click every other
   UDP row and choose Apply as Filter → …and not Selected → A ↔ B. Then use
   File → Export Specified Packets, choose "All packets" and "Displayed", and
   save as `aggregator-rbn.pcapng`. Open the saved file and check that the
   TCP and UDP tabs of Statistics → Conversations list only the rows you
   kept. Send that file, the text file and your notes of connections and
   addresses to the developer privately, then delete the unfiltered capture.

## Developer steps

`tshark` ships with Wireshark. Recording started before Aggregator restarted,
so the file can hold two connections to RBN: the old one, with no login, and
the restarted one. Only the restarted one is useful. First list the
endpoints, then every TCP connection opened while recording, with its stream
index:

```sh
tshark -r aggregator-rbn.pcapng -q -z conv,tcp -z conv,udp       # C1, C2: which endpoints exist
tshark -r aggregator-rbn.pcapng -Y 'tcp.flags.syn == 1 && tcp.flags.ack == 0' \
  -T fields -e frame.time -e tcp.stream -e ip.dst -e ipv6.dst -e tcp.dstport
```

The file holds only the TCP connections the operator noted before and after
the restart (operator steps 1 and 3), each matched on its address and ports,
and the UDP traffic on the UDP LocalPorts noted after the restart, with any
address but a LAN address or one of the operator's Skimmers. Any TCP
connection the operator did not note, Aggregator's or another program's, and
any UDP on a port the operator did not note after the restart, was removed in
step 5, so the file cannot show it. A noted address with no conversation in
the file is such a connection: list it under C1 as not recorded. Each row is a
connection Aggregator opened while recording, normally the one after the
restart. The old connection opened before recording started, so it has no row.
It is often stream 0. If no row goes to any of the addresses the operator
noted, the restarted connection was either not recorded or not noted: ask the
operator to record it again. Set `N` to a row's `tcp.stream` value and run,
over the whole file:

```sh
N=1   # the tcp.stream value from that row
tshark -r aggregator-rbn.pcapng -Y "tcp.stream == $N && (tls || tcp.payload[0:2] == 16:03)"   # any output: C2 = TLS, stop at C2
tshark -r aggregator-rbn.pcapng -q -z follow,tcp,ascii,$N > transcript-$N.txt
tshark -r aggregator-rbn.pcapng -q -z follow,tcp,raw,$N > transcript-$N.hex
```

Do this for every row to every address the operator noted, one pair of
transcripts each (C1). A refused or failed attempt gives an empty transcript,
so skip it. The `16:03` test catches a TLS handshake on a port tshark does not
decode as TLS. The `.txt` view is for reading: `ascii` mode turns
non-printable bytes into dots, and a text view hides whether a line ends in
CR, LF or CRLF. The `.hex` view (`raw` mode) keeps every byte as hex, so take
the bytes for C2, C3, C4 and C7 from it. If Aggregator reaches RBN over UDP
instead, list its flows with
`tshark -r aggregator-rbn.pcapng -Y udp -T fields -e udp.stream -e ip.dst -e ipv6.dst -e udp.dstport | sort -u`
and write each flow, including one to an address the operator did not note,
to a pair of transcripts with
`tshark -r aggregator-rbn.pcapng -q -z follow,udp,ascii,$N > transcript-udp-$N.txt`
and
`tshark -r aggregator-rbn.pcapng -q -z follow,udp,raw,$N > transcript-udp-$N.hex`.
Then fill a C1–C7 table by comparing each `.hex` transcript with the decision
record's reference session (the bytes
`the_uplink_session_bytes_match_the_man90_reference` pins):

- **C1:** the remote endpoint(s) Aggregator holds open to RBN, and whether any
  is a login service.
- **C2:** transport (TCP or UDP), TLS or not, text or binary.
- **C3:** the first bytes in each direction after connect.
- **C4:** take one `+` row from the Skimmer Traffic text, find it in the
  `.txt` transcript, take its exact bytes from the `.hex` one, and compare them
  column by column with the reference spot line.
- **C5:** how that spot names its spotter, or whether identity comes only from
  the login.
- **C6:** any server→Aggregator traffic after login, and Aggregator's answers.
- **C7:** whether the server sends a prompt at all, and how it ends (newline or
  not).

## Recording a result

Append a dated entry under `## Captures` with:

- the operator, named only with their consent;
- the Aggregator version and the capture length;
- the C1–C7 results;
- a redacted excerpt of at most 10 lines (any password replaced by
  `<redacted>`), approved by the operator.

Then update the decision record's Status cells and open follow-ups as its D-5
says. Raw files stay out of the repository (decision record D-4).

## Captures

(Append entries here.) None yet. As of 2026-10-11 no Aggregator→RBN traffic
has been recorded.
