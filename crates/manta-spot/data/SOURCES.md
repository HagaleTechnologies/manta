# Vendored data sources

## cty.dat

- Source: https://www.country-files.com/cty/cty.dat (AD1C's "big CTY" file)
- Retrieved: 2026-07-25
- Format: AD1C `cty.dat` -- see https://www.country-files.com/cty-dat-format/
- License/redistribution: freely distributed for use in amateur radio
  contest/logging software -- the convention every major contest logger
  (N1MM+, Win-Test, CQRLOG, TR4W) follows. No separate license file is
  published upstream. Flagged here for visibility, not treated as a
  blocker; revisit if this ever needs a stricter provenance trail.
- Refresh: re-run the `curl` in this crate's implementation plan (Task 1)
  and replace this file by hand -- no refresh automation yet.

## dxcc.tsv

- Source: derived from https://www.country-files.com/bigcty/cty.csv (AD1C),
  columns 1 (primary prefix) and 3 (ADIF DXCC entity code).
- Retrieved: 2026-09-07
- Format: `<primary-prefix>\t<adif-dxcc-entity-number>\t<name, comment only>`;
  `#`-prefixed and blank lines are ignored.
- License/redistribution: same AD1C source and same convention as cty.dat.
- Refresh: `scripts/gen-dxcc-table.sh` (no argument = fetch upstream). Refresh
  it whenever cty.dat is refreshed, ideally from the same AD1C release;
  `cty::tests::every_entity_in_the_vendored_cty_dat_resolves_an_adif_dxcc_number`
  fails if the two drift apart, and at runtime any spot emitted while they
  have drifted is counted by `manta_spots_unresolved_geography_total`. The
  script's `awk` splitter is not CSV-aware, so it requires every row to have
  exactly 10 comma-separated fields and aborts (writing nothing) if upstream
  ever ships an unescaped comma inside a field -- better a loud failure than a
  row whose ADIF number silently shifted a column.
- Why keyed on primary prefix, not entity name: measured 2026-09-07 against a
  cty.dat retrieved 2026-07-25 and a cty.csv published 2026-09-06 -- the prefix
  join matched 346/346, the name join 343/346 (Cape Verde -> Cabo Verde, Juan
  de Nova, Europa -> Juan de Nova & Europa, Tristan da Cunha & Gough -> ...
  Gough Islands). ADIF 3.1.7's own release notes likewise renamed entities
  207/462/468/502/518.

## master.scp

- Source: https://www.supercheckpartial.com/MASTER.SCP
- Retrieved: 2026-07-25
- Upstream release: per the file's own header comment (`# Release ...`)
- Format: one callsign per line; `#`/`!!`-prefixed lines are comments/headers.
- License/redistribution: same convention as cty.dat -- bundled by contest
  logging software as a matter of course; no separate license published
  upstream. Same flag-not-block note applies.
- Refresh: re-run the `curl` in this crate's implementation plan (Task 1)
  and replace this file by hand -- no refresh automation yet.
