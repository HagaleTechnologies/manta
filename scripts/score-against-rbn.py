#!/usr/bin/env python3
"""Score a `manta decode --json` report against a Reverse Beacon Network
(RBN) ground-truth window for the same recording.

The RBN truth CSV is RBN's own raw daily-dump format (see
`data.reversebeacon.net/rbn_history/<YYYYMMDD>.zip`), pre-filtered to the
recording's time/frequency window -- e.g.
`crates/manta-testkit/audio-corpus/ground-truth/B2_20251129_000000_7080kHz.rbn.csv`.

Matches manta spots to RBN truth by callsign + frequency (within
--freq-tol-hz, checked against the nearest kHz bins) -- not by exact time,
since a CQ can repeat many times across the window and neither manta nor
any individual RBN skimmer necessarily catches the same repetition.

Usage:
  score-against-rbn.py <decode_report.json> <rbn_truth_csv> \
      --capture-start 2025-11-29T00:00:00Z --sample-rate-hz 192000
"""
import sys
import json
import csv
import argparse
from datetime import datetime, timedelta, timezone


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


def load_rbn_truth(path):
    truth = []
    with open(path) as f:
        reader = csv.DictReader(f)
        for row in reader:
            truth.append({
                "callsign": row["dx"].upper(),
                "freq_hz": float(row["freq"]) * 1000.0,
            })
    return truth


def dedup_truth(truth):
    seen = {}
    for t in truth:
        key = (t["callsign"], round(t["freq_hz"] / 1000.0))  # nearest kHz bin
        seen.setdefault(key, []).append(t)
    return seen


def match(manta_spots, truth_by_key, freq_tol_hz):
    tp, fp = [], []
    matched_truth_keys = set()
    for m in manta_spots:
        best_key = None
        for df_khz in (0, -1, 1):
            key = (m["callsign"], round(m["freq_hz"] / 1000.0) + df_khz)
            candidates = truth_by_key.get(key)
            if not candidates:
                continue
            if any(abs(t["freq_hz"] - m["freq_hz"]) <= freq_tol_hz for t in candidates):
                best_key = key
                break
        if best_key:
            tp.append(m)
            matched_truth_keys.add(best_key)
        else:
            fp.append(m)
    fn_keys = set(truth_by_key.keys()) - matched_truth_keys
    return tp, fp, fn_keys


def parse_iso(s):
    return datetime.fromisoformat(s.replace("Z", "+00:00"))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("decode_report")
    ap.add_argument("rbn_truth_csv")
    ap.add_argument("--capture-start", required=True, type=parse_iso,
                     help="Recording's capture start, ISO-8601 UTC (e.g. 2025-11-29T00:00:00Z)")
    ap.add_argument("--sample-rate-hz", required=True, type=float)
    ap.add_argument("--freq-tol-hz", type=float, default=500.0)
    ap.add_argument("--show-samples", type=int, default=10)
    args = ap.parse_args()

    manta_spots = load_manta_spots(args.decode_report, args.capture_start, args.sample_rate_hz)
    truth = load_rbn_truth(args.rbn_truth_csv)
    truth_by_key = dedup_truth(truth)

    tp, fp, fn_keys = match(manta_spots, truth_by_key, args.freq_tol_hz)

    n_manta = len(manta_spots)
    n_truth = len(truth_by_key)
    precision = len(tp) / n_manta if n_manta else 0.0
    recall = len(tp) / n_truth if n_truth else 0.0

    print(f"manta spots: {n_manta}")
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
