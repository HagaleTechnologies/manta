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
apply them only when `--config` or `MANTA_CONFIG` names a file. Given
`--addr`, `status` and `health` load no config file and apply no `MANTA_*`
variable: they dial that address even when `MANTA_CONFIG` names a missing or
invalid file, and check neither the file nor the variables. `decode`,
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
| `run` (alias `listen`) | all nine | yes |
| `soak` | `[input]` `[spot]` `[detector]` `[decode]`; `[server]` is ignored with a note | yes |
| `doctor` | `[input]` `[spot]` `[detector]` `[decode]`, plus `[server]`, `[[rbn_uplink]]` and `[rbn_compare]` to check them without starting a server or logging in (MAN-126; see [RUNBOOKS/setup-checks.md](RUNBOOKS/setup-checks.md)) | yes |
| `config check` | all nine, resolved exactly as `run` resolves them | yes |
| `check` | `[input]`: the source, `capture_rate_hz` and `center_freq_hz` | yes |
| `status` | `[server]` `metrics_bind_addr` and `metrics_port`, to find the daemon | yes, but `MANTA_SERVER_*` only alongside a config file; `--addr` loads no config file and applies no `MANTA_*` variable |
| `health` | `[server]` `metrics_bind_addr` and `metrics_port`, to find the daemon's `/healthz`; a named config that resolves to no `[server]` table is an error | yes, but `MANTA_SERVER_*` only alongside a config file; `--addr` loads no config file and applies no `MANTA_*` variable |
| `decode` | `[decode]` `[detector]` `[spot]` and `input.freq_correction_ppm`; several `[[input]]` entries that disagree on it need `--freq-correction-ppm` | no |
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
| `metrics_port` | `7302` | live: `run` with `[server]`, `status` and `health` | Prometheus metrics port; `manta status` and `manta health` dial it when given `--config` or `MANTA_CONFIG` and no `--addr`. |
| `live_port` | none; example `7303`; unset runs no live-state listener | live: `run` with `[server]`; restart required | Live-state stream port; see [LIVE-STATE.md](LIVE-STATE.md). |
| `live_bind_addr` | `"127.0.0.1"` | live: `run` with `[server]` and `live_port`; restart required | Address of the live-state listener, which has no password. The stream carries every track's decoded text; set `"0.0.0.0"` only for a dashboard on another machine, and firewall `live_port`. See [RUNBOOKS/network-exposure.md](RUNBOOKS/network-exposure.md). |
| `line_format` | `"rbn"` | live: `run` with `[server]` | Telnet spot-line layout, `"rbn"` or `"skimmer"`. The uplink always sends the `"rbn"` layout. |
| `status_interval_secs` | `60` | live: `run` with `[server]` | Seconds between one-line status reports in the log; `0` turns them off. |
| `silent_input_warning_secs` | `600` | live: `run` with `[server]` and 2+ `[[input]]` | Seconds an input may decode no spots, while another input decodes some, before manta warns and sets `manta_input_silent{source}` to 1; `0` turns the check off; otherwise 60 to 86400. See [MAN-14](DECISIONS/2026-10-19-man14-per-receiver-hearing-visibility.md). |
| `history_minutes` | `30` | live: `run` with `[server]`; restart required | Telnet replay retention in whole minutes, 0–1440, measured from the newest published spot's sample timestamp. Keeps at least the newest 50 spots and at most 10,000; `0` keeps only the newest 50. Loggers request `sh/dx 30m`; bare `sh/dx` returns 10 rows. See [MAN-93](DECISIONS/2026-10-11-man93-sh-dx-history-window.md). Also bounds the separate JSON resume history, with spots and grades together under the same floor and ceiling, counted in records; see [MAN-137](DECISIONS/2026-10-14-man137-json-stream-resume.md). |
| `telnet_max_connections_per_ip` | `16` | live: `run` with `[server]` | Open telnet connections per client IP; `0` removes the limit. See [RUNBOOKS/network-exposure.md](RUNBOOKS/network-exposure.md). |
| `json_max_connections_per_ip` | `16` | live: `run` with `[server]` | Open JSON and WebSocket connections per client IP; `0` removes the limit. The live-state listener applies the same quota, counted separately. |
| `metrics_max_connections_per_ip` | `8` | live: `run` with `[server]` | Open metrics connections per client IP; `0` removes the limit. |
| `telnet_max_commands_per_ip` | `30` | live: `run` with `[server]` | Telnet commands per client IP per 10 seconds; `0` removes the limit. |
| `json_max_pings_per_ip` | `10` | live: `run` with `[server]` | WebSocket pings per client IP per 60 seconds; `0` removes the limit. The live-state listener applies the same budget, counted separately. |
| `metrics_max_requests_per_ip` | `60` | live: `run` with `[server]` | Metrics connections per client IP per 60 seconds; `0` removes the limit. |

## [[rbn_uplink]]

One block per upstream collector. A block needs a `[server]` table, and the
environment cannot set these keys. Going live (`dry_run = false`) also needs
`[server].operator_grid` set to a 6-character Maidenhead locator (e.g.
`"FN03ab"`); `config check` refuses a live block without one (MAN-319). Only
`run` logs in; `doctor` opens and
immediately closes one TCP connection to each enabled target, sending
nothing, and flags an example `login_callsign`.

