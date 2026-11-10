# MAN-546: per-connection telnet filters for CQ, bands and minimum SNR

**Status:** accepted, implemented.

## Context

manta's telnet cluster port (`:7300`) sent every spot to every client. Its grammar accepted
`sh/dx`, `sh/version`, `set/dx/filter unique > n`, `SKIMMER/SETT` and `BYE`, and
`SettSettings.cq_only` was documented "Always false today". Measured at `acbc00f`, before this
change:

```
PROBE "set/dx/filter cq" -> ["Unknown command"]
PROBE "set/dx/filter band 40m,20m snr 10" -> ["Unknown command"]
PROBE "set dx filter off" -> ["Unknown command"]
PROBE "SKIMMER/SETT" -> ["SETT: vlNormal 14000.0-14088.0"]
```

CW Skimmer Server offers a "send only CQ spots" option and reports it as the `CQ` token in its
`SETT` reply (MAN-86). A logger or a downstream aggregator that wants only CQ spots, only some
bands or only strong signals had to filter on its own side. N1MM+, DXLog and RBN's Aggregator do
filter themselves, and Aggregator needs only an answered `SETT` (MAN-86), so this is a convenience
for clients, not an admission requirement.

## Decisions

1. **Grammar.** `set/dx/filter <clauses>`. A clause is `cq`, `band <list>` or `snr <n>`, in any
   order, each at most once. A band list is one token, comma separated with no spaces; each
   element is an allocation name from `band::allocations()`, with or without the `m` (`40m` or
   `40`). A repeated element (`40m,40m`) is accepted, because a set has one meaning for it. `snr`
   takes a whole number with an optional `+` or `-`. Any other shape makes the whole line
   `Unknown command`, MAN-92's "never a partial match" rule, and the connection's current filter
   does not change. The `unique > n` form is unchanged. A bare `set/dx/filter` clears, as
   AR-Cluster's clear form does.
2. **Replace, not merge.** Each `set/dx/filter <clauses>` replaces the whole cq/band/snr filter; a
   clause it does not name turns off. `unique` is independent of it. `off` and a bare
   `set/dx/filter` clear all four. The last command is the whole truth, so the ack describes the
   full state; clauses combine on one line, so replacing loses nothing.
3. **`cq` means `SpotType::Cq` only.** DE, beacon and untyped spots are withheld. The row label
   `CQ` comes only from that type (`rbn::spot_type_label`). This is narrower than the uplink's
   `cq_beacon` default (MAN-91), as the ticket's "spots whose type is CQ" says.
4. **`snr` compares the row's figure.** `rbn::wire_snr_db` is the 500 Hz whole-dB figure the row
   prints; `rbn::format_line` and the filter both call it, so they cannot drift. A client typing
   `snr 10` is reading the `dB` column, and a 2500 Hz comparison would hide a row reading `10 dB`.
5. **Band set.** A bitmask over `band::allocations()` indices. A spot outside every allocation
   never passes a band clause; with no band clause, any band passes, `unknown` included.
   `band::allocation_index_for_freq_hz` is the one lookup behind both the filter and the JSON
   `band` field.
6. **`sh/dx` honours the filter before count truncation.** The filter joins the mode, `BAND` and
   minutes-window selectors, so `sh/dx 3` means three matching rows. A persistent `band 40m` with
   `sh/dx BAND 20m` selects nothing. `unique` still runs after count selection (MAN-92 decision 4).
   Rows the filter keeps out of a replay are outside the result and not counted; counting them
   would add up to 10,000 per `sh/dx` and count one spot many times.
7. **One helper for the live stream and the drain.** `ConnectionFilters::admits_live` in
   `telnet.rs` does the mode check (uncounted, MAN-322 D5), then the cq/band/snr filter and
   `unique`. A spot either withholds adds 1 to `manta_spots_suppressed_by_filter_total`, never 2.
   The shutdown drain calls the same helper, so it cannot leak a spot the live arm withholds.
8. **`SETT` reports the connection's own state.** A connection with `cq` set gets
   `SETT: vlNormal CQ <segments>`. The reply ORs the connection's `cq` into the station profile's
   `cq_only`, so a future server-wide CQ mode cannot be switched off by a client. Segments do not
   change: they describe what manta decodes, not what a client asked for. This refines MAN-86 D4
   and MAN-91 D5; both stay true for a connection with no filter, whose reply is byte-identical.
9. **Acks.** `Filter set: <clauses>` renders the clauses in command form and fixed order: `cq`,
   `band` with names in `band::allocations()` order, then `snr <n>`. Clearing replies
   `Filter set: off`. The ack is a command the client can send again, and the same text is the
   audit log's `set dx filter <clauses>`.
10. **No JSON or WebSocket filter.** See "JSON stream" below.

## Grammar

```
set/dx/filter cq                        only spots whose type is CQ
set/dx/filter band <b>[,<b>...]         only spots in the listed bands: 2200m 630m 160m 80m 60m
                                        40m 30m 20m 17m 15m 12m 10m 6m, with or without the m,
                                        comma separated, no spaces
set/dx/filter snr <n>                   only spots whose row shows n dB or more (500 Hz figure)
set/dx/filter cq band 40m,20m snr 10    clauses combine; any order; each clause at most once
set/dx/filter off                       clear every filter on this connection, unique included
set/dx/filter                           the same (AR-Cluster's clear form)
set/dx/filter unique > <n>              unchanged (MAN-12)
```

The space form (`set dx filter cq`), any letter case and a `CR NUL` line ending all work, as they
do for every other command. Malformed forms (a duplicate clause, an unknown band, an empty list
element, a space after a comma, a missing or non-integer SNR, `off` with other words, `unique`
combined with clauses) get `Unknown command`.

