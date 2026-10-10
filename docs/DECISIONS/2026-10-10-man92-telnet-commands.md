# MAN-92: telnet replies for unknown commands, `sh/version`, scoped `sh/dx`

**Status:** accepted, implemented.

## Context

Before this change the telnet cluster port (`crates/manta-server/src/telnet.rs`) matched
`Command::Unknown` with an empty arm. `help`, `set/skimmer`, `sh/version`, and any `sh/dx` with
more than a bare count (`sh/dx 20 CW`, `sh/dx 20m`) produced zero reply bytes. A DX-cluster client
or an operator at a terminal could not tell a rejected command from a slow one.

The ticket asked for `sh/dx 20 CW` and `sh/dx 20m` to scope replay by band, "matching
Aggregator's Local User Port command forms". The protocol it cites says something different.
[RBN Aggregator manual v6.0](https://cms.reversebeacon.net/sites/cms.reversebeacon.net/files/2019/12/21/Using%20Aggregator%20-%20v6.0.pdf)
§10.5, verbatim:

```
sh/dx      Shows the last 10 spots
sh/dx XX   Shows the last XX spots
sh/dx XXm  Shows spots from the last XX minutes
You can add " CW" or " RTTY" to each sh/dx command to restrict which spots are displayed.
```

In that grammar `20` is a count and `20m` is a twenty-minute window. Neither selects the 20 m band,
and the manual has no band form at all. The MAN-86 plan had already deferred the minutes and mode
forms with these same meanings.

## Decisions

1. **Aggregator's meanings win.** `sh/dx 20 CW` returns the last twenty spots, CW only (all bands).
   `sh/dx 20m` returns every retained spot heard in the last twenty minutes (all bands). A client
   written against Aggregator gets the answer it expects from manta.
2. **Band selection is a manta extension with its own keyword.** `sh/dx BAND 20m` selects the 20 m
   band. The keyword keeps band names from colliding with the minutes suffix.
3. **Unknown commands get a fixed reply.** Any line that is not a recognised command, including a
   recognised command with malformed or extra arguments and an empty or whitespace-only line, gets
   exactly `Unknown command\r\n`. The connection stays open. The client's text is never echoed.
4. **`sh/version`** replies `manta <version>\r\n`, using the same compiled `CARGO_PKG_VERSION` as
   the greeting banner. Extra arguments make the line `Unknown`.
5. **Valid empty queries stay silent.** A query that matches nothing (empty history, a band with no
   spots, `RTTY`, `sh/dx 0`, `sh/dx 0m`) writes zero rows: no error, no header, no trailer, as
   before. Only a malformed query gets `Unknown command`.

## Grammar

```
sh/version
show version
sh/dx [N | Nm] [BAND band] [CW | RTTY]
show dx [N | Nm] [BAND band] [CW | RTTY]
```

- Tokenizing is unchanged from MAN-86/MAN-87: `/`, whitespace and NUL separate tokens, matching is
  case-insensitive, and a `CR NUL` terminator works. `SHOW/DX/5/BAND/20M/CW` is valid.
- Optional parts must appear in the order shown. Duplicates, reversed order and trailing tokens make
  the whole line `Unknown`. A partial match never counts as success.
- `N` is a count. When neither a count nor a window is given, the default is ten. A leading `+` is
  accepted, as it was before. Negative values and overflow are `Unknown`.
- `Nm` is a minutes window. `N × 60` must fit in a signed 64-bit seconds value, or the line is
  `Unknown`. It is never wrapped or replaced with a default.
- `band` is a name from `band::allocations()` (`2200m`, `630m`, `160m` … `10m`, `6m`), with or
  without its `m` suffix: `BAND 20` and `BAND 20m` are the same. Any other name is `Unknown`.
- `CW` matches every manta spot, because manta decodes CW only. `RTTY` matches none.

## Selection semantics

One request is evaluated in this order (`telnet::select_history_at`):

1. Start from the bus's retained history: the last **fifty** published spots
   (`RECENT_HISTORY_CAP`, `bus.rs`). No request can reach further back.
2. Keep entries in the requested band and mode. For a minutes query, also keep only entries whose
   heard time (the Zulu time the row shows, `SpotBus::unix_ts_for`) falls within
   `max(0, now − N·60) ≤ heard ≤ now`. Both ends are inclusive, and an entry stamped in the future
   is excluded.
3. For a count query, keep the newest `N` survivors. A minutes query keeps every survivor and is
   not cut to ten rows. Output is always in publication order, oldest first.
4. The connection's persistent `set dx filter unique > n` filter runs during replay, as before:
   after count selection, with no backfill. Each spot it rejects still counts in
   `manta_spots_suppressed_by_filter_total`.

Entries a query does not ask for are outside its result, not lost or suppressed spots, so no
counter records them. Band and mode selectors apply to that one request only. They never filter
the connection's live stream. The replay loop is unchanged: it checks for shutdown before each
entry, bounds every write, and keeps replay-abandonment accounting separate from live loss.

## Wire examples

The fixture spot is heard at 22:13 UTC, the query runs at 22:14 UTC, and `> ` marks client input:

```
> help
Unknown command
> sh/version
manta 0.1.0
> sh/dx 20 CW
DX de W3XYZ-#:  14027.10  JA1ABC         CW    30 dB  28 WPM  CQ      2213Z
> sh/dx BAND 20m CW
DX de W3XYZ-#:  14027.10  JA1ABC         CW    30 dB  28 WPM  CQ      2213Z
> sh/dx BAND 40m
> sh/dx 20 SSB
Unknown command
```

Rows use the configured `[server].line_format` renderer, the same one live spots use.

## Out of scope

This change adds no `help` catalogue, `set/skimmer`, `set/nocq`, Ctrl-D, new decode modes, new
config keys, deeper history or richer version provenance (git SHA, features). `help` and
`set/skimmer` are answered by the generic `Unknown command` reply. The greeting, `SETT`, `BYE`,
IAC handling, the JSON and uplink contracts and the spot-line columns are unchanged. Whether a live
Aggregator tolerates the new control replies has not been measured.

## Sources

- RBN Aggregator manual v6.0 §10.5, linked above. It is the same document, URL and byte size that
  `docs/DECISIONS/2026-09-07-man86-aggregator-sett-handshake.md` pins.
- Tests: `crates/manta-server/src/command.rs` (grammar), `telnet.rs` (`select_history_at`
  boundaries and ordering), `crates/manta-server/tests/telnet_acceptance.rs` (exact reply bytes and
  scoped replay over real sockets).
