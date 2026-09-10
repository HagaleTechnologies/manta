#!/usr/bin/env python3
"""Score a `manta decode --json` report against a Reverse Beacon Network
(RBN) ground-truth window for the same recording.

The RBN truth CSV is RBN's own raw daily-dump format (see
`data.reversebeacon.net/rbn_history/<YYYYMMDD>.zip`), pre-filtered to the
recording's time/frequency window -- e.g.
`crates/manta-testkit/audio-corpus/ground-truth/B2_20251129_000000_7080kHz.rbn.csv`.

Matches manta spots to RBN truth by callsign + frequency (within
--freq-tol-hz, checked against the nearest kHz bins) AND time (within
--time-tol-s of some real RBN observation of that call+freq -- not just
"anywhere in the whole recording"): without a time bound, a station that
moves frequency mid-recording lets a later, unrelated garbled decode of
its old callsign on its old (now-vacated) frequency count as a true
positive against an RBN observation from minutes earlier, inflating both
precision and recall (Codex review, PR #144). --time-tol-s defaults to
90s, matching RepetitionGate's own repetition-confirmation window
(crates/manta-spot/src/gate.rs) as a reasoned "same episode" scale, not an
arbitrary pick.

Truth entries whose callsign manta's own grammar would structurally
reject (crates/manta-spot/src/grammar.rs -- e.g. prefix-portable calls
like "EA8/TF3CW", which manta's `is_valid_portable` doesn't recognize as
a portable suffix and so rejects the whole call) are excluded from the
denominator, not counted as false negatives: manta cannot spot these
regardless of decode quality, so counting them as misses would
misrepresent what "better decode accuracy" could achieve, not just
under-count recall (same review). `_is_plausible_callsign` below is a
Python port of that Rust function -- keep it in sync if grammar.rs
changes.

Usage:
  score-against-rbn.py <decode_report.json> <rbn_truth_csv> \
      --capture-start 2025-11-29T00:00:00Z --sample-rate-hz 192000
"""
import sys
import json
import csv
import argparse
from datetime import datetime, timedelta, timezone


def _is_valid_portable(p):
    return p in ("P", "QRP", "MM", "AM", "M") or (len(p) == 1 and p.isdigit())


def _is_valid_base(base):
    if not (3 <= len(base) <= 7):
        return False
    if not base.isalnum() or not base.isascii():
        return False
    has_digit = any(c.isdigit() for c in base)
    has_letter = any(c.isalpha() for c in base)
    return has_digit and has_letter and base[-1].isalpha()


def _is_plausible_callsign(call):
    """Port of crates/manta-spot/src/grammar.rs::is_plausible."""
    if "/" in call:
        base, portable = call.split("/", 1)
        if not _is_valid_portable(portable):
            return False
    else:
        base = call
    return _is_valid_base(base)


def load_manta_spots(path, capture_start, sample_rate_hz):
    with open(path) as f:
        report = json.load(f)
    out = []
    for s in report["spots"]:
        t = capture_start + timedelta(seconds=s["sample_ts"] / sample_rate_hz)
        out.append({
            "callsign": s["callsign"].upper(),
            "freq_hz": s["freq_hz"],
            "time": t,
            "snr_db": s["snr_db"],
            "confidence": s["confidence"],
            "spot_type": s["spot_type"],
        })
    return out


def load_rbn_truth(path, capture_start, spotters=None, min_spotters=1):
    """RBN daily-dump rows -> ([{callsign, freq_hz, time}], excluded_count).

    `capture_start` is accepted for call-site signature compatibility with the rest of
    this script (main's decode-report loading uses it); this function itself has never
    used it for anything -- truth-row timestamps come entirely from each row's own `date`
    column, not from the recording's start time.

    `spotters`, if given, restricts truth rows to that set of spotter callsigns (e.g.
    {"K5TR"} for the co-located reference spotter) before grammar exclusion or dedup --
    this makes `excluded` count only rows within the spotter-filtered scope, not the
    ones filtered out entirely. `min_spotters`, if > 1, additionally drops any call+kHz
    bin not corroborated by at least that many distinct (post-spotter-filter) spotters.
    """
    truth = []
    excluded = 0
    with open(path) as f:
        reader = csv.DictReader(f)
        for row in reader:
            spotter = row["callsign"].upper()
            if spotters and spotter not in spotters:
                continue
            call = row["dx"].upper()
            if not _is_plausible_callsign(call):
                excluded += 1
                continue
            ts = datetime.strptime(row["date"], "%Y-%m-%d %H:%M:%S").replace(tzinfo=timezone.utc)
            truth.append({
                "spotter": spotter,
                "callsign": call,
                "freq_hz": float(row["freq"]) * 1000.0,
                "time": ts,
            })
    if min_spotters > 1:
        heard = {}
        for t in truth:
            heard.setdefault((t["callsign"], round(t["freq_hz"] / 1000.0)), set()).add(t["spotter"])
        truth = [t for t in truth if len(heard[(t["callsign"], round(t["freq_hz"] / 1000.0))]) >= min_spotters]
    return truth, excluded


def dedup_truth(truth):
    seen = {}
    for t in truth:
        key = (t["callsign"], round(t["freq_hz"] / 1000.0))  # nearest kHz bin
        seen.setdefault(key, []).append(t)
    return seen


def match(manta_spots, truth_by_key, freq_tol_hz, time_tol_s):
    tp, fp = [], []
    matched_truth_keys = set()
    for m in manta_spots:
        best_key = None
        for df_khz in (0, -1, 1):
            key = (m["callsign"], round(m["freq_hz"] / 1000.0) + df_khz)
            candidates = truth_by_key.get(key)
            if not candidates:
                continue
            for t in candidates:
                if abs(t["freq_hz"] - m["freq_hz"]) > freq_tol_hz:
                    continue
                if abs((t["time"] - m["time"]).total_seconds()) > time_tol_s:
                    continue
                best_key = key
                break
            if best_key:
                break
        if best_key:
            tp.append(m)
            matched_truth_keys.add(best_key)
        else:
            fp.append(m)
    fn_keys = set(truth_by_key.keys()) - matched_truth_keys
    return tp, fp, fn_keys


