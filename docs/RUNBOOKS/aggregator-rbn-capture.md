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
> - **What is recorded:** only your Aggregator host's traffic with RBN's
>   server. That includes Aggregator's RBN login and the spots it sends.
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
   ```

   The established connection whose RemoteAddress is not `127.0.0.1` or a LAN
   address is RBN's server. Note its address and port. If there is none, open
   Resource Monitor (`resmon.exe`), go to Network, tick Aggregator, and note
   the remote addresses under Network Activity. That view shows UDP as well
   (this answers part of C2).
2. **Start recording.** Open Wireshark on the internet-facing interface with
   the capture filter `host <address from step 1>`, and start the capture.
3. **Restart Aggregator** (close it, then start it again) so its RBN login is
   recorded. Confirm that "RBN Connection Status" turns green. Leave it running
   for at least 15 minutes while the band is active, until the Skimmer Traffic
   tab shows at least 20 spots marked `+` (sent to RBN, manual §6.2.1).
4. **Copy the Skimmer Traffic tab** for the same window ("Copy to Clipboard",
   manual §6.3) into a text file. Note the Aggregator version shown in its
   title bar.
5. **Stop and save.** Stop Wireshark, save as `.pcapng`, and send it and the
   text file to the developer privately.

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

Each row whose destination is RBN's address (operator step 1) is a connection
Aggregator opened while recording, normally the one after the restart. The old
connection opened before recording started, so it has no row. It is often
stream 0. If no row goes to RBN's address, the restart was not recorded: ask
the operator to record it again. Set `N` to a row's `tcp.stream` value and run,
over the whole file:

```sh
N=1   # the tcp.stream value from that row
tshark -r aggregator-rbn.pcapng -Y "tcp.stream == $N && (tls || tcp.payload[0:2] == 16:03)"   # any output: C2 = TLS, stop at C2
tshark -r aggregator-rbn.pcapng -q -z follow,tcp,ascii,$N > transcript-$N.txt
```

Do this for every row to RBN's address, one transcript each (C1). A refused or
failed attempt gives an empty transcript, so skip it. The `16:03` test catches
a TLS handshake on a port tshark does not decode as TLS. If Aggregator reaches
RBN over UDP instead, list its flows with
`tshark -r aggregator-rbn.pcapng -Y udp -T fields -e udp.stream -e ip.dst -e ipv6.dst -e udp.dstport | sort -u`
and write each flow to RBN's address to a transcript with
`tshark -r aggregator-rbn.pcapng -q -z follow,udp,ascii,$N > transcript-udp-$N.txt`.
Then fill a C1–C7 table by comparing each transcript with the decision record's
reference session (the bytes `the_uplink_session_bytes_match_the_man90_reference`
pins):

- **C1:** the remote endpoint(s) Aggregator holds open to RBN, and whether any
  is a login service.
- **C2:** transport (TCP or UDP), TLS or not, text or binary.
- **C3:** the first bytes in each direction after connect.
- **C4:** take one `+` row from the Skimmer Traffic text, find its bytes in the
  transcript, and compare them column by column with the reference spot line.
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
