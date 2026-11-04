# Sending manta's spots to PSKReporter and to LAN loggers

manta's validated spots normally leave the computer through telnet (port
7300), the JSON stream (port 7301) and the RBN uplink. MAN-142 adds two more
outputs, and both are off until you turn them on:

- `[pskreporter]` reports spots to PSKReporter (pskreporter.info), the public
  reception map that WSJT-X, JTDX and CW Skimmer also feed.
- `[[wsjtx_output]]` sends spots to your local network as WSJT-X UDP
  messages. Loggers that already listen for WSJT-X, such as GridTracker or
  JTAlert, can read them.

Neither output changes what manta decodes or what telnet, JSON or the
uplink send.

Nothing in this guide has been checked against a real PSKReporter server or
a real logger yet. manta's encoders were written from PSKReporter's
developer page and WSJT-X's `NetworkMessage.hpp`, and they are tested against
fake listeners built from those formats. The
[MAN-142 decision record](../DECISIONS/2026-11-03-man142-pskreporter-and-wsjtx-udp-outputs.md)
lists what still needs a live check.

## 1. What each output sends

**PSKReporter reports are public.** Anyone can look up a call on
pskreporter.info and see that your station heard it, under your
`[server].station_callsign`, at your grid square.

- PSKReporter gets IPFIX datagrams over UDP, sent to
  `report.pskreporter.info:4739`.
- Each report carries the spotted call, its RF frequency in whole hertz, its
  SNR (2500 Hz reference, rounded), the mode (`CW` or `RTTY`) and the time
  manta heard it.
- Each datagram also names your station: your call without its `-N` SSID,
  your grid, `manta <version>` as the software, and the `antenna` text if
  you set one.
- By default only CQ and beacon spots are reported, as for the RBN uplink.
  `spot_types = "all"` reports every spot.
- A call is reported at most once per band every 5 minutes. That is
  PSKReporter's own rule. A station calling CQ for an hour is reported about
  12 times, not once per decode.
- manta sends at most one datagram every 5 minutes. The first one goes out
  5 to 5.5 minutes after startup. A busy band can fill a datagram sooner,
  and then it goes out early.
- When nothing is waiting, manta sends nothing.

The WSJT-X output sends four kinds of WSJT-X message, and no others:

- `Heartbeat`, the first time a band is used and then every 15 s.
- `Status`, which tells the logger the band's frequency and mode (`CW`).
- `Decode`, one per validated spot. The text is `CQ <call>` for a CQ spot
  and `DE <call>` for every other spot.
- `Close`, one per band when manta stops.

Each band shows up in the logger as its own WSJT-X instance, named
`manta-20m`, `manta-40m` and so on. manta never sends the `QSO Logged` or
`Logged ADIF` messages, so a logger never writes a manta spot into your log
as a contact.

**Anyone on the network who can receive the destination can read the
spots.** A broadcast or multicast destination reaches every computer on that
network segment.

## 2. Report to PSKReporter

PSKReporter places every report at the receiver's grid, so `[server]` needs
your location. Set `operator_grid`, or `operator_lat` and `operator_lon`:

```toml
[server]
station_callsign = "N0CALL"
operator_grid = "EM10dh"
```

Then add the table and turn it on:

```toml
[pskreporter]
enabled = true
antenna = "Inverted V at 12 m"   # optional; shown on PSKReporter's map
```

Check the file, then run manta:

```sh
manta config check --config manta.toml
manta run --config manta.toml
```

`manta config check` prints a `pskreporter:` line with the settings it will
use. `manta run` prints one line when the output starts, like this one:

```text
pskreporter: reporting cq_beacon spots to report.pskreporter.info:4739 as W5AU at EM10dh; reports are public; the first datagram goes out within 5.5 minutes, then at most one every 5 minutes
```

Reports appear on PSKReporter's map some minutes later. manta waits 5
minutes before its first datagram, and PSKReporter batches its own updates.

**Do not run a second PSKReporter reporter for the same call.** If WSJT-X,
JTDX or CW Skimmer on the same station also reports to PSKReporter, both
programs report the same stations, and PSKReporter receives each one twice.
Turn PSKReporter reporting off in the other program, or leave `[pskreporter]`
off.

### Try it first with PSKReporter's test listener

PSKReporter's developer page (<https://pskreporter.info/pskdev.html>)
describes a test listener on port 14739, which analyses the packets it
receives. Point manta at it before you go live:

```toml
[pskreporter]
enabled = true
port = 14739
```

Then read the result on the analysis page the developer page links to.

### File replays never reach the public map

A file replay reports only to a loopback host (`127.0.0.1` or `localhost`).
A recording's spots carry the recording's times, and they must not appear on
PSKReporter as if they were heard now. With any other host, `manta run`
prints this note and does not start the output:

