# Showing manta's spectrum in N1MM+'s Spectrum Display

N1MM Logger+ has a Spectrum Display window that draws a band scope from
spectrum data another program sends it over UDP. CW Skimmer's "Send Spectrum
via UDP" option is the usual sender. manta can send the same kind of data
from any receiver it decodes, so a contester running manta on an SDR gets a
band scope in N1MM+ without running CW Skimmer (MAN-141).

manta draws nothing itself. It sends numbers; N1MM+ owns the window, its
colours, floor and reference controls. N1MM+'s band map (the decoded calls)
comes from its DX cluster connection, which manta's telnet server on port
7300 already serves; see [Running an RBN node with manta](rbn-node.md) for
that side.

Nothing in this guide has been checked against a real N1MM+ yet. manta's
sender was written from N1MM+'s published "Sending Spectrum Data to N1MM+"
format and is tested against a fake N1MM+ listener built from that page. The
[MAN-141 decision record](../DECISIONS/2026-11-01-man141-n1mm-spectrum-feed.md)
lists what still needs a real N1MM+ session.

## 1. What manta sends

- One XML `<Spectrum>` datagram about 3 times a second, to the address you
  set. N1MM+ listens on UDP port 13064.
- Each datagram is the mean power of the last third of a second (125
  channelizer hops) in every 93.75 Hz channel inside the receiver's
  passband, lowest frequency first. A 192 kS/s SDR sends 2048 levels across
  about 192 kHz; a 48 kS/s source 512 levels; rig audio about 29 levels
  across 2.7 kHz.
- Levels are 0.5 dB steps above a -140 dB floor on manta's own relative
  scale (`ScalingFactor` 0.5).
- The source name N1MM+ lists is `manta <input name>` unless you set
  `spectrum_name`.
- Spots do not change. The spectrum is a side output of the channelizer
  manta already runs.

## 2. Configure manta

Add `spectrum_udp` to the `[input]` table. This example decodes 20 m from an
HPSDR radio and sends its spectrum to N1MM+ on the same computer:

```toml
[server]
station_callsign = "N0CALL"

[input]
type = "hpsdr"
freq_hz = 14050000.0
rate_hz = 192000.0
spectrum_udp = "127.0.0.1:13064"   # N1MM+'s Spectrum Display port on this computer
```

When N1MM+ runs on another computer, use that computer's address, and give
the feed a name you will recognise in N1MM+'s list:

```toml
[input]
type = "hpsdr"
freq_hz = 14050000.0
rate_hz = 192000.0
spectrum_udp = "192.168.1.50:13064"   # the N1MM+ computer's address
spectrum_name = "manta 20m"
```

Rules the loader checks:

- `spectrum_udp` is an IP address and a port. A host name such as
  `localhost` is refused, and so are port 0, `0.0.0.0`, multicast and
  broadcast addresses.
- `spectrum_name` needs `spectrum_udp`, and is 1 to 64 characters with no
  control characters.
- With several `[[input]]` entries each one takes its own `spectrum_udp`.
  Two inputs may send to the same N1MM+ only under different names. A
  `bands` entry that plans several streams sends each stream as
  `manta <stream name>` and refuses `spectrum_name`.
- The receiver needs a real RF frequency. Rig audio without
  `center_freq_hz` refuses to start, because N1MM+ would draw audio offsets.

Check the file, then run manta:

```sh
manta config check --config manta.toml
manta run --config manta.toml
```

`manta config check` prints `spectrum_udp=` (and `spectrum_name=` when set)
on the input line. `manta run` prints one line per feed when the receiver
opens, like this one:

```text
spectrum: input "hpsdr" sends 2048 levels, 13954 to 14145.906 kHz, to 127.0.0.1:13064 as "manta hpsdr" for N1MM+'s Spectrum Display, 3 per second
```

Only `manta run` sends. `manta soak` and `manta doctor` decode without
sending and print a note saying so. Changing either key needs a restart; a
SIGHUP reload reports it as a key that needs one.

## 3. Show it in N1MM+

These steps follow the N1MM+ manual's Spectrum Display page
(<https://n1mmwp.hamdocs.com/manual-windows/spectrum-display-window/>).

1. Start manta first, so the source is already sending when N1MM+ looks for
   it. The first datagram goes out about 2 s after the receiver opens, once
   manta's startup calibration is done.
2. In N1MM+, open the Spectrum Display window from the Window menu.
3. Choose "For all other radios, source named", and pick `manta <input name>`
   (or your `spectrum_name`) from the drop-down. Only sources that are
   already sending appear there.
4. Adjust the floor and reference in N1MM+ until the noise sits near the
   bottom of the scope.

## 4. N1MM+ on another computer: open the firewall

UDP gives manta no signal that a datagram arrived, so a blocked port looks
like a working feed on manta's side. On the N1MM+ computer, allow inbound
UDP port 13064 from the manta computer in Windows Defender Firewall (or the
firewall you use).

Datagrams from a wide receiver are large: about 8.5 KB at 192 kS/s and about
17 KB at 384 kS/s. On a normal 1500-byte LAN they travel as IP fragments, so
a network that drops fragments drops the whole datagram.

## 5. Limits

- Bins are manta's channels, 93.75 Hz wide, with no resampling. A wide SDR
  sends every bin in its passband; manta does not cap the count.
- Rig audio covers only the radio's audio passband, about 2.7 kHz.
- The span edges are the first and last bin centres, which may differ from
  what N1MM+ takes as the scope edges by up to one bin.
- Not checked against a real N1MM+ yet: how it draws more than 1024 levels,
  a span wider than 48 kHz, and the exact meaning of its frequency fields.
  Not checked either: whether DXLog.net accepts this feed.
- manta does not listen on 13064 and ignores N1MM+'s QSY packets.

## 6. Troubleshooting

With a `[server]` table, `/metrics` counts each feed (see
[METRICS.md](../METRICS.md)):

```sh
curl -s http://127.0.0.1:7302/metrics | grep manta_spectrum
```

- `manta_spectrum_datagrams_total{input="..."}` rises about 3 times a
  second while manta runs. If it rises and N1MM+ shows nothing, check the
  address in `spectrum_udp`, the firewall on the N1MM+ computer, and that
  N1MM+'s Spectrum Display is set to the `manta ...` source.
- `manta_spectrum_dropped_total{reason="send_error"}` rising means the OS
  refused the send, for example no route to that address. manta logs the
  first error per feed as a warning and keeps trying.
- `reason="rate_limit"` counts frames that came less than 100 ms after the
  last datagram. A few at startup are normal (the calibration burst). Many
  mean an unpaced file replay; set `realtime = true` for a file input.
- `reason="queue_full"` counts frames the decode thread could not queue
  because the sender fell behind. It should stay at 0.

Every key is in [CONFIG.md](../CONFIG.md).
