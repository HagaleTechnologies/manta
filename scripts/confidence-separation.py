#!/usr/bin/env python3
"""Report confidence/SNR separation between true and bogus manta spots (MAN-106 evidence).

Splits a `manta decode --json` report's spots into true positives and bogus (false-positive)
spots using the exact same RBN-truth matching rule as `score-against-rbn.py` (imported, not
reimplemented), then reports each class's median/IQR confidence and SNR and the confidence
separation `median(true) - median(bogus)`. A wide separation is evidence that `confidence`
is a meaningful signal for filtering bogus spots downstream (MAN-106); a narrow or negative
one means it isn't, at least not yet.

Usage:
  confidence-separation.py <decode_report.json> <rbn_truth_csv> \
      --capture-start 2025-11-29T00:00:00Z --sample-rate-hz 192000 [--spotter K5TR] \
      [--min-separation 0.1]
"""
import sys
import os
import argparse
import statistics
import importlib.util

_HERE = os.path.dirname(os.path.abspath(__file__))
_spec = importlib.util.spec_from_file_location(
    "score_against_rbn", os.path.join(_HERE, "score-against-rbn.py")
)
scorer = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(scorer)


def quartiles(values):
    """(q1, median, q3). NaN triple for an empty list; degenerate but well-defined for a
    single value (all three equal it) since `statistics.quantiles` requires len(data) >= 2."""
    if not values:
        return (float("nan"), float("nan"), float("nan"))
    if len(values) == 1:
        return (values[0], values[0], values[0])
    q1, _, q3 = statistics.quantiles(sorted(values), n=4, method="inclusive")
    return (q1, statistics.median(values), q3)


def summarize(label, spots):
    conf = [s["confidence"] for s in spots]
    snr = [s["snr_db"] for s in spots]
    c_q1, c_med, c_q3 = quartiles(conf)
    s_q1, s_med, s_q3 = quartiles(snr)
    print(f"{label}: n={len(spots)}")
    print(f"  confidence: median={c_med:.3f} IQR=[{c_q1:.3f}, {c_q3:.3f}]")
    print(f"  snr_db:     median={s_med:.2f} IQR=[{s_q1:.2f}, {s_q3:.2f}]")
    return c_med


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("decode_report")
    ap.add_argument("rbn_truth_csv")
    ap.add_argument("--capture-start", required=True, type=scorer.parse_iso)
    ap.add_argument("--sample-rate-hz", required=True, type=scorer._finite("--sample-rate-hz", min_exclusive=0))
    ap.add_argument("--freq-tol-hz", type=scorer._finite("--freq-tol-hz", min_inclusive=0), default=500.0)
    ap.add_argument("--time-tol-s", type=scorer._finite("--time-tol-s", min_inclusive=0), default=90.0)
    ap.add_argument("--spotter", action="append", default=None,
                     help="Only use truth rows from this spotter callsign (repeatable)")
    ap.add_argument("--min-spotters", type=int, default=1)
    # Codex review, PR #161 round 2: a plain `type=float` accepted `nan`,
    # and `separation < args.min_separation` is always False against nan --
    # the gate below would then exit 0 unconditionally instead of ever
    # failing, silently turning off the check `--min-separation` exists
    # for. Reuse the existing finite-number validator (no bound needed
    # here -- unlike score-against-rbn.py's tolerances, a negative
    # separation threshold is a legitimate, meaningful value to gate on).
    ap.add_argument("--min-separation", type=scorer._finite("--min-separation"), default=None,
                     help="Exit 1 if median(true) - median(bogus) confidence is below this")
    args = ap.parse_args()

    manta_spots = scorer.load_manta_spots(args.decode_report, args.capture_start, args.sample_rate_hz)
    spotters = {s.upper() for s in args.spotter} if args.spotter else None
    truth, _excluded = scorer.load_rbn_truth(
        args.rbn_truth_csv, args.capture_start, spotters=spotters, min_spotters=args.min_spotters
    )
    truth_by_key = scorer.dedup_truth(truth)
    tp, fp, _fn_keys = scorer.match(manta_spots, truth_by_key, args.freq_tol_hz, args.time_tol_s)

    print(f"true positives: {len(tp)}, bogus (false positives): {len(fp)}")

    if not tp or not fp:
        print(
            f"cannot compute a meaningful separation: {len(tp)} true, {len(fp)} bogus "
            "(need at least one of each)",
            file=sys.stderr,
        )
        sys.exit(2)

    true_med = summarize("true", tp)
    bogus_med = summarize("bogus", fp)
    separation = true_med - bogus_med
    print(f"confidence separation (median true - median bogus): {separation:.3f}")

    if args.min_separation is not None and separation < args.min_separation:
        print(f"FAIL: separation {separation:.3f} < required {args.min_separation}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