```text
note: [pskreporter] is off for file replay: report.pskreporter.info is not a loopback address, and a recording's spots must not reach PSKReporter's public map
```

## 3. Send spots to a logger on your network

`destination` has no default. Point it at the UDP port your logger listens
on. A logger on the same computer, at WSJT-X's usual port:

```toml
[[wsjtx_output]]
destination = "127.0.0.1:2237"
```

A logger on another computer: use that computer's address, or a broadcast
address so every computer on the network can listen:

```toml
[[wsjtx_output]]
destination = "192.168.1.255:2237"
```

Many stations share one multicast group between WSJT-X, GridTracker and
other tools. manta can send to the same group:

```toml
[[wsjtx_output]]
destination = "224.0.0.1:2237"
```

Multicast leaves manta with a TTL of 1, so it stays on the local network
segment. Loopback delivery is on, so a logger on the same computer receives
it too.

If you also run manta's `[[wsjtx_relay]]` on that group (it forwards
WSJT-X's FT8 and FT4 decodes to the RBN uplink), the relay ignores every
datagram whose instance name starts with `manta-`. manta's own spots
therefore never come back to the uplink as relayed spots, and they do not
count as "WSJT-X heard". `/metrics` counts them as
`manta_relay_dropped_total{reason="own_output"}`.

You can list several `[[wsjtx_output]]` blocks, one per destination.
`enabled = false` keeps a block but sends nothing.

`manta run` prints one line per running block:

```text
wsjtx_output 192.168.1.255:2237: sending spots as WSJT-X Status and Decode messages for LAN loggers (one instance per band, "manta-<band>", heartbeat every 15 s)
```

On a file replay this output still runs, and every `Decode` carries WSJT-X's
`Off air` flag. WSJT-X sets the same flag when it plays back a recording. A
logger that honours it ignores or marks those decodes.

Both tables need a restart to change. A SIGHUP reload reports a change as a
key that needs a restart. Neither table can be set from the environment.

## 4. Firewall notes

UDP gives manta no signal that a datagram arrived, so a blocked port looks
like a working output on manta's side.

- PSKReporter needs outbound UDP to port 4739 (14739 for the test listener).
  See [the network exposure runbook](../RUNBOOKS/network-exposure.md) for
  every port manta uses.
- Broadcast and multicast stay on the local network segment. A router does
  not pass them to another subnet.
- The logger's computer must accept inbound UDP on the logger's port from
  the manta computer. On Windows, allow it in Windows Defender Firewall (or
  the firewall you use).

## 5. What to watch in /metrics

With a `[server]` table, `/metrics` counts both outputs (see
[METRICS.md](../METRICS.md)):

```sh
curl -s http://127.0.0.1:7302/metrics | grep -E 'manta_pskreporter|manta_wsjtx_output'
```

- `manta_pskreporter_reports_total` rising means PSKReporter is being fed.
  `manta_pskreporter_datagrams_total` counts the datagrams.
- `manta_pskreporter_dropped_total{reason="rate_gate"}` is usually much
  larger than `reports_total`. That is normal: a CQing station is decoded
  many times, and each call goes out at most once per band every 5 minutes.
- `reason="spot_type"` counts DE and untyped spots that `spot_types` held
  back.
- `reason="resolve_error"` or `reason="send_error"` rising means a DNS or
  network problem on the manta computer. manta logs the first one as a
  warning and keeps trying.
- A PSKReporter outage shows nothing here. Look up your call on
  pskreporter.info to see whether reports arrive.
- `manta_wsjtx_output_spots_total{destination="..."}` counts spots sent as a
  `Decode`, and `manta_wsjtx_output_datagrams_total` every datagram. If they
  rise and the logger shows nothing, check the destination address and the
  logger computer's firewall.
- `reason="lagged"` on either output counts spots the output lost because it
  fell behind manta's spot stream. It should stay at 0.

## 6. Limits

- No real logger has been tested. How GridTracker, JTAlert or Log4OM show a
  `CW` instance, a `CW` `Decode`, `DE <call>` text and `Off air` decodes is
  not verified.
- No live report to PSKReporter has been sent yet, including to its test
  listener.
- When manta has nothing to report, it sends nothing. A quiet manta may
  therefore drop off PSKReporter's list of active monitors.
- PSKReporter gets manta's 2500 Hz-referenced SNR, the figure WSJT-X uses.
  RBN and telnet quote SNR in 500 Hz, about 7 dB higher.
- Only CW (and RTTY) spots go out. FT8 and FT4 spots, and spots relayed from
  other skimmers or from WSJT-X, do not.
- PSKReporter has no IPv6 address, so `host` must be a name or an IPv4
  address. A `[[wsjtx_output]]` destination cannot be an IPv6 multicast
  group.

Every key is in [CONFIG.md](../CONFIG.md).
