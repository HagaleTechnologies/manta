#!/usr/bin/env bash
# Summarize one `manta listen --json` capture into the same shape as
# `manta doctor`'s report (chars/distinct-chars/tracks/SNR stats), plus
# every confirmed Spot's full content -- something `doctor` deliberately
# doesn't expose (crates/manta-engine/src/doctor.rs discards the spot in
# its callback, `|_spot| spots_confirmed += 1`, and only counts it).
#
# Use this after a live-hardware field-test capture (see
# wiki/pages/live-hardware-field-testing.md) to see what `doctor` can only
# tell you happened, not what it was.
set -euo pipefail

command -v jq >/dev/null 2>&1 || {
  echo "ERROR: jq is required (install it before running this script)." >&2
  exit 2
}

if [ $# -ne 1 ]; then
    echo "usage: $0 <listen --json output file>" >&2
    exit 1
fi
file="$1"

echo "chars_decoded:   $(grep -c '"event":"CharDecoded"' "$file" || true)"
echo "distinct_chars:  $(jq -c 'select(.event == "CharDecoded") | .glyph.Char // empty' "$file" | sort -u | wc -l | tr -d ' ')"
echo "tracks_promoted: $(grep -c '"event":"TrackPromoted"' "$file" || true)"
echo "tracks_closed:   $(grep -c '"event":"TrackClosed"' "$file" || true)"
echo "track_meta_count: $(grep -c '"event":"TrackMeta"' "$file" || true)"

# Bounded-memory median: stream the raw file line-by-line (jq -c, never
# -s/--slurp) so the dominant CharDecoded/WordBoundary/TrackMeta volume of
# an overnight capture (can run into the tens of millions of events near
# manta's 500-track cap at a 1 Hz TrackMeta cadence) never gets
# materialized as one in-memory array. The extracted SNR floats still need
# an exact sort for an exact median, so hand that off to `sort -n`
# (disk-backed once its input exceeds its in-memory buffer, on both GNU
# and BSD sort) rather than `jq -s | sort`, which would hold the whole
# array in one process's heap (Codex review, PR #153, round 2).
snr_file=$(mktemp)
trap 'rm -f "$snr_file" "${sorted_file:-}"' EXIT
jq -c 'select(has("event") and .event == "TrackMeta") | .snr_2500_db' "$file" > "$snr_file"
n=$(wc -l < "$snr_file" | tr -d ' ')
if [ "$n" -eq 0 ]; then
    echo "snr_2500_db: no TrackMeta events"
else
    sorted_file=$(mktemp)
    sort -n "$snr_file" > "$sorted_file"
    min=$(head -n 1 "$sorted_file")
    max=$(tail -n 1 "$sorted_file")
    mid=$((n / 2))
    if [ $((n % 2)) -eq 0 ]; then
        v1=$(sed -n "${mid}p" "$sorted_file")
        v2=$(sed -n "$((mid + 1))p" "$sorted_file")
        median=$(awk -v a="$v1" -v b="$v2" 'BEGIN { print (a + b) / 2 }')
    else
        median=$(sed -n "$((mid + 1))p" "$sorted_file")
    fi
    echo "snr_2500_db: min=${min} median=${median} max=${max}"
fi

echo
echo "confirmed spots:"
jq -c 'select(has("spot")) | .spot' "$file" | jq -r '
  # Round to the nearest Hz, not kHz -- the channelizer spacing is 93.75 Hz,
  # so whole-kHz rounding collapses ~10 distinct channel positions into one
  # displayed value and hides exactly the clustering/dial-shift movement
  # this script exists to help spot (Codex review, PR #153).
  "  \(.callsign)\tfreq=\((.freq_hz | round) / 1000)kHz\tsnr_db=\(.snr_db | round)\tconfidence=\(.confidence)\twpm=\(.wpm | round)\ttype=\(.spot_type)"
'
