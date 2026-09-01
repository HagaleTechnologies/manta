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
