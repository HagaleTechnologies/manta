#!/usr/bin/env python3
"""Compare two decode engines' results on the same truth identities: how often
one engine recovers a call the other misses (MAN-205, the "oracle gap").

manta has two classical decoders on the same evidence: the legacy
character-local beam (SPEC v1 §4) and the HSMM token-passing decoder (SPEC v2
§4). Fusing them is only worth building if they make complementary mistakes.
This tool measures that from two runs, one per engine, of the same corpus. It
records the comparison; it does not pass or fail anything.

Two modes, one report shape:

  bench   `manta bench sensitivity --json` reports (format_version 1), one or
          more per side: `--a` files from engine A, `--b` files from engine B.
          The bench renders each recording from (seed, trial) and the station
          signals from (condition, wpm, trial, seed) alone, so both engines
          decode byte-identical recordings. Points pair on
          (seed, condition, wpm, snr_db) and stations on
          (trial, call, offset_hz).
  oracle  `manta oracle --jsonl` files, one per side. Both runs must share the
          recording, the RBN CSV, `--spotter`, `--capture-start`,
          `--window-s` and `--mode`, and differ only in `--engine`. Each line
          is one reference spot; both engines saw identical channel samples
          for it (the tracker is bypassed). Lines pair on (call, khz).

Metric definitions (each is a row or column of the report):

  Truth identity   bench: one station in one recording, the call it sent at
                   the frequency it sent on. oracle: one reference spot.
  Identity (bench) the engine spotted that call within the bench's match_hz
                   of the station (`per_station[].spotted`).
  Copy (bench)     the station's decoded copy has a character error rate
                   strictly below 0.10 (`per_station[].cer`, the bench's
                   GOOD_COPY_CER).
  as_word (oracle) the reference call is a whole word of the decoded window.
  framed (oracle)  the reference call follows CQ, DE or CQ TEST there.
  Both, A only, B only, Neither
                   the 2x2 of "recovered" across the two engines, per level.
  Recall           (Both + only) / n, per engine.
  Exactly one      (A only + B only) / n: the ticket's rate, identities
                   recovered by exactly one engine and missed by the other.
  Union recall     (Both + A only + B only) / n: what a perfect per-identity
                   selector between the two engines would reach.
  Headroom         union recall minus the better engine's recall, which is
                   min(A only, B only) / n: the most fusion could add over
                   running the better engine alone. Printed in percentage
                   points (pp).
  False identities (bench only) calls an engine spotted that no station sent,
                   each trial's distinct bogus calls pooled per point as the
                   bench reports them (`points[].bogus_calls`). Shared is the
                   per-point multiset intersection of the two engines' lists;
                   Either = A + B - Shared. The oracle scores reference calls
                   only, so oracle mode has no false-identity count.
  95% interval     percentile bootstrap over whole signal sets
                   (seed, condition, wpm, trial) in bench mode and over
                   reference spots in oracle mode. The bench reuses one signal
                   set and one noise trial at every SNR of a sweep, so a
                   recording at one SNR is not independent of the same signal
                   set at another; resampling the signal set keeps them
                   together. --bootstrap resamples
                   (default 2000, 0 disables), random.Random(205), cluster
                   index int(random() * k); bounds are the sorted values at
                   int(0.025 * N) and int(0.975 * N) - 1.

No metric reads an engine's own claimed values: spots, first_spot_s,
spot_snr_db (bench) and text, wpm_ratio (oracle) never change the result.
Correctness comes only from truth.

Fusion gate: the headroom at either level at or above --bar-pp (default 1.00
pp) reads "meets the bar"; otherwise "below the bar at both levels". Below 1
pp, even a perfect selector adds less than one identity in 100, bought with a
second decoder at several times the CPU. The gate is read by a person; the
exit code never depends on it.

Refusals (exit 2, one line on stderr): a bench file that is not a `manta bench
sensitivity` format_version 1 report; any of duration_s, trials,
stations_per_trial, sample_rate_hz, center_freq_hz, payload, match_hz,
snr_ref_hz differing across files (false identities pool over trials, so
differing trial counts cannot be compared); one side mixing engines; a point
twice on one side; a paired point whose stations differ; nothing pairing. A
point on one side only is counted and printed, not fatal. Oracle mode refuses
malformed lines, a reference spot twice in one file, and two files that do
not hold the same reference spots.

Output: Markdown on stdout, byte-identical for identical inputs (no paths,
clock or host); `--json OUT` writes the same numbers as JSON.

Exit codes: 0 on success, 2 on a usage or input error.
See docs/DECISIONS/2026-11-03-man205-legacy-hsmm-engine-overlap.md.

Usage:
  engine-overlap.py bench --a A.json [A2.json ...] --b B.json [B2.json ...]
                          [--bar-pp 1.0] [--bootstrap 2000] [--json OUT]
  engine-overlap.py oracle --a A.jsonl --b B.jsonl [--a-name A] [--b-name B]
                          [--bar-pp 1.0] [--bootstrap 2000] [--json OUT]
"""
import argparse
import json
import math
import os
import random
import sys
from collections import Counter
from dataclasses import dataclass

