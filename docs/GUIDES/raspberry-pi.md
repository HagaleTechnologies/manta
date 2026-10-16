# manta on a Raspberry Pi

This quickstart builds manta on a Pi and installs the packaged systemd service
for a network receiver. Use the [RBN node tutorial](rbn-node.md) alongside it for
uplink dry-run and firewall setup.

## Board and OS

A Raspberry Pi 5 is the board to use. A Pi 4 is a project target, but whether it
keeps up with manta has not been measured and the measurement is paused
([D6 of the 2026-09-06 broad review](../DECISIONS/2026-09-06-broad-review-decisions.md#d6--pi4-cpu-budget-story-paused)).
Don't publish Pi 4 results;
[docs/RUNBOOKS/m2-pi4-cpu-budget.md](../RUNBOOKS/m2-pi4-cpu-budget.md) is how to
measure one once that work resumes. Using a Pi 5 is not a claim that the Pi 4
CPU-budget gate passed.

Install [64-bit Raspberry Pi OS](https://www.raspberrypi.com/documentation/computers/os.html),
Bookworm or later. The Lite edition is enough for a headless node. Configure the
network and SSH during OS installation, then log in. This guide does not cover
32-bit Raspberry Pi OS. Check the architecture:

```sh
uname -m
```

It must print `aarch64`. Use the official power supply and a heatsink or fan so
power or heat does not throttle the board during decoding.

## 1. Install the build tools

```sh
sudo apt update && sudo apt install -y git build-essential pkg-config libasound2-dev telnet
sudo apt install -y curl netcat-openbsd ufw
```

ALSA headers are required even for a network-only receiver. Telnet is not
installed by default; curl, netcat and UFW are used by the node tutorial.

## 2. Get manta

Today, build from source. Install rustup, which reads the checkout's
[rust-toolchain.toml](../../rust-toolchain.toml). The Pi OS packaged `rustc` is
older than manta's pinned 1.98.0 toolchain. The installer prompts for its options;
use the default installation, then load Cargo's environment in this shell:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
. "$HOME/.cargo/env"
git clone https://github.com/HagaleTechnologies/manta
cd manta
cargo build --release -p manta-cli
sudo install -m 0755 target/release/manta /usr/local/bin/manta
manta --version
```

The version line includes `features: none`; HPSDR is included. Keep this source
checkout for the config and service files used below; all subsequent relative
paths start here.

Once a release exists, its `manta-linux-arm64` archive replaces the build.
Follow [release verification](../RUNBOOKS/release.md#verifying-a-downloaded-release),
extract the archive, and install its `manta` binary at `/usr/local/bin/manta`.
A Pi running a prebuilt binary needs only the ALSA runtime, `libasound2` on
Bookworm or `libasound2t64` on Debian 13-based releases, plus the diagnostic tools
above. Keep the extracted kit directory for the remaining steps.

## 3. Configure your receiver

```sh
cp manta.example.toml manta.toml
chmod 0600 manta.toml
```

Edit the copy before checking it. Set your station callsign and use the KiwiSDR
config in [Write the config](rbn-node.md#3-write-the-config), replacing the existing
tables rather than appending duplicates. Read [KiwiSDR etiquette](kiwisdr.md)
before using a public receiver.

For a Hermes-Lite 2 or other OpenHPSDR receiver, this is a complete alternative
minimal config. Replace `N0CALL` and `<your-hpsdr-host>`, choose the actual centre
frequency, and use a sample rate the radio supports. This example requests 192 kS/s:

```toml
[server]
station_callsign = "N0CALL"

[input]
type = "hpsdr"
host = "<your-hpsdr-host>"
freq_hz = 7030000.0
rate_hz = 192000.0
```

It uses the default HPSDR port 1024. Protocol support is implemented, but live
HPSDR hardware validation remains outstanding. Check your edited config:

```sh
manta config check --config manta.toml
manta check --config manta.toml --duration 5
```

The first command must print `valid`; the second must receive samples. Fix any
error before continuing. This recipe supports network receivers only. A USB SDR
needs a build with `soapy`, its driver, and a service user in the device groups;
the packaged `DynamicUser` service grants no such access. For that setup use the
[static-user field-node unit](../RUNBOOKS/secondary-skimmer-field-node.md#3-install).

## 4. Test it in the foreground

Apply [Firewall and exposure](rbn-node.md#6-firewall-and-exposure) before starting
on an internet-reachable host. Then:

```sh
manta run --config manta.toml
```

From a second SSH session on the Pi:

```sh
telnet localhost 7300
```

Enter your callsign. Expect the greeting and, when calls are decoded, `DX de`
lines as in [the telnet check](rbn-node.md#4-run-it-and-watch-the-telnet-output).
If no spots arrive, use that section's `/status` check before blaming CPU speed.
Set up the [uplink in dry-run](rbn-node.md#5-connect-the-uplink-in-dry-run) if a
collector operator has supplied a target. Stop the foreground process with Ctrl+C
and wait for it to exit before installing the service.

## 5. Install the service

Check `systemctl --version`: the kit requires systemd 247 or newer; Bookworm
ships 252. Run the following from the source checkout or extracted release kit.
The binary was installed in step 2, so only the edited config and unit remain:

```sh
systemctl --version
sudo install -d -m 0755 /etc/manta
sudo install -m 0600 manta.toml /etc/manta/manta.toml
sudo /usr/local/bin/manta config check --config /etc/manta/manta.toml
sudo install -m 0644 packaging/systemd/manta.service /etc/systemd/system/manta.service
sudo systemd-analyze verify /etc/systemd/system/manta.service
sudo systemctl daemon-reload
sudo systemctl enable --now manta
journalctl -u manta -f
```

These install lines match the [service kit](../../packaging/README.md#linux-systemd).
They are silent on success; `systemd-analyze verify` must report no errors. If the
journal is inaccessible, use `sudo journalctl -u manta -f`. Ctrl+C leaves the
journal viewer; manta continues to run and will start at boot. Verify:

```sh
systemctl is-active manta
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:7302/healthz
sudo manta status --config /etc/manta/manta.toml
```

Expect `active`, HTTP `200`, and a connected dry-run uplink if configured. Complete
the [node checklist](rbn-node.md#8-your-node-is-working-when), including probes
from another machine. The service copies the root-owned config with
`LoadCredential`; after editing `/etc/manta/manta.toml`, check it and restart with
`sudo systemctl restart manta`. Reload cannot see edits to that source file.
To stop the service without its automatic restart, use `sudo systemctl stop manta`.

## 6. Is the Pi keeping up?

After the service has received samples for a minute, check power and cooling:

```sh
vcgencmd get_throttled
```

Anything but `throttled=0x0` means power or heat problems now or since boot.
Resolve these before assessing performance. Then read the decode time:

```sh
curl -s 127.0.0.1:7302/metrics | awk '/^manta_decode_latency_seconds_(sum|count) /{v[$1]=$2} END{printf "%.2f ms per chunk\n", 1000*v["manta_decode_latency_seconds_sum"]/v["manta_decode_latency_seconds_count"]}'
```

The Pi keeps up while this stays well under 21.3 ms for a KiwiSDR (96 kS/s) or
10.7 ms at 192 kS/s. That is a running average; for the p99 use the Prometheus
query in [node-health.md](../RUNBOOKS/node-health.md). A low average alone does
not rule out stalls; keep p99 below the same chunk budget. If the count is zero,
no chunks were decoded yet: check `/healthz` and the receiver before interpreting
the calculation. These thresholds use `2048 / sample_rate` seconds per chunk;
they are not a Pi benchmark result. Pi hardware has not been tested for this guide.

## Upgrading

Stop the service, build or download the new binary, then replace it. For a source
build from your updated checkout:

```sh
sudo systemctl stop manta
cargo build --release -p manta-cli
sudo install -m 0755 target/release/manta /usr/local/bin/manta
sudo install -m 0644 packaging/systemd/manta.service /etc/systemd/system/manta.service
sudo systemctl daemon-reload
sudo /usr/local/bin/manta config check --config /etc/manta/manta.toml
sudo systemctl start manta
```

Keep `/etc/manta/manta.toml`; do not reinstall the example over your edited config.
Compare new example keys with your settings, then repeat the health and telnet
checks. The [kit's upgrade notes](../../packaging/README.md#upgrading-or-reinstalling)
also cover replacing a release binary.
