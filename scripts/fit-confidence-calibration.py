#!/usr/bin/env python3
"""MAN-106: fit the committed spot-confidence calibration.

Input: the committed measurement pair under
crates/manta-spot/data/calibration/ (`fit.csv`, `heldout.csv`; columns
`c_call_raw,reps,tier,correct`), extracted from `manta bench sensitivity
--json` reports with `extract`. The fit is a joint model, not stacked
post-hoc multipliers: the raw `c_call` composition is divided back to its
geo-mean, and the repetition factor's exponent (`rep_scale`) and the two
pattern-tier factors are re-fitted in log space while a monotone
piecewise-linear map (isotonic regression, then reduction to at most 8
knots anchored at (0, 0)) is fitted over the joint raw score
`geo_mean * (1 - 0.5^r)^rep_scale * tier_factor[tier]`.

Everything is deterministic: fixed grids, fixed split rule (alternating
rows at extraction), fixed knot quantiles, fixed float formatting. The
python test `scripts.tests.test_fit_calibration` pins that running the
fit on the committed CSVs reproduces the committed constants byte for
byte, so `crates/manta-spot/src/confidence/calibration.rs` (which carries
the same numbers as Rust literals) cannot drift from its provenance.

Usage:
  fit-confidence-calibration.py extract REPORT_JSON ... --out DIR
  fit-confidence-calibration.py fit [--write]
"""
import argparse
import csv
import json
import math
import os
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DATA_DIR = os.path.join(REPO, "crates", "manta-spot", "data", "calibration")
TIERS = ("common", "listed", "unlisted")

REP_SCALES = [round(0.1 * i, 1) for i in range(0, 21)]          # 0.0 .. 2.0
TIER_FACTORS = {
    "common": [1.0],
    "listed": [round(0.70 + 0.05 * i, 2) for i in range(0, 13)],  # 0.70 .. 1.30
    "unlisted": [round(0.50 + 0.05 * i, 2) for i in range(0, 15)],  # 0.50 .. 1.20
}
MAX_KNOTS = 8
# Isotonic shrinkage toward the identity (a step fit would publish 1.0
# for everything above its single jump; the blend keeps graded
# probabilities). Fitted on the fit split by Brier score.
SHRINK_ALPHAS = [round(0.50 + 0.05 * i, 2) for i in range(0, 11)]  # 0.50 .. 1.00


def rep_base(reps):
    return 1.0 - 0.5 ** max(int(reps), 1)


def read_rows(path):
    rows = []
    with open(path, newline="") as f:
        for rec in csv.DictReader(f):
            rows.append(
                (
                    float(rec["c_call_raw"]),
                    int(rec["reps"]),
                    rec["tier"],
                    float(rec["correct"]),
                )
            )
    return rows


def extract(report_paths, out_dir):
    """Split one or more bench `--json` reports' `confidence.rows` into
    the committed fit/held-out pair: rows in file and array order,
    alternating (even index -> fit, odd index -> held-out)."""
    fit, held = [], []
    for path in report_paths:
        with open(path) as f:
            report = json.load(f)
        for point in report["points"]:
            for row in point["confidence"]["rows"]:
                out = fit if (len(fit) + len(held)) % 2 == 0 else held
                out.append(
                    (
                        row["score"],
                        row["reps"],
                        row["tier"],
                        1.0 if row["correct"] else 0.0,
                    )
                )
    os.makedirs(out_dir, exist_ok=True)
    for name, rows in (("fit", fit), ("heldout", held)):
        with open(os.path.join(out_dir, f"{name}.csv"), "w", newline="") as f:
            w = csv.writer(f)
            w.writerow(["c_call_raw", "reps", "tier", "correct"])
            for r in rows:
                w.writerow([f"{r[0]:.6f}", r[1], r[2], int(r[3])])
    print(f"wrote {len(fit)} fit rows and {len(held)} held-out rows to {out_dir}")


