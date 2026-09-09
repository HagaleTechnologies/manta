# Running manta unattended (MAN-75)

This directory, plus `manta.example.toml` and `docker-compose.yml` at the
repo root, are the files that let an operator who downloaded a release
binary run manta as a 24/7 background service without hand-writing any of
this. **Every release archive ships them**, at exactly these relative paths
(`.github/workflows/release-publish.yml`), so the install commands below
run verbatim from an extracted `manta-<platform>.tar.gz`/`.zip` — no clone
required. CI checks all four against the code they describe
(`crates/manta-cli/tests/packaging_examples.rs`): a config key manta adds,
a flag that gets renamed, or a stop-signal regression fails the build here,
not silently in a stale example an operator copies later.

| File | Platform |
| --- | --- |
| [`manta.example.toml`](../manta.example.toml) | all — every `[server]`/`[[rbn_uplink]]` config key at its real default |
| [`systemd/manta.service`](systemd/manta.service) | Linux |
| [`launchd/com.hagaletechnologies.manta.plist`](launchd/com.hagaletechnologies.manta.plist) | macOS |
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
| launchd | `SIGTERM`, always | **No fix exists.** launchd has no `KillSignal=`-equivalent key; stop, logout, and reboot all send `SIGTERM` unconditionally. `launchctl bootout`/stop will kill manta abruptly until manta itself handles `SIGTERM`. `kill -INT <pid>` (found via `launchctl print gui/$(id -u)/com.hagaletechnologies.manta`) drains cleanly, but the shipped plist's `KeepAlive` is unconditional, so launchd relaunches manta the moment that drained process exits — see *Installing on macOS* below for the two-step sequence that actually stops it. |

Once manta handles `SIGTERM` itself, `KillSignal=SIGINT` can be dropped
from the systemd unit and this whole section shrinks to "manta drains
within 27 s of any stop signal, so a 30 s grace period is enough" — both
the unit and this file mark that removal point explicitly.

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
mkdir -p ~/Library/Application\ Support/manta ~/Library/LaunchAgents
cp manta.example.toml ~/Library/Application\ Support/manta/manta.toml
$EDITOR ~/Library/Application\ Support/manta/manta.toml   # set station_callsign
cp packaging/launchd/com.hagaletechnologies.manta.plist ~/Library/LaunchAgents/
$EDITOR ~/Library/LaunchAgents/com.hagaletechnologies.manta.plist
# replace YOUR_USERNAME (three places) and YOUR_KIWI_HOST, and set --kiwi-freq
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.hagaletechnologies.manta.plist
launchctl print gui/$(id -u)/com.hagaletechnologies.manta
```

This is a **LaunchAgent**, not a LaunchDaemon: macOS gates audio-input
access behind a per-user TCC permission prompt that a system-wide
LaunchDaemon can never be granted, so a LaunchAgent is the right default
even for the shown KiwiSDR example, in case you later switch it to rig
audio.

Stopping it cleanly is two steps, in that order — the shipped plist's
`KeepAlive` is unconditional, so a bare `kill -INT` drains manta and then
launchd immediately relaunches it:

```
$ kill -INT $(launchctl print gui/$(id -u)/com.hagaletechnologies.manta | awk '/pid = /{print $3}')
# wait for it to exit (up to ExitTimeOut, 30s) -- this is the clean drain
$ launchctl bootout gui/$(id -u)/com.hagaletechnologies.manta
# unloads the job so KeepAlive can't respawn it
```

Running `launchctl bootout` by itself (skipping the `kill -INT` step) is
abrupt — see the `SIGTERM` caveat above.

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
