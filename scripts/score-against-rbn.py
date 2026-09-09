#!/usr/bin/env python3
"""Score decoded spots against RBN truth."""

import csv
import argparse
import datetime
import sys


def load_rbn_truth(path, spotters=None, min_spotters=1):
    """RBN daily-dump rows -> [{callsign, freq_hz}], optionally restricted
    to rows from `spotters` (a set of spotter callsigns) and to call+kHz
    bins heard by at least `min_spotters` distinct spotters."""
    rows = []
    with open(path) as f:
        for row in csv.DictReader(f):
            if spotters and row["callsign"].upper() not in spotters:
                continue
            rows.append({
                "spotter": row["callsign"].upper(),
                "callsign": row["dx"].upper(),
                "freq_hz": float(row["freq"]) * 1000.0,
            })
    if min_spotters > 1:
        heard = {}
        for r in rows:
            heard.setdefault((r["callsign"], round(r["freq_hz"] / 1000.0)), set()).add(r["spotter"])
        rows = [r for r in rows if len(heard[(r["callsign"], round(r["freq_hz"] / 1000.0))]) >= min_spotters]
    return rows


def main():
    ap = argparse.ArgumentParser(description="Score decoded spots against RBN truth")
    ap.add_argument("rbn_truth_csv", help="RBN truth CSV file")
    ap.add_argument("--spotter", action="append", default=None,
                    help="Only use truth rows from this spotter callsign (repeatable), e.g. --spotter K5TR for the co-located reference")
    ap.add_argument("--min-spotters", type=int, default=1,
                    help="Only count call+kHz bins heard by at least N distinct spotters")
    args = ap.parse_args()

    spotters = {s.upper() for s in args.spotter} if args.spotter else None
    truth = load_rbn_truth(args.rbn_truth_csv, spotters=spotters, min_spotters=args.min_spotters)

    print(f"truth filter: spotters={sorted(spotters) if spotters else 'all'} min_spotters={args.min_spotters}")
    print(f"Loaded {len(truth)} truth rows")


if __name__ == "__main__":
    main()