GOOD_COPY_CER = 0.10  # crates/manta-cli/src/bench.rs GOOD_COPY_CER
BENCH_REPORT = "manta bench sensitivity"
BENCH_FORMAT = 1
MUST_MATCH = ("duration_s", "trials", "stations_per_trial", "sample_rate_hz",
              "center_freq_hz", "payload", "match_hz", "snr_ref_hz")
CONDITION_ORDER = ("awgn", "good", "poor")
BOOTSTRAP_SEED = 205
DEFAULT_RESAMPLES = 2000
DEFAULT_BAR_PP = 1.0
BENCH_LEVELS = ("identity", "copy")
ORACLE_LEVELS = ("as_word", "framed")
# (strata key, JSON key, section heading, column header, value formatter)
BREAKDOWNS = (
    ("condition", "by_condition", "By condition", "Condition", str),
    ("wpm", "by_wpm", "By speed", "Speed", lambda v: f"{v} WPM"),
    ("snr_db", "by_snr_db", "By SNR, 500 Hz", "SNR", lambda v: f"{fmt_db(v)} dB"),
    ("trial", "by_trial", "By trial", "Trial", str),
)


class InputError(Exception):
    """An input the tool refuses; main() prints it on one line and returns 2."""


@dataclass(frozen=True)
class Pair:
    """One truth identity and both engines' outcomes, one bool per level."""
    cluster: tuple  # bench: (seed, condition, wpm, trial), one signal set; oracle: (call, khz)
    strata: dict  # bench: condition, wpm, snr_db, trial; oracle: empty
    a: tuple
    b: tuple


@dataclass
class Counts:
    n: int = 0
    both: int = 0
    a_only: int = 0
    b_only: int = 0
    neither: int = 0

    def add(self, x, y):
        self.n += 1
        if x and y:
            self.both += 1
        elif x:
            self.a_only += 1
        elif y:
            self.b_only += 1
        else:
            self.neither += 1

    def rates(self):
        return rates(self.n, self.both, self.a_only, self.b_only)

    def as_dict(self):
        out = {"n": self.n, "both": self.both, "a_only": self.a_only,
               "b_only": self.b_only, "neither": self.neither}
        out.update(self.rates())
        out["headroom_pp"] = 100 * out["headroom"]
        return out


def rates(n, both, a_only, b_only):
    return {
        "recall_a": (both + a_only) / n,
        "recall_b": (both + b_only) / n,
        "exactly_one": (a_only + b_only) / n,
        "union_recall": (both + a_only + b_only) / n,
        "headroom": min(a_only, b_only) / n,
    }


def fmt_db(x):
    """The bench's fmt_db: whole numbers without a decimal, else one."""
    x = float(x)
    return str(int(x)) if x == int(x) else f"{x:.1f}"


def pct(x):
    return f"{100 * x:.1f}%"


def pp(x):
    return f"{100 * x:.2f} pp"


def plural(n, one, many):
    return f"{n} {one if n == 1 else many}"


# ---------------------------------------------------------------- bench input


def _read_json(path):
    try:
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    except OSError as e:
        raise InputError(f"{path}: cannot read: {e.strerror}")
    except (json.JSONDecodeError, UnicodeDecodeError) as e:
        raise InputError(f"{path}: not JSON: {e}")


def _is_int(v):
    return isinstance(v, int) and not isinstance(v, bool)


def _is_num(v):
    return (isinstance(v, (int, float)) and not isinstance(v, bool)
            and math.isfinite(v))


