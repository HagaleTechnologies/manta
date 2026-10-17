# MAN-79: operator-supplied country and known-callsign tables

Operators can update `cty.dat` and `MASTER.SCP` without rebuilding manta.
The bundled files remain the defaults. A country prefix absent from the
selected table fails validation unless the operator allowlists the call.
SCP membership raises confidence and exempts a call from confusable-variant
arbitration; absence alone never rejects a call.

## Decisions

1. **Flags.** `--cty <PATH>` and `--scp <PATH>` belong to the shared filtering
   options on `decode`, `run`, `listen`, `soak` and `doctor`. Relative flag
   paths resolve against the working directory.
2. **Config and environment.** `[spot] cty_path` and `scp_path` resolve
   against the config file's directory. `MANTA_SPOT_CTY_PATH` and
   `MANTA_SPOT_SCP_PATH` are string values with paths relative to the working
   directory. Precedence is CLI, environment, file, bundled. `decode`
   reads the file and flags, but never the environment.
3. **Independent selection.** Each override replaces only its own table.
   Leaving one unset uses that table's bundled copy.
4. **Read once, fail before I/O.** The CLI reads UTF-8, strips a leading BOM,
   parses each override, and rejects an empty result before opening the
   source or binding listeners. Missing or unreadable paths fail with
   `reading cty.dat file <path>` or `reading master.scp file <path>`.
   An entry-less country table names AD1C's `cty.dat` and warns against
   accidentally downloading `cty.csv` or a web page. A failed override
   never falls back silently. `config check` runs the same validation.
5. **Share the parsed table.** `PipelineConfig` holds optional `Arc` tables.
   Its single validator builder serves replay, live and soak paths.
   The daemon passes the same country-table `Arc` to JSON geography
   lookup, so new prefixes receive the selected table's zones and
   coordinates as well as passing validation.
6. **DXCC stays bundled.** `dxcc.tsv` has no override. An entity found only
   in a supplied country table can have geography but no known ADIF number.
   The existing unknown-DXCC sentinel and unresolved-geography counter
   still describe that case. Adding an alias to an existing entity can
   resolve its ADIF number through the bundled join.
7. **Age.** `CTY_DAT_RETRIEVED` records midnight UTC on the retrieval date
   in `data/SOURCES.md`; a test pins the two together. The fixed threshold
   is 180 whole days. At day 180 no warning appears; day 181 warns. A clock
   before retrieval produces no age, accommodating a Pi before NTP sync.
8. **Warning placement.** `prepare_live` prints one stderr line naming the
   age, retrieval date, download URL and override options. `run`, `listen`,
   `soak`, `doctor` and `config check` share this path. `decode` and `oracle`
   never read the clock for this warning. The plain stderr sink works
   before the tracing subscriber starts and keeps stdout deterministic.
9. **Tests survive time.** Unit tests inject time relative to the retrieval
   constant. The binary warning test compares times before and after the
   child runs. The banner-first test supplies `--cty` to suppress the
   warning regardless of date. A scratch-copy test run changes the date
   and provenance together to exercise stale-table startup.
10. **Inspection.** `config check` appends `cty_path` and `scp_path` to its
    `spot:` line. Unset values read `bundled`, because the table still exists.
11. **Warning scope.** Only the bundled country table receives an age
    warning. Override files and SCP do not. The country table can reject
    new prefixes outright, which is the ticket's second scenario.
12. **Restart and MAN-78.** Neither table reloads while manta runs. This
    base has no `restart_only_changes` function. MAN-78 must classify
    `spot.cty_path` and `spot.scp_path` as restart-only, even if it otherwise
    excludes `spot.*` from restart warnings. This coordination requirement
    must accompany the runner-created PR.

The review defaults are adopted: 180 days is fixed, a broken override is
fatal, and deterministic replay prints no age warning. No downloader,
scheduled refresh, new dependency or bundled-data refresh is included.

## Evidence and tests

The test fixture transmits `CQ CQ DE QQ9ZZZ QQ9ZZZ K` for 30 seconds.
The bundled country table yields zero spots. Appending a `QQ9` entity to
an operator file yields exactly one `QQ9ZZZ` spot through `--cty`, a
config-relative `cty_path`, or the live command's environment variable.
`decode_cty_flag_lets_a_newly_allocated_prefix_spot` and its config/env
companions exercise the actual binary. `from_tables_applies_the_supplied_scp_set`
proves the supplied SCP changes the emitted confidence.

A `cty.csv` row parses to an empty table in the existing tolerant parser.
`a_cty_file_with_no_prefixes_is_rejected_before_source_io` verifies that
startup rejects this mistake before touching a nonexistent WAV. Unit
coverage also checks missing and non-UTF-8 files, BOM handling, shared
`Arc` identity, and JSON geography lookup.

## Checkout coordination

The authoritative plan was researched on `99c70e4`, after MAN-268's
packaging work. This phase is pinned to `13131ca`, where `packaging/`,
`manta.example.toml`, `packaging_examples.rs` and `test_packaging.py` do
not exist. Operator download, validation, restart, service-account and
container-mount guidance therefore lives in the existing README. The
config-init scaffold contains both keys. When MAN-268 is integrated,
copy that scaffold block into its example config and name both paths in
its absolute-path and mount guidance. No packaging implementation is
recreated here.
