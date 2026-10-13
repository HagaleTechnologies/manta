# Reading a ka9q-radio channel with manta

ka9q-radio's `radiod` turns one SDR (an RX-888 covering all of HF, an Airspy,
anything it drives) into many separately tuned channels, and sends each one as
RTP over UDP multicast. manta can read one of those channels when it is in
`mode = iq`. This guide goes from a running radiod to manta decoding that
channel. For the telnet check, uplink, firewall and service, continue with
[Running an RBN node with manta](rbn-node.md) from its step 4.

Nothing in this guide has been run against a live radiod yet. manta's
ka9q-radio input was written from ka9q-radio's documentation and headers, and
is tested against a fake radiod that replays recordings over loopback
multicast. The [MAN-140 decision record](../DECISIONS/2026-10-13-man140-ka9q-radio-input.md)
lists the facts that still need a live radiod.

## 1. What this input is, and what it is not

manta joins the multicast group of one radiod channel that you have already
set up. It reads the channel's sample rate, encoding, frequency and filter edges
from the status packets radiod sends to that group, then decodes the channel's
IQ samples as it would any other receiver's.

- It does not create, tune or retune radiod channels. radiod's config does that.
- It reads IPv4 groups only. An IPv6 group is refused.
- One manta process reads one channel. For several bands, give each band its
  own radiod channel and run one manta per channel, each with its own config
  and ports. Several manta processes can read the same group.
- Every manta build includes it. There is no build feature and no native
  library to install.

## 2. Set up the radiod channel

Add a channel section like this one to radiod's config file, then restart radiod
so it reads it. These are radiod's documented keys; the section has not been run
against a live radiod.

```ini
[global]
# ttl = 0 (radiod's default) keeps every stream on the radiod machine.
# Set 1 only if manta runs on another machine on the same LAN.
ttl = 1

[cw-20m]
data = cw-20m.local     # the multicast group manta joins
mode = iq               # complex I/Q, two channels, no envelope
samprate = 192000       # must be 93.75 Hz x a power of two: 48000, 96000, 192000, 384000 ...
low = -90000            # filter edges; manta reports them as its passband
high = +90000
agc = off               # the iq preset turns AGC on; a skimmer needs fixed gain
encoding = f32le        # or s16be (radiod's default); see section 10
freq = 14100000         # centre frequency, Hz
```

What each key does for manta:

- `ttl`, in `[global]`: see [section 4](#4-same-machine-or-another-machine).
- `data` names the channel's multicast group. radiod derives the group's
  address from this name and announces the name over mDNS.
  [Section 3](#3-find-the-channels-data-group) finds the address.
- `mode = iq` makes radiod send complex I/Q as two channels. manta refuses a
  channel with one output channel, with its envelope on the second channel, or
  in independent-sideband mode, and the error says which.
- `samprate` is the channel's output rate. manta's channelizer needs 93.75 Hz
  times a power of two: 12000, 24000, 48000, 96000, 192000, 384000 and so on,
  up to 10 MS/s. manta refuses any other rate (16000, for example) when it
  opens the channel. A higher rate covers more of the band and costs more CPU
  and network.
- `low` and `high` are the channel's filter edges in Hz either side of `freq`.
  manta uses them as its passband, clamped to the ±96 kHz that a 192 kS/s
  stream can carry. ka9q-radio's own `iq` preset is ±5 kHz at 12 kS/s, so set
  them, or most of the stream is empty.
- `agc = off`: the `iq` preset turns AGC on. With AGC, one strong signal
  changes the gain for every signal in the passband, and a skimmer measures
  them all at once. manta still runs with AGC on, but each time it opens the
  channel it prints
  `warning: ka9q-radio channel ssrc ... has AGC on; set agc = off and a fixed gain in its radiod section ...`.
  With AGC off, radiod applies the channel's fixed `gain`; radiod's own
  documentation gives its default.
- `encoding`: see [section 10](#10-choose-an-encoding).
- `freq` is the channel's centre frequency. manta reads it from radiod, so
  spots carry absolute frequencies with no frequency key in manta's config.
  With these values the channel covers 14.010 to 14.190 MHz. To start at the
  band edge, set `freq = 14090000`, which covers 14.000 to 14.180 MHz.

Leave radiod's `update` key at its default. manta learns the channel's format
from the status packets radiod sends to the data group about twice a second;
with `update = 0` there are none, and manta gives up after 10 s.

## 3. Find the channel's data group

manta needs the group as an IPv4 multicast address, or as the `data` name when
this machine resolves mDNS (`.local`) names. Check whether it does:

```sh
getent hosts cw-20m.local
```

If that prints an address, this machine resolves the name (on Debian-family
Linux this needs Avahi with the `libnss-mdns` package), and you can give manta
`cw-20m.local`. If it prints nothing, give manta the address. List the streams
radiod announces:

```sh
sudo apt install -y avahi-utils
avahi-browse -rt _rtp._udp
```

Find the entry for your channel's `data` name. Its `address` is the group and
its `port` is the data port, 5004 by default. The address follows from the name,
so it stays the same while the `data` line does. Inside the Docker image `.local`
names do not resolve, so use the address there.

## 4. Same machine, or another machine

**On the radiod machine**, nothing else is needed. With radiod's default
`ttl = 0` its streams stay on that machine, and manta always joins on 127.0.0.1
as well as on the network. You can drop the `ttl = 1` line from section 2.

**On another machine:**

- Set `ttl = 1` in radiod's `[global]` section and restart radiod. A TTL of 1
  reaches the local network segment and stops at the first router, so manta and
  radiod must be on the same segment.
- Use a switch with IGMP snooping. Without it, the switch sends every radiod
  channel to every port; one 192 kS/s `f32le` channel is about 1.5 MB/s. Running
  manta through an IGMP-snooping switch has not been tested yet.
- If this machine has more than one network interface, set `interface` in
  `[input]` (or `--ka9q-interface`) to this machine's IPv4 address on radiod's
  network. Without it, manta joins on the interface the kernel picks for the
  group, plus 127.0.0.1.
- A host firewall that denies incoming traffic by default, as the node guide's
  [UFW setup](rbn-node.md#6-firewall-and-exposure) does, also drops radiod's
  multicast. Allow its data port (5004) and status port (5006) from radiod's
  machine:

```sh
sudo ufw allow proto udp from <radiod-host-ip> to any port 5004,5006
```

## 5. Write the manta config

Create `manta.toml` with your call and the group from section 3. Replace
`N0CALL` and `<your-radiod-data-group>`; `config check` refuses both until you
do.

```toml
[server]
station_callsign = "N0CALL"           # your call

[input]
type = "ka9q"
group = "<your-radiod-data-group>"    # the address from section 3, or the data = name if this machine resolves .local names
# port = 5004                         # RTP data port; radiod's default
# ssrc = 14100000                     # needed only when the group carries more than one channel
# interface = "192.168.1.20"          # this machine's IPv4 address on radiod's network; default: the kernel's choice, plus 127.0.0.1
```

There are no rate, encoding or frequency keys: manta reads them from the
channel. When one group carries several channels, set `ssrc` to the one you
want; [section 6](#6-check-the-stream) shows how to list them. On another
machine with two interfaces, the `[input]` table looks like this:

```toml
[input]
type = "ka9q"
group = "<your-radiod-data-group>"
ssrc = 14100000                       # the channel's SSRC, from manta check's list
interface = "192.168.1.20"            # this machine's IPv4 address on radiod's network
```

Validate it. This does not contact radiod:

```sh
manta config check --config manta.toml
```

It prints `manta.toml: valid (from --config)`, and an `input:` line saying what
manta will join, for example
`input: type=ka9q group=239.101.6.1 port=5004 ssrc=any interface=default`.

On the command line, `--ka9q-group` selects this input, with `--ka9q-port`,
`--ka9q-ssrc` and `--ka9q-interface` for the other keys. Like every source flag,
`--ka9q-group` replaces the config's whole `[input]` table.

## 6. Check the stream

Read a few seconds of the channel without decoding:

```sh
manta check --config manta.toml
```

For the section 2 channel the report starts like this. These lines are worked
out from the section 2 values, not captured from a live radiod:

```text
Source: ka9q
Stream sample rate: 192000 Hz
Center frequency: 14100000 Hz
Passband offsets: -90000 to 90000 Hz
Samples received: 576000 (3.000 s)
```

| Line | What it tells you |
| --- | --- |
| `Stream sample rate` | The channel's `samprate`, as radiod reports it. |
| `Center frequency` | The channel's `freq`. Spots are reported against it. |
| `Passband offsets` | The channel's `low`/`high` filter edges, clamped to half the sample rate. manta decodes only inside them. |
| `Samples received` | How many samples arrived, and how long that is at the stream rate. |
| `Input power` | Average power of the delivered IQ in dBFS, or `digital silence` when every sample was exactly zero. |
| `Noise floor` | A brief per-channel noise-floor estimate across the passband. Not calibrated RF power. |
| `Stopped` | `sample window complete` means the whole window arrived. `sampling deadline reached` exits 1. |

If the check fails, the error says what to change:

| Error contains | Fix |
| --- | --- |
| `no ka9q-radio status from <group> (port 5006) within 10 s` | radiod is not running, the group is wrong, radiod's `ttl` is 0 while manta runs on another machine, `interface` names the wrong interface, or a firewall drops the traffic (section 4). |
| `<group> carries N ka9q-radio channels` | The message lists each channel's SSRC, frequency, rate and encoding. Set `ssrc` (or `--ka9q-ssrc`) to the one you want. |
| `no ka9q-radio status for ssrc ...` | No channel on the group has that SSRC; the message lists the ones it saw. |
| `... is not an IQ stream`, `... sends its envelope, not Q`, `... is in independent-sideband mode` | Set `mode = iq` in the channel's radiod section. |
| `... uses opus encoding, which manta cannot read` | Set `encoding = f32le` or `s16be` (section 10). |
| `... runs at 16000 Hz; manta needs 93.75 Hz times a power of two` | Set `samprate` to one of the rates in section 2. |
| `... sends its data to <group>:5010, not port 5004` | Set `port` (or `--ka9q-port`) to the port the message names. |
| `... reports no tuned frequency (0 Hz)` | Tune the channel in radiod first. |
| `could not resolve ka9q-radio group cw-20m.local` | This machine has no mDNS resolver. Give the address instead (section 3). |

Then let `manta doctor` check the whole setup and decode for a while:

```sh
manta doctor --config manta.toml --duration 10
```

Its `receiver` line names the channel, as `ka9q-radio group 239.101.6.1:5004`
(with ` ssrc ...` when one is set). When it cannot open the channel, its fix line
says to check `--ka9q-group`/`input.group` and `--ka9q-ssrc`/`input.ssrc` against
the radiod channel section, that radiod is running, and that its `ttl` is at
least 1 when it runs on another machine. The
[setup-checks runbook](../RUNBOOKS/setup-checks.md) explains the other checks.

## 7. Raise the receive buffer

Packets wait in the kernel's receive buffer until manta reads them, and the
kernel drops what does not fit. manta asks for 4 MiB and uses whatever the
kernel grants. With Linux's default `net.core.rmem_max` the grant is
425 984 bytes. On loopback, a reader that stopped draining kept 33 300 samples
of `f32le` IQ before the kernel dropped the rest: 0.35 s at 96 kS/s, 0.17 s at
192 kS/s. A network card's per-packet overhead can make that shorter. Raise the
limit, and keep it across reboots:

```sh
sudo sysctl -w net.core.rmem_max=4194304
echo 'net.core.rmem_max = 4194304' | sudo tee /etc/sysctl.d/90-manta-ka9q.conf
```

Restart manta afterwards; it asks for the buffer when it opens the channel.
(While `open` waits for the channel's status manta also drains the socket
itself, every millisecond, into a stash that keeps the newest 4 MiB, so the 1.5 s
no-`ssrc` settle window does not overflow the default grant at any rate manta
accepts; the raised limit is about steady-state reading.)
Lost packets show on manta's [metrics endpoint](../METRICS.md) as
`manta_input_dropped_packets_total{source="ka9q"}`, counted from RTP sequence
numbers, and `manta_input_gaps_detected_total{source="ka9q"}`:

```sh
curl -s 127.0.0.1:7302/metrics | grep 'source="ka9q"'
```

A dropped-packet count that keeps rising means the buffer, the CPU or the
network cannot keep up. `manta_source_health{source="ka9q"}` reads 0 until the
first packet of the channel arrives, then 1. How many packets a Raspberry Pi 4
loses at 192 kS/s, with the default and the raised limit, has not been measured.

## 8. Docker

The image's default bridge network does not receive radiod's multicast. Run the
container on the host's network. In the kit's `docker-compose.yml`
([Docker Compose](../../packaging/README.md#docker-compose)), add
`network_mode: host` to the `manta` service, delete its `ports:` list (host
networking publishes nothing; manta binds the host's ports directly), and delete
the `MANTA_SERVER_METRICS_BIND_ADDR: "0.0.0.0"` line so metrics stay on the
host's loopback:

```yaml
services:
  manta:
    network_mode: host
    environment:
      RUST_LOG: info
```

Keep the rest of the service as it is. With host networking, telnet and JSON
listen on every host interface, so apply the
[firewall step](rbn-node.md#6-firewall-and-exposure). The image has no mDNS
resolver: set `group` to the address. This was written for Docker Engine on
Linux.

## 9. Retunes end the run

manta reads the channel's frequency when it opens it. If radiod retunes the
channel, manta's read fails, manta reopens the channel, finds the new
frequency, and stops with exit 1. A running manta cannot move its decoder to a
new centre frequency:

```text
source ka9q lost: ka9q-radio channel ssrc 14100000 retuned from 14100.0 kHz to 14050.0 kHz; reconnecting in 1s
error: source ka9q reopened with a different center frequency (14050.0 kHz, expected 14100.0 kHz)
```

Run manta under the packaging kit's supervisor, which starts it again at the
new frequency: the systemd unit (`Restart=always`, after 10 s) in
[step 7 of the node guide](rbn-node.md#7-run-it-as-a-service), or Compose's
`restart: unless-stopped`. A stream that stops for 10 s is a different case:
manta logs `source ka9q lost: ka9q-radio stream stalled: ...` and reconnects,
waiting 1 s at first and up to 60 s, until radiod is back. A change of encoding
alone reopens cleanly. Changing the channel's `low` or `high` filter edge ends
the run the same way as a retune (`reopened with a different passband`), since
manta decodes and reports only the passband it opened with.

## 10. Choose an encoding

manta reads `s16le`, `s16be`, `f16le`, `f16be`, `f32le` and `f32be`.

- `f32le` sends 32-bit floats: 8 bytes per IQ sample, about 1.5 MB/s at
  192 kS/s. A float does not clip, so a fixed gain set for weak signals still
  carries strong ones.
- `s16be`, radiod's default, sends 16-bit integers: half the bandwidth. With
  AGC off, the fixed gain must keep the strongest signal below full scale and
  the weakest above the 16-bit floor. Whether it decodes as well as `f32le` on
  a live radiod has not been measured.

On the radiod machine, or on a wired LAN with IGMP snooping, use `f32le`. Use
`s16be` when the network is the constraint.

radiod builds from 2026-01-07 to 2026-01-10 numbered the float encodings
differently. If your radiod was built from source in that window, update it.

## Next

- Steps 4 to 8 of [Running an RBN node with manta](rbn-node.md): the telnet
  check, the uplink in dry-run, the firewall and the service.
- [Node health](../RUNBOOKS/node-health.md) for watching a running node.
- The [MAN-140 decision record](../DECISIONS/2026-10-13-man140-ka9q-radio-input.md)
  for the wire format manta expects and what is still unverified.