def pava(xs, ys):
    """Weighted pool-adjacent-violators: returns fitted values, monotone
    non-decreasing in x. Ties in x are pooled by averaging first."""
    pairs = sorted(zip(xs, ys), key=lambda t: t[0])
    # pool equal x
    pooled_x, pooled_y, weights = [], [], []
    for x, y in pairs:
        if pooled_x and pooled_x[-1] == x:
            pooled_y[-1] = (pooled_y[-1] * weights[-1] + y) / (weights[-1] + 1)
            weights[-1] += 1
        else:
            pooled_x.append(x)
            pooled_y.append(y)
            weights.append(1)
    # PAVA on blocks
    block_y, block_w, block_n = [], [], []
    for y, w in zip(pooled_y, weights):
        block_y.append(y)
        block_w.append(w)
        block_n.append(1)
        while len(block_y) > 1 and block_y[-2] > block_y[-1]:
            y2, w2, n2 = block_y.pop(), block_w.pop(), block_n.pop()
            y1, w1, n1 = block_y.pop(), block_w.pop(), block_n.pop()
            merged_w = w1 + w2
            block_y.append((y1 * w1 + y2 * w2) / merged_w)
            block_w.append(merged_w)
            block_n.append(n1 + n2)
    fitted, i = [], 0
    for n in block_n:
        fitted.extend([block_y[i]] * n)
        i += 1
    return pooled_x, fitted


class PiecewiseLinear:
    """A monotone step function (from PAVA) sampled as piecewise linear
    through (x0, y0) .. with the (0, 0) anchor."""

    def __init__(self, xs, ys):
        self.xs, self.ys = xs, ys

    def at(self, x):
        if x <= self.xs[0]:
            return self.ys[0]
        if x >= self.xs[-1]:
            return self.ys[-1]
        lo, hi = 0, len(self.xs) - 1
        while hi - lo > 1:
            mid = (lo + hi) // 2
            if self.xs[mid] <= x:
                lo = mid
            else:
                hi = mid
        x0, x1, y0, y1 = self.xs[lo], self.xs[hi], self.ys[lo], self.ys[hi]
        if x1 == x0:
            return y1
        return y0 + (x - x0) / (x1 - x0) * (y1 - y0)


def joint_scores(rows, rep_scale, tf_listed, tf_unlisted):
    out = []
    for raw, reps, tier, _ in rows:
        geo = raw / rep_base(reps)
        tf = {"common": 1.0, "listed": tf_listed, "unlisted": tf_unlisted}[tier]
        out.append(geo * (rep_base(reps) ** rep_scale) * tf)
    return out


def fit_isotonic(rows, rep_scale, tf_listed, tf_unlisted):
    xs = joint_scores(rows, rep_scale, tf_listed, tf_unlisted)
    ys = [r[3] for r in rows]
    px, py = pava(xs, ys)
    return PiecewiseLinear(px, py)


def brier(model, rows, rep_scale, tf_listed, tf_unlisted):
    xs = joint_scores(rows, rep_scale, tf_listed, tf_unlisted)
    n = len(rows)
    return sum((model.at(x) - r[3]) ** 2 for x, r in zip(xs, rows)) / n


def reduce_knots(model, xs):
    """Deterministic knot reduction: (0, 0) plus MAX_KNOTS-1 quantile
    points of the observed joint scores."""
    knots = [(0.0, 0.0)]
    xs_sorted = sorted(xs)
    for k in range(1, MAX_KNOTS):
        q = k / (MAX_KNOTS - 1)
        idx = min(int(q * (len(xs_sorted) - 1)), len(xs_sorted) - 1)
        x = xs_sorted[idx]
        y = model.at(x)
        if x <= knots[-1][0]:
            continue
        y = max(y, knots[-1][1])  # keep monotone after the 0 anchor
        knots.append((x, y))
    knots = [(x, min(y, 1.0)) for x, y in knots]
    return PiecewiseLinear([k[0] for k in knots], [k[1] for k in knots])


def ece(model, rows, rep_scale, tf_listed, tf_unlisted):
    xs = joint_scores(rows, rep_scale, tf_listed, tf_unlisted)
    bins = [[0.0, 0.0, 0.0] for _ in range(10)]
    for x, r in zip(xs, rows):
        p = min(max(model.at(x), 0.0), 1.0)
        b = min(int(p * 10), 9)
        bins[b][0] += 1
        bins[b][1] += p
        bins[b][2] += r[3]
    total = len(rows)
    return sum(
        (n / total) * abs((c / n) - (a / n)) for n, c, a in bins if n > 0
    )


