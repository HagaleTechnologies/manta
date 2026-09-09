#!/usr/bin/env bash
# Summarize one `manta listen --json` capture into the same shape as
# `manta doctor`'s report (chars/tracks/SNR stats), plus every confirmed
# Spot's full content -- something `doctor` deliberately doesn't expose
# (crates/manta-engine/src/doctor.rs discards the spot in its callback,
# `|_spot| spots_confirmed += 1`, and only counts it).
#
# Use this after a live-hardware field-test capture (see
# wiki/pages/live-hardware-field-testing.md) to see what `doctor` can only
# tell you happened, not what it was.
set -euo pipefail

if [ $# -ne 1 ]; then
    echo "usage: $0 <listen --json output file>" >&2
    exit 1
fi
file="$1"

echo "chars_decoded:   $(grep -c '"event":"CharDecoded"' "$file" || true)"
echo "tracks_promoted: $(grep -c '"event":"TrackPromoted"' "$file" || true)"
echo "tracks_closed:   $(grep -c '"event":"TrackClosed"' "$file" || true)"

jq -s '
  map(select(has("event") and .event == "TrackMeta") | .snr_2500_db) as $snrs
  | if ($snrs | length) > 0 then
      "snr_2500_db: min=\($snrs | min | tostring) median=\($snrs | sort | .[length/2 | floor] | tostring) max=\($snrs | max | tostring)"
    else
      "snr_2500_db: no TrackMeta events"
    end
' "$file" -r

echo
echo "confirmed spots:"
jq -s 'map(select(has("spot")) | .spot)' "$file" | \
    jq -r '.[] | "  \(.callsign)\tfreq=\(.freq_hz/1000 | round)kHz\tsnr_db=\(.snr_db | round)\tconfidence=\(.confidence)\twpm=\(.wpm | round)\ttype=\(.spot_type)"'
