# Running manta unattended

This kit runs manta as a service that starts at boot, starts again when it
exits, and stops cleanly: a systemd unit for Linux, a LaunchDaemon for
macOS, and a Docker Compose file. It ships in every release archive. Paths
in this guide are relative to the top-level directory of the extracted
archive (in a source checkout, the repository root); run every command from
there.

| File | Purpose |
|---|---|
| [`manta.example.toml`](../manta.example.toml) | Example config: the station callsign is active, every other setting is commented out at its default |
| [`docker-compose.yml`](../docker-compose.yml) | Docker Compose service |
| [`packaging/systemd/manta.service`](systemd/manta.service) | systemd unit |
| [`packaging/launchd/com.hagaletechnologies.manta.plist`](launchd/com.hagaletechnologies.manta.plist) | macOS LaunchDaemon |
| [`packaging/launchd/com.hagaletechnologies.manta-logrotate.plist`](launchd/com.hagaletechnologies.manta-logrotate.plist) | macOS log rotation job |
| [`packaging/launchd/create-service-account.sh`](launchd/create-service-account.sh) | Creates the macOS `_manta` service account |
| [`packaging/launchd/rotate-log.sh`](launchd/rotate-log.sh) | The script the log rotation job runs |
| [`docs/RUNBOOKS/network-exposure.md`](../docs/RUNBOOKS/network-exposure.md) | Which ports are public, and how to change that |

## Before you start

- **A network receiver.** The recipes are written for a KiwiSDR, which
  every build supports and which needs no device permissions. An HPSDR
  radio on the network works the same way. As shipped, none of the recipes
  gives manta access to a local sound card or USB SDR; the Docker recipe
  can pass a USB receiver through (see "USB receivers" under Docker
  Compose). Every build includes KiwiSDR
  and HPSDR; SoapySDR is optional and has separate Linux x86_64 and arm64
  release archives, and the Docker image includes it.
  Before you run the service against a receiver you don't own, read [the KiwiSDR guide](../docs/GUIDES/kiwisdr.md).
