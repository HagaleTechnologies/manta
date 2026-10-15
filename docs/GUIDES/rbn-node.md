# Running an RBN node with manta

This guide ends with manta decoding your receiver, serving DX-cluster telnet on
7300 and JSON on 7301, logged in to your upstream collector in dry-run,
firewalled, and running as a service. The commands use a Linux host and a network
receiver. Run them from the manta source checkout or an extracted release kit.
Replace every `<...>` placeholder before running a command; omit the angle brackets.

What this guide does not do: it does not get you admitted to the Reverse Beacon
Network. The RBN team admits skimmers. The supported route into RBN is an RBN
Aggregator reading manta's telnet server: follow steps 1–4 here, then the
[secondary-skimmer field-node runbook](../RUNBOOKS/secondary-skimmer-field-node.md),
which adds the private soak and spot-quality checks that path requires. Step 5's
direct uplink is experimental. Its login and spot format are unverified against
RBN's real ingest, so this guide keeps it in dry-run. Arrange a collector and
login with its operator before step 5.

One manta process covers one receiver passband on one band.

## 1. Install manta

Today, build from source using [Installation](../../README.md#installation),
including its Linux ALSA development packages and Rust toolchain prerequisites:

```sh
git clone https://github.com/HagaleTechnologies/manta
cd manta
cargo install --path crates/manta-cli --features hpsdr
manta --version
```

Keep `--features hpsdr` to support OpenHPSDR receivers. The version output names
the version, git commit and enabled features. If manta is already installed, run
only the version check. Once a release exists, use the download and verification
notes in [Installation](../../README.md#installation); keep the kit files next to
the binary. On Debian-family hosts, the probes and firewall below need these tools:

```sh
sudo apt update
sudo apt install -y telnet netcat-openbsd curl ufw
```

## 2. Set up and check your receiver

Connect the receiver's antenna, choose a CW segment, and use its actual frequency.
Stop other manta processes using that receiver before checking it.

| Receiver | Setup |
| --- | --- |
| KiwiSDR | Prefer your own receiver, or read [KiwiSDR etiquette](kiwisdr.md) first. Get its hostname, port and any user password. Reserve one channel; obtain the owner's agreement before unattended use of a public receiver. |
| OpenHPSDR / Hermes-Lite 2 | Use an `hpsdr` build and the radio's IP, centre frequency and sample rate. Protocol support has not yet been verified against hardware. |
| SoapySDR | Install the native SoapySDR library and receiver driver, then build with `--features hpsdr,soapy`. Release binaries do not include SoapySDR. |
| Sound card | Use a 48000 Hz input device and set `[input].center_freq_hz` to the actual radio dial frequency. The packaged service below needs a network receiver. |

This tutorial uses KiwiSDR. The command assumes port 8073; add
`--kiwi-port <your-kiwi-port>` if yours differs. For a password-protected receiver,
first write the config in step 3, set `[input].password`, then use
`manta check --config manta.toml --duration 5` instead.

The following is a captured example. Sample counts, power and noise depend on
your receiver; 96000 Hz is manta's resampled stream, not 96 kHz of RF coverage.

```console
$ manta check --kiwi-host <your-kiwi-host> --kiwi-freq-hz 7030000 --duration 5
Source: kiwi
Stream sample rate: 96000 Hz
Center frequency: 7030000 Hz
Passband offsets: -5000 to 5000 Hz
Samples received: 480000 (5.000 s)
Input power: -17.2 dBFS
Noise floor: -44.8 dBFS median; -48.2 to -43.2 dBFS range across 107 channels
Floor estimate: brief, per-channel lower quartile; not calibrated RF power.
Stopped: sample window complete
```

A failed check means fix the receiver address, port, password or antenna path
before starting the node. `manta devices` lists local inputs; the
[config reference](../SPEC-decode-core.md#9-configuration-keys) covers other sources.

## 3. Write the config

Create the commented scaffold, or copy [manta.example.toml](../../manta.example.toml):

```sh
manta config init
chmod 0600 manta.toml
```

Edit `manta.toml` into the following minimal config. Replace existing tables;
don't append a second `[server]` or `[input]`. Replace `N0CALL` with your call and
`<your-kiwi-host>` with the receiver's hostname. `config check` refuses either
placeholder until you replace it. Set the port and frequency for your receiver.

```toml
[server]
station_callsign = "N0CALL"      # your call; RBN SSIDs like N0CALL-1 are allowed
# bind_addr defaults to 0.0.0.0 (every interface); section 6 decides who can reach it
# metrics_bind_addr defaults to 127.0.0.1 (this machine only)

[input]
type = "kiwi"
host = "<your-kiwi-host>"
port = 8073                      # <your-kiwi-port>: change if your receiver is not on 8073
freq_hz = 7030000.0              # centre of the CW segment you want; manta covers ±5 kHz of it
# password = "<receiver-password>"   # see kiwisdr.md
```

Validate before opening a receiver or listener:

```sh
manta config check --config manta.toml
```

It prints `manta.toml: valid (from --config)` and the resolved settings. Check the
`environment:` line too: `MANTA_*` variables override the file, and CLI flags
override both. Validation does not contact the receiver. The full captured output
below includes the uplink added in step 5; before that, your uplink line differs.
`W5AU`, `kiwi.example.net` and `collector.example.net` are substituted example
values, not a receiver or collector to connect to. The final notes are on stderr.

```console
$ manta config check --config manta.toml
manta.toml: valid (from --config)
environment: none
server: station_callsign=W5AU bind_addr=0.0.0.0 metrics_bind_addr=127.0.0.1 telnet_port=7300 json_port=7301 metrics_port=7302 line_format=rbn
rbn_uplink: target_host=collector.example.net target_port=7000 enabled=true dry_run=true spot_types=cq_beacon login_callsign=W5AU
input: type=kiwi host=kiwi.example.net port=8073 freq_hz=7030000 password=none
spot: allowlist=none blocklist_path=none notch_path=none cty_path=bundled scp_path=bundled cty_sha256=none scp_sha256=none refresh_lists=off list_cache_dir=none cty_url=http://www.country-files.com/cty/cty.dat scp_url=http://www.supercheckpartial.com/MASTER.SCP
detector: defaults
decode: engine=legacy (other keys default)
note: JSON spots will carry no station position (deGrid, deLat and deLon are null) -- set [server].operator_grid, or operator_lat and operator_lon
note: the telnet and JSON servers listen on every network interface (bind_addr = "0.0.0.0"), as a public cluster node does -- set bind_addr = "127.0.0.1" to keep them local
note: [[rbn_uplink]] is experimental -- its login and spot format are unverified against RBN's real ingest; to reach RBN today, let an RBN Aggregator read this node's telnet server (docs/RUNBOOKS/secondary-skimmer-field-node.md)
```

Before the first run on an internet-reachable host, apply the firewall in step 6.
Telnet and JSON bind every interface as soon as manta starts.

## 4. Run it and watch the telnet output

Keep this running in the first terminal:

```sh
manta run --config manta.toml
```

From a second terminal, connect with telnet and enter your callsign at the prompt.
The greeting below was captured live; the `DX de` line is a format example from a
separate 20 m replay. Your call, frequency, timestamp and stations will differ.

```console
$ telnet localhost 7300
Welcome to the manta Telnet cluster port!
manta 0.1.0 is operated by W5AU
Please enter your callsign: N0CALL-9
de W5AU-# >
DX de W5AU-#:   14012.36  W1AW           CW    28 dB  20 WPM  CQ      0719Z
```

To fill a logger's band map when it connects, add `sh/dx 30m` to its
cluster connect commands. With the default `[server].history_minutes`,
manta retains the last 30 minutes for replay. A bare `sh/dx` shows the
last 10. The newest 50 spots are always retained, up to a ceiling of
10,000. Changes to the configured window need a restart. While
`manta_spots_history_truncated_total` on the
[metrics endpoint](../RUNBOOKS/node-health.md) is rising, the ceiling is
cutting history short of the configured window.

Wait for calls on an active band. A login prompt proves that the listener is up;
a `DX de` line proves a spot reached the client. Exit telnet with Ctrl+] then
`quit`. In another terminal, watch the JSON stream:

```sh
nc localhost 7301
```

It prints a `hello` line, then one JSON object per new spot. Ctrl+C closes the client.

### No spots yet?

A spot needs a decoded callsign in a CQ. A quiet 10 kHz slice can go many minutes
without one. `curl -s localhost:7302/status` shows `active_tracks`: above 0 means
manta hears CW signals and is decoding; 0 for minutes on a busy segment means
check the frequency and antenna with `manta check`.

The same document has `spots_total`. Stop the foreground daemon with Ctrl+C before
running another receiver check, so you consume only one Kiwi channel. Decoder
activity alone does not prove spot accuracy; compare what you hear and the spots
before arranging live forwarding.

## 5. Connect the uplink in dry-run

Stop the foreground daemon with Ctrl+C. Append this block to `manta.toml`, replacing
the host and port with the collector operator's values. The login defaults to
`station_callsign`; if the collector assigned a different login, set
`login_callsign` in this block to that callsign. The uplink is experimental (see
[the MAN-90 record](https://github.com/HagaleTechnologies/manta/blob/main/docs/DECISIONS/2026-10-11-man90-uplink-protocol-verification.md));
do not point it at an RBN server.

```toml
[[rbn_uplink]]
enabled = true
target_host = "<collector-host>"   # the collector you were given
target_port = 7000                 # <collector-port>
# dry_run defaults to true: manta connects and logs in but sends no spots.
```

Check the config again, confirm `dry_run=true` in the summary, then restart it:

```sh
manta config check --config manta.toml
manta run --config manta.toml
```

Look for this startup message, preceded by the log timestamp:

```text
INFO manta_server::uplink: uplink: dry_run is ON (the default) -- connecting and logging in, but NOT transmitting spots. Set `dry_run = false` in this [[rbn_uplink]] block to transmit for real.
```

The message confirms dry-run, not a successful login. In the second terminal:

```sh
manta status --config manta.toml
```

It exits 0 when every enabled target is connected. Confirm that your target is
listed as connected: exit 0 can also mean no uplink is configured. `SENT` stays
0 in dry-run; `SUPPR` rises when there are spots to withhold. The
[uplink-health runbook](../RUNBOOKS/uplink-health.md) explains down and flapping
targets. Dry-run does not suppress local telnet or JSON spots.

### Going live

Keep dry-run on for the rest of this tutorial. Only after the collector's operator
agrees to accept your spots and confirms their collector expects manta's login and
`DX de` lines, set `dry_run = false`. For the service in step 7, edit
`/etc/manta/manta.toml`, validate it with
`sudo manta config check --config /etc/manta/manta.toml`, then run
`sudo systemctl restart manta`. The kit uses `LoadCredential`, so
`sudo systemctl reload manta` reads the startup copy and cannot pick up an edit
to `/etc/manta/manta.toml`. SIGHUP can apply `dry_run` without a restart when manta
reads an editable config directly; see the
[service kit's config-access notes](../../packaging/README.md#linux-systemd).
Confirm the target's `SENT` count rises when new qualifying spots arrive.

## 6. Firewall and exposure

`bind_addr` covers telnet **and** JSON together. To keep 7300 reachable and 7301
private, use a firewall. Metrics (7302, including `/status` and `/healthz`) is
loopback by default. Leave it there. The uplink is outbound and needs no inbound
rule. A telnet callsign is an identifier, not authentication.

On a host using UFW, first check existing rules with `sudo ufw status numbered`.
Remove any existing broad allow rule that exposes 7301 or 7302; setting the default
to deny does not cancel existing allows. Allow your actual SSH port before enabling
the firewall if it differs from the OpenSSH profile. Then:

```sh
sudo ufw default deny incoming
sudo ufw allow OpenSSH
sudo ufw allow 7300/tcp            # or: sudo ufw allow from <aggregator-ip> to any port 7300 proto tcp
sudo ufw enable
# from another machine:
nc -vz <node> 7300    # succeeds
nc -vz <node> 7301    # fails
nc -vz <node> 7302    # fails
```

For a private feed, use the source-IP rule instead of the unrestricted 7300 rule,
and test from that allowed machine. If the host sits behind a router or cloud
security group, allow or forward only the intended telnet traffic there too.
Check the effective addresses with `manta config check`. For proxies, containers
and remote metrics scraping, follow the
[network-exposure runbook](../RUNBOOKS/network-exposure.md).

## 7. Run it as a service

Stop the foreground manta with Ctrl+C and wait for it to exit before starting a
second process. This kit uses a network receiver; its temporary user has no sound
card or USB device permissions. Check systemd is 247 or newer:

```sh
systemctl --version
```

Run these commands from the repository root or extracted release kit. The install
lines match [packaging/README.md](../../packaging/README.md#linux-systemd).
For a source build, replace `./manta` in the first line with `target/release/manta`
after `cargo build --release -p manta-cli --features hpsdr`, or with the full path
to the binary installed by `cargo install` in step 1.

```sh
sudo install -m 0755 ./manta /usr/local/bin/manta
sudo install -d -m 0755 /etc/manta
sudo install -m 0600 manta.toml /etc/manta/manta.toml
sudo /usr/local/bin/manta config check --config /etc/manta/manta.toml
sudo install -m 0644 packaging/systemd/manta.service /etc/systemd/system/manta.service
sudo systemd-analyze verify /etc/systemd/system/manta.service
sudo systemctl daemon-reload
sudo systemctl enable --now manta
journalctl -u manta -f
```

The install commands are silent on success; unit verification must report no
errors. Ctrl+C exits the journal viewer without stopping manta. Use
`sudo systemctl stop manta` to stop the service. It otherwise restarts after an
exit, including a receiver or config error. If journal access is denied, use
`sudo journalctl -u manta -f`. For macOS launchd and Docker Compose, follow the
[service kit](../../packaging/README.md).

## 8. Your node is working when

- `systemctl is-active manta` prints `active`.
- `curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:7302/healthz` prints `200`.
- `sudo manta status --config /etc/manta/manta.toml` exits 0, with the uplink target connected (dry-run).
- `telnet <node> 7300` from another machine reaches the login prompt, and `DX de` lines appear while the band is active.
- `sudo ufw status` lists only the ports you meant to open.

## Next

- [KiwiSDR etiquette](kiwisdr.md) and [manta on a Raspberry Pi](raspberry-pi.md).

- [Node health](../RUNBOOKS/node-health.md) and [uplink health](../RUNBOOKS/uplink-health.md).
- [Keeping the prefix table current](../../README.md#keeping-the-prefix-table-current).
- [Secondary-skimmer field node](../RUNBOOKS/secondary-skimmer-field-node.md) for an Aggregator and a longer quality/uptime trial.