| Key | Default | Wired | Notes |
|---|---|---|---|
| `enabled` | none; example `true`; required in every block | live: `run` with `[server]` | `false` keeps the block but does not connect. |
| `target_host` | none; example `"<collector-host>"`; required | live: `run` with `[server]` | The collector's host name. `config check` rejects the `<...>` placeholder. |
| `target_port` | none; example `7000`; required | live: `run` with `[server]` | The collector's port. |
| `login_callsign` | none; example `"N0CALL"`; unset logs in as `station_callsign` | live: `run` with `[server]` | Callsign sent at login. `run` and `config check` reject `N0CALL`. |
| `dry_run` | `true` | live: `run` with `[server]`; reloads on SIGHUP | `true` connects and logs in but sends no spots. `false` (going live) needs `[server].operator_grid` set to a 6-character Maidenhead locator (MAN-319); a SIGHUP reload goes live only if the daemon started with one, since `[server]` needs a restart. See [RUNBOOKS/uplink-health.md](RUNBOOKS/uplink-health.md). |
| `spot_types` | `"cq_beacon"` | live: `run` with `[server]` | `"cq_beacon"` sends CQ and beacon spots only, as RBN requires; `"all"` sends every spot. Telnet and JSON still carry every spot. |
| `max_spots_per_minute` | `600` | live: `run` with `[server]` | Max spots written to this target per 60 s window (the window resets at the first spot after it elapses); further spots are counted in `manta_uplink_ceiling_suppressed_total` and skipped, and the connection stays up. `0` disables the ceiling. Needs a restart; SIGHUP reloads only `dry_run`. |

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

## [[skimmer_relay]]

Forwards the spots of another CW skimmer on your network (CW Skimmer, SkimSrv
or a second manta) through `[[rbn_uplink]]`, under that skimmer's own call
(MAN-320). One block per skimmer. manta connects to `host:port` by telnet,
sends `callsign` plus CRLF when the skimmer's prompt arrives (with or without
a trailing newline), and then only reads, writing a blank line every 300 s to
notice a vanished peer. It reads `DX de` lines in the RBN layout (with the
mode column) and in CW Skimmer's and manta's `skimmer` layout (without it),
and forwards a line only when it is a CW spot whose `DX de` spotter, with
`-#` stripped, equals `callsign`. It sends no `SKIMMER/SETT` and no password.
Failed or dropped connections are retried with 1 s→60 s backoff.

An enabled block needs an enabled `[[rbn_uplink]]` (which needs `[server]`),
and the environment cannot set these keys. Every key needs a restart; a
SIGHUP never applies them. The uplink's `dry_run`, `spot_types` and
`max_spots_per_minute` apply to relayed spots as to manta's own, and
relayed spots never appear on manta's telnet, JSON or stdout output. The
SNR is forwarded as the skimmer quoted it (500 Hz). `doctor` does not
check this table. See `manta_relay_*` in [METRICS.md](METRICS.md).

| Key | Default | Wired | Notes |
|---|---|---|---|
| `enabled` | `true` | live: `run` with `[server]` and an enabled `[[rbn_uplink]]` | `false` keeps the block but does not connect. |
| `host` | none; example `"<skimmer-host>"`; required | live: `run` with `[server]` and an enabled `[[rbn_uplink]]` | The skimmer's host name or address. `config check` rejects the `<...>` placeholder. |
| `port` | none; example `7300`; required | live: `run` with `[server]` and an enabled `[[rbn_uplink]]` | The skimmer's telnet port: CW Skimmer 7300 and SkimSrv 7310 by default; for a second manta, its `telnet_port`. |
| `callsign` | none; example `"N0CALL-2"`; required | live: `run` with `[server]` and an enabled `[[rbn_uplink]]` | `CALL` or `CALL-N` (`-1` through `-99`), uppercased. Sent as the login and used as the spotter on every forwarded line; only lines the skimmer spotted under this call are forwarded. It may not be `[server].station_callsign` or any `[[rbn_uplink]].login_callsign`, since manta already forwards its own spots. `N0CALL` is rejected. |

Load errors (`check`, `run`, `doctor`, `config check` and a SIGHUP reload all
raise them, exit 1):

```
manta.toml: [[skimmer_relay]] 192.168.1.20:7300 forwards spots only through [[rbn_uplink]] -- add an enabled [[rbn_uplink]] block, or set enabled = false on this relay
manta.toml: [[skimmer_relay]] 192.168.1.20:7300 has callsign = "W1ABC", this station's own call -- a skimmer relay forwards another skimmer's spots under that skimmer's own call (e.g. "W1ABC-2"); manta already forwards its own
manta.toml: [[skimmer_relay]] 192.168.1.20:7300 is listed twice
manta.toml: skimmer_relay must be written [[skimmer_relay]] (an array of tables)
manta.toml: [[skimmer_relay]]: callsign "W1ABC-100" is not a plausible callsign (an SSID must be -1 through -99)
manta.toml: [[skimmer_relay]]: host "<skimmer-host>" is the scaffold placeholder -- set the skimmer's host name or address
```

## [[wsjtx_relay]]

