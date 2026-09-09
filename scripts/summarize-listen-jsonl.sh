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

# Streams the raw file line-by-line (jq -c, not -s/--slurp) so an overnight
# capture's dominant volume -- CharDecoded/WordBoundary/TrackMeta can run
# into the tens of millions of events over many hours -- never gets
# materialized as one big array (Codex review, PR #153). Only the small
# per-track SNR floats extracted here get slurped for the min/median/max
# below; an exact median inherently needs the full value set, but that set
# is orders of magnitude smaller than the raw event stream.
jq -c 'select(has("event") and .event == "TrackMeta") | .snr_2500_db' "$file" | jq -s -r '
  sort as $s
  | ($s | length) as $n
  | if $n == 0 then
      "snr_2500_db: no TrackMeta events"
    else
      # Matches doctor.rs::median exactly: average the two middle values on
      # an even count, not just the upper-middle one (Codex review, PR #153).
      (if $n % 2 == 0 then ($s[$n/2 - 1] + $s[$n/2]) / 2 else $s[($n-1)/2] end) as $median
      | "snr_2500_db: min=\($s[0]) median=\($median) max=\($s[$n - 1])"
    end
'

echo
echo "confirmed spots:"
jq -c 'select(has("spot")) | .spot' "$file" | jq -r '
  # Round to the nearest Hz, not kHz -- the channelizer spacing is 93.75 Hz,
  # so whole-kHz rounding collapses ~10 distinct channel positions into one
  # displayed value and hides exactly the clustering/dial-shift movement
  # this script exists to help spot (Codex review, PR #153).
  "  \(.callsign)\tfreq=\((.freq_hz | round) / 1000)kHz\tsnr_db=\(.snr_db | round)\tconfidence=\(.confidence)\twpm=\(.wpm | round)\ttype=\(.spot_type)"
'