def _require(path, obj, key, check, what):
    if not isinstance(obj, dict) or key not in obj or not check(obj[key]):
        raise InputError(f"{path}: `{key}` missing or not {what}")
    return obj[key]


def _bench_station(path, st):
    _require(path, st, "trial", _is_int, "an integer")
    _require(path, st, "call", lambda v: isinstance(v, str), "a string")
    _require(path, st, "offset_hz", _is_num, "a number")
    _require(path, st, "spotted", lambda v: isinstance(v, bool), "a boolean")
    _require(path, st, "cer", _is_num, "a number")
    return st


def _bench_point(path, p):
    _require(path, p, "condition", lambda v: isinstance(v, str), "a string")
    _require(path, p, "wpm", _is_int, "an integer")
    _require(path, p, "snr_db", _is_num, "a number")
    stations = _require(path, p, "per_station", lambda v: isinstance(v, list), "a list")
    _require(path, p, "bogus_calls",
             lambda v: isinstance(v, list) and all(isinstance(c, str) for c in v),
             "a list of strings")
    for st in stations:
        _bench_station(path, st)
    return p


def load_bench(paths, side):
    """One side's reports, merged: {point key: point} plus the side's meta."""
    engine, meta, seeds, points = None, None, [], {}
    for path in paths:
        rep = _read_json(path)
        if not isinstance(rep, dict) or rep.get("report") != BENCH_REPORT:
            raise InputError(f"{path}: not a `{BENCH_REPORT}` report")
        if not _is_int(rep.get("format_version")) or rep["format_version"] != BENCH_FORMAT:
            raise InputError(f"{path}: format_version {rep.get('format_version')!r},"
                             f" expected {BENCH_FORMAT}")
        eng = _require(path, rep, "engine", lambda v: isinstance(v, str), "a string")
        seed = _require(path, rep, "seed", _is_int, "an integer")
        for key in MUST_MATCH:
            if key not in rep:
                raise InputError(f"{path}: `{key}` missing")
        if engine is None:
            engine = eng
        elif eng != engine:
            raise InputError(f"side {side} mixes engines {engine} and {eng}")
        file_meta = {key: rep[key] for key in MUST_MATCH}
        if meta is None:
            meta = file_meta
        else:
            check_meta(meta, file_meta, path)
        if seed not in seeds:
            seeds.append(seed)
        for p in _require(path, rep, "points", lambda v: isinstance(v, list), "a list"):
            _bench_point(path, p)
            key = (seed, p["condition"], p["wpm"], float(p["snr_db"]))
            if key in points:
                raise InputError(f"side {side} has point {describe_point(key)} twice")
            points[key] = p
    return {"engine": engine, "meta": meta, "seeds": sorted(seeds),
            "files": [os.path.basename(p) for p in paths], "points": points}


def check_meta(meta, other, path):
    for key in MUST_MATCH:
        if other[key] != meta[key]:
            raise InputError(f"{path}: {key} {other[key]!r} differs from {meta[key]!r}"
                             " in the other reports")


def describe_point(key):
    seed, condition, wpm, snr_db = key
    return f"seed {seed} {condition}/{wpm} WPM/{fmt_db(snr_db)} dB"


def condition_rank(name):
    return (CONDITION_ORDER.index(name), "") if name in CONDITION_ORDER else (len(CONDITION_ORDER), name)


def point_order(key):
    seed, condition, wpm, snr_db = key
    return (seed, condition_rank(condition), wpm, snr_db)


def pair_bench(a, b):
    """(pairs, unpaired A points, unpaired B points, paired point keys)."""
    check_meta(a["meta"], b["meta"], "side B")
    common = sorted(set(a["points"]) & set(b["points"]), key=point_order)
    if not common:
        raise InputError("no point appears in both the A and the B reports")
    pairs = []
    for key in common:
        sa = _stations_by_key(a["points"][key], key, "A")
        sb = _stations_by_key(b["points"][key], key, "B")
        if set(sa) != set(sb):
            raise InputError(f"point {describe_point(key)}: the A and B stations differ"
                             " (not the same recordings)")
        seed, condition, wpm, snr_db = key
        for skey, st_a in sa.items():
            st_b = sb[skey]
            trial = skey[0]
            pairs.append(Pair(
                cluster=(seed, condition, wpm, trial),
                strata={"condition": condition, "wpm": wpm, "snr_db": snr_db, "trial": trial},
                a=station_outcome(st_a), b=station_outcome(st_b)))
    if not pairs:
        raise InputError("the paired points hold no stations")
    unpaired_a = len(set(a["points"]) - set(b["points"]))
    unpaired_b = len(set(b["points"]) - set(a["points"]))
    return pairs, unpaired_a, unpaired_b, common


