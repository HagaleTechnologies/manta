# MAN-542: scheduled refresh of the callsign-pattern list

Date: 2026-11-09. Base: `7020ede76042fe0ec9c1704f3780840df1ed5b11`.

manta decides how many repetitions a call needs (2, 3 or 5 at
`validation_level = "normal"`, MAN-95) from a callsign-pattern list. The
built-in list, `crates/manta-spot/data/patterns.lst` (6,036 patterns, 4,018
common), was derived on 2026-10-14 from the 2026-07-24 MASTER.SCP. MAN-316's
`refresh_lists` keeps `cty.dat` and `MASTER.SCP` current while `manta run`
reads a live receiver, but it never touched the pattern list. A node could
run a current SCP with a stale pattern list, and a call whose pattern is
missing needs 5 repetitions instead of 2 or 3.

With `[spot] refresh_lists = true` and the new `[spot] pattern_url` set,
`manta run` from a live receiver now checks the pattern list every 7 days,
with the same conditional GET, fetch limits, cache and retry rules as
`cty.dat`. A download is used only after it parses and passes the
acceptance checks, and the validator takes it from the next decoder event.
`manta_list_refresh_total{list="patterns",outcome}` counts each attempt.
With `pattern_url` unset nothing is fetched and the pattern list is the one
`pattern_path` or the built-in copy gives, as before.

## Context

The ticket came from the 2026-11-07 features review. The capability matrix
had marked Aggregator's `Patt3Ch.lst` sync a non-goal because manta had "no
bespoke sync client"; MAN-316 since built one, and its record never
mentions the pattern list. MAN-95 deferred automatic refresh (its decision
11).

The ticket's premises, corrected:

1. "`pattern_path` is read once at start and on SIGHUP" is wrong. It is
   restart-only: `is_reloadable` (`crates/manta-cli/src/reload.rs`) allows
   four keys, and `pattern_settings_need_a_restart` passes. Nothing here
   makes `pattern_path` or `pattern_url` reloadable.
2. The ticket's `docs/CONFIG.md` line numbers had moved; the `[spot]` rows
   are found by key.
3. The ticket says the Skimmer grammar "differs". The line syntax is the
   same (`[+]PATTERN`, `@` and `#` wildcards, spaces after `+`). The
   difference is content: 7% of RBN's lines describe patterns manta's rule
   cannot produce, and manta's strict parser fails the whole file on the
   first one (line 67, `3E5##@@@`).

## Decisions

1. **D1. A third `ListKind::Patterns`, on the existing `manta-lists`
   thread.** `name()` is `"patterns"` (cache directory and metric label),
   `extension()` is `"lst"`, `interval()` is 7 days (the RBN file last
   changed 140 days before this record). `file_name()` is renamed
   `operator_name()` and returns `"cty.dat"`, `"MASTER.SCP"` and `"the
   pattern list"`.
2. **D2. `[spot] pattern_url`: no default, `http://` only.** Validated by
   `check_list_url` like `cty_url`, overlaid by `MANTA_SPOT_PATTERN_URL`,
   restart-only, no CLI flag. MAN-316 made every download opt-in, the
   ticket's second scenario needs "no URL, nothing fetched", and the RBN
   file's licence is unstated (SOURCES.md calls Skimmer's `Patt3Ch.lst`
   licensed; no licence text is published at the URL). The RBN URL appears
   in the docs only as an example.
3. **D3. The pattern list is managed only when `pattern_url` is set and
   `pattern_path` is unset.** `pattern_path` is an operator choice, as
   `cty_path` is (MAN-316 D3). Startup precedence: `pattern_path`, then the
   cache (only when managed), then the built-in copy. There is no pattern
   pin.
4. **D4. Two parsers.** `pattern::Table::parse` stays strict and unchanged
   for `pattern_path`/`--patterns` (MAN-95 decision 9: a silent omission in
   an operator file would change admission thresholds). A new
   `pattern::Table::parse_download` serves downloads and cache loads only.
   Per line it strips one leading BOM from the text, trims, skips blank and
   `#` lines, strips an optional `+` and the spaces after it, and
   ASCII-uppercases. A remainder that is empty or holds anything outside
   `[A-Z0-9@#]` fails the whole parse with its 1-based line number. A
   remainder the strict rule accepts is kept, `+` winning duplicates.
   Anything else is counted in `skipped_lines` and dropped. An empty result
   is `ParseError::Empty`.
