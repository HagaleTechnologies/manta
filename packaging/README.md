# Running manta unattended (MAN-75)

This directory, plus `manta.example.toml` and `docker-compose.yml` at the
repo root, are the files that let an operator who downloaded a release
binary run manta as a 24/7 background service without hand-writing any of
this. **Every release archive ships them**, at exactly these relative paths
(`.github/workflows/release-publish.yml`) — together with
`docs/RUNBOOKS/network-exposure.md`, which the example config and the
Compose file both point at by that relative path — so the install commands
below run verbatim from an extracted `manta-<platform>.tar.gz`/`.zip` — no
clone required. CI checks these files against the code they describe
(`crates/manta-cli/tests/packaging_examples.rs`): a config key manta adds,
a flag that gets renamed, a stop-signal regression, or a shipped file that
stops being packaged fails the build here, not silently in a stale example
an operator copies later.

| File | Platform |
| --- | --- |
| [`manta.example.toml`](../manta.example.toml) | all — every `[server]`/`[[rbn_uplink]]` config key at its real default |
| [`systemd/manta.service`](systemd/manta.service) | Linux |
| [`launchd/com.hagaletechnologies.manta.plist`](launchd/com.hagaletechnologies.manta.plist) | macOS |
| [`launchd/com.hagaletechnologies.manta-logrotate.plist`](launchd/com.hagaletechnologies.manta-logrotate.plist) | macOS — caps the LaunchAgent's log file, which launchd itself never rotates |
| [`docker-compose.yml`](../docker-compose.yml) | anywhere Docker runs |

## Choosing a source

None of these files can select an input from TOML yet — source selection
(`--device`, `--source`, `--kiwi-*`, `--hpsdr-*`, `--soapy-*`) and
`--dial-freq-hz` are CLI-flag-only today (tracked as MAN-74). Every shipped
example's command line therefore carries explicit source flags alongside
`--server-config`. All three examples default to KiwiSDR:

| Source | Flags | Caveat |
| --- | --- | --- |
| **KiwiSDR** (shown default) | `--kiwi-host <host> --kiwi-freq <hz>` | Plain outbound TCP. No local device permissions, no feature flag — present in every manta build, including a plain `cargo build`. |
| Rig audio (sound card) | `--device "<name>" --dial-freq-hz <hz>` | Needs `SupplementaryGroups=audio` in the systemd unit (a `DynamicUser=` UID is in no static group) and `/dev/snd` passthrough in Compose — not a good fit for a container. |
| HPSDR / Hermes-Lite 2 | `--hpsdr-host <host> --hpsdr-freq <hz> --hpsdr-rate <hz>` | Requires a `--features hpsdr` build. Release binaries and the Docker image already have it; a from-source build needs the flag added. High-rate UDP — awkward through a Docker bridge NAT, consider `network_mode: host` on Linux. |
| SoapySDR (RTL-SDR etc.) | `--soapy-driver <args> --soapy-freq <hz> --soapy-rate <hz>` | Requires a `--features soapy` build; not in official release binaries. Add the device's own udev group to `SupplementaryGroups=`. |
| WAV replay | `--source <path> --dial-freq-hz <hz>` | Testing only — unpaced, so it finishes as fast as it can decode rather than in real time. Must be 48 kHz mono/stereo; `manta gen`'s own IQ output (96 kHz) is rejected. |

## Stopping manta cleanly

manta registers a shutdown handler for `SIGINT` only (`ctrlc` is built
without its optional `termination` feature) — `SIGTERM` is not handled at
all, and the kernel's default disposition kills the process outright with
no drain and no log line. Measured directly against this repo's own tree:

```
$ kill -INT  $PID; wait $PID; echo exit=$?
exit=0            # clean shutdown: servers drain, tasks finish
$ kill -TERM $PID; wait $PID; echo exit=$?
exit=143          # 128+15: killed by the OS default action, no app code ran
```