def _stations_by_key(point, key, side):
    out = {}
    for st in point["per_station"]:
        skey = (st["trial"], st["call"], float(st["offset_hz"]))
        if skey in out:
            raise InputError(f"side {side} point {describe_point(key)} has station"
                             f" {st['call']} of trial {st['trial']} twice")
        out[skey] = st
    return out


def station_outcome(st):
    return (st["spotted"], st["cer"] < GOOD_COPY_CER)


def false_identities(a, b, common):
    ta = tb = shared = 0
    for key in common:
        ca = Counter(a["points"][key]["bogus_calls"])
        cb = Counter(b["points"][key]["bogus_calls"])
        ta += sum(ca.values())
        tb += sum(cb.values())
        shared += sum((ca & cb).values())
    return {"a": ta, "b": tb, "shared": shared, "either": ta + tb - shared}


# --------------------------------------------------------------- oracle input


def load_oracle(path, side):
    try:
        with open(path, encoding="utf-8") as f:
            lines = f.read().splitlines()
    except OSError as e:
        raise InputError(f"{path}: cannot read: {e.strerror}")
    except UnicodeDecodeError as e:
        raise InputError(f"{path}: not UTF-8: {e}")
    spots = {}
    for i, line in enumerate(lines, 1):
        if not line.strip():
            continue
        where = f"{path}:{i}"
        try:
            obj = json.loads(line)
        except json.JSONDecodeError as e:
            raise InputError(f"{where}: not JSON: {e}")
        _require(where, obj, "call", lambda v: isinstance(v, str), "a string")
        _require(where, obj, "khz", _is_num, "a number")
        _require(where, obj, "as_word", lambda v: isinstance(v, bool), "a boolean")
        _require(where, obj, "framed", lambda v: isinstance(v, bool), "a boolean")
        key = (obj["call"], float(obj["khz"]))
        if key in spots:
            raise InputError(f"side {side} has reference spot {key[0]} at {key[1]} kHz twice")
        spots[key] = (obj["as_word"], obj["framed"])
    if not spots:
        raise InputError(f"{path}: no reference spots")
    return spots


def pair_oracle(a, b):
    if set(a) != set(b):
        raise InputError(f"A and B are not the same reference spots"
                         f" (A only {len(set(a) - set(b))}, B only {len(set(b) - set(a))})")
    return [Pair(cluster=key, strata={}, a=a[key], b=b[key]) for key in sorted(a)]


# -------------------------------------------------------------------- tallies


def tally(pairs, level, key=None):
    """Counts at one level; with key, {stratum value: Counts} in sorted order."""
    if key is None:
        c = Counts()
        for p in pairs:
            c.add(p.a[level], p.b[level])
        return c
    out = {}
    for p in pairs:
        out.setdefault(p.strata[key], Counts()).add(p.a[level], p.b[level])
    order = (lambda v: condition_rank(v)) if key == "condition" else (lambda v: v)
    return {v: out[v] for v in sorted(out, key=order)}


def bootstrap(pairs, levels, resamples, seed=BOOTSTRAP_SEED):
    """{level: {stat: (lo, hi)}}, resampling whole clusters; {} when resamples is 0."""
    if resamples <= 0:
        return {}
    clusters = {}
    for p in pairs:
        row = clusters.setdefault(p.cluster, [0] * (1 + 3 * len(levels)))
        row[0] += 1
        for li in range(len(levels)):
            x, y = p.a[li], p.b[li]
            base = 1 + 3 * li
            if x and y:
                row[base] += 1
            elif x:
                row[base + 1] += 1
            elif y:
                row[base + 2] += 1
    rows = list(clusters.values())
    k = len(rows)
    width = len(rows[0])
    rng = random.Random(seed)
    stats = {lv: {"exactly_one": [], "headroom": [], "recall_a": [], "recall_b": []}
             for lv in levels}
    for _ in range(resamples):
        acc = [0] * width
        for _ in range(k):
            row = rows[int(rng.random() * k)]
            for j in range(width):
                acc[j] += row[j]
        for li, lv in enumerate(levels):
            base = 1 + 3 * li
            r = rates(acc[0], acc[base], acc[base + 1], acc[base + 2])
            for stat, vals in stats[lv].items():
                vals.append(r[stat])
    lo_i = int(0.025 * resamples)
    hi_i = max(lo_i, int(0.975 * resamples) - 1)
    out = {}
    for lv, by_stat in stats.items():
        out[lv] = {}
        for stat, vals in by_stat.items():
            vals.sort()
            out[lv][stat] = (vals[lo_i], vals[hi_i])
    return out