5. **D5. Skip, do not expand.** The skipped lines are unreachable by
   construction: an over-length pattern fails the grammar check before any
   tier lookup, and a non-canonical one never equals a key `pattern_of`
   produces. See "Known divergence".
6. **D6. `accept_patterns(text, bundled)` in `manta_spot::reference`,**
   applied to every download and every startup cache load, in this order:
   BOM strip; `parse_download` (syntax: `line L is "<text>", not a
   pattern`, the text cut at 60 characters with control characters shown
   as `?`; empty: `it lists no patterns`); canaries (each `CTY_CANARIES`
   call's pattern must be listed or common: `it does not list <CALL>'s
   pattern <PATTERN> (<entity>), so it is truncated or not a
   callsign-pattern list`); floor (at least half the built-in table's
   patterns: `it lists N patterns, under half the built-in copy's 6036`).
   Transport rules are MAN-316's: `FetchLimits::default()`, no redirects,
   no chunked bodies, swap only after acceptance, retry in an hour.
7. **D7. Hot swap through a third `Live<pattern::Table>`.**
   `ReferenceLists::new` takes three tables, so every caller states its
   pattern table and none silently gets the built-in one. Production
   callers pass `cfg.pattern_table()` or the cache's accepted table.
   `ReferenceWatch` gains `seen_patterns` and calls the new
   `Validator::replace_patterns`; `replace_reference_tables` keeps its
   signature.
8. **D8. Observability.** `manta_list_info{list="patterns"}` on every
   `[server]` daemon; `manta_list_refresh_total{list="patterns",outcome}`
   and `manta_list_last_success_timestamp_seconds{list="patterns"}` only
   when the list is managed. The `lists:` line and the file-replay note
   always name the pattern list; the `list refresh: on` line names it only
   when managed. The `updated` line reports `N patterns, M unmatchable
   lines skipped` (the clause is omitted when none were skipped).
9. **D9. Out of scope, with follow-ups:** a `pattern_sha256` pin and replay
   with a downloaded list; deriving the pattern list in-process from a
   refreshed MASTER.SCP; HTTPS, redirects and configurable intervals
   (MAN-316's follow-ups).
10. **D10. `PATTERNS_LST_GENERATED = "2026-10-14"` in
    `manta_spot::vintage`,** pinned to SOURCES.md's `## patterns.lst`
    "Retrieved: generated YYYY-MM-DD" line by a test, and returned by
    `bundled_retrieved_at(Patterns)`. A cached download made on or before
    the built-in list's generation date loses at startup, as for cty and
    scp.
11. **D11. The cache supplies the pattern list at startup only when it is
    managed.** An operator who removes `pattern_url` gets the built-in list
    (or `pattern_path`) back at the next start, even though `patterns/`
    still holds a version. `resolve_startup` takes the list of kinds the
    cache may supply instead of a `refresh_on` flag.
12. **D12. `pattern_url=` in the `config check` summary only when set,**
    appended after `calls_heard_path` (MAN-269's shown-only-when-set rule),
    so a config without the key prints byte-identical output.
    `download_endpoints` joins three or more hosts as `A, B and C`; two
    hosts print as before.
13. **D13. The `pattern_path` left-alone note only when `pattern_url` is
    set,** both at `run` startup (`note: pattern_path is set, so list
    refresh leaves the pattern list alone`) and in `config check`. Without
    a URL there is no refresh to skip.

### Operator surface

- One new `[spot]` key, `pattern_url`. See
  [CONFIG.md](../CONFIG.md#spot); the scaffold and `manta.example.toml`
  show the RBN URL commented out.
- The cache gains `<list_cache_dir>/patterns/<sha256>.lst` and
  `patterns/state.json` on nodes that set `pattern_url`, at most 30
  versions of about 100 KB.
- An opted-in node opens outbound TCP port 80 to the `pattern_url` host
  (`data.reversebeacon.net` for the RBN file). `packaging/README.md` names
  it in the firewall bullet. The systemd unit and Compose file need no
  change.
- `docs/METRICS.md` names `patterns` as a third `list` label; no new
  metric family.

## Measurements

All on 2026-11-09 in the research and planning containers, checkout
`7020ede`.

- `http://data.reversebeacon.net/downloads/patt3ch/patt3ch.lst`:
  `HTTP/1.1 200 OK`, 101,682 bytes, `Last-Modified: Mon, 22 Jun 2026
  13:50:20 GMT`, no redirect, no chunked encoding, SHA-256
  `a2ec8da7f8f6066a8abc9e1667ea6fd97bb33ec01f09770522b41056a2a668aa`. With
  `If-Modified-Since: <that date>` it returns `304`. MAN-316's
  `list_http::get` fetched it unchanged.
- The file: CRLF, no BOM, no comments, 10,956 lines, lines `+ PATTERN` or
  two spaces then `PATTERN`, one lowercase line (`+ 8n6@@`).
- Strict `Table::parse` on it: `Err` at line 67 (`3E5##@@@`).
- The download-mode parse on it: 10,192 lines kept, 764 skipped (578
  longer than 7 characters and 202 not in canonical shape, 16 of them
  both), 0 malformed. Result: 10,190 distinct patterns, 5,650 common. The floor is
  3,018. All eight canary calls' patterns are common in both lists.
- Over the 49,950 grammar-plausible calls of MASTER.SCP release
  2026.11.06: the stale built-in list leaves 169 (0.34%) unlisted (5
  repetitions); a list derived from that SCP leaves 0; the RBN list leaves
  1,601 (3.21%). Of the patterns in both lists, 1,612 differ in common
  status. Setting `pattern_url` to the RBN file is a different policy, not
  a pure loosening: it lowers the bar for many calls heard on the air and
  raises it for about 3% of SCP-only calls whose patterns RBN has not
  listed. No spot-weighted measurement exists.

## Threat note

Downloads are plain HTTP, the same accepted risk as MAN-316. An on-path
attacker can substitute a pattern list that passes acceptance. That can
move tiers between 2 and 5 repetitions, never below the 2 that
`validation_level = "minimal"` already gives, and it cannot create a spot
with no signal behind it. The canary and floor checks (D6) limit a
truncated or hostile file. An operator who wants fixed bytes sets
`pattern_path`.

## Known divergence

RBN's file holds 202 lines in a shape manta's `pattern_of` never produces,
mostly a `#` at index 2 where manta keeps a literal (`A2#@`, `A4##@@`); 16
of them are also over-length. Expanding a `#` at index 2 into ten literal
digits would change 172 SCP calls' tiers,
resting on an unverified reading of how CW Skimmer matches `#` in that
position (its matcher is closed source). manta skips those lines instead
(D5) and counts them in the `updated` line, so a call manta admits never
gets a different tier from a line manta cannot interpret.

## Supersedes

- [Legacy capability matrix](2026-09-01-legacy-capability-matrix.md)'s
  `Patt3Ch.lst` sync row: the non-goal is now covered by MAN-542.
- [MAN-95](2026-10-14-man95-pattern-rarity-repetition-tiers.md) decision 2,
  for the refresh part only: "an operator's `--scp` replacement does not
  change the pattern table" stays true, but a `pattern_url` download now
  can.
- MAN-95 decision 11, for automatic refresh only. Staleness warnings for
  the pattern list are still not built.

## Not done here

- No change to `Table::parse`, to `pattern_path`/`--patterns` behaviour or
  error text, or to SIGHUP (`pattern_path` and `pattern_url` stay
  restart-only).
- No pin (`pattern_sha256`, `--pattern-sha256`), so a recording cannot yet
  be replayed with a downloaded pattern list.
- No in-process `derive_list` from a refreshed SCP.
- No default URL, no HTTPS, no redirects, no configurable interval, no
  SIGHUP-triggered refresh.
- No refresh from `soak`, `doctor`, `check`, `calibrate`, `decode` or file
  replay.
- No change to the bundled `patterns.lst`, to MAN-95's tier rules or
  thresholds, or to the JSON spot schema.
- The inherited "changed URL waits for the old schedule" behaviour of
  `Refresher::new` is unchanged.

## Follow-ups

1. An operator replaying a recording should be able to pin the pattern
   list a daemon downloaded so that replay uses the same repetition tiers.
   `[spot] pattern_sha256` / `--pattern-sha256` resolved like
   `cty_sha256`, reading the cache through `parse_download`.
2. manta should regenerate its pattern list from a refreshed MASTER.SCP
   when no `pattern_url` is set so that the default list never trails the
   SCP in use. In-process `derive_list`, no URL and no licence question.
3. A changed list URL should be fetched at the next start so that a new
   mirror takes effect without waiting for the old schedule.
   `Refresher::new` should treat a `state.url` that differs from the
   configured URL as due (applies to all three lists).
