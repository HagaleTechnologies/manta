# MAN-93: a configurable heard-time window for `sh/dx` history

**Status:** accepted, implemented.

## Context

The bus previously retained only the last 50 published spots. The planning
probe published 300 spots across ten minutes before a client logged in.
Both `sh/dx 30m` and `sh/dx 1000` returned only 50 rows. A logger connecting
mid-contest could not fill its band map with the earlier spots it requested.
The implementation's socket regression reproduced the same 50-row limit
before the retention change.

## Decisions

### D1. Retain heard time, with a floor and ceiling

`[server].history_minutes` defaults to 30. A bus retains every spot heard
within that window of the highest `sample_ts` ever published, inclusive,
subject to `MAX_HISTORY_SPOTS` of 10,000. It always keeps the newest 50
publications, or all publications if fewer than 50 exist. The floor protects
quiet-band history. The ceiling bounds memory and the size of one replay.

The window in samples is `ceil(minutes * 60 * sample_rate_hz)`. Eviction
uses publication order and the sample clock only, never wall time, so it
is deterministic. Each publish removes front entries while above the
ceiling, or while above the floor and the front entry is outside the window.
Zero minutes explicitly selects count-only retention, including when
multiple spots share a timestamp.

### D2. Late publications cannot move the window backwards

The highest heard timestamp is a high-water mark. Eviction examines only
the front of the deque, with amortized constant cost per publish. An older
entry may remain behind a younger front until a later publish removes it.
Minutes queries still filter each entry by heard time. An in-window entry
is never evicted except at the ceiling.

### D3. Bare `sh/dx` stays at ten rows

[MAN-92](2026-10-10-man92-telnet-commands.md) adopted the meanings in the
[RBN Aggregator manual v6.0, section 10.5](https://cms.reversebeacon.net/sites/cms.reversebeacon.net/files/2019/12/21/Using%20Aggregator%20-%20v6.0.pdf).
A bare `sh/dx` returns the newest ten spots. `sh/dx N` requests a count;
`sh/dx Nm` requests a minutes window. The
[DXSpider user manual, section 4.1](https://www.dxcluster.org/usermanual_en-4.html)
also documents a ten-row bare query. MAN-93 deepens the store without
changing the command grammar or making a terminal's bare request produce
thousands of lines.

To fill a logger's band map when it connects, add `sh/dx 30m` to its
cluster connect commands. With the default retention, manta can replay
spots heard in the last 30 minutes, subject to the ceiling and the client's
filters. A query cannot retrieve entries the store has already evicted.
A larger configured window allows a correspondingly larger minutes query.

### D4. Configure zero through one day, with a restart

`history_minutes` is an optional whole number from 0 through 1440 in
`[server]`. Zero restores the previous 50-publication store exactly.
The one-day upper bound limits the requested duration; the spot ceiling
still applies. `MANTA_SERVER_HISTORY_MINUTES` overrides the file through
the existing environment overlay. `manta config check` prints the key
when set, and `manta config init` includes its commented default.
Invalid values fail config loading before opening a receiver or listener.

A SIGHUP change reports `server.history_minutes` as restart-only and
leaves the running bus unchanged. History starts empty on every restart.

### D5. Count premature ceiling eviction

`manta_spots_history_truncated_total` counts spots evicted at the 10,000-entry
ceiling while still inside the configured window. Normal ageing is not loss
and does not increment it. `SpotBus::publish` returns the truncation count;
the daemon records it in `Metrics` once per publish, independent of clients.
The counter's HELP text explains that non-zero means history reaches back
less than the configured window.

### D6. Clone only the selected rows

`SpotBus::select_recent(keep, limit)` filters under the history lock,
selects the newest matching entries, and returns them oldest first.
It clones only those entries, so a bare query clones at most ten.
Predicates must not reacquire the history lock; the heard-time conversion
`unix_ts_for` needs no lock. `recent(n)` uses the same selector.
Band and mode filtering precede count selection. The per-client unique
filter still runs afterward without backfill. Replay checks shutdown
before every row, bounds every write, and counts abandoned replay
separately from live loss, as before MAN-93.

## Costs and validation

The planning probe at base `40b6cf0`, with the old fixed cap temporarily
raised to 10,000, measured 389-422 ns per publish for stores of 3,000-10,000
entries. Cloning 10,000 entries under the lock took 0.82 ms; a 10,000-row
loopback replay was 770 KB and took 80 ms. These are development-profile
container measurements from the plan, not benchmarks of this implementation
or Raspberry Pi 4 measurements. A `BusSpot` was 72 bytes plus its callsign
allocation, putting the full history near 1 MB. Existing command budgets
bound how often a client can request a full replay.

Unit tests pin inclusive time boundaries, the floor, ceiling, zero mode,
late publication, selector order and truncation accounting. Socket tests
publish 600 spots over an hour before login: the default bus returns the
300 recent rows, while a 90-minute bus also returns the 300 older rows for
`sh/dx 60m`. Config tests cover file and environment values, validation,
summary output, generated defaults, daemon wiring and restart-only reporting.

## Out of scope

History is not persisted. The floor and ceiling are not config keys.
JSON/WebSocket resume, the JSON schema, uplink output, wire line formats
and decoder output are unchanged. This change does not claim a live logger
check, a 24-hour soak or Pi 4 timing.