- **Linux:** the binary needs the ALSA runtime library even when it never
  opens a sound card: `sudo apt install libasound2` on Debian, Ubuntu and
  Raspberry Pi OS (`libasound2t64` on Debian 13). Without it, `./manta`
  does not start at all and the system prints
  `error while loading shared libraries: libasound.so.2: cannot open shared object file`;
  see [setup-checks](https://github.com/HagaleTechnologies/manta/blob/main/docs/RUNBOOKS/setup-checks.md#manta-will-not-start-at-all).
- **macOS:** release binaries are not signed or notarized. If macOS
  refuses to run `./manta` because it was downloaded from the internet,
  clear the flag with `xattr -d com.apple.quarantine ./manta`.
- **Source checkout:** build with
  `cargo build --release -p manta-cli` and use
  `target/release/manta` wherever a command below says `./manta`.

## Configure first

Every recipe needs an edited `manta.toml`. Make it before you install
anything:

```sh
cp manta.example.toml manta.toml
```

Copy the example, replace N0CALL with your station callsign, and configure your receiver. For audio or WAV input, set the actual radio dial frequency in input.center_freq_hz or pass --dial-freq-hz. Never use the example frequency as a substitute for your receiver's frequency.

**Station.** Under `[server]`, edit the active `station_callsign = "N0CALL"`
line, not the commented reference line above it. Your call is sent as the
spotter of every spot; an RBN SSID such as `-1` is allowed. The
`operator_*` lines are optional. Set `operator_grid` (or `operator_lat` and
`operator_lon`) to put your station's position on JSON spots; without it
they carry none.

**Receiver.** For a KiwiSDR, find these lines in the `[input]` table that
is already in the file, delete the `#` at the start of each, and edit the
values. Do not add a second `[input]` table: a duplicate table is a TOML
error.

```toml
[input]
type = "kiwi"
host = "<your-receiver-host>"
port = 8073
freq_hz = 7030000.0
```

Replace `<your-receiver-host>` with your receiver's host name or address,
change `port` if it is not 8073, and set `freq_hz` to the frequency, in Hz,
you want the receiver tuned to. Set `password` only if the receiver needs
one. For an HPSDR radio, set `type = "hpsdr"`, `host`, `freq_hz` and
`rate_hz` instead; its `port` defaults to 1024.

**Check it.**

```sh
./manta config check --config manta.toml
```

It prints `manta.toml: valid (from --config)` and one line per table with
the settings it resolved, plus notes on stderr, such as that telnet and
JSON listen on every interface. When something is wrong it exits 1 naming
the setting, including any `<...>` example value you uncommented but did
not edit. It does not contact the receiver: a wrong host name or port
shows up only when manta runs.

Then check the machine and the receiver together:

```sh
./manta doctor --config manta.toml
```

It checks that the server ports are free, the clock against an NTP server,
each enabled uplink target, and that the receiver opens and hears
something, printing `PASS`, `WARN`, `FAIL` or `SKIP` with a fix for every
problem, and exits 1 when a check fails. Run it before you install the
service, not while it runs. See
[setup-checks](https://github.com/HagaleTechnologies/manta/blob/main/docs/RUNBOOKS/setup-checks.md).

**Uplinks.** Leave the `[[rbn_uplink]]` block commented out. If you enable
an uplink later, `dry_run` defaults to `true`: it connects and logs in but
sends no spots until you set `dry_run = false`. Going live also needs
`[server].operator_grid` set to a 6-character Maidenhead locator (e.g.
`"FN03ab"`); `config check` refuses the flip without one. The uplink is
experimental: its login and spot format are unverified against RBN's real
ingest. To reach RBN, let an RBN Aggregator read the telnet server.

**If `N0CALL` is left in place**, manta refuses to start:

```console
$ ./manta run --config manta.toml
error: manta.toml: server.station_callsign is still the example "N0CALL" -- set your own callsign
```

It exits with status 1 before it opens the receiver or any listener;
`manta listen` behaves the same, and so does an uncommented
`[[rbn_uplink]]` block whose `login_callsign` is still `N0CALL`. The check reads the effective value, so a
`MANTA_SERVER_STATION_CALLSIGN` variable holding your call also satisfies
it. Under a service manager the service fails, is started again, and logs
this line each time until you fix the config.

**What the service logs.** manta writes a `SPOT:` line to stdout for each
confirmed spot, and its startup, status, warning and error lines to
stderr. It does not log decoded text; add `--decoded-text` to the command
line to include it, one line per track.

## Debian, Ubuntu and Raspberry Pi OS: the package

On Debian 12 (bookworm) or later, Ubuntu 24.04 and 64-bit Raspberry Pi OS
bookworm, install the Debian package instead of the archive: it puts the
binary, the systemd unit and a config in place, so there is nothing to copy
by hand (MAN-540, [decision record](https://github.com/HagaleTechnologies/manta/blob/main/docs/DECISIONS/2026-11-09-man540-debian-package.md)). Use
`manta-linux-arm64.deb` on a Pi or other arm64 board and
`manta-linux-x86_64.deb` on a PC:

```sh
curl -fLO https://github.com/HagaleTechnologies/manta/releases/latest/download/manta-linux-arm64.deb
curl -fLO https://github.com/HagaleTechnologies/manta/releases/latest/download/SHA256SUMS
sha256sum -c --ignore-missing SHA256SUMS
sudo apt install ./manta-linux-arm64.deb
sudoedit /etc/manta/manta.toml
sudo manta config check --config /etc/manta/manta.toml
sudo systemctl start manta
journalctl -u manta -f
```

`sha256sum` prints `manta-linux-arm64.deb: OK`. The end of `apt install`
prints:

```text
Setting up manta (0.2.0-1) ...
manta: wrote /etc/manta/manta.toml. manta.service is enabled but not started.
manta: set [server] station_callsign and an [input] receiver in it, then run
manta:   sudo manta config check --config /etc/manta/manta.toml
manta:   sudo systemctl start manta
```

If the `.deb` sits in a directory the `_apt` user cannot read, such as your
home directory, apt also prints `N: Download is performed unsandboxed as
root ...`. That notice is harmless: the file is already on your disk.

What the package puts on disk:

- `/usr/bin/manta`, the same binary as the matching `.tar.gz` archive, and
  its man pages under `/usr/share/man/man1/`.
- `/usr/lib/systemd/system/manta.service`, this kit's unit with one change:
  `ExecStart=` runs `/usr/bin/manta` instead of `/usr/local/bin/manta`.
- `/usr/share/manta/manta.toml`, this release's config scaffold, byte for
  byte what `manta config init --out -` prints. It changes with each
  release.
- `/etc/manta/manta.toml`, your config. On first install the package copies
  the scaffold there, mode 0600 and owned by root, only when no file exists.
  It is not a dpkg conffile: no upgrade replaces it or asks about it.
- `/var/cache/manta` appears when the service first starts: systemd's
  `CacheDirectory=manta` creates it for the service's temporary user.

The package enables `manta.service` but does not start it. The scaffold's
`N0CALL` callsign and missing receiver stop manta at once, and
`Restart=always` would start it again every 10 seconds until you edit the
config (see "If `N0CALL` is left in place" above). `systemctl is-enabled
manta` prints `enabled` and `systemctl is-active manta` prints `inactive`
until you start it.

The service runs as systemd's transient `manta` user (`DynamicUser=yes`);
the package creates no static user. The rest of "Linux: systemd" below
(config access, reload, absolute paths, no local devices, restarts, stopping,
logs and what the service may do) applies unchanged, with `/usr/bin/manta`
in place of `/usr/local/bin/manta`. Upgrading and removing the package are
under "Upgrading or reinstalling" and "Removing a service". To move an
existing archive install to the package, see "Moving from an archive install
to the package".

## Linux: systemd

Requires systemd 247 or newer (`systemctl --version`). Install the binary
and your edited config, check the installed config, then install and start
the unit:

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
# Stop, including while Restart=always is configured:
sudo systemctl stop manta
```

The `install` commands print nothing on success, and `systemd-analyze
verify` must report no errors for the unit.

- **Config access.** `DynamicUser=yes` runs manta as a temporary user that
  exists only while the service runs. `LoadCredential=manta.toml:/etc/manta/manta.toml`
  has systemd, which runs as root, copy the root-owned mode-0600 config
  into a private credentials directory that only the service can read, and
  the unit runs `manta run --config ${CREDENTIALS_DIRECTORY}/manta.toml`.
  A receiver password in the config is never world-readable. The copy is
  made each time the service starts: after editing
  `/etc/manta/manta.toml`, run the `config check` line again, then
  `sudo systemctl restart manta`.
- **Changing lists without a restart.** `sudo systemctl reload manta`
  sends SIGHUP, which re-reads the `[spot]` lists and each
  `[[rbn_uplink]]` `dry_run` while clients, the `sh/dx` history and
  uplink connections stay up (MAN-78). It re-reads the start-time
  credential copy of the config, so it picks up edits to the
  `blocklist_path` and `notch_path` files, which the config names by
  absolute path; an edit to `/etc/manta/manta.toml` itself still needs a
  restart. `journalctl -u manta` shows `reload: applied` or
  `reload: rejected` with the reason; a rejected reload changes nothing.
- **Absolute paths.** manta resolves a relative path in the config against
  the config's directory, and at run time that is the credentials
  directory, not `/etc/manta`. Give `path` under `[input]`, and
  `blocklist_path` and `notch_path` under `[spot]`, as absolute paths to
  files any user can read, outside `/tmp`: the service gets a private
  `/tmp` of its own.
- **No local devices.** The temporary user belongs to no device group such
  as `audio` or `plugdev`, so this unit cannot open a sound card or USB
  SDR. Use a network receiver. The secondary-skimmer field-node runbook
  (MAN-96) has a separate unit with a static `manta` user for a local
  SDR; it is not part of this kit.
- **Restarts.** `Restart=always` starts manta again 10 seconds
  (`RestartSec=10`) after it exits for any reason, including a config
  error or a receiver it cannot reach at startup, and
  `StartLimitIntervalSec=0` means systemd never gives up. `systemctl stop`
  does not trigger a restart.
- **Stopping.** `systemctl stop` and `systemctl restart` send SIGTERM.
  manta stops decoding, gives connected clients up to 50 seconds to drain,
  and gives its runtime up to 2 more seconds to shut down.
  `TimeoutStopSec=60` leaves a margin before systemd sends SIGKILL.
  A second SIGTERM while manta drains (for example a second
  `systemctl kill manta`) ends it at once, without the rest of the drain,
  with exit status 143.
- **Logs.** stdout and stderr go to the journal under the identifier
  `manta`. Reading them with `journalctl -u manta` needs root or
  membership in the `systemd-journal` or `adm` group. journald keeps them
  within the host's limits: `SystemMaxUse=` for the persistent journal in
  `/var/log/journal` and `RuntimeMaxUse=` for the volatile one in
  `/run/log/journal`, set in `/etc/systemd/journald.conf` or a drop-in
  under `/etc/systemd/journald.conf.d/`. Each defaults to 10% of its file
  system, capped at 4 GiB. The kit installs no journald override.
  `journalctl --disk-usage` shows what the journal uses now. The journal
  gets plain text, with no colour codes. To change the level or format,
  run `sudo systemctl edit manta` and add a drop-in that clears
  `ExecStart=` and sets it again with `-q` or `--log-format json`
  appended, or that sets `Environment=RUST_LOG=warn`; see the `## Logs`
  section of the top-level README.md.

### What the service may do

The unit adds no sandbox directives of its own. `DynamicUser=yes` makes
systemd apply the ones below, for the archive install and the Debian
package alike. The release workflow installs the package on a systemd host,
starts it and reads these back with `systemctl show manta`
(`scripts/check-deb.py --systemd`):

| Property | What it means for manta |
|---|---|
| `DynamicUser=yes` | A transient user named manta, in no device group |
| `User=manta` | The name `systemctl show -p User manta` prints |
| `NoNewPrivileges=yes` | No privilege gain through setuid binaries or file capabilities |
| `RestrictSUIDSGID=yes` | It cannot create setuid or setgid files |
| `ProtectSystem=strict` | The whole file system is read-only, except `/var/cache/manta` |
| `ProtectHome=read-only` | `/home`, `/root` and `/run/user` are read-only |
| `PrivateTmp=yes` | A private `/tmp` and `/var/tmp` (systemd 257 and later report `PrivateTmp=disconnected`) |
| `RemoveIPC=yes` | System V and POSIX IPC objects are removed when it stops |

## macOS: LaunchDaemon

The LaunchDaemon starts manta at boot as the service account `_manta`,
with nobody logged in. It is the only macOS setup this kit supports.
Create the account first: both plists name `_manta`, and launchd cannot
start a job whose account does not exist.

```sh
sudo sh packaging/launchd/create-service-account.sh
sudo install -d -m 0755 /usr/local/bin /usr/local/libexec
sudo install -m 0755 ./manta /usr/local/bin/manta
sudo install -d -o root -g _manta -m 0750 /usr/local/etc/manta
sudo install -o root -g _manta -m 0640 manta.toml /usr/local/etc/manta/manta.toml
sudo install -d -o _manta -g _manta -m 0750 /var/log/manta
sudo -u _manta /usr/local/bin/manta config check --config /usr/local/etc/manta/manta.toml
sudo install -o root -g wheel -m 0755 packaging/launchd/rotate-log.sh /usr/local/libexec/manta-rotate-log.sh
sudo install -o root -g wheel -m 0644 packaging/launchd/com.hagaletechnologies.manta.plist /Library/LaunchDaemons/
sudo install -o root -g wheel -m 0644 packaging/launchd/com.hagaletechnologies.manta-logrotate.plist /Library/LaunchDaemons/
plutil -lint /Library/LaunchDaemons/com.hagaletechnologies.manta.plist
plutil -lint /Library/LaunchDaemons/com.hagaletechnologies.manta-logrotate.plist
sudo launchctl bootstrap system /Library/LaunchDaemons/com.hagaletechnologies.manta-logrotate.plist
sudo launchctl bootstrap system /Library/LaunchDaemons/com.hagaletechnologies.manta.plist
sudo launchctl print system/com.hagaletechnologies.manta
# Stop the daemon before unloading its log rotation job:
sudo launchctl bootout system/com.hagaletechnologies.manta
sudo launchctl bootout system/com.hagaletechnologies.manta-logrotate
```

The account script prints `manta service account ready: _manta`, the
`install` commands print nothing, and `plutil` prints `<path>: OK` for
each file.

- **Service account.** `create-service-account.sh` creates a `_manta` user
  and group with a free ID between 400 and 499, no login shell
  (`/usr/bin/false`), home `/var/empty` and no password. Running it again
  accepts an existing matching account. If `_manta` exists in another
  form, is only partly set up, no ID in the range is free, or a directory
  command fails, it exits non-zero with a message on stderr and deletes or
  overwrites nothing.
- **Config.** `/usr/local/etc/manta/manta.toml` is owned by root, group
  `_manta`, mode 0640: the service can read it and other users cannot. The
  `sudo -u _manta` check proves the service account can read it. Relative
  paths in the config resolve against `/usr/local/etc/manta`, and any file
  it names must be readable by `_manta`. To change the config, edit it with
  `sudo -e /usr/local/etc/manta/manta.toml`, run the check line again, and
  restart manta.
- **Stopping and restarting.** `KeepAlive` makes launchd start manta again
  whenever it exits, at most once every 10 seconds (`ThrottleInterval`).
  Stop it with `launchctl bootout`, not `kill`: after a `kill`, launchd
  just starts a new manta. `bootout` sends SIGTERM and waits up to
  `ExitTimeOut`, 60 seconds, before SIGKILL; manta needs at most 50
  seconds to drain its clients plus 2 seconds of runtime shutdown. To
  restart, boot the daemon out and bootstrap it again. Boot out the daemon
  before its rotation job.
- **Logs.** launchd writes manta's stdout and stderr to
  `/var/log/manta/manta.log`; follow it with
  `sudo tail -f /var/log/manta/manta.log`. The rotation job runs as
  `_manta` once when it is loaded and then once a minute. When the log is
  larger than 10 MiB, it keeps the final 1 MiB in place: it copies that
  tail to a temporary file in the same directory, overwrites the log with
  it, and removes the temporary file. The log keeps its inode because
  manta never reopens its stdout and stderr, and SIGHUP reloads the
  `[spot]` lists (MAN-78) instead of reopening anything. A rename-based rotator such as newsyslog
  would leave manta writing to the renamed file. Limits: the log can grow
  past 10 MiB between runs, or while the job cannot run, and lines manta
  writes during the copy can be lost. This is operational log retention,
  not a strict disk quota or a lossless audit log. The log gets plain
  text, with no colour codes. To change the level or format, add `-q` or
  `--log-format json` to the installed plist's `ProgramArguments`.
- **Checking rotation.** The job discards its own output, so a failure
  shows only in its exit status:
  `sudo launchctl print system/com.hagaletechnologies.manta-logrotate`
  reports the last exit code, which is 0 after a run that rotated the log
  or had nothing to do.

## Docker Compose

`docker-compose.yml` runs manta in a container with your `manta.toml`
mounted read-only.

**Image.** The file names `ghcr.io/hagaletechnologies/manta:latest`. No
release has been published, so that image does not exist yet. Until it
does, build the image in a source checkout, which has the `Dockerfile`
that release archives lack:

```sh
docker build -t manta:local .
```

Then change the `image:` line in `docker-compose.yml` to
`image: manta:local`.

**Config.** Create and edit `manta.toml` next to `docker-compose.yml`
first, as in [Configure first](#configure-first). The container runs
manta as the non-root user `manta`, which the image's Dockerfile creates
with `useradd --system`; its UID and GID are not fixed. The mount keeps
the host file's owner and mode, so that user must be able to read the
file. For a public receiver with no password, make it world-readable:

```sh
chmod 0644 manta.toml
```

Before you store a receiver password in it, make it readable by the
container user's group only. This asks the image for that group's ID:

```sh
gid=$(docker compose run --rm -T --entrypoint id manta -g)
sudo chgrp "$gid" manta.toml
chmod 0640 manta.toml
```

**Run it.**

```sh
docker compose config
docker compose up -d
docker compose logs -f
# Stop and remove the container:
docker compose down
```

`docker compose config` prints the resolved file, or an error if the file
is invalid.

- **The mount.** `./manta.toml` is bound read-only at
  `/etc/manta/manta.toml`, and the service runs
  `manta run --config /etc/manta/manta.toml`. With
  `create_host_path: false`, a missing `./manta.toml` stops the container
  from starting instead of Docker creating an empty directory in its
  place. Besides it, only the `manta-lists` cache volume is mounted (see
  [Keeping cty.dat and MASTER.SCP current](#keeping-ctydat-and-masterscp-current)):
  a blocklist, notch list or WAV file the config names needs a mount of
  its own, at the path the config gives.
- **SELinux.** `selinux: Z` matters only on a host that enforces SELinux,
  such as Fedora or RHEL. There Docker relabels `./manta.toml` on the host
  with a private label for this container, so this container can read it
  and other containers cannot. The label stays on the host file. Keep the
  file dedicated to this container, and never point a `Z` mount at a
  shared file or directory.
- **Ports.** Telnet (7300) and JSON (7301) are published on every host
  interface, as a public cluster node expects. Inside the container manta
  binds metrics to `0.0.0.0` (`MANTA_SERVER_METRICS_BIND_ADDR`, which
  overrides `metrics_bind_addr` in `manta.toml`), because a published port
  reaches the container's network interface, not its loopback. The host
  publishes metrics only on `127.0.0.1:7302`. If you change `telnet_port`,
  `json_port` or `metrics_port` in `manta.toml`, change the container side
  (the right-hand number) of the matching `ports:` entry too. Keep
  `bind_addr` at its default inside the container: `127.0.0.1` there makes
  telnet and JSON unreachable through the published ports.
- **Stopping and restarts.** `docker compose down` and
  `docker compose stop` send SIGTERM, Docker's default stop signal, and
  wait up to `stop_grace_period: 60s` before SIGKILL, longer than manta's
  50-second client drain plus 2 seconds of runtime shutdown.
  `restart: unless-stopped` starts manta again after it exits and when
  Docker starts, unless you stopped it.
- **Logs.** The `json-file` driver rotates the container's log at 10 MB
  and keeps 3 files (`max-size: "10m"`, `max-file: "3"`), about 30 MB in
  all. `RUST_LOG=info` sets the log level. For JSON log lines, append
  `"--log-format", "json"` to `command:`.
- **Health.** The image carries a `HEALTHCHECK` that runs
  `manta health --config "${MANTA_CONFIG:-/etc/manta/manta.toml}"` inside
  the container every 10 seconds. `manta health` reads the same config the
  daemon runs, with the same `MANTA_SERVER_*` overrides, asks the daemon's
  own `/healthz`, and exits 0 or 1. It re-reads `manta.toml` at every check,
  and `metrics_port` and `metrics_bind_addr` take effect only after a
  restart, so restart the container after editing them; until then the
  check dials the new address, where the daemon may not be listening. An
  edit after which `manta.toml` no longer loads, or has no `[server]`
  table, also fails the check until it is fixed. `docker ps` shows
  `(health: starting)` for up to 60 seconds after start (long enough for a
  KiwiSDR to connect), then `(healthy)`, or `(unhealthy)` after three
  failed checks in a row.
  `docker inspect --format '{{json .State.Health.Log}}' <container>` shows
  the daemon's own reasons, such as `decode: stalled 12s`. The check runs
  inside the container, so `metrics_bind_addr` can stay `127.0.0.1`; it
  costs 6 of the metrics listener's 60 requests a minute per address. The
  RBN uplink is not part of the verdict: `manta status` reports it. A
  container started for something other than the daemon (`decode`, or
  `listen` without a config) has nothing to probe and shows `(unhealthy)`.
  Turn the check off for those with `docker run --no-healthcheck`, or with
  `healthcheck: disable: true` under the service in Compose.
- **USB receivers.** The image includes SoapySDR and Debian's modules for
  RTL-SDR, Airspy R2 and Mini, and HackRF. Compose passes no device by
  default. To use a USB receiver, uncomment the `devices:` and
  `group_add:` lines in `docker-compose.yml`, or add
  `--device /dev/bus/usb --group-add plugdev` to `docker run`. The
  container's `manta` user can open the receiver only if a udev rule on
  the host gives the `plugdev` group access to it. Debian's `librtlsdr0`
  and `libhackrf0` packages install such a rule; Airspy's package does
  not, so add one for it. Docker resolves `plugdev` inside the image (GID
  46, Debian's); if the host's `plugdev` GID differs, pass the host's
  number instead. Replug the receiver after adding a rule. A receiver
  plugged in after the container started is not visible until it
  restarts. This setup has not yet been tested with a receiver.
  Privileged mode and host networking stay off.

### Without Compose

With no arguments, the image runs the daemon from a config mounted at
`/etc/manta/manta.toml`:

```sh
docker run -d --name manta --restart unless-stopped --stop-timeout 60 --mount type=bind,src="$PWD/manta.toml",dst=/etc/manta/manta.toml,readonly -p 7300:7300 -p 7301:7301 ghcr.io/hagaletechnologies/manta:latest
docker logs -f manta
docker ps    # STATUS shows (healthy) once the daemon answers
# A USB receiver: list what SoapySDR finds through the passthrough
docker run --rm --device /dev/bus/usb --group-add plugdev ghcr.io/hagaletechnologies/manta:latest devices
```

- Use `--mount`, not `-v`. When `manta.toml` is missing on the host,
  `--mount` refuses to start the container. `-v` creates an empty
  directory in its place instead, which manta then reports as
  `Is a directory (os error 21)` and exits.
- `--stop-timeout 60` gives manta's 50-second client drain time to finish
  before Docker's SIGKILL, as `stop_grace_period` does in Compose.
- To mount the config somewhere else, also set `MANTA_CONFIG` to that
  path (`-e MANTA_CONFIG=/config/manta.toml`): the daemon and the health
  check both read it. Passing `run --config <path>` as arguments works for
  the daemon, but the health check still reads `/etc/manta/manta.toml`.
- With no config mounted and no `MANTA_CONFIG`, the container prints
  `manta --help` and exits 0. Any arguments run `manta <arguments>`
  unchanged, for example
  `docker run --rm ghcr.io/hagaletechnologies/manta:latest --version`.
- The image is built for `linux/amd64`, `linux/arm64` and `linux/arm/v7`
  (32-bit Raspberry Pi OS). The `linux/arm/v7` image's decode CPU budget
  is unmeasured.

## Windows

The Windows archive has `manta.exe` and the same reference files. This kit
has no Windows service wrapper. To check the config and run manta in the
foreground, use `.\manta.exe config check --config manta.toml` and then
`.\manta.exe run --config manta.toml` in a PowerShell window; Ctrl+C stops
it.

## Keeping cty.dat and MASTER.SCP current

manta ships `cty.dat` (country prefixes: a call whose prefix it lacks is
not spotted) and `MASTER.SCP` (known calls: a listed call is spotted with
more confidence) built in, so a new prefix is rejected until the next
release. To have `manta run` download current copies while it reads a live
receiver, set this in `manta.toml`:

```toml
[spot]
refresh_lists = true
```

and restart the service. `cty.dat` is then checked every 7 days and
`MASTER.SCP` every day. A download replaces the list in use, without a
restart, only after it parses and looks complete. A failed or rejected one
is logged as a `warning: list refresh:` line, counted in
`manta_list_refresh_total{outcome="failed"}` and retried an hour later; the
copy already in use stays. Without network access manta runs on the
built-in copies. File replay never downloads. Refresh is off by default.

The callsign-pattern list (which calls need 2, 3 or 5 repetitions) is
downloaded too only if you also name a source for it:

```toml
[spot]
refresh_lists = true
pattern_url = "http://data.reversebeacon.net/downloads/patt3ch/patt3ch.lst"
```

It is then checked every 7 days under the same rules. manta accepts its
own `[+]PATTERN` files and RBN's `Patt3Ch.lst`, and skips the lines no
call it accepts can match. `pattern_url` has no default: the URL above is
an example, not a recommendation. RBN's list is a different policy from
the built-in one, not just a newer copy. It lists about 10,190 patterns
against the built-in 6,036, but leaves about 3% of MASTER.SCP's calls
unlisted (5 repetitions) where the built-in list leaves 0.34%. See
`docs/DECISIONS/2026-11-09-man542-pattern-list-refresh.md` in the source
repository.

- **Firewall.** Downloads are HTTPS: allow outbound TCP port 443 to
  `www.country-files.com` and `www.supercheckpartial.com`. Also allow
  the `pattern_url` host and port if set (TCP port 80 for the RBN example
  above). manta connects
  directly and does not use `HTTPS_PROXY`. To keep the node off the
  internet, or to share one download between nodes, serve the two files
  from a local web server and set `cty_url` and `scp_url` in `[spot]` to
  its URLs: `http://`, or `https://` with a certificate from a public CA,
  because manta trusts only the Mozilla roots built into it.
- **Where downloads are kept.** Each accepted version is stored under
  `[spot].list_cache_dir`, so a restart starts from the last good
  download. The systemd unit sets it with
  `Environment=MANTA_SPOT_LIST_CACHE_DIR=/var/cache/manta` and creates the
  directory with `CacheDirectory=manta`; because of `DynamicUser=yes` its
  files live in `/var/cache/private/manta`. Compose mounts the
  `manta-lists` named volume at `/var/cache/manta` and sets the same
  variable. LaunchDaemon users set `list_cache_dir` in their config; with
  it unset, downloads are kept in memory only.
- **Watching it.** At start, `run` logs a `lists:` line naming the SHA-256
  of each list in use and where it came from, and `/metrics` carries
  `manta_list_info`, `manta_list_refresh_total` and
  `manta_list_last_success_timestamp_seconds` (`docs/METRICS.md` in the
  source repository). Alert when
  `time() - manta_list_last_success_timestamp_seconds{list="cty"}` passes
  8 days, and likewise for `{list="patterns"}` when `pattern_url` is set.
- **Replaying a recording with the list the daemon used.** Copy the hash
  from the `lists:` line and pass it to `decode`, `run`, `soak` or
  `doctor` as `--cty-sha256` or `--scp-sha256`, with a config whose
  `list_cache_dir` holds that version (or `--cty`/`--scp` naming the
  file). manta stops before reading the recording if no copy has that
  hash.
- **Choosing a list yourself.** A list that `cty_path`/`scp_path`/
  `pattern_path` or `cty_sha256`/`scp_sha256` sets is never downloaded.
  `pattern_path` takes manta's `[+]PATTERN` format only.

Every `[spot]` key is described in `manta.example.toml` and in
[SPEC-decode-core.md](../docs/SPEC-decode-core.md) §9; the design is in
`docs/DECISIONS/2026-10-12-man316-reference-list-refresh.md` and
`docs/DECISIONS/2026-11-09-man542-pattern-list-refresh.md` in the source
repository.

## Network exposure

By default telnet (7300) and JSON (7301) accept connections on every
interface (`bind_addr = "0.0.0.0"`), as a public cluster node does. The
metrics endpoint (7302, `/metrics` and `/healthz`) has no password and
accepts connections only from the same machine
(`metrics_bind_addr = "127.0.0.1"`). With any of the three setups,
`curl -s http://127.0.0.1:7302/healthz` on that machine reports the node's
health. Read [network-exposure.md](../docs/RUNBOOKS/network-exposure.md)
before you change either address.

For a Grafana dashboard and Prometheus alerts, see
[monitoring](https://github.com/HagaleTechnologies/manta/blob/main/docs/RUNBOOKS/monitoring.md).

## Moving from an archive install to the package

A host that runs the archive install has `/etc/systemd/system/manta.service`,
which overrides the packaged unit and runs `/usr/local/bin/manta`, and
`/usr/local/bin/manta`, which comes before `/usr/bin/manta` on `PATH`.
Remove both, then install the package:

```sh
sudo systemctl disable --now manta
sudo rm /etc/systemd/system/manta.service /usr/local/bin/manta
sudo systemctl daemon-reload
sudo apt install ./manta-linux-arm64.deb
sudo manta config check --config /etc/manta/manta.toml
sudo systemctl start manta
```

The package keeps your existing `/etc/manta/manta.toml`, which from then on
is the package's config: `apt purge manta` deletes it. If you skip the `rm`
line, the install names what still shadows the package:

```text
manta: /etc/systemd/system/manta.service overrides the packaged unit and runs /usr/local/bin/manta.
manta: remove it to run this package's binary: https://github.com/HagaleTechnologies/manta/blob/main/packaging/README.md#moving-from-an-archive-install-to-the-package
manta: /usr/local/bin/manta comes before /usr/bin/manta on PATH; remove it: https://github.com/HagaleTechnologies/manta/blob/main/packaging/README.md#moving-from-an-archive-install-to-the-package
```

## Upgrading or reinstalling

Stop the service, replace the binary and any kit files that changed, keep
your edited config, check it, and start again. Do not repeat the step that
installs `manta.toml`: it would replace your config. Compare the new
`manta.example.toml` with your config for settings a release adds;
settings you left commented out follow the new release's defaults.

systemd:

```sh
sudo systemctl stop manta
sudo install -m 0755 ./manta /usr/local/bin/manta
sudo install -m 0644 packaging/systemd/manta.service /etc/systemd/system/manta.service
sudo systemctl daemon-reload
sudo /usr/local/bin/manta config check --config /etc/manta/manta.toml
sudo systemctl start manta
```

Debian package: download the new release's `.deb` and install it over the
old one. It never prompts, keeps `/etc/manta/manta.toml` byte for byte, and
restarts the service if it was running (`deb-systemd-invoke try-restart`);
a stopped service stays stopped. The `diff` line shows the settings the new
release documents beside your edits:

```sh
curl -fLO https://github.com/HagaleTechnologies/manta/releases/latest/download/manta-linux-arm64.deb
sudo apt install ./manta-linux-arm64.deb
manta config init --out - | sudo diff - /etc/manta/manta.toml
```

macOS (the account script only confirms the existing account here):

```sh
sudo launchctl bootout system/com.hagaletechnologies.manta
sudo launchctl bootout system/com.hagaletechnologies.manta-logrotate
sudo sh packaging/launchd/create-service-account.sh
sudo install -m 0755 ./manta /usr/local/bin/manta
sudo -u _manta /usr/local/bin/manta config check --config /usr/local/etc/manta/manta.toml
sudo install -o root -g wheel -m 0755 packaging/launchd/rotate-log.sh /usr/local/libexec/manta-rotate-log.sh
sudo install -o root -g wheel -m 0644 packaging/launchd/com.hagaletechnologies.manta.plist /Library/LaunchDaemons/
sudo install -o root -g wheel -m 0644 packaging/launchd/com.hagaletechnologies.manta-logrotate.plist /Library/LaunchDaemons/
sudo launchctl bootstrap system /Library/LaunchDaemons/com.hagaletechnologies.manta-logrotate.plist
sudo launchctl bootstrap system /Library/LaunchDaemons/com.hagaletechnologies.manta.plist
```

Docker Compose: run `docker compose down`, rebuild `manta:local` from the
updated checkout (or, once a released image exists, run
`docker compose pull`), then `docker compose up -d`.

## Removing a service

Removing a service installed from an archive does not delete its config,
its binary or, on macOS, its account and log. Delete them yourself if you no longer need them.

systemd:

```sh
sudo systemctl disable --now manta
sudo rm /etc/systemd/system/manta.service
sudo systemctl daemon-reload
```

This leaves `/etc/manta/manta.toml`, `/usr/local/bin/manta` and any
downloaded lists in `/var/cache/private/manta`. The unit's temporary user
goes away with the service.

Debian package:

```sh
sudo apt remove manta
sudo apt purge manta
```

`apt remove` stops the service and removes the binary and the unit, and
keeps `/etc/manta/manta.toml`. `apt purge` also deletes
`/etc/manta/manta.toml` and the service's enable state. Both leave any
downloaded lists in `/var/cache/private/manta`, as the archive removal does.

macOS:

```sh
sudo launchctl bootout system/com.hagaletechnologies.manta
sudo launchctl bootout system/com.hagaletechnologies.manta-logrotate
sudo rm /Library/LaunchDaemons/com.hagaletechnologies.manta.plist /Library/LaunchDaemons/com.hagaletechnologies.manta-logrotate.plist
```

This leaves `/usr/local/bin/manta`, `/usr/local/libexec/manta-rotate-log.sh`,
`/usr/local/etc/manta`, `/var/log/manta` and the `_manta` account;
`sudo dscl . -delete /Users/_manta` and `sudo dscl . -delete /Groups/_manta`
remove the account.

Docker Compose: `docker compose down` removes the container and leaves
`manta.toml`, the image and the `manta-lists` volume; `docker compose down
--volumes` removes the volume too.
