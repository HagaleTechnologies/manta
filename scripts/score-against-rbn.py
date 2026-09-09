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


def load_rbn_truth(path, capture_start):
    truth = []
    excluded = 0
    with open(path) as f:
        reader = csv.DictReader(f)
        for row in reader:
            call = row["dx"].upper()
            if not _is_plausible_callsign(call):
                excluded += 1
                continue
            ts = datetime.strptime(row["date"], "%Y-%m-%d %H:%M:%S").replace(tzinfo=timezone.utc)
            truth.append({
                "callsign": call,
                "freq_hz": float(row["freq"]) * 1000.0,
                "time": ts,
            })
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
    return datetime.fromisoformat(s.replace("Z", "+00:00"))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("decode_report")
    ap.add_argument("rbn_truth_csv")
    ap.add_argument("--capture-start", required=True, type=parse_iso,
                     help="Recording's capture start, ISO-8601 UTC (e.g. 2025-11-29T00:00:00Z)")
    ap.add_argument("--sample-rate-hz", required=True, type=float)
    ap.add_argument("--freq-tol-hz", type=float, default=500.0)
    ap.add_argument("--time-tol-s", type=float, default=90.0,
                     help="Max seconds between a manta spot and some real RBN observation of that call+freq (default: 90s, matching RepetitionGate's own window)")
    ap.add_argument("--show-samples", type=int, default=10)
    args = ap.parse_args()

    manta_spots = load_manta_spots(args.decode_report, args.capture_start, args.sample_rate_hz)
    truth, excluded = load_rbn_truth(args.rbn_truth_csv, args.capture_start)
    truth_by_key = dedup_truth(truth)

    tp, fp, fn_keys = match(manta_spots, truth_by_key, args.freq_tol_hz, args.time_tol_s)

    n_manta = len(manta_spots)
    n_truth = len(truth_by_key)
    precision = len(tp) / n_manta if n_manta else 0.0
    recall = len(tp) / n_truth if n_truth else 0.0

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
