# manta configuration reference

One TOML file configures `manta run`, `soak` and `doctor`. `manta config init`
writes a scaffold that lists every key, commented out at its default, and
`manta config check --config FILE` validates a file without opening a receiver
or binding a port. [`manta.example.toml`](../manta.example.toml) is that
scaffold with `[server]` and `station_callsign` already uncommented, ready to
copy to `manta.toml`. The design is recorded in
[DECISIONS/2026-10-06-man261-config-surface.md](DECISIONS/2026-10-06-man261-config-surface.md).

This page gives each key's default, which commands act on it, and the flag that
overrides it. For what a decode-core key means, follow the link in its Notes
cell to [SPEC §9](SPEC-decode-core.md#9-configuration-keys) or
[SPEC-v2 §7](SPEC-decode-core-v2.md#7-configuration-keys-v1-9-additive); this
page does not repeat them.

## Precedence

A value comes from the first of these that sets it:

1. a command-line flag;
2. a `MANTA_<TABLE>_<KEY>` environment variable, such as
   `MANTA_INPUT_FREQ_HZ` for `input.freq_hz`;
3. the config file;
4. the built-in default.

`--config FILE` names the file. Without it, `run`, `soak`, `doctor`, `check`,
`status`, `health` and `config check` read the path from `MANTA_CONFIG`. The
same seven commands apply the `MANTA_*` variables, though `status` and `health`
apply them only when `--config` or `MANTA_CONFIG` names a file. `decode`,
`oracle` and `bench sensitivity` read only the file `--config` names and never
the environment, so their output does not depend on it. `[[rbn_uplink]]` cannot be
set from the environment. An unknown table, key or `MANTA_*` variable is an
error that names it; `MANTA_GIT_SHA` and `MANTA_FEATURES` are build-time
variables and are skipped at run time.

A `MANTA_*` variable set to the empty string counts as unset: it does not
override or clear the file's value. `MANTA_INPUT_PASSWORD=""` therefore keeps
a password set in the file, even alongside a `MANTA_INPUT_HOST` that points at
a different receiver; to drop a file password, remove it from the file.
`MANTA_INPUT_TYPE` set to a type other than the file's `input.type` discards
the file's whole `[input]` table, shared keys such as `freq_correction_ppm`,
`center_freq_hz` and `capture_rate_hz` included, before the other
`MANTA_INPUT_*` variables apply. A relative path set in the file resolves against the
file's directory; one from a flag or a `MANTA_*` variable resolves against the
working directory. [SPEC §9](SPEC-decode-core.md#9-configuration-keys) gives
the full rules, including how variable values are typed.

## Which commands read which tables

Every command that takes `--config` validates the whole file. Only the tables
listed here change what the command does.

| Command | Tables it applies | Environment |
|---|---|---|
| `run` (alias `listen`) | all seven | yes |
| `soak` | `[input]` `[spot]` `[detector]` `[decode]`; `[server]` is ignored with a note | yes |
| `doctor` | `[input]` `[spot]` `[detector]` `[decode]`, plus `[server]`, `[[rbn_uplink]]` and `[rbn_compare]` to check them without starting a server or logging in (MAN-126; see [RUNBOOKS/setup-checks.md](RUNBOOKS/setup-checks.md)) | yes |
| `config check` | all seven, resolved exactly as `run` resolves them | yes |
| `check` | `[input]`: the source, `capture_rate_hz` and `center_freq_hz` | yes |
| `status` | `[server]` `metrics_bind_addr` and `metrics_port`, to find the daemon | yes, but `MANTA_SERVER_*` only alongside a config file |
| `health` | `[server]` `metrics_bind_addr` and `metrics_port`, to find the daemon's `/healthz`; a named config that resolves to no `[server]` table is an error | yes, but `MANTA_SERVER_*` only alongside a config file |
| `decode` | `[decode]` `[detector]` `[spot]` and `input.freq_correction_ppm` | no |
| `oracle` | `[decode]` | no |
| `bench sensitivity` | `[decode]` `[detector]` | no |
| `grade` | `[rbn_compare]` `window_secs`, the tolerances and the spotter lists, from the file `--config` names; its flags win | no |

`run` starts the telnet, JSON and metrics servers, the live-state stream
when `live_port` is set, and any uplink, only when the resolved config has a
`[server]` table. The table can come from the file
or a `MANTA_SERVER_*` environment variable.

## Checking a file

```console
$ manta config init --out manta.toml
$ manta config check --config manta.toml
manta.toml: valid (from --config)
environment: none
server: none (manta run starts no servers)
rbn_uplink: none
input: type=unset (the default sound card, unless a command-line flag picks the source)
spot: allowlist=none blocklist_path=none notch_path=none cty_path=bundled scp_path=bundled cty_sha256=none scp_sha256=none refresh_lists=off list_cache_dir=none cty_url=http://www.country-files.com/cty/cty.dat scp_url=http://www.supercheckpartial.com/MASTER.SCP
detector: defaults
decode: engine=legacy (other keys default)
```

`config check` runs `run`'s config stage, `MANTA_*` variables included, and
prints one line per table on stdout: the keys the file or environment set, plus
the values that matter for that table. Notes go to stderr, for example a
warning that the built-in `cty.dat` is more than 180 days old. It exits 0 when
the file is valid and 1, naming the key and the problem, when it is not. It
also rejects an uncommented `<...>` placeholder, the example callsign `N0CALL`
and two servers on one port. Without `--config` or `MANTA_CONFIG` it checks
`manta.toml` in the current directory, which `run` never reads on its own.
`manta config init --out -` prints the scaffold instead of writing it, and
`init` will not replace an existing file without `--force`. See
[DECISIONS/2026-10-07-man76-config-check-init.md](DECISIONS/2026-10-07-man76-config-check-init.md).

**Reloading without a restart.** On Unix, `SIGHUP` to a `run` daemon (a config
with a `[server]` table) re-reads the file and applies `[spot]` `allowlist`,
`blocklist_path` and `notch_path`, and each `[[rbn_uplink]]` block's `dry_run`.
Every other key, `cty_path`, `scp_path` and the list-refresh keys included,
needs a restart; a reload logs the changed keys it did not apply. A
`refresh_lists` download replaces `cty.dat` or `MASTER.SCP` on its own
schedule, not on `SIGHUP`. A file that fails the checks above is
logged and ignored. See
[DECISIONS/2026-10-10-man78-live-reload.md](DECISIONS/2026-10-10-man78-live-reload.md).

## Columns

- **Default** is the built-in value. A key with no default reads
  `none; example ...`, with the scaffold's example value, followed by what
  leaving it unset means.
- **Wired** says when the key changes behaviour. `live` means every command
  that applies the table (see the matrix above) acts on it.
  `live: <condition>` means only under that condition. `; reloads on SIGHUP`
  marks the keys a reload applies.
- **Notes** gives units, range and the flag that overrides the key, if any.

## [server]

The telnet, JSON and metrics servers, and the optional live-state stream
(`live_port`). Only `run` starts them. `doctor` reads
the table to check that each listener's address and port can bind right now
and that `station_callsign` is not the example `N0CALL`. `status` reads
`metrics_bind_addr` and `metrics_port` to find a running daemon, from the file
`--config` or `MANTA_CONFIG` names plus any `MANTA_SERVER_*` overrides; with
neither it dials `127.0.0.1:7302`. `health` reads the same two keys the same
way to probe `/healthz`, but exits 1 when the named file plus overrides resolve
to no `[server]` table, since `run` then serves no `/healthz`. `--addr` skips
the config for both. The table needs `station_callsign`.

| Key | Default | Wired | Notes |
|---|---|---|---|
| `station_callsign` | none; example `"N0CALL"`; required whenever `[server]` is present | live: `run` with `[server]` | Sent as the spotter of every spot. `run` and `config check` reject `N0CALL`; the accepted forms are in [SPEC §9](SPEC-decode-core.md#9-configuration-keys). |
| `operator_name` | none; example `"Art"`; unset leaves it out of the greeting | live: `run` with `[server]` | Your name in the telnet greeting that clients and RBN's Aggregator read. Free text, no control characters. |
| `operator_qth` | none; example `"Richmond Hill, ON"`; unset leaves it out of the greeting | live: `run` with `[server]` | Your location in the telnet greeting. Free text, no control characters. |
| `operator_grid` | none; example `"FN03GW"`; unset leaves it out of the greeting | live: `run` with `[server]` | Maidenhead locator for the greeting, 4 or 6 characters. Also every JSON spot's `deGrid`, and its square's centre is `deLat`/`deLon` unless `operator_lat`/`operator_lon` are set (MAN-138). |
| `operator_lat` | none; example `43.9375`; unset with no `operator_grid` sends `deLat` as `null` | live: `run` with `[server]` | Station latitude for JSON spots' `deLat`, decimal degrees, -90 to 90, south negative. Set with `operator_lon` or not at all; alongside `operator_grid` the point must lie inside that square, or config loading fails. See [DECISIONS/2026-10-11-man138-station-location.md](DECISIONS/2026-10-11-man138-station-location.md). |
| `operator_lon` | none; example `-79.4583`; unset with no `operator_grid` sends `deLon` as `null` | live: `run` with `[server]` | Station longitude for JSON spots' `deLon`, decimal degrees, -180 to 180, west negative. Same pair and inside-the-grid rules as `operator_lat`. |
| `bind_addr` | `"0.0.0.0"` | live: `run` with `[server]` | Address the telnet and JSON servers listen on; it does not move the metrics listener. See [RUNBOOKS/network-exposure.md](RUNBOOKS/network-exposure.md). |
| `metrics_bind_addr` | `"127.0.0.1"` | live: `run` with `[server]`, `status` and `health` | Address of `/metrics` and `/healthz`, which have no password. See [DECISIONS/2026-10-08-man132-metrics-loopback-bind.md](DECISIONS/2026-10-08-man132-metrics-loopback-bind.md). |
| `telnet_port` | `7300` | live: `run` with `[server]` | DX-cluster telnet port. |
| `json_port` | `7301` | live: `run` with `[server]` | JSON Lines and WebSocket spot stream; both share this port. |
| `metrics_port` | `7302` | live: `run` with `[server]`, `status` and `health` | Prometheus metrics port; `manta status` and `manta health` dial it when given `--config` or `MANTA_CONFIG`. |
| `live_port` | none; example `7303`; unset runs no live-state listener | live: `run` with `[server]`; restart required | Live-state stream port; see [LIVE-STATE.md](LIVE-STATE.md). |
| `live_bind_addr` | `"127.0.0.1"` | live: `run` with `[server]` and `live_port`; restart required | Address of the live-state listener, which has no password. The stream carries every track's decoded text; set `"0.0.0.0"` only for a dashboard on another machine, and firewall `live_port`. See [RUNBOOKS/network-exposure.md](RUNBOOKS/network-exposure.md). |
| `line_format` | `"rbn"` | live: `run` with `[server]` | Telnet spot-line layout, `"rbn"` or `"skimmer"`. The uplink always sends the `"rbn"` layout. |
| `status_interval_secs` | `60` | live: `run` with `[server]` | Seconds between one-line status reports in the log; `0` turns them off. |
| `history_minutes` | `30` | live: `run` with `[server]`; restart required | Telnet replay retention in whole minutes, 0–1440, measured from the newest published spot's sample timestamp. Keeps at least the newest 50 spots and at most 10,000; `0` keeps only the newest 50. Loggers request `sh/dx 30m`; bare `sh/dx` returns 10 rows. See [MAN-93](DECISIONS/2026-10-11-man93-sh-dx-history-window.md). |
| `telnet_max_connections_per_ip` | `16` | live: `run` with `[server]` | Open telnet connections per client IP; `0` removes the limit. See [RUNBOOKS/network-exposure.md](RUNBOOKS/network-exposure.md). |
| `json_max_connections_per_ip` | `16` | live: `run` with `[server]` | Open JSON and WebSocket connections per client IP; `0` removes the limit. The live-state listener applies the same quota, counted separately. |
| `metrics_max_connections_per_ip` | `8` | live: `run` with `[server]` | Open metrics connections per client IP; `0` removes the limit. |
| `telnet_max_commands_per_ip` | `30` | live: `run` with `[server]` | Telnet commands per client IP per 10 seconds; `0` removes the limit. |
| `json_max_pings_per_ip` | `10` | live: `run` with `[server]` | WebSocket pings per client IP per 60 seconds; `0` removes the limit. The live-state listener applies the same budget, counted separately. |
| `metrics_max_requests_per_ip` | `60` | live: `run` with `[server]` | Metrics connections per client IP per 60 seconds; `0` removes the limit. |

## [[rbn_uplink]]

One block per upstream collector. A block needs a `[server]` table, and the
environment cannot set these keys. Only `run` logs in; `doctor` opens and
immediately closes one TCP connection to each enabled target, sending
nothing, and flags an example `login_callsign`.

| Key | Default | Wired | Notes |
|---|---|---|---|
| `enabled` | none; example `true`; required in every block | live: `run` with `[server]` | `false` keeps the block but does not connect. |
| `target_host` | none; example `"<collector-host>"`; required | live: `run` with `[server]` | The collector's host name. `config check` rejects the `<...>` placeholder. |
| `target_port` | none; example `7000`; required | live: `run` with `[server]` | The collector's port. |
| `login_callsign` | none; example `"N0CALL"`; unset logs in as `station_callsign` | live: `run` with `[server]` | Callsign sent at login. `run` and `config check` reject `N0CALL`. |
| `dry_run` | `true` | live: `run` with `[server]`; reloads on SIGHUP | `true` connects and logs in but sends no spots. See [RUNBOOKS/uplink-health.md](RUNBOOKS/uplink-health.md). |
| `spot_types` | `"cq_beacon"` | live: `run` with `[server]` | `"cq_beacon"` sends CQ and beacon spots only, as RBN requires; `"all"` sends every spot. Telnet and JSON still carry every spot. |

## [rbn_compare]

Live comparison with the Reverse Beacon Network (MAN-301), off by default.
Enabling it needs a `[server]` table. `run` (and so `config check` and a
SIGHUP reload) refuses it on a replayed recording; `soak` never starts it. `doctor` opens and immediately closes
one TCP connection to `feed_host:feed_port`, sending nothing. See
[RUNBOOKS/rbn-comparison.md](RUNBOOKS/rbn-comparison.md).

| Key | Default | Wired | Notes |
|---|---|---|---|
| `enabled` | `false` | live: `run` with `[server]` | `--rbn-compare` turns it on for one run. |
| `feed_host` | `"telnet.reversebeacon.net"` | live: `run` with `[server]`; `doctor` | RBN's public telnet feed. manta logs in and sends nothing else. |
| `feed_port` | `7000` | live: `run` with `[server]`; `doctor` | RBN's CW and RTTY feed; manta keeps CW reports only. |
| `login_callsign` | none; example `"N0CALL"`; unset logs in as `station_callsign` | live: `run` with `[server]` | Callsign sent at login. |
| `window_secs` | `180` | live: `run` with `[server]`; `grade` | 30 to 3600. RBN reports within this many seconds of a spot count. |
| `freq_tol_hz` | `500.0` | live: `run` with `[server]`; `grade` | Further than this from RBN's modal frequency is a frequency disagreement. |
| `speed_tol_wpm` | `3.0` | live: `run` with `[server]`; `grade` | A speed disagreement needs more than this many WPM ... |
| `speed_tol_pct` | `20.0` | live: `run` with `[server]`; `grade` | ... and more than this percent of RBN's median speed. |
| `spotters` | `[]` | live: `run` with `[server]`; `grade` | Only these RBN skimmers count. `--rbn-spotter` replaces it. |
| `exclude_spotters` | `[]` | live: `run` with `[server]`; `grade` | Never count these skimmers. This station's own callsign is always excluded. |

## [input]

The receiver. `decode` never reads the source keys, and a command-line source
flag (`--device`, `--source`, `--kiwi-host`, `--soapy-driver`, `--hpsdr-host`,
`--ka9q-group`) replaces a typed `[input]` whole, its shared keys included. So does a
`MANTA_INPUT_TYPE` that differs from the file's `type` (see
[Precedence](#precedence)): only the `MANTA_INPUT_*` variables then set
`[input]`.

| Key | Default | Wired | Notes |
|---|---|---|---|
| `type` | none; example `"kiwi"`; unset uses the default sound card, or the source the command line names | live: `run`, `soak`, `doctor`, `check` | `"audio"`, `"file"`, `"kiwi"`, `"soapy"`, `"hpsdr"` or `"ka9q"`. Each type accepts only its own keys below. |
| `device` | none; example `"USB Audio"`; unset uses the system's default input | live: `type = "audio"` | Part of the device's name; `manta devices` lists them. `--device` overrides it. |
| `path` | none; example `"capture.wav"`; required for `type = "file"` | live: `type = "file"` | WAV recording replayed as if live. A relative path set in the file is relative to the file's directory; from `--source`, which overrides it, or `MANTA_INPUT_PATH` it is relative to the working directory. |
| `iq` | `false` | live: `type = "file"` | `true` for a two-channel IQ recording. `--source-iq` sets it. |
| `realtime` | `false` | live: `run`, `type = "file"`; `soak` and `doctor` ignore it with a note | `true` paces the replay to the recording's own clock, so telnet/JSON clients can watch it. `--realtime` sets it; a live source refuses it. See [DECISIONS/2026-10-10-man269-paced-looping-replay.md](DECISIONS/2026-10-10-man269-paced-looping-replay.md). |
| `loop` | `false` | live: `run`, `type = "file"`; `soak` and `doctor` ignore it with a note | `true` replays the file again from the start each time it ends, until stopped; implies `realtime`. `--loop` sets it; a live source refuses it. |
| `host` | none; example `"<your-receiver-host>"`; required for kiwi and hpsdr | live: `type = "kiwi"` or `"hpsdr"` (hpsdr needs the build feature) | Receiver host name. `--kiwi-host` or `--hpsdr-host` overrides it. |
| `port` | `8073`; `1024` for hpsdr; `5004` for ka9q | live: `type = "kiwi"`, `"hpsdr"` (hpsdr needs the build feature) or `"ka9q"` | Receiver port, 1 to 65535; for ka9q, the group's RTP data port. `--kiwi-port`, `--hpsdr-port` or `--ka9q-port` overrides it. |
| `password` | `""` | live: `type = "kiwi"` | KiwiSDR password; empty for a public receiver. `config check` prints only whether it is set. `--kiwi-password` overrides it. |
| `driver` | none; example `"driver=rtlsdr"`; required for soapy | live: `type = "soapy"` (needs the build feature) | SoapySDR device arguments. `--soapy-driver` overrides it. |
| `gain_db` | none; example `30.0`; unset uses the device's automatic gain | live: `type = "soapy"` (needs the build feature) | Receiver gain, dB. `--soapy-gain` overrides it. |
| `freq_hz` | none; example `7030000.0`; required for kiwi, soapy and hpsdr | live: `type = "kiwi"`, `"soapy"` or `"hpsdr"` | Receiver centre frequency, Hz. `--kiwi-freq-hz`, `--soapy-freq-hz` or `--hpsdr-freq-hz` overrides it. |
| `rate_hz` | none; example `192000.0`; required for soapy and hpsdr | live: `type = "soapy"` or `"hpsdr"` | Sample rate, Hz. `--soapy-rate-hz` or `--hpsdr-rate-hz` overrides it. |
| `group` | none; example `"239.101.6.1"`; required for ka9q | live: `type = "ka9q"` | Multicast data group of one ka9q-radio (radiod) IQ channel: an IPv4 multicast address, or the name in the channel's `data =` line, resolved by the system resolver when the source opens. Checked when the file loads; an IPv6 group is refused. manta reads the channel's sample rate, encoding and frequency from radiod's status packets. `--ka9q-group` overrides it. |
| `ssrc` | none; example `14100000`; unset reads the group's only channel | live: `type = "ka9q"` | SSRC of the channel to read, 0 to 4294967295; needed only when the group carries more than one channel. `--ka9q-ssrc` overrides it. |
| `interface` | none; example `"192.168.1.20"`; unset joins on the kernel's choice | live: `type = "ka9q"` | IPv4 address of the local interface to join the group on (an interface name is refused). manta also joins on `127.0.0.1`, for a radiod on the same machine. `--ka9q-interface` overrides it. |
| `freq_correction_ppm` | `0.0` | live: `run`, `soak`, `doctor`, `decode`; dropped when a source flag replaces a typed `[input]` | Oscillator correction, ppm, -1000 to 1000. `--freq-correction-ppm` overrides it. |
| `center_freq_hz` | none; example `14030000.0`; unset leaves frequencies as audio offsets, and `run` with `[server]` refuses a source that carries no frequency of its own | live: `run`, `soak`, `doctor`, `check`; dropped when a source flag replaces a typed `[input]` | Your radio's dial frequency, Hz, for a source that carries none. `--dial-freq-hz` overrides it. |
| `capture_rate_hz` | none; example `48000.0`; unset decodes at the receiver's own rate | live: `run`, `soak`, `doctor`, `check`; dropped when a source flag replaces a typed `[input]` | Decimated sample rate, Hz. It must be at least 1000 (checked before the receiver opens), divide the receiver's rate by a power of two, and be 93.75 Hz (the channel spacing) times a power of two of at least 4, so in practice 1500, 3000, 6000, 12000, 24000, 48000, 96000 and so on. A rate that divides evenly but fails the last rule, such as 50000 from a 100 kS/s receiver, is rejected at startup. `--capture-rate-hz` overrides it. |
| `replay_epoch` | none; example `1700000000`; unset uses the file's modification time | live: `run` with `[server]` and a file source; ignored elsewhere | Unix seconds that the recording's first sample stands for. `--replay-epoch` overrides it. |

## [spot]

Which decodes become spots. `allowlist`, `blocklist_path` and `notch_path`
reload on SIGHUP; every other key needs a restart. With `refresh_lists` on,
`run` from a live receiver also replaces `cty.dat` and `MASTER.SCP` on its
own schedule. See
[DECISIONS/2026-10-12-man316-reference-list-refresh.md](DECISIONS/2026-10-12-man316-reference-list-refresh.md).

| Key | Default | Wired | Notes |
|---|---|---|---|
| `allowlist` | `[]` | live: `run`, `soak`, `doctor`, `decode`; reloads on SIGHUP | Callsigns that skip the plausibility checks (grammar, country allocation, repetition). They are still suppressed by `blocklist_path` and `notch_path`, which are checked first. A non-empty `--allowlist` replaces the list. |
| `blocklist_path` | none; example `"bad-calls.txt"`; unset blocks no callsign | live: `run`, `soak`, `doctor`, `decode`; reloads on SIGHUP | File of callsigns never spotted, one per line. A relative path set in the file is relative to the file's directory; from `--blocklist`, which overrides it, or `MANTA_SPOT_BLOCKLIST_PATH` it is relative to the working directory. |
| `notch_path` | none; example `"notches.txt"`; unset notches no frequency | live: `run`, `soak`, `doctor`, `decode`; reloads on SIGHUP | File of `low_hz-high_hz` ranges never spotted, one per line. A relative path set in the file is relative to the file's directory; from `--notch`, which overrides it, or `MANTA_SPOT_NOTCH_PATH` it is relative to the working directory. |
| `cty_path` | none; example `"cty.dat"`; unset uses the built-in `cty.dat` | live: `run`, `soak`, `doctor`, `decode` | AD1C's country-prefix file. A relative path set in the file is relative to the file's directory; from `--cty`, which overrides it, or `MANTA_SPOT_CTY_PATH` it is relative to the working directory. See [DECISIONS/2026-10-10-man79-operator-cty-scp-override.md](DECISIONS/2026-10-10-man79-operator-cty-scp-override.md). |
| `scp_path` | none; example `"MASTER.SCP"`; unset uses the built-in `master.scp` | live: `run`, `soak`, `doctor`, `decode` | Known-callsign list that raises confidence. A relative path set in the file is relative to the file's directory; from `--scp`, which overrides it, or `MANTA_SPOT_SCP_PATH` it is relative to the working directory. |
| `refresh_lists` | `false` | live: `run` with a live source; file replay ignores it | Downloads `cty.dat` every 7 days and `MASTER.SCP` every day from `cty_url` and `scp_url`, with `If-Modified-Since`. A download replaces the list in use only after it parses and passes the completeness checks; a failed or rejected one is logged as a `warning: list refresh:` line, counted in `manta_list_refresh_total{outcome="failed"}` and retried an hour later, and the list in use stays. Skipped for a list that `cty_path`/`scp_path` or `cty_sha256`/`scp_sha256` sets. `soak`, `decode` and file replay never download. Needs outbound TCP port 80. |
| `list_cache_dir` | none; example `"/var/cache/manta"`; unset keeps downloads in memory only | live: `run` with a live source; file replay ignores it | Where accepted downloads are kept (`cty/<sha256>.dat`, `scp/<sha256>.scp`, each list's `state.json`; the 30 newest versions per list), so a restart starts from the last good download. `cty_sha256`/`scp_sha256` pins also look here, for every validating command, when `cty_path`/`scp_path` is not set. A relative path set in the file is relative to the file's directory; from `MANTA_SPOT_LIST_CACHE_DIR` it is relative to the working directory. The systemd unit and Compose file set it to `/var/cache/manta`. |
| `cty_url` | `"http://www.country-files.com/cty/cty.dat"` | live: `run` with a live source; file replay ignores it | Where `refresh_lists` downloads `cty.dat`. Only `http://` URLs: `https://` is refused at load, because manta has no TLS support. Redirects and chunked responses count as failures. Point it at a local mirror to share one download. |
| `scp_url` | `"http://www.supercheckpartial.com/MASTER.SCP"` | live: `run` with a live source; file replay ignores it | Where `refresh_lists` downloads `MASTER.SCP`. Same rules as `cty_url`. |
| `cty_sha256` | none; example `"e9ff91a3c3ab28c5f7dbcffb4280944a84645ed53d41cb4c68a41f132ee13fd1"`; unset uses `cty_path`, the cache or the built-in copy | live: `run`, `soak`, `doctor`, `decode` | Use exactly the `cty.dat` whose SHA-256 is this (64 hex digits, as in the `lists:` log line). If `cty_path` is set, that file must match and nothing else is tried; otherwise manta looks in the built-in copy, then `list_cache_dir`. manta stops before reading the source if no copy matches. A pinned list is never refreshed. `--cty-sha256` overrides it; `MANTA_SPOT_CTY_SHA256` sets it. |
| `scp_sha256` | none; example `"00692208fa86b1498dcd6fc0f88e14d2a1a0462ba5d481855da5de69be4e3cf0"`; unset uses `scp_path`, the cache or the built-in copy | live: `run`, `soak`, `doctor`, `decode` | Use exactly the `MASTER.SCP` whose SHA-256 is this, looked up like `cty_sha256`. `--scp-sha256` overrides it; `MANTA_SPOT_SCP_SHA256` sets it. |
| `validation_level` | `"normal"` | live: `run`, `soak`, `doctor`, `decode` | `"normal"`: 2 repetitions for a common pattern, 3 for a listed pattern, 5 for an unlisted pattern. `"minimal"`: 2 for every call. Allowlisted calls and beacons are exempt. Advertised to the Aggregator as `vlNormal` or `vlMinimal`. `MANTA_SPOT_VALIDATION_LEVEL` overrides the file for live commands. |
| `pattern_path` | none; example `"patterns.lst"`; unset uses the built-in list | live: `run`, `soak`, `doctor`, `decode` | Callsign-pattern list, independent of `scp_path`. One pattern per line; `+K5@@@` marks a common pattern, `K5@@@` a listed one. Leading `#` starts a comment; `@` represents a letter, `#` a digit after the literal prefix. A relative path set in the file is relative to the file's directory; `--patterns` or `MANTA_SPOT_PATTERN_PATH` overrides it relative to the working directory. |

## [detector]

When a signal becomes a decoded track and when the track ends. Durations are in
milliseconds, 0 to 3600000.

| Key | Default | Wired | Notes |
|---|---|---|---|
| `on_snr_db` | `12.0` | live | SNR, dB, that starts a track; 0 to 100. See [SPEC §9](SPEC-decode-core.md#9-configuration-keys). |
| `off_snr_db` | `3.0` | live | SNR, dB, below which a track counts as silent; 0 to `on_snr_db`. |
| `confirm_ms` | `50.0` | live | How long a signal must last before it is decoded; at most 3600000 (1 h), and must round to at least one hop. |
| `hang_ms` | `5000.0` | live | How long a silent track waits before it closes; at most 3600000 (1 h), and must round to at least one hop. |
| `gc_ms` | `30000.0` | live | A track that decodes no character for this long is closed; at most 3600000 (1 h), and must round to at least one hop. |
| `warmup_ms` | `25.0` | live | Start-up time spent measuring the noise; no track opens before it ends. 0 to 3600000 (1 h); 0 means no warmup. |
| `track_cap` | `1200` | live | Most tracks open at once, counting unconfirmed candidates that have no decoder yet; past it the lowest-SNR track is evicted. At least 1. |
| `silent_respawn_cooldown_ms` | `30000.0` | live | Wait before a channel whose track went silent may start another. 0 to 3600000 (1 h); 0 means no cooldown. |
| `crash_filter` | `"full"` | live | What a static crash (a band-wide lift of 3 dB or more on at least three quarters of the passband's channels for 8 ms or more) does. `"full"`: no track starts on it, and running tracks do not decode it. `"starts-only"`: no track starts on it, but running tracks decode it as received. `"off"`: no filtering. Crashes are counted in `manta_static_crashes_total` whatever the value. Inactive when the source passband holds fewer than 96 channels (rig audio; captures of 6 kHz or less). Needs a restart. See [SPEC §2.7](SPEC-decode-core.md#27-band-wide-static-crash-detection-man-312) and [MAN-312](DECISIONS/2026-10-13-man312-static-crash-filter.md). |
| `rtty_filter` | `true` | live | Recognise RTTY (two-tone FSK) signals and start no CW track on them or their keying sidebands; `false` restores the previous behaviour. See [SPEC §2.6](SPEC-decode-core.md#26-two-tone-fsk-rtty-recognition) and [MAN-314](DECISIONS/2026-10-11-man314-rtty-recognition.md). |
| `rtty_dwell_ms` | `20000.0` | live | How long an FSK signal must last before it is marked as RTTY; 1000 to 3600000 (1 h), counted in whole 1000 ms detector blocks. |

## [decode]

Expert tuning. Which keys take effect depends on the engine. The defaults are
the measured best values.

| Key | Default | Wired | Notes |
|---|---|---|---|
| `engine` | `"legacy"` | live | `"legacy"`, `"edge-legacy"` or `"hsmm"`. `--engine` overrides it on `run`, `decode`, `oracle` and `bench sensitivity`; `hsmm` is experimental, see [DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md](DECISIONS/2026-09-09-decode-core-v2-stage2-gate.md). |
| `timing_sigma` | `0.25` | live: legacy and edge-legacy | Timing tolerance when matching dots and dashes; at least 0.001. See [SPEC §9](SPEC-decode-core.md#9-configuration-keys). |
| `beam_width` | `4` | live: legacy and edge-legacy | Character guesses kept at each step; at least 1. |
| `debounce_ms` | `12.0` | live: legacy and edge-legacy | Shortest key-down or key-up accepted, ms; above 0. |
| `hyst_frac` | `0.15` | live: legacy only | Keying-threshold hysteresis as a fraction of keying depth, above 0 and below 0.5. See [SPEC §9](SPEC-decode-core.md#9-configuration-keys). |
| `tau_lo_ms` | `500.0` | live: legacy only | Noise-level tracking time constant, ms; above 0. |
| `tau_hi_bounds_ms` | `[100.0, 400.0]` | live: legacy only | Signal-level tracking time constant, `[low, high]` ms, with 0 < low <= high. |
| `flush_gap_dits` | `7.0` | live: legacy and edge-legacy | Silence, in dot lengths, that ends a word; above 0. |
| `sigma_u` | `0.3` | live: hsmm only | Keying-edge timing spread; at least 0.001. edge-legacy reads it but uses only the sign of the evidence, which this key cannot change; see [SPEC-v2 §7](SPEC-decode-core-v2.md#7-configuration-keys-v1-9-additive). |
| `llr_clip` | `10.0` | live: hsmm only | Cap on per-sample mark/space evidence; above 0. edge-legacy reads it but uses only the sign of the evidence, which a positive cap cannot change. |
| `hold_dits` | `4.0` | live: edge-legacy and hsmm | Mark-level hold window, dot lengths; above 0 and below about 73.1: `round(hold_dits × 56)` (56 hops/dit is the fixed HSMM `u_max`) must stay under the 4096-sample evidence retention limit, under every engine. See [SPEC-v2 §7](SPEC-decode-core-v2.md#7-configuration-keys-v1-9-additive). |
| `fallback_hops` | `8` | live: hsmm only | Evidence interval when no keying edge is seen; at least 1. edge-legacy computes these anchors but never reads them. |
| `noise_window_ms` | `1500.0` | live: edge-legacy and hsmm | Noise-floor window, ms; above 0. |
| `noise_min_bias_db` | `2.16` | live: edge-legacy and hsmm | Noise-floor minimum correction, dB. |
| `spectral_min_bias_db` | `1.5` | live: edge-legacy and hsmm; not `oracle` | Neighbouring-channel noise correction, dB. |
| `spectral_beta` | `0.5` | live: edge-legacy and hsmm; not `oracle` | Weight of the neighbouring-channel noise estimate; 0 or more. |
| `refine_bw_hz` | `0.0` | live: every engine; not `oracle` | Narrowband refiner bandwidth before decoding, Hz; 0 or more, and `0` turns it off. The track manager applies it, and `oracle` decodes without one. |
| `dur_sigma` | `0.22` | live: hsmm only | Element-length tolerance; at least 0.001. See [SPEC-v2 §7](SPEC-decode-core-v2.md#7-configuration-keys-v1-9-additive). |
| `mark_insert_penalty` | `-1.5` | live: hsmm only | Penalty against reading noise as an extra element. |
| `beam` | `12` | live: hsmm only | Hypotheses kept per step; at least 1. |
| `lookahead_dits` | `25.0` | live: hsmm only | How far ahead, in dot lengths, the decoder looks before a character is final; 0 or more. |
| `speed_alpha` | `0.2` | live: hsmm only | How quickly the speed estimate follows the sender; 0 or more. |
| `seed_units_hops` | `[9.0, 13.0, 18.0, 26.0, 38.0]` | live: hsmm only | Starting dot lengths tried, hops; at least one, each within the HSMM decoder's supported speed range of 7.5 to 56 hops per dit (fixed, not configurable); `manta config check` and startup reject anything outside it. |
| `conf_kappa` | `6.0` | live: hsmm only | Confidence scale; above 0. |

## Not settable (compile-time constants)

[SPEC §9](SPEC-decode-core.md#9-configuration-keys) lists these keys with their
values, commented out. They are still constants in the code: the loader rejects
each one with "not configurable yet" rather than ignoring it.

| Key | Table | Fixed in |
|---|---|---|
| `floor_quantile` | `[detector]` | `manta-dsp::floor` |
| `floor_window_ms` | `[detector]` | `manta-dsp::floor` |
| `block_channels` | `[detector]` | `manta-dsp::floor` |
| `block_allowance_db` | `[detector]` | `manta-dsp::floor` |
| `mu_ratio_bounds` | `[decode]` | `manta-decode::timing` |
| `char_gap_dits` | `[decode]` | `manta-decode::timing` |
| `word_gap_dits` | `[decode]` | `manta-decode::timing` |
| `cluster_alpha` | `[decode]` | `manta-decode::timing` |