def parse_iso(s):
    """Parses --capture-start. A timezone-naive value (no trailing Z or
    explicit offset) is assumed UTC, matching every other timestamp this
    script handles (RBN truth rows, sample_ts-derived spot times) --
    otherwise a naive value here would crash the time-tolerance matching
    in `match()` (Codex review, PR #144) the first time it's subtracted
    against an RBN truth row's timezone-aware UTC timestamp."""
    dt = datetime.fromisoformat(s.replace("Z", "+00:00"))
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt


def _finite(name, *, min_exclusive=None, min_inclusive=None):
    """argparse type for a numeric option: rejects non-finite (nan/inf)
    values, plus an optional lower bound (Codex review, PR #144). Without
    this, `x > nan` is always False in Python, so e.g. an un-validated
    `--time-tol-s nan` would silently disable the time bound entirely
    (any spot, arbitrarily far from a truth observation, would pass); a
    negative tolerance would silently reject even an exact match; and a
    zero or negative `--sample-rate-hz` would crash or corrupt every
    spot's derived time (dividing `sample_ts` by it)."""
    def parse(s):
        v = float(s)
        finite = (v == v) and v not in (float("inf"), float("-inf"))
        in_bounds = finite and (
            (min_exclusive is None or v > min_exclusive)
            and (min_inclusive is None or v >= min_inclusive)
        )
        if not in_bounds:
            bound = (
                f"> {min_exclusive}" if min_exclusive is not None
                else f">= {min_inclusive}" if min_inclusive is not None
                else "finite"
            )
            raise argparse.ArgumentTypeError(f"{name} must be finite and {bound}, got {s!r}")
        return v
    return parse


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("decode_report")
    ap.add_argument("rbn_truth_csv")
    ap.add_argument("--capture-start", required=True, type=parse_iso,
                     help="Recording's capture start, ISO-8601 UTC (e.g. 2025-11-29T00:00:00Z)")
    ap.add_argument("--sample-rate-hz", required=True, type=_finite("--sample-rate-hz", min_exclusive=0))
    ap.add_argument("--freq-tol-hz", type=_finite("--freq-tol-hz", min_inclusive=0), default=500.0)
    ap.add_argument("--time-tol-s", type=_finite("--time-tol-s", min_inclusive=0), default=90.0,
                     help="Max seconds between a manta spot and some real RBN observation of that call+freq (default: 90s, matching RepetitionGate's own window)")
    ap.add_argument("--show-samples", type=int, default=10)
    ap.add_argument("--spotter", action="append", default=None,
                     help="Only use truth rows from this spotter callsign (repeatable), e.g. --spotter K5TR for the co-located reference")
    ap.add_argument("--min-spotters", type=int, default=1,
                     help="Only count call+kHz bins heard by at least N distinct spotters")
    args = ap.parse_args()

    manta_spots = load_manta_spots(args.decode_report, args.capture_start, args.sample_rate_hz)
    spotters = {s.upper() for s in args.spotter} if args.spotter else None
    truth, excluded = load_rbn_truth(args.rbn_truth_csv, args.capture_start,
                                      spotters=spotters, min_spotters=args.min_spotters)
    truth_by_key = dedup_truth(truth)

    tp, fp, fn_keys = match(manta_spots, truth_by_key, args.freq_tol_hz, args.time_tol_s)

    n_manta = len(manta_spots)
    n_truth = len(truth_by_key)
    precision = len(tp) / n_manta if n_manta else 0.0
    # Codex review, PR #161: `len(tp)` counts manta SPOTS, not unique matched
    # truth bins -- a re-spot of the same call+kHz (e.g. after the 10-minute
    # dedupe window) appends a second entry to `tp` for the same truth item,
    # which would double-count that one truth item and can inflate recall
    # past 100%. `n_truth - len(fn_keys)` is the count of truth bins that
    # matched at least one manta spot, which is what recall means.
    recall = (n_truth - len(fn_keys)) / n_truth if n_truth else 0.0

    print(f"manta spots: {n_manta}")
    print(f"RBN truth rows excluded (callsign shape manta's grammar can never accept): {excluded}")
    print(f"RBN truth unique call+kHz bins in window: {n_truth}")
    print(f"true positives: {len(tp)}")
    print(f"false positives: {len(fp)}")
    print(f"false negatives: {len(fn_keys)}")
    print(f"precision: {precision:.3%}")
    print(f"recall:    {recall:.3%}")

    n = args.show_samples
    if n:
        print(f"\n--- sample true positives (up to {n}) ---")
        for s in tp[:n]:
            print(f"  {s['callsign']:12s} {s['freq_hz']/1000:9.2f} kHz  {s['time'].isoformat()}  conf={s['confidence']:.2f} type={s['spot_type']}")

        print(f"\n--- sample false positives (manta spot, no RBN match; up to {n}) ---")
        for s in fp[:n]:
            print(f"  {s['callsign']:12s} {s['freq_hz']/1000:9.2f} kHz  {s['time'].isoformat()}  conf={s['confidence']:.2f} type={s['spot_type']}")

        print(f"\n--- sample false negatives (RBN heard, manta missed; up to {n}) ---")
        for k in list(fn_keys)[:n]:
            print(f"  {k[0]:12s} ~{k[1]} kHz")


if __name__ == "__main__":
    main()