manta's own drain budget is 25 s for in-flight client writes
(`SHUTDOWN_DRAIN_DEADLINE`) plus a 2 s hard runtime cutoff — 27 s worst
case. Every stop-budget setting below (`TimeoutStopSec=30`,
`stop_grace_period: 30s`, `ExitTimeOut 30`) clears that with margin before
the process manager escalates to `SIGKILL`.

| Platform | Sends on stop | Fix |
| --- | --- | --- |
| systemd | `SIGTERM` by default | `KillSignal=SIGINT` in the shipped unit retargets it, mirroring the Dockerfile's own `STOPSIGNAL SIGINT`. |
| Docker / Compose | the image's `STOPSIGNAL` | Already `SIGINT` (set in `Dockerfile` under MAN-21) — no compose-level override needed or wanted. |
| launchd | `SIGTERM`, always | **No signal-level fix exists.** launchd has no `KillSignal=`-equivalent key; stop, logout, and reboot all send `SIGTERM` unconditionally, so `launchctl bootout`/`stop` on a *running* manta is abrupt until manta itself handles `SIGTERM`. The shipped plist works around it from the other side: `KeepAlive` is the conditional `SuccessfulExit=false` form, so signalling `SIGINT` yourself drains manta *and leaves it down* instead of triggering an instant relaunch — see *Installing on macOS* below for the sequence, which has to wait for the drain to finish before it unloads the job, since `launchctl kill` returns as soon as the signal is delivered. |

Once manta handles `SIGTERM` itself, `KillSignal=SIGINT` can be dropped
from the systemd unit, the plist's `KeepAlive` can go back to a plain
`<true/>`, and this whole section shrinks to "manta drains within 27 s of
any stop signal, so a 30 s grace period is enough" — the unit, the plist
and this file each mark that removal point explicitly.

## Validate your config before you install the unit

manta opens its input source *before* it parses `--server-config`, so a
config typo is not reported while the source is unreachable — under
`Restart=always` a broken config then looks exactly like a network outage
instead of a startup error. There is no offline preflight yet (a `manta
config check` verb is tracked separately, not part of this ticket).
Before installing any of these units, run the same command line by hand
against a local WAV file first, so a config mistake surfaces immediately
instead of retrying forever:

```
$ manta listen --server-config /etc/manta/manta.toml --source some.wav --dial-freq-hz 14000000
Error: TOML parse error at line 2, column 20
  |
2 | station_callsign = "!!!"
  |                    ^^^^^
station_callsign "!!!" is not a plausible callsign
```

`journalctl -u manta` (systemd), `docker compose logs` (Compose), or
`~/Library/Logs/manta.log` (launchd, per the shipped plist's
`StandardErrorPath`) is where the same error appears once the unit itself
is running against a live, and therefore restart-looping, source.

## Installing on Linux (systemd)

```
sudo install -m 0755 ./manta /usr/local/bin/manta
sudo install -d -m 0755 /etc/manta
sudo install -m 0644 manta.example.toml /etc/manta/manta.toml
sudo "$EDITOR" /etc/manta/manta.toml          # set station_callsign
sudo install -m 0644 packaging/systemd/manta.service /etc/systemd/system/manta.service
sudo "$EDITOR" /etc/systemd/system/manta.service   # set ExecStart's source flags
sudo systemctl daemon-reload
sudo systemctl enable --now manta
journalctl -u manta -f
```

`/etc/manta/manta.toml` is installed mode `0644` (world-readable)
deliberately: under `DynamicUser=yes` the service runs as a fresh,
transient UID on every start, so the config has to be readable by "anyone"
for the service to read it at all. Do not put a secret in it — a
`--kiwi-password` belongs on `ExecStart`'s command line instead (still
visible to `systemctl cat`/`ps`; use a password-free receiver, or an
`EnvironmentFile=` with mode `0600`, if that matters to you).

## Installing on macOS (launchd)