# --------------------------------------------------------------------- report


def side_labels(name_a, name_b):
    """Column labels and title halves; equal names get (A)/(B) appended."""
    if name_a == name_b:
        la, lb = f"{name_a} (A)", f"{name_b} (B)"
        return la, lb, la, lb
    # A name that is the other side's letter would head the wrong column; tag it.
    la = f"{name_a} (A)" if name_a == "B" else name_a
    lb = f"{name_b} (B)" if name_b == "A" else name_b
    ta = name_a if name_a == "A" else f"{name_a} (A)"
    tb = name_b if name_b == "B" else f"{name_b} (B)"
    return la, lb, ta, tb


def gate(overall, levels, bar_pp):
    met = [lv for lv in levels
           if 100 * min(overall[lv].a_only, overall[lv].b_only) >= bar_pp * overall[lv].n]
    return met


def build_report(mode, pairs, levels, names, bar_pp, resamples, extra):
    overall = {lv: tally(pairs, li) for li, lv in enumerate(levels)}
    breakdowns = {}
    if mode == "bench":
        for key, json_key, _, _, _ in BREAKDOWNS:
            breakdowns[key] = {lv: tally(pairs, li, key) for li, lv in enumerate(levels)}
    return {
        "mode": mode,
        "levels": levels,
        "names": names,
        "labels": side_labels(*names),
        "pairs": pairs,
        "overall": overall,
        "breakdowns": breakdowns,
        "ci95": bootstrap(pairs, levels, resamples),
        "resamples": resamples,
        "bar_pp": bar_pp,
        "met": gate(overall, levels, bar_pp),
        **extra,
    }


def table(header, rows):
    out = ["| " + " | ".join(header) + " |", "|" + "---|" * len(header)]
    out += ["| " + " | ".join(str(c) for c in row) + " |" for row in rows]
    return out