## Delivery and selection order

- Live stream and shutdown drain: the listener's mode (uncounted), then the cq/band/snr filter
  and `unique` (a withheld spot is counted once).
- `sh/dx`: the listener's mode, the query's mode word, `BAND`, the minutes window and the
  cq/band/snr filter select rows before the count is applied; `unique` runs after. Rows the
  filter keeps out are not counted.
- A filter that matches nothing sends no rows and no message, live or from `sh/dx`, the same as a
  `sh/dx` query that matches nothing (MAN-92 decision 5).
- Each filter command gets its one reply line at once. The new filter governs every spot written
  after its ack line.

## SNR

`snr <n>` compares `rbn::wire_snr_db(snr_db)`: the 2500 Hz pipeline figure plus
`RBN_REF_BW_CORRECTION_DB` (MAN-102), rounded to a whole dB. Measured at `acbc00f`:
`snr_db = 2.5` prints `9 dB` and `snr_db = 2.9` prints `10 dB`, so `snr 10` withholds the first
and passes the second.

## Wire examples

`> ` marks what the client types. Comments in `( )` are not on the wire.

```
de W3XYZ-# >
> set/dx/filter cq
Filter set: cq
> SKIMMER/SETT
SETT: vlNormal CQ 14000.0-14088.0
      (bus publishes K2DEE 40 m DE 12 dB: withheld from this client)
      (bus publishes K1CQQ 20 m CQ 12 dB: delivered)
DX de W3XYZ-#:  14027.10  K1CQQ          CW    12 dB  28 WPM  CQ      2213Z
> set/dx/filter band 40m,20m snr 10
Filter set: band 40m,20m snr 10
      (bus publishes K3LOW 40 m CQ, row would read 9 dB: withheld)
      (bus publishes K5HIG 15 m CQ 30 dB: withheld, wrong band)
      (bus publishes K4EDG 40 m CQ, row reads 10 dB: delivered)
DX de W3XYZ-#:   7031.00  K4EDG          CW    10 dB  28 WPM  CQ      2213Z
> sh/dx
DX de W3XYZ-#:   7027.00  K2DEE          CW    12 dB  28 WPM  DE      2213Z
DX de W3XYZ-#:  14027.10  K1CQQ          CW    12 dB  28 WPM  CQ      2213Z
DX de W3XYZ-#:   7031.00  K4EDG          CW    10 dB  28 WPM  CQ      2213Z
> SKIMMER/SETT
SETT: vlNormal 14000.0-14088.0
> set/dx/filter band 40m, 20m
Unknown command
> set/dx/filter off
Filter set: off
```

A second client that sent no filter receives all five spots, and its `SETT` reply has no `CQ`
token.

## JSON stream

`:7301` (JSON Lines and WebSocket) stays unfiltered:

- Raw JSON Lines has no request channel to carry a filter.
- A WebSocket query-string filter would make `seq` non-contiguous for that client, so it could
  not tell filtering from loss, and resume replay (MAN-137) would need the same filter.
- The JSON spot record has no spot-type field, so a CQ filter there would act on a field
  consumers cannot see.

Follow-up: the JSON spot stream should say whether a spot is CQ. Add a spot-type field to
`spots.v1` (a dispensa contract change) first, then revisit a WebSocket-only filter with a `seq`
rule.

## Out of scope

- No filter on the JSON Lines or WebSocket stream, the live-state stream, the RBN uplink, the
  WSJT-X outputs or PSKReporter.
- No config key for a server-wide default filter; the station profile's `cq_only` stays false.
- No `sh/filter` display command and no `set/nocq` alias; both stay `Unknown command`.
- No `unique > n` combined with clauses on one line, no `snr >= n` spelling, no `dB` suffix, no
  space after a comma in a band list.
- No persistence: a filter ends with its connection.

## Sources

- Ticket MAN-546.
- `docs/DECISIONS/2026-10-10-man92-telnet-commands.md` (grammar strictness, `sh/dx` order,
  uncounted non-matches).
- `docs/DECISIONS/2026-09-07-man86-aggregator-sett-handshake.md` (SETT wire format, `CQ` token).
- `docs/DECISIONS/2026-10-10-man91-uplink-spot-type-filter.md` (uplink `cq_beacon` default).
- `docs/DECISIONS/2026-09-07-man102-snr-reference-and-estimator.md` (500 Hz versus 2500 Hz).
- `docs/DECISIONS/2026-10-14-man137-json-stream-resume.md` (`seq` and resume).
- AR-Cluster's bare-`set/dx/filter` clear form:
  [dxcluster.info, Telnet Filters](https://dxcluster.info/filters.htm).
- Tests: `parses_every_spot_filter_form` and
  `malformed_spot_filters_are_unknown_not_a_partial_match` (`command.rs`);
  `client_filter.rs`'s unit tests; `connection_filters_admit_live_spots_and_count_each_rejection_once`
  and `the_spot_filter_selects_before_count_truncation` (`telnet.rs`);
  `wire_snr_db_is_the_figure_the_row_prints` (`rbn.rs`); and in
  `crates/manta-server/tests/telnet_acceptance.rs`:
  `set_dx_filter_cq_withholds_non_cq_spots_and_sett_reports_cq`, `filters_are_per_connection`,
  `band_and_snr_filter_passes_only_matching_live_spots`, `sh_dx_honours_the_connection_filter`,
  `set_dx_filter_off_and_the_bare_form_clear_every_filter_including_unique`,
  `a_malformed_filter_command_leaves_the_current_filter_in_place`,
  `the_shutdown_drain_applies_the_client_filter` and
  `unique_and_the_spot_filter_compose_and_each_withheld_spot_counts_once`.