```
# The plist runs an absolute path and launchd searches no PATH, so the
# binary has to be installed where ProgramArguments points, exactly as on
# Linux. Release binaries are not code-signed or notarized, so clear the
# download quarantine flag first or Gatekeeper blocks every spawn.
xattr -d com.apple.quarantine ./manta 2>/dev/null || true
sudo install -d -m 0755 /usr/local/bin
sudo install -m 0755 ./manta /usr/local/bin/manta
mkdir -p ~/Library/Application\ Support/manta ~/Library/LaunchAgents ~/Library/Logs
cp manta.example.toml ~/Library/Application\ Support/manta/manta.toml
$EDITOR ~/Library/Application\ Support/manta/manta.toml   # set station_callsign
cp packaging/launchd/com.hagaletechnologies.manta.plist ~/Library/LaunchAgents/
$EDITOR ~/Library/LaunchAgents/com.hagaletechnologies.manta.plist
# replace YOUR_USERNAME (three places) and YOUR_KIWI_HOST, and set --kiwi-freq
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.hagaletechnologies.manta.plist
launchctl print gui/$(id -u)/com.hagaletechnologies.manta
```

### LaunchAgent or LaunchDaemon: pick one deliberately

This ships as a **LaunchAgent**, in the per-user `gui/` domain, because
macOS gates audio-input access behind a per-user TCC permission prompt that
a system-wide LaunchDaemon can never be granted — so rig audio (`--device`)
only ever works from an agent.

The cost is that a `gui/` agent is a *login-session* job, not a boot-time
one: [Apple's launchd
guide](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html)
distinguishes the two explicitly. As installed above, manta therefore does
**not** start when the Mac boots — it starts when that user logs in to the
GUI, and launchd tears it down again at logout. For a 24/7 skimmer that is
usually not what you want, so choose:

- **Rig audio (`--device`), or you are fine with the login requirement** —
  keep the LaunchAgent, and make sure the machine actually stays logged in:
  enable automatic login (System Settings → Users & Groups → Automatic
  login) and do not log out. A locked screen is fine; a logged-out Mac runs
  no agent.
- **KiwiSDR, HPSDR or SoapySDR — no audio device, so no TCC prompt to
  answer** — install the same plist as a system **LaunchDaemon** instead. It
  starts at boot, survives logout, and needs no logged-in user:

  ```
  # Same plist, three edits: point StandardOutPath/StandardErrorPath and
  # --server-config somewhere outside a user's home (a daemon has no
  # ~/Library), and add a UserName so it does not run as root.
  sudo install -d -m 0755 /usr/local/etc/manta /usr/local/var/log
  sudo install -m 0644 manta.example.toml /usr/local/etc/manta/manta.toml
  sudo "$EDITOR" /usr/local/etc/manta/manta.toml   # set station_callsign
  sudo cp packaging/launchd/com.hagaletechnologies.manta.plist       /Library/LaunchDaemons/
  sudo "$EDITOR" /Library/LaunchDaemons/com.hagaletechnologies.manta.plist
  # in that copy: replace the three /Users/YOUR_USERNAME paths with
  # /usr/local/etc/manta/manta.toml and /usr/local/var/log/manta.log, and
  # add   <key>UserName</key><string>_manta</string>   (or your own
  # unprivileged account) so the daemon does not run as root
  sudo chown root:wheel /Library/LaunchDaemons/com.hagaletechnologies.manta.plist
  sudo launchctl bootstrap system       /Library/LaunchDaemons/com.hagaletechnologies.manta.plist
  ```

  Everything else in this section applies unchanged, with `system/` in place
  of `gui/$(id -u)` in every `launchctl` invocation — including the stop
  sequence below and the log-rotation caveat, whose agent then has to be a
  root-owned LaunchDaemon pointed at the daemon's own log path.

### Log rotation is not automatic

launchd appends both streams to the single file `StandardOutPath` names and
never rotates or caps it, and manta's console output is one line per
accepted spot plus one character per decoded character
(`crates/manta-cli/src/main.rs`), so an active receiver grows
`~/Library/Logs/manta.log` indefinitely — eventually filling the disk. This
is a launchd-only problem: journald caps the systemd unit's output itself,
and a container's logs go to Docker's own log driver.

The shipped
[`launchd/com.hagaletechnologies.manta-logrotate.plist`](launchd/com.hagaletechnologies.manta-logrotate.plist)
is the rotation policy. Install it alongside the service agent:

```
$EDITOR packaging/launchd/com.hagaletechnologies.manta-logrotate.plist
# replace YOUR_USERNAME (two places)
cp packaging/launchd/com.hagaletechnologies.manta-logrotate.plist ~/Library/LaunchAgents/
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.hagaletechnologies.manta-logrotate.plist
```

It runs hourly, does nothing while the log is under 32 MiB, and otherwise
truncates it in place down to its last 8 MiB — in place, because manta
holds launchd's descriptor for that file open for its whole lifetime, so
rotating by rename would leave the renamed file growing and the new one
empty. Edit `cap`/`keep` in the plist's script to change either size.
Alternatively, drop it and redirect `StandardOutPath`/`StandardErrorPath`
to `/dev/null` if you do not want the log at all — but then the startup
errors described under *Validate your config* above have nowhere to appear.

Stopping it cleanly is three steps, in that order — signal, wait, unload
(`system/...` in place of `gui/$(id -u)/...` if you installed it as a
LaunchDaemon above):

```
$ svc=gui/$(id -u)/com.hagaletechnologies.manta
$ launchctl kill SIGINT "$svc"
# manta drains and exits 0. KeepAlive is the conditional
# SuccessfulExit=false form, so launchd treats that clean exit as
# intentional and does NOT relaunch -- the job stays loaded but idle.
$ while launchctl print "$svc" 2>/dev/null | grep -qE '^[[:space:]]*pid = [0-9]+'
  do sleep 1; done
# WAIT for it. `launchctl kill` only DELIVERS the signal -- launchctl(1) says
# it "[s]ends the specified signal to the service instance" and nothing more,
# so it returns while manta is still draining its in-flight clients (up to
# ExitTimeOut, 30 s). Booting out at that moment hits the still-running
# process with launchd's unhandled SIGTERM and throws away exactly the drain
# this sequence exists to preserve. A loaded-but-idle job prints no `pid =`
# line, which is the signal that the drain has finished.
$ launchctl bootout "$svc"
# unloads the job for good. Nothing is running by now, so this sends no
# signal to anything.
```

That ordering — signal, *wait*, then unload — is what makes the stop clean,
and it only works because the plist's `KeepAlive` is conditional. Under the
unconditional `<true/>` this file previously shipped, the drained process
was relaunched the instant it exited and the `bootout` then killed the
*replacement* with launchd's unhandled `SIGTERM` — the same abrupt kill the
sequence was there to avoid. Skipping the wait reaches the same bad end by
the other route: `bootout` arriving mid-drain `SIGTERM`s the process that
is still draining.

Running `launchctl bootout` (or `launchctl stop`) by itself, while manta
is still running, skips the drain and is abrupt — see the `SIGTERM` caveat
above. To bring manta back up after stopping it: `launchctl kickstart
gui/$(id -u)/com.hagaletechnologies.manta` if you only signalled it and
the job is still loaded, or the `launchctl bootstrap` line above if you
went on to boot it out.

The cost of that `KeepAlive` choice, versus the systemd unit's
`Restart=always`: launchd relaunches manta after a crash, a non-zero exit
or a kill by signal, but *not* after any exit 0 — including one manta
takes for its own reasons, such as a `--source` file ending. A 24/7
KiwiSDR or rig-audio source does not exit 0 on its own, so this only
changes behaviour for a deliberate stop or a finite input.

## Installing with Docker Compose

```
cp manta.example.toml manta.toml
$EDITOR manta.toml          # set station_callsign
$EDITOR docker-compose.yml  # set the source flags in `command:`
docker compose up -d
docker compose logs -f
docker compose down         # honours stop_grace_period: 30s
```

`image: ghcr.io/hagaletechnologies/manta:latest` only resolves once a real
tagged release exists and the GHCR package's one-time "make public" step
has run (tracked separately as MAN-65); until then, comment out `image:`
and uncomment `build: .` in `docker-compose.yml` to build the same image
locally from this checkout.