def render_markdown(rep):
    la, lb, ta, tb = rep["labels"]
    levels = rep["levels"]
    lines = []
    if rep["mode"] == "bench":
        meta = rep["meta"]
        lines.append(f"## Engine overlap: {ta} vs {tb}")
        lines.append("")
        seeds = rep["seeds"]
        seed_text = ("seed " if len(seeds) == 1 else "seeds ") + ", ".join(str(s) for s in seeds)
        lines.append(
            f"{rep['overall'][levels[0]].n} paired truth identities:"
            f" {plural(len(rep['common']), 'point', 'points')},"
            f" {plural(meta['trials'], 'trial', 'trials')} of {meta['stations_per_trial']} stations,"
            f" {seed_text}, {meta['duration_s']} s recordings.")
        lines.append(f"Unpaired points left out: {la} {rep['unpaired'][0]}, {lb} {rep['unpaired'][1]}.")
        lines.append(f"Identity: the station's call spotted within {meta['match_hz']} Hz of it."
                     f" Copy: its decoded copy has CER below {GOOD_COPY_CER:.2f}.")
    else:
        lines.append(f"## Engine overlap: {ta} vs {tb}, manta oracle reference spots")
        lines.append("")
        lines.append(f"{plural(rep['overall'][levels[0]].n, 'paired reference spot', 'paired reference spots')}.")
        lines.append("as_word: the reference call is a whole word of the decoded window."
                     " framed: it follows CQ, DE or CQ TEST there.")
        lines.append("The oracle scores only reference calls, so there is no false-identity count.")
    lines.append("Exactly one is the ticket's oracle-gap rate: identities one engine recovers"
                 " and the other misses.")
    lines.append("")
    lines.append("### Overall")
    lines.append("")
    rows = []
    for lv in levels:
        c = rep["overall"][lv]
        r = c.rates()
        rows.append([lv.capitalize() if rep["mode"] == "bench" else lv, c.n, c.both, c.a_only,
                     c.b_only, c.neither, pct(r["recall_a"]), pct(r["recall_b"]),
                     pct(r["exactly_one"]), pct(r["union_recall"]), pp(r["headroom"])])
    lines += table(["Level", "n", "Both", f"{la} only", f"{lb} only", "Neither",
                    f"{la} recall", f"{lb} recall", "Exactly one", "Union recall", "Headroom"],
                   rows)
    if rep["ci95"]:
        ci = rep["ci95"]
        l0, l1 = levels
        unit = ("signal-set (seed, condition, speed, trial)" if rep["mode"] == "bench"
                else "reference-spot")

        def span_pct(v):
            return f"{100 * v[0]:.2f}% to {100 * v[1]:.2f}%"

        def span_pp(v):
            return f"{100 * v[0]:.2f} to {100 * v[1]:.2f} pp"

        lines.append("")
        lines.append(
            f"95% intervals, {rep['resamples']} {unit} resamples:"
            f" exactly one ({l0}) {span_pct(ci[l0]['exactly_one'])};"
            f" headroom ({l0}) {span_pp(ci[l0]['headroom'])};"
            f" headroom ({l1}) {span_pp(ci[l1]['headroom'])};"
            f" {la} recall ({l0}) {span_pct(ci[l0]['recall_a'])};"
            f" {lb} recall ({l0}) {span_pct(ci[l0]['recall_b'])}.")
    if rep["mode"] == "bench":
        fi = rep["false_identities"]
        lines.append("")
        lines.append("### False identities")
        lines.append("")
        lines += table(["", la, lb, "Shared", "Either"],
                       [["Calls spotted that no station sent", fi["a"], fi["b"], fi["shared"],
                         fi["either"]]])
        lines.append("")
        lines.append("Each trial's distinct bogus calls, pooled per point as the bench reports them;"
                     " shared is the same call at the same point from both engines.")
        for key, _, heading, column, fmt in BREAKDOWNS:
            for lv in levels:
                lines.append("")
                lines.append(f"### {heading} ({lv})")
                lines.append("")
                rows = []
                for value, c in rep["breakdowns"][key][lv].items():
                    r = c.rates()
                    rows.append([fmt(value), c.n, pct(r["recall_a"]), pct(r["recall_b"]),
                                 c.a_only, c.b_only, pct(r["exactly_one"]), pp(r["headroom"])])
                lines += table([column, "n", f"{la} recall", f"{lb} recall", f"{la} only",
                                f"{lb} only", "Exactly one", "Headroom"], rows)
    lines.append("")
    lines.append("### Fusion gate")
    lines.append("")
    heads = ", ".join(f"{pp(rep['overall'][lv].rates()['headroom'])} ({lv})" for lv in levels)
    lines.append(f"Headroom over the better engine: {heads}. Bar: {rep['bar_pp']:.2f} pp.")
    if rep["met"]:
        lines.append(f"Reading: meets the bar at {' and '.join(rep['met'])}.")
    else:
        lines.append("Reading: below the bar at both levels.")
    return "\n".join(lines) + "\n"


def render_json(rep):
    levels = rep["levels"]
    out = {"mode": rep["mode"], "levels": list(levels)}
    for tag, name, side in (("a", rep["names"][0], rep.get("side_a")),
                            ("b", rep["names"][1], rep.get("side_b"))):
        entry = {"name": name}
        if side is not None:
            entry.update(side)
        out[tag] = entry
    out["bar_pp"] = rep["bar_pp"]
    out["bootstrap"] = {"resamples": rep["resamples"], "seed": BOOTSTRAP_SEED}
    if rep["mode"] == "bench":
        out["unpaired_points"] = {"a": rep["unpaired"][0], "b": rep["unpaired"][1]}
    out["overall"] = {lv: rep["overall"][lv].as_dict() for lv in levels}
    out["ci95"] = {lv: {stat: list(span) for stat, span in by_stat.items()}
                   for lv, by_stat in rep["ci95"].items()}
    out["false_identities"] = rep.get("false_identities")
    if rep["mode"] == "bench":
        for key, json_key, _, _, _ in BREAKDOWNS:
            rows = []
            first = rep["breakdowns"][key][levels[0]]
            for value in first:
                row = {key: value}
                for lv in levels:
                    row[lv] = rep["breakdowns"][key][lv][value].as_dict()
                rows.append(row)
            out[json_key] = rows
    out["gate"] = {f"{lv}_headroom_pp": 100 * rep["overall"][lv].rates()["headroom"]
                   for lv in levels}
    out["gate"]["bar_pp"] = rep["bar_pp"]
    out["gate"]["meets_bar"] = bool(rep["met"])
    out["gate"]["levels_meeting_bar"] = list(rep["met"])
    return out