Listens for WSJT-X or JTDX decodes by UDP and forwards new FT8/FT4 `CQ`
decodes through `[[rbn_uplink]]` as digital spots (MAN-320). In WSJT-X, set
Settings > Reporting > UDP Server to the `listen` address. manta never sends a
datagram back. It learns each WSJT-X instance's dial frequency and mode from
its `Status` messages (several instances can share one listener), adds the
decode's audio offset to the dial frequency, and drops replays, `.wav`
playback, low-confidence decodes, decodes from an instance it has no dial
frequency for yet, modes other than FT8 and FT4, messages that are not a
`CQ`, hashed `<...>` calls, and the same call, mode and band within 600 s.
The SNR is WSJT-X's own figure (2500 Hz), not converted to 500 Hz. The spot
is timed by its arrival.

An enabled block needs an enabled `[[rbn_uplink]]` (which needs `[server]`),
and the environment cannot set these keys. Every key needs a restart; a
SIGHUP never applies them. The uplink's `dry_run`, `spot_types` and
`max_spots_per_minute` apply. `doctor` does not check this table.

**The listener is unauthenticated.** Anything that can send a UDP datagram to
`listen` can put spots on the uplink under `callsign`. Keep it on
`127.0.0.1` unless WSJT-X runs on another machine; see
[RUNBOOKS/network-exposure.md](RUNBOOKS/network-exposure.md).

**Multicast.** Only one program can own a unicast UDP port. If another program
(GridTracker, JTAlert, a logger) already listens on WSJT-X's port, set WSJT-X's
UDP Server to an IPv4 multicast group (e.g. `224.0.0.1`) and point every
listener, manta included, at it: `listen = "224.0.0.1:2237"` binds
`0.0.0.0:2237` with address reuse and joins the group. IPv6 multicast is
rejected. A taken unicast port stops `manta run` at startup:

```
error: [[wsjtx_relay]] cannot listen on 127.0.0.1:2237: Address already in use (os error 98) -- another program may own the port; set WSJT-X's UDP Server to a multicast group (e.g. 224.0.0.1) and listen there
```

| Key | Default | Wired | Notes |
|---|---|---|---|
| `enabled` | `true` | live: `run` with `[server]` and an enabled `[[rbn_uplink]]` | `false` keeps the block but does not listen. |
| `listen` | `"127.0.0.1:2237"` | live: `run` with `[server]` and an enabled `[[rbn_uplink]]` | `IP:port`; `127.0.0.1:2237` is WSJT-X's default UDP server. An IPv4 multicast group binds `0.0.0.0:port` with address reuse and joins the group; IPv6 multicast is rejected. Port `0` picks a free port, which the startup log names. |
| `callsign` | none; example `"N0CALL"`; required | live: `run` with `[server]` and an enabled `[[rbn_uplink]]` | `CALL` or `CALL-N` (`-1` through `-99`), uppercased. The spotter on every forwarded FT8/FT4 line. The station's own call is allowed here, since FT8 spots are a different mode from manta's CW. `N0CALL` is rejected. |

