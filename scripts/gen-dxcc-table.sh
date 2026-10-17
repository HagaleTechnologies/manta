#!/usr/bin/env bash
# Regenerates crates/manta-spot/data/dxcc.tsv from AD1C's cty.csv, whose
# third column is the ADIF DXCC entity code for that entity. Key is column 1,
# the primary prefix -- the SAME key cty.dat's header field 8 carries, and
# empirically stable across releases where entity NAMES are not (a 6-week
# version gap already renamed 3 of 346 entities; 0 prefixes changed).
# See crates/manta-spot/data/SOURCES.md.
set -euo pipefail
SRC="${1:-https://www.country-files.com/bigcty/cty.csv}"
OUT="$(dirname "$0")/../crates/manta-spot/data/dxcc.tsv"
TMP="$(mktemp)"
# Staged next to $OUT and moved into place only on success: `awk` below can
# now exit non-zero mid-pipeline, and writing straight to $OUT would leave a
# truncated table behind when it does.
OUT_TMP="$(mktemp "$(dirname "$OUT")/.dxcc.tsv.XXXXXX")"
trap 'rm -f "$TMP" "$OUT_TMP"' EXIT
if [[ "$SRC" == http* ]]; then curl -fsSL "$SRC" -o "$TMP"; else cp "$SRC" "$TMP"; fi
{
  echo "# ADIF DXCC entity number per cty.dat primary prefix (AD1C cty.csv col 1 -> col 3)."
  echo "# Regenerate with scripts/gen-dxcc-table.sh; see data/SOURCES.md."
  printf '# <primary-prefix>\t<adif-dxcc-entity-number>\t<entity name, comment only>\n'
  # `awk -F,` is NOT CSV-aware: a quoted entity name containing a comma (AD1C
  # shipped "Juan de Nova, Europa" until recently) would shift $3 off the ADIF
  # number while a permissive NF>=10 still admitted the row -- the parser then
  # drops it silently and that entity emits dxDxcc: -1. cty.csv rows are
  # exactly 10 fields, so require exactly 10 and fail the whole run loudly
  # otherwise, rather than emitting a row nobody will notice is wrong.
  awk -F, '
    /^[[:space:]]*$/ { next }
    NF != 10 {
      printf "gen-dxcc-table.sh: line %d has %d comma-separated fields, expected 10 (unescaped comma in a field?): %s\n", NR, NF, $0 > "/dev/stderr"
      malformed = 1
      next
    }
    { p = $1; sub(/^\*/, "", p); printf "%s\t%s\t%s\n", toupper(p), $3, $2 }
    END { if (malformed) exit 1 }
  ' "$TMP" \
    | LC_ALL=C sort -u
} > "$OUT_TMP"
chmod 644 "$OUT_TMP"
mv "$OUT_TMP" "$OUT"
echo "wrote $OUT ($(grep -cv '^#' "$OUT") entities)"