# ----------------------------------------------------------------------- main


def run_bench(args):
    a = load_bench(args.a, "A")
    b = load_bench(args.b, "B")
    pairs, ua, ub, common = pair_bench(a, b)
    extra = {
        "meta": a["meta"],
        "seeds": sorted(set(a["seeds"]) | set(b["seeds"])),
        "unpaired": (ua, ub),
        "common": common,
        "false_identities": false_identities(a, b, common),
        "side_a": {"engine": a["engine"], "files": a["files"], "seeds": a["seeds"]},
        "side_b": {"engine": b["engine"], "files": b["files"], "seeds": b["seeds"]},
    }
    return build_report("bench", pairs, BENCH_LEVELS, (a["engine"], b["engine"]),
                        args.bar_pp, args.bootstrap, extra)


def run_oracle(args):
    a = load_oracle(args.a, "A")
    b = load_oracle(args.b, "B")
    pairs = pair_oracle(a, b)
    extra = {"side_a": {"files": [os.path.basename(args.a)]},
             "side_b": {"files": [os.path.basename(args.b)]}}
    return build_report("oracle", pairs, ORACLE_LEVELS, (args.a_name, args.b_name),
                        args.bar_pp, args.bootstrap, extra)


def parser():
    ap = argparse.ArgumentParser(
        prog="engine-overlap.py",
        description="How often one decode engine recovers a truth identity the other misses"
                    " (MAN-205).")
    sub = ap.add_subparsers(dest="mode", required=True)
    bench = sub.add_parser("bench", help="pair `manta bench sensitivity --json` reports")
    bench.add_argument("--a", nargs="+", action="extend", required=True, metavar="PATH",
                       help="engine A's reports")
    bench.add_argument("--b", nargs="+", action="extend", required=True, metavar="PATH",
                       help="engine B's reports")
    oracle = sub.add_parser("oracle", help="pair two `manta oracle --jsonl` files")
    oracle.add_argument("--a", required=True, metavar="PATH", help="engine A's JSONL")
    oracle.add_argument("--b", required=True, metavar="PATH", help="engine B's JSONL")
    oracle.add_argument("--a-name", default="A", help="engine A's label (default A)")
    oracle.add_argument("--b-name", default="B", help="engine B's label (default B)")
    for p in (bench, oracle):
        p.add_argument("--bar-pp", type=float, default=DEFAULT_BAR_PP,
                       help="fusion-gate headroom bar in percentage points (default 1.0)")
        p.add_argument("--bootstrap", type=int, default=DEFAULT_RESAMPLES, metavar="N",
                       help="bootstrap resamples for the 95%% intervals (default 2000, 0 off)")
        p.add_argument("--json", metavar="OUT", help="also write the numbers as JSON")
    return ap


def main(argv=None):
    args = parser().parse_args(argv)
    try:
        if not (math.isfinite(args.bar_pp) and args.bar_pp > 0):
            raise InputError(f"--bar-pp must be finite and positive, got {args.bar_pp}")
        if args.bootstrap < 0:
            raise InputError(f"--bootstrap must be 0 or more, got {args.bootstrap}")
        rep = run_bench(args) if args.mode == "bench" else run_oracle(args)
        text = render_markdown(rep)
        if args.json:
            try:
                with open(args.json, "w", encoding="utf-8") as f:
                    json.dump(render_json(rep), f, indent=2)
                    f.write("\n")
            except OSError as e:
                raise InputError(f"{args.json}: cannot write: {e.strerror}")
    except InputError as e:
        print(f"engine-overlap.py: error: {e}", file=sys.stderr)
        return 2
    sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