def fit(fit_rows, held_rows):
    best = None
    for rep_scale in REP_SCALES:
        for tf_listed in TIER_FACTORS["listed"]:
            for tf_unlisted in TIER_FACTORS["unlisted"]:
                model = fit_isotonic(fit_rows, rep_scale, tf_listed, tf_unlisted)
                xs = joint_scores(fit_rows, rep_scale, tf_listed, tf_unlisted)
                knots_model = reduce_knots(model, xs)
                for alpha in SHRINK_ALPHAS:
                    shrunk = shrink(knots_model, alpha)
                    b = brier(shrunk, fit_rows, rep_scale, tf_listed, tf_unlisted)
                    key = (round(b, 8), rep_scale, tf_listed, tf_unlisted, alpha)
                    if best is None or key < best[0]:
                        best = (
                            key,
                            rep_scale,
                            tf_listed,
                            tf_unlisted,
                            alpha,
                            shrunk,
                        )
    _, rep_scale, tf_listed, tf_unlisted, alpha, shrunk = best
    return {
        "knots": [[round(x, 6), round(y, 6)] for x, y in zip(shrunk.xs, shrunk.ys)],
        "tier_factors": [1.0, round(tf_listed, 2), round(tf_unlisted, 2)],
        "rep_scale": round(rep_scale, 1),
        "shrink_alpha": round(alpha, 2),
    }, shrunk


def shrink(model, alpha):
    """Blend the reduced isotonic map with the identity at its knots:
    `alpha * p(x) + (1 - alpha) * x`. Both pieces are linear between the
    same knots, so the blend is exactly the piecewise-linear map through
    the blended knot values."""
    xs = model.xs
    ys = [round(alpha * y + (1.0 - alpha) * x, 6) for x, y in zip(xs, model.ys)]
    # Rounding can dent monotonicity by 1e-6; repair it.
    for i in range(1, len(ys)):
        ys[i] = max(ys[i], ys[i - 1])
    ys = [min(y, 1.0) for y in ys]
    return PiecewiseLinear(xs, ys)


def constants_text(constants):
    return json.dumps(constants, indent=2) + "\n"


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    ex = sub.add_parser("extract", help="extract rows from bench --json reports")
    ex.add_argument("reports", nargs="+")
    ex.add_argument("--out", default=DATA_DIR)
    ft = sub.add_parser("fit", help="fit the committed measurement pair")
    ft.add_argument("--write", action="store_true", help="rewrite constants.json")
    ft.add_argument("--check", action="store_true", help="compare against constants.json")
    args = ap.parse_args(argv)

    if args.cmd == "extract":
        extract(args.reports, args.out)
        return 0

    fit_rows = read_rows(os.path.join(DATA_DIR, "fit.csv"))
    held_rows = read_rows(os.path.join(DATA_DIR, "heldout.csv"))
    if not fit_rows or not held_rows:
        print("measurement pair is empty; run `extract` first", file=sys.stderr)
        return 2
    constants, model = fit(fit_rows, held_rows)
    text = constants_text(constants)
    fit_b = brier(model, fit_rows, constants["rep_scale"],
                  constants["tier_factors"][1], constants["tier_factors"][2])
    held_ece = ece(model, held_rows, constants["rep_scale"],
                   constants["tier_factors"][1], constants["tier_factors"][2])
    held_b = brier(model, held_rows, constants["rep_scale"],
                   constants["tier_factors"][1], constants["tier_factors"][2])
    print(f"fit rows {len(fit_rows)}, held-out rows {len(held_rows)}")
    print(f"fit Brier {fit_b:.4f}; held-out ECE {held_ece:.4f} (<= 0.20), Brier {held_b:.4f}")
    print(text, end="")
    if args.write:
        with open(os.path.join(DATA_DIR, "constants.json"), "w") as f:
            f.write(text)
        print(f"wrote {os.path.join(DATA_DIR, 'constants.json')}")
    if args.check:
        with open(os.path.join(DATA_DIR, "constants.json")) as f:
            committed = f.read()
        if committed != text:
            print("committed constants differ from a fresh fit:", file=sys.stderr)
            print(text, file=sys.stderr)
            return 1
        print("committed constants reproduce byte-for-byte")
    return 0


if __name__ == "__main__":
    sys.exit(main())