Load errors (exit 1, from the same entry points as `[[skimmer_relay]]`'s):

```
manta.toml: [[wsjtx_relay]] 127.0.0.1:2237 forwards spots only through [[rbn_uplink]] -- add an enabled [[rbn_uplink]] block, or set enabled = false on this relay
manta.toml: [[wsjtx_relay]] 127.0.0.1:2237 is listed twice
manta.toml: [[wsjtx_relay]]: listen "[ff02::1]:2237" is an IPv6 multicast group -- use an IPv4 group such as "224.0.0.1:2237", or a unicast address
```

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
| `name` | none; example `"20m"`; unset uses the input's `type` (`audio` when untyped) | live: `run`, `soak`, `doctor`, `calibrate`, `check` | The input's name in log lines and in `--input NAME`. Letters, digits, `.`, `_` and `-`, starting with a letter or digit, at most 32 characters. Required on every entry when there are several `[[input]]` entries (MAN-13). |
| `device` | none; example `"USB Audio"`; unset uses the system's default input | live: `type = "audio"` | Part of the device's name; `manta devices` lists them. `--device` overrides it. |
| `path` | none; example `"capture.wav"`; required for `type = "file"` | live: `type = "file"` | WAV recording replayed as if live. A relative path set in the file is relative to the file's directory; from `--source`, which overrides it, or `MANTA_INPUT_PATH` it is relative to the working directory. |
| `record_iq` | none; example `"captures/session.cf32"`; unset records nothing | `run` only; any type | Records this input's IQ while decoding: little-endian cf32 at this path plus `PATH.meta.jsonl`. Never overwrites, so a restart needs a new name; the directory must exist. A relative path set in the file is relative to the file's directory; from `MANTA_INPUT_RECORD_IQ` or `--record-iq`, which overrides it for a single input, it is relative to the working directory. Each `[[input]]` takes its own; two inputs, or the streams of one `bands` plan, cannot share one. See [GUIDES/iq-recording.md](GUIDES/iq-recording.md). |
| `iq` | `false` | live: `type = "file"` | `true` for a two-channel IQ recording or a manta IQ recording (`run --record-iq`, found by its `PATH.meta.jsonl`). `--source-iq` sets it. See [GUIDES/iq-recording.md](GUIDES/iq-recording.md). |
| `iq_format` | none; example `"cf32"`; unset reads a WAV or a manta recording | live: `type = "file"` with `iq = true` | Reads `path` as headerless little-endian IQ: `cf32` (float32 I, Q) or `ci16` (int16 I, Q, divided by 32768). Needs `sample_rate_hz` and a centre (`center_freq_hz` or `--dial-freq-hz`). `--iq-format` overrides it. Omit it for a manta recording, whose metadata describes it. |
| `sample_rate_hz` | none; example `96000`; required with `iq_format` | live: `type = "file"` with `iq_format` | The headerless file's stored sample rate, above 0 and at most 10 MS/s; `capture_rate_hz` still decimates after it. `--sample-rate-hz` overrides it. |
| `realtime` | `false` | live: `run`, `type = "file"`; `soak` and `doctor` ignore it with a note | `true` paces the replay to the recording's own clock, so telnet/JSON clients can watch it. `--realtime` sets it; a live source refuses it. See [DECISIONS/2026-10-10-man269-paced-looping-replay.md](DECISIONS/2026-10-10-man269-paced-looping-replay.md). |
| `loop` | `false` | live: `run`, `type = "file"`; `soak` and `doctor` ignore it with a note | `true` replays the file again from the start each time it ends, until stopped; implies `realtime`. `--loop` sets it; a live source refuses it. |
| `host` | none; example `"<your-receiver-host>"`; required for kiwi; optional for hpsdr | live: `type = "kiwi"` or `"hpsdr"` | Receiver host name. `--kiwi-host` or `--hpsdr-host` overrides it. For hpsdr, set host or mac, or neither to discover the only available radio. An explicit host skips the LAN broadcast, though under `protocol = "auto"` the protocol probe still goes to that host; host and mac together are refused. |
| `mac` | none; example `"02:11:22:33:44:55"`; unset selects the only discovered radio | live: `type = "hpsdr"` | Pin a radio across address changes. `--hpsdr-mac` or `MANTA_INPUT_MAC` sets it. Accepts colon, dash or bare hexadecimal spelling, case-insensitively; summaries print uppercase colon-separated bytes. Cannot be combined with host. |
| `port` | `8073`; `1024` for hpsdr; `5004` for ka9q | live: `type = "kiwi"`, `"hpsdr"` or `"ka9q"` | Receiver port, 1 to 65535; for ka9q, the group's RTP data port; for an hpsdr protocol-2 radio, the base of its port map (see [HPSDR protocol 2](#hpsdr-protocol-2)). `--kiwi-port`, `--hpsdr-port` or `--ka9q-port` overrides it. |
| `protocol` | `"auto"` | live: `type = "hpsdr"` | openHPSDR protocol the radio speaks: `"auto"`, `"1"` or `"2"`; the integers `1` and `2` are accepted too. `"auto"` sends a discovery probe to `host` on every open and uses protocol 2 when the radio answers as protocol 2, protocol 1 otherwise (a warning names the key when the radio does not answer within 1 s). `"1"` (Hermes-Lite 2) and `"2"` (ANAN) skip the probe. `--hpsdr-protocol` or `MANTA_INPUT_PROTOCOL` (read verbatim as a string) overrides it. Any other `type` refuses the key. |
| `auto_attenuation` | `false` | live: `type = "hpsdr"` with protocol 2; restart required | `true` lets manta set the radio's ADC0 step attenuator: ADC overload reported for 2 s adds 6 dB (up to 30 dB), and 10 minutes with none removes 6 dB. Off by default because it changes the radio's settings. Refused with `protocol = "1"` and on a radio with several receivers; under `"auto"` a radio that answers as protocol 1 gets a warning and is left alone. `MANTA_INPUT_AUTO_ATTENUATION` sets it. See [Automatic attenuation](#automatic-attenuation). |
| `password` | `""` | live: `type = "kiwi"` | KiwiSDR password; empty for a public receiver. `config check` prints only whether it is set. `--kiwi-password` overrides it. |
| `driver` | none; example `"driver=rtlsdr"`; required for soapy | live: `type = "soapy"` (needs the build feature) | SoapySDR device arguments. `--soapy-driver` overrides it. |
| `gain_db` | none; example `30.0`; unset uses the device's automatic gain | live: `type = "soapy"` (needs the build feature) | Receiver gain, dB. `--soapy-gain` overrides it. |
| `freq_hz` | none; example `7030000.0`; required for kiwi, soapy and hpsdr unless `bands` is set | live: `type = "kiwi"`, `"soapy"` or `"hpsdr"` | Receiver centre frequency, Hz. `--kiwi-freq-hz`, `--soapy-freq-hz` or `--hpsdr-freq-hz` overrides it. |
| `bands` | none; example `["40m"]`; set instead of `freq_hz` | live: `type = "kiwi"`, `"soapy"` or `"hpsdr"`; restart required | `"all"`, band names such as `"40m"`, or centre frequencies in kHz such as `"7030"` (a string, a number or a list of them). manta plans one stream per slice of each band's CW segment and runs each as an input. See [Band plans: bands](#band-plans-bands) (MAN-310). |
| `tuning_range_hz` | `[0.0, 30000000.0]`; only with `bands` | live: `type = "kiwi"`, `"soapy"` or `"hpsdr"`; restart required | The receiver's tuning range, `[low, high]` in Hz. A band outside it is skipped and a band partly outside it is clipped, each with a note. See [Band plans: bands](#band-plans-bands). |
| `rate_hz` | none; example `192000.0`; required for soapy and hpsdr | live: `type = "soapy"` or `"hpsdr"` | Sample rate, Hz. `--soapy-rate-hz` or `--hpsdr-rate-hz` overrides it. |
| `receivers` | none; example `4` | live: `type = "hpsdr"` | Optional hardware capacity declaration, integer 1 to 255, on the first entry for a radio only. Shared HL2 radios report capacity at startup; other gateware needs this fallback. The smaller reported/declared count applies. This is not the active receiver count. |
| `group` | none; example `"239.101.6.1"`; required for ka9q | live: `type = "ka9q"` | Multicast data group of one ka9q-radio (radiod) IQ channel: an IPv4 multicast address, or the name in the channel's `data =` line, resolved by the system resolver when the source opens. Checked when the file loads; an IPv6 group is refused. manta reads the channel's sample rate, encoding and frequency from radiod's status packets. `--ka9q-group` overrides it. |
| `ssrc` | none; example `14100000`; unset reads the group's only channel | live: `type = "ka9q"` | SSRC of the channel to read, 0 to 4294967295; needed only when the group carries more than one channel. `--ka9q-ssrc` overrides it. |
| `interface` | none; example `"192.168.1.20"`; unset joins on the kernel's choice | live: `type = "ka9q"` | IPv4 address of the local interface to join the group on (an interface name is refused). manta also joins on `127.0.0.1`, for a radiod on the same machine. `--ka9q-interface` overrides it. |
| `freq_correction_ppm` | `0.0` | live: `run`, `soak`, `doctor`, `decode`; dropped when a source flag replaces a typed `[input]` | Oscillator correction, ppm, -1000 to 1000. `--freq-correction-ppm` overrides it. |
| `center_freq_hz` | none; example `14030000.0`; unset leaves frequencies as audio offsets, and `run` with `[server]` refuses a source that carries no frequency of its own | live: `run`, `soak`, `doctor`, `check`; dropped when a source flag replaces a typed `[input]` | Your radio's dial frequency, Hz, for a source that carries none. `--dial-freq-hz` overrides it. |
| `capture_rate_hz` | none; example `48000.0`; unset decodes at the receiver's own rate | live: `run`, `soak`, `doctor`, `check`; dropped when a source flag replaces a typed `[input]` | Decimated sample rate, Hz. It must be at least 1000 (checked before the receiver opens), divide the receiver's rate by a power of two, and be 93.75 Hz (the channel spacing) times a power of two of at least 4, so in practice 1500, 3000, 6000, 12000, 24000, 48000, 96000 and so on. A rate that divides evenly but fails the last rule, such as 50000 from a 100 kS/s receiver, is rejected at startup. The decimator's transition band (the outer ~9.4% of each side, 24 of 256 channels at 48 kS/s — beyond the <= 1 dB point at 0.4526 x the rate) is not usable: the reported passband, `manta check`, SETT segments and tracked channels stop at the trusted band, and `run`/`soak` say so in one startup line naming the source's intersected passband. `--capture-rate-hz` overrides it. |
| `replay_epoch` | none; example `1700000000`; unset uses the file's modification time | live: `run` with `[server]` and a file source; ignored elsewhere | Unix seconds that the recording's first sample stands for. `--replay-epoch` overrides it. |

### HPSDR protocol 2

MAN-305: ANAN-class radios (Hermes, Angelia, Orion, Orion II, Saturn/G2)
speak openHPSDR protocol 2; Hermes-Lite 2 and older boards speak protocol 1.
Both use `type = "hpsdr"` and `--hpsdr-*`, chosen by `protocol` above
(`--hpsdr-protocol auto|1|2`). The probe under `"auto"` goes to one
address: the configured host, or, with no host, the radio that LAN discovery
selected (MAN-306), so a hostless protocol-2 radio streams through this
driver too. `manta devices` lists the radios that answer discovery, of
either protocol.

Protocol 2 uses a port map relative to `port` (`1024` by default):

| Direction | Stream | Port |
|---|---|---|
| manta → radio | general (and discovery) | `port` (1024) |
| manta → radio | receiver settings | `port + 1` (1025) |
| manta → radio | transmitter settings | `port + 2` (1026) |
| manta → radio | high priority (run, frequency, filters) | `port + 3` (1027) |
| radio → manta | command response | from `port` (1024) |
| radio → manta | high-priority status | from `port + 1` (1025) |
| radio → manta | microphone | from `port + 2` (1026) |
| radio → manta | wideband | from `port + 3` to `port + 10` (1027–1034) |
| radio → manta | DDC0 IQ, the stream manta decodes | from `port + 11` (1035) |

manta receives every stream on one unconnected UDP socket and sorts the
datagrams by the radio's source port.

- **Firewall.** Protocol 2 needs a host firewall rule allowing inbound UDP
  from the radio: IQ arrives from `port + 11`, which manta never sent to, so
  a stateful firewall does not treat it as a reply. Without the rule the
  open succeeds and the stream stalls after 10 s with an error that names
  the firewall.
- **Network.** Protocol 2 radios want Gigabit Ethernet. One receiver at
  192 kHz is about 9.6 Mbit/s, which 100 Mbit/s carries, but the radios are
  built and tested for Gigabit.
- **Receive buffer.** manta asks for a 4 MiB socket receive buffer; Linux
  grants at most `net.core.rmem_max` (about 416 KiB by default, 0.36 s at
  192 kHz). Raise `net.core.rmem_max` if
  `manta_input_dropped_packets_total{source="hpsdr"}` climbs on a quiet LAN.
- A radio that reports itself already streaming to another program is
  refused under `"auto"`; `protocol = "2"` takes it over.
- One receiver (DDC0) per radio: several receivers on one radio (MAN-307)
  speak protocol 1, so a shared radio refuses `protocol = "2"`. The rate
  must be exactly 48000, 96000, 192000 or 384000 Hz, and `freq_hz` a whole
  number of Hz below 61440000.
- With `protocol = "2"`, or a board manta does not know, the radio's
  front-end filters are bypassed (ANT1, no high-pass, the 6 m low-pass).

See [DECISIONS/2026-10-19-man305-hpsdr-protocol-2-input.md](DECISIONS/2026-10-19-man305-hpsdr-protocol-2-input.md).

#### Automatic attenuation

MAN-308: with `auto_attenuation = true`, manta reads the radio's
high-priority status datagram (from `port + 1`) and drives ADC0's step
attenuator:

- ADC overload reported for 2 s (reports no more than 500 ms apart count as
  one overload) adds 6 dB, up to 30 dB. Overload that continues adds
  another 6 dB every 2 s.
- 10 minutes with no overload reported removes 6 dB, and every further quiet
  10 minutes another 6 dB, down to 0 dB.

It writes to the radio, so it is off by default. Another program driving
the same radio's attenuator would fight it. It needs protocol 2: config
load refuses it with `protocol = "1"` and on a radio that several
`[[input]]` entries share, and under `"auto"` a radio that answers as
protocol 1 gets a warning naming the key and is left alone.

Each change prints one line, for example:

```
note: HPSDR radio 192.0.2.10:1024: ADC overload for 2 s; automatic attenuation raised the step attenuator from 0 dB to 6 dB
note: HPSDR radio 192.0.2.10:1024: no ADC overload for 10 min; automatic attenuation lowered the step attenuator from 12 dB to 6 dB
```

With a `[server]` table, `/metrics` carries `manta_input_attenuation_db` and
`manta_input_attenuation_changes_total` (see [METRICS.md](METRICS.md)). A
reconnect keeps the level manta had learned; a manta restart starts at
0 dB. See
[DECISIONS/2026-10-27-man308-hpsdr-auto-attenuation.md](DECISIONS/2026-10-27-man308-hpsdr-auto-attenuation.md).

### Band plans: bands

`bands` names what a receiver skims instead of one centre (MAN-310). Write
`"all"`, band names, or centre frequencies in kHz:

```toml
[input]
type = "kiwi"
host = "kiwi.example.net"
bands = ["60m"]            # or "all", ["40m", "20m"], "7030", 7030
#tuning_range_hz = [3000000.0, 30000000.0]
```

The band table below is the one manta uses (`manta_server::band_plan::CW_SEGMENTS`;
a test holds the two equal). Each band's skimmed span runs from the bottom
of its CW segment up to the higher of the segment's top and its NCDXF/IARU
beacon. The segment limits are not configurable.

| band | CW segment, kHz | NCDXF beacon, kHz | skimmed span, kHz |
|---|---|---|---|
| 160m | 1810–1840 | – | 1810–1840 |
| 80m | 3500–3580 | – | 3500–3580 |
| 60m | 5351.5–5366.5 | – | 5351.5–5366.5 |
| 40m | 7000–7040 | – | 7000–7040 |
| 30m | 10100–10130 | – | 10100–10130 |
| 20m | 14000–14070 | 14100 | 14000–14100 |
| 17m | 18068–18095 | 18110 | 18068–18110 |
| 15m | 21000–21070 | 21150 | 21000–21150 |
| 12m | 24890–24915 | 24930 | 24890–24930 |
| 10m | 28000–28070 | 28200 | 28000–28200 |

How a plan is made:

- **Width.** One stream skims 2 × 0.4526 × its width: the decimator's
  <= 1 dB band. The width is one KiwiSDR channel (10 kHz) for kiwi, and
  `rate_hz` for soapy and hpsdr, capped by `capture_rate_hz` when set. A
  96000 Hz stream skims 86.899 kHz.
- **Split.** A span wider than that is cut into equal slices, one stream
  each, centred on whole Hz. `"all"` on a 0 to 30 MHz receiver is 14
  streams at 96 kS/s, 11 at 192 kS/s and 20 at 48 kS/s.
- **Ownership.** Streams whose bands overlap split the overlap at the
  midpoint of their centres: each starts new tracks only on its own side,
  plus one channel (93.75 Hz). A signal in the overlap is decoded by the
  nearer stream; within one channel of the split both may decode it, and
  the cross-input dedupe spots it once. A running track is never stopped.
- **Tuning range.** A band outside `tuning_range_hz` is skipped, a band
  partly outside it is clipped, and a bare centre outside it is skipped,
  each with a note. Nothing left to skim is an error.
- **Repeats.** A band listed twice, a band `"all"` already includes, or a
  bare centre inside a listed band is refused.

**Names.** A plan of one stream keeps the entry's name (`[input]`'s is its
type). Several streams are named `<entry>-<label>`: `kiwi-60m-1`,
`kiwi-60m-2`, `kiwi-7030`. Each is an ordinary input: its own `/healthz`
line, metric series and `--input` name. `calibrate --input kiwi-60m-1`
saves into the entry the stream came from, so one measurement corrects
every stream of that receiver.

**Capacity.** A kiwi entry opens one KiwiSDR channel per stream, within the
receiver's free channels. An hpsdr or soapy entry opens one stream; a plan
that needs more is refused naming it (several receivers on one HPSDR radio
are one `[[input]]` each — see
[the multiple-receivers guide](GUIDES/multiple-receivers.md#several-bands-on-one-hpsdr-radio)).
All inputs together stay within manta's 16, and one entry
plans at most 256 streams before that check (a near-zero `rate_hz` is
refused rather than split into millions of slices). A plan of several
streams refuses `center_freq_hz`: one fixed centre cannot describe
differently tuned streams. ka9q, file and audio inputs do not take
`bands`.

**CPU.** Each stream is one input thread with its own channelizer. At one
Raspberry Pi 4 core per 192 kS/s stream, 14 streams at 96 kS/s need about
seven cores.

**Environment.** `MANTA_INPUT_BANDS=all`, `MANTA_INPUT_BANDS=7030` or
`MANTA_INPUT_BANDS='["40m","20m"]'`.

`config check` and `run` print the plan and every skipped or clipped band
as notes before their output:

```text
note: [input]: bands = ["160m", "60m"] plans 2 streams: 60m (2)
note: [input]: 160m skipped: 1810 to 1840 kHz is outside the receiver's tuning range, 3000 to 30000 kHz
```

`run` logs, for each stream of a split, where it starts new tracks:
`input kiwi-60m-1: new tracks start only from 5350.25 to 5359.094 kHz; the
rest of its passband is nearer another stream's centre`.

### Several receivers: [[input]]

MAN-13: one `run` can decode several receivers -- several SDRs, or several
receiver channels of one SDR (ka9q-radio SSRCs on one radiod, channels of one
KiwiSDR) -- into one spot output. Write the input table as `[[input]]` once
per receiver. Each entry takes the keys above, plus `name`. A one-entry
`[[input]]` is the same as `[input]`.

- With two or more entries, every entry needs a `name` and a `type`. Names
  are unique; at most 16 entries.
- `freq_correction_ppm`, `center_freq_hz` and `capture_rate_hz` belong to
  their entry. An `audio` entry needs `center_freq_hz`, and a file entry
  needs a sidecar or `center_freq_hz`, so every input's frequencies can be
  compared.
- File replay and live receivers do not mix in one run. `replay_epoch` goes
  on the first entry only, and every file entry has the same `realtime` and
  `loop`.
- HPSDR entries sharing a case-insensitive host and effective port, or
  hostless entries sharing an `input.mac` (or neither), use one radio
  session, with receivers assigned in file order. Native `rate_hz`
  and `protocol` must match, and `protocol = "2"` is refused there.
  Capacity is checked at startup, while `config check` remains
  offline. See [the four-band example and diagnostics](GUIDES/multiple-receivers.md#several-bands-on-one-hpsdr-radio).
- `MANTA_INPUT_*` and the per-receiver flags `--freq-correction-ppm`,
  `--dial-freq-hz` and `--capture-rate-hz` cannot say which entry they set,
  so they are refused; a source flag (`--source`, `--kiwi-host`, ...) still
  replaces every entry with the one receiver it names.
- `calibrate`, `doctor`, `soak` and `check` examine one receiver: pass
  `--input NAME`.
- A station two receivers both hear is spotted once (the shared validator's
  cross-input dedupe), on stdout and, with `[server]`, on the one telnet,
  JSON and metrics server. `/healthz` stays `ok` while at least one input
  is healthy. SIGHUP reloads `[spot]` lists and `dry_run`, `refresh_lists`
  refreshes `cty.dat` and `MASTER.SCP` for every input, `manta_rtty_signals`
  counts every input's RTTY marks, each input keeps its own
  `manta_input_*{source}` counters, and with `live_port` set each input
  sends its own `bandState` record, named in its `source` field. See
  [DECISIONS/2026-10-15-man13-multi-input-daemon.md](DECISIONS/2026-10-15-man13-multi-input-daemon.md).

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
| `reacquire_ms` | `10000.0` | live | After `hang_ms`, how long a track that has decoded characters keeps its identity and frequency while its signal is gone. No decoder runs in that time. If the signal comes back, the track resumes under the same track ID with a fresh decoder. 0 to 3600000 (1 h); `0` restores the previous behaviour (close at `hang_ms`). See [SPEC §2.4](SPEC-decode-core.md#24-track-lifecycle-state-machine) and [MAN-108](DECISIONS/2026-10-17-man108-dormant-track-reacquisition.md). |
| `gc_ms` | `30000.0` | live | A track that decodes no character for this long is closed; at most 3600000 (1 h), and must round to at least one hop. |
| `warmup_ms` | `25.0` | live | Start-up time spent measuring the noise; no track opens before it ends. 0 to 3600000 (1 h); 0 means no warmup. |
| `track_cap` | `1200` | live | Most tracks open at once, counting unconfirmed candidates that have no decoder yet; past it the lowest-SNR track is evicted. At least 1. |
| `silent_respawn_cooldown_ms` | `30000.0` | live | Wait before a channel whose track went silent may start another. 0 to 3600000 (1 h); 0 means no cooldown. |
| `crash_filter` | `"full"` | live | What a static crash (a band-wide lift of 3 dB or more on at least three quarters of the passband's channels for 8 ms or more) does. `"full"`: no track starts on it, and running tracks do not decode it. `"starts-only"`: no track starts on it, but running tracks decode it as received. `"off"`: no filtering. Crashes are counted in `manta_static_crashes_total` whatever the value. Inactive when the source passband holds fewer than 96 channels (rig audio; captures of 6 kHz or less). Needs a restart. See [SPEC §2.7](SPEC-decode-core.md#27-band-wide-static-crash-detection-man-312) and [MAN-312](DECISIONS/2026-10-13-man312-static-crash-filter.md). |
| `rtty_filter` | `true` | live | Recognise RTTY (two-tone FSK) signals and start no CW track on them or their keying sidebands; `false` restores the previous behaviour. See [SPEC §2.6](SPEC-decode-core.md#26-two-tone-fsk-rtty-recognition) and [MAN-314](DECISIONS/2026-10-11-man314-rtty-recognition.md). |
| `rtty_dwell_ms` | `20000.0` | live | How long an FSK signal must last before it is marked as RTTY; 1000 to 3600000 (1 h), counted in whole 1000 ms detector blocks. |
| `digital_window_filter` | `true` | live | Skip the common HF FT8, FT4, WSPR and JS8 windows when starting CW tracks. Set `false` to include them. Applies to file decoding and every service input. Restart required. |
| `non_cw_filter` | `true` | live | A new track is decoded only once it shows CW keying. Noise-like wideband data, swept carriers and strong SSB voice start no CW track (0 in-band tracks at +15 to +45 dB is not yet universal: 8 at +15 dB and 2 at +20 dB remain, none spotted; 0 at +30/+45 dB, against 81 at +30 dB with the filter off, testkit voice seed 1), and a track whose signal stops keying closes after `unkeyed_close_ms`. `false` restores the pre-MAN-313 behaviour. See [SPEC §2.8](SPEC-decode-core.md#28-non-cw-rejection-man-313), [MAN-313](DECISIONS/2026-10-13-man313-non-cw-rejection.md) and [MAN-372](DECISIONS/2026-10-21-man372-speech-band-veto.md). Restart required (every `[detector]` key is). |
| `unkeyed_close_ms` | `10000.0` | live | With `non_cw_filter` on, two limits: how long a qualified track may stay key-down with no keying, no decoded character and no callsign read before it closes; and how long a new track may take to show CW keying. Range 1000 to 3600000 (1 h). |

### Digital-mode windows

`digital_window_filter` defaults to `true`, including in existing files that
omit the key. To include CW signals in these windows, set a boolean:

```toml
[detector]
digital_window_filter = false
```

The environment override is `MANTA_DETECTOR_DIGITAL_WINDOW_FILTER=false`.
The quoted TOML string `"false"` is invalid. Restart the process after a change.
Opting in enables CW decoding there; it does not decode digital protocols or
disable other signal filters.

Each window starts at the USB dial below and ends 3 kHz above it. All values
are in kHz; `-` means no entry. Overlapping windows act as their union.

| Band | FT8 | FT4 | WSPR | JS8 |
|---|---:|---:|---:|---:|
| 160 m | 1840 | - | 1836.6 | 1842 |
| 80 m | 3573 | 3575 | 3568.6 | 3578 |
| 60 m | 5357 | 5357 | - | - |
| 40 m | 7074 | 7047.5 | 7038.6 | 7078 |
| 30 m | 10136 | 10140 | 10138.7 | 10130 |
| 20 m | 14074 | 14080 | 14095.6 | 14078 |
| 17 m | 18100 | 18104 | 18104.6 | 18104 |
| 15 m | 21074 | 21140 | 21094.6 | 21078 |
| 12 m | 24915 | 24919 | 24924.6 | 24922 |
| 10 m | 28074 | 28180 | 28124.6 | 28078 |

Membership is half-open, `[dial_hz, dial_hz + 3000)`, evaluated at each
channel's source-reported RF center before frequency correction. Thus
14074.5 kHz is excluded, while 14077 kHz is outside the isolated FT8 window.
An endpoint can still lie in another mode's overlapping window. Boundaries
follow the channel grid, normally 93.75 Hz. This is a track-start restriction;
leakage near an edge can reach an allowed channel, and an existing track can
drift across an edge without being closed.

The table covers common HF defaults, including 60 m. It excludes regional
and DXpedition alternatives and VHF/UHF. It makes no statement about transmit
permissions. See the [MAN-311 decision](DECISIONS/2026-10-19-man311-digital-window-exclusions.md)
for pinned frequency sources and verification.

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

The loader also rejects `[detector] guard_hz`, which SPEC §9 does not list:
it is the internal DC/Nyquist spawn guard, set from the input source
(600 Hz for live audio, 0 otherwise; see
[MAN-273](DECISIONS/2026-10-16-man273-hilbert-image-guard.md)).
