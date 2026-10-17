# Using a KiwiSDR with manta

Read this before leaving manta connected to a shared receiver. For the complete
config, telnet check and service setup, follow
[Running an RBN node with manta](rbn-node.md).

## What one KiwiSDR channel gives manta

manta asks a KiwiSDR for one channel: a 10 kHz IQ slice, ±5 kHz around `freq_hz`
(`manta check` prints `Passband offsets: -5000 to 5000 Hz`). That is one CW
sub-band on one band. An RBN skimmer covers whole CW segments, typically on
several bands at once. A Kiwi-fed manta is not an RBN-grade node on its own.
It is a way to try manta, a local spot feed, or, with the collector's agreement,
extra coverage where there are no skimmers.

The Kiwi's native IQ stream is about 12 kHz. manta resamples it to 96 kS/s for
its channelizer; that does not widen the 10 kHz of RF it requested. See the
[KiwiSDR input notes](../DECISIONS/2026-07-25-m2-kiwisdr-input-pins.md).

## Several CW segments from one Kiwi: `bands`

`bands` plans one Kiwi channel per slice of a band's CW segment, instead of
one `freq_hz` (MAN-310, [Band plans](../CONFIG.md#band-plans-bands)):

```toml
[server]
station_callsign = "N0CALL"

[input]
type = "kiwi"
host = "<your-kiwi-host>"
bands = ["60m"]
```

60 m (5351.5 to 5366.5 kHz) is wider than one channel, so this opens two
sessions, `kiwi-60m-1` at 5355.25 kHz and `kiwi-60m-2` at 5362.75 kHz, each
starting tracks only on its own side of the split. Each stream is one Kiwi
channel: keep a plan within the channels the owner lets you use. 40 m is
five channels, and `"all"` (86) is over manta's 16 inputs.

## Use your own receiver, or a reserved slot

Best: your own Kiwi, on your own network. Connections from the local network are
exempt from a Kiwi's time limits. To keep a slot for manta, set a User password
and leave the other channels free with 'Number of channels not requiring a
password even if password set'. Then put that password in `[input] password`.
These are the settings in the
[KiwiSDR admin UI](https://github.com/jks-prv/Beagle_SDR_GPS/blob/master/web/kiwi/admin.js).

For example, leave three of four channels open and use the fourth as the private
password slot. Ask the owner to reserve a slot if it is someone else's receiver;
a publicly listed receiver does not imply permission for a permanent service.

## Someone else's public receiver: the rules

1. Ask the owner before you run manta unattended against their receiver, and use
   the password they give you.
2. One manta process uses one channel. Don't run several against the same receiver
   to cover more spectrum.
3. Check the [directory entry](https://kiwisdr.com/public/). A full receiver, or
   one whose owner allows no non-Kiwi apps (the admin setting "channels available
   for connection by non-Kiwi apps"), is not yours to use.
4. Respect time limits. manta can't present a time-limit exemption password and
   doesn't recognise being refused or kicked. It reconnects after 1 s, backing off
   to 60 s, forever, and the systemd service restarts it too. Never install the
   service against a public receiver without the owner's agreement, and stop
   manta if the owner asks.

The admin's User exemption password for time limits is different from the User
password that manta sends. Supplying it as `[input].password` does not implement
the separate exemption protocol. Stop a foreground process with Ctrl+C. For the
packaged service, use `sudo systemctl stop manta`, which prevents its automatic
restart. Close a check or listening session before opening the next one, including
when following the [node tutorial](rbn-node.md#2-set-up-and-check-your-receiver).

## The password

The password travels unencrypted (`ws://`). Use one that isn't used anywhere
else. `config check` prints `password=set`, never the value. Keep the config at
mode 0600; the systemd kit already does this
([packaging/README.md](../../packaging/README.md#linux-systemd)).

In the tutorial's existing `[input]` table, uncomment
`password = "<receiver-password>"` and replace the placeholder. Don't add a second
`[input]` table. Use the config file instead of putting the password in shell
history:

```sh
chmod 0600 manta.toml
manta config check --config manta.toml
manta check --config manta.toml --duration 5
```

For a Pi host, continue with [manta on a Raspberry Pi](raspberry-pi.md).
