#!/usr/bin/env python3
"""Compare a secondary (shadow) manta node's spots with the primary skimmer's
Reverse Beacon Network (RBN) spots over a UTC window (MAN-96, scenario 2).

The node's spots come from its own `manta record-spots` archive (SpotMessage
JSON Lines, files or directories of `spots-*.jsonl`). The primary's spots come
from RBN's public daily archive
(`data.reversebeacon.net/rbn_history/<YYYYMMDD>.zip`, one CSV per day, header
`callsign,de_pfx,de_cont,freq,band,dx,dx_pfx,dx_cont,mode,db,date,speed,tx_mode`,
`freq` in kHz, `date` in UTC), filtered by the primary's spotter callsign.
The output is markdown meant to be pasted verbatim into the field report;
`--json OUT` also writes the same numbers as JSON.

Defaults and rationale (decision D-H,
docs/DECISIONS/2026-10-07-man96-secondary-skimmer-field-node.md):

  --freq-tol-hz 500   same tolerance as scripts/score-against-rbn.py.
  --time-tol-s 600    manta's dedupe SUPPRESSION_SECONDS: the longest gap
                      between two spots of a continuing station, so a re-spot
                      at a different phase is not miscounted as disagreement.
  CW rows only        RBN rows with a `tx_mode` column other than "CW" (FT8,
                      RTTY, ...) are dropped before anything else.
  --passband-khz      required: only spots inside the node's passband(s) can
                      be compared fairly, on both sides.

Self-exclusion rule: every `deCall` seen in the node's spots, both as-is and
without its `-N` SSID suffix (e.g. `N0CALL-1` and `N0CALL`), plus every
`--exclude-spotter`, never counts as a corroborating RBN spotter. Otherwise the
node's own spots, forwarded to RBN, would corroborate themselves.

Matching reuses `_is_plausible_callsign`, `dedup_truth` and `match` from
scripts/score-against-rbn.py (callsign + nearest kHz bin +/-1 + frequency and
time tolerances). Three passes:
  1. node -> primary: node spots confirmed by the primary;
  2. primary -> node: primary spots heard by the node;
  3. node-only -> other RBN spotters: corroborated vs uncorroborated.
Primary rows whose callsign manta's grammar can never accept (e.g.
`EA8/TF3CW`) are counted separately, not in any denominator.

The tool records the comparison; it does not pass or fail anything. The
Stage 1 go/no-go (D-I: <= 10% of node spots uncorroborated, with >= 20 node
spots) is read by the operator from the "Uncorroborated" line in the
`## Agreement` section; see docs/RUNBOOKS/secondary-skimmer-field-node.md.
`--missed-spots N` (the `field-node.py report` spots_missed count) adds the
spots the recorder missed to that line as uncorroborated, so a recorder gap
cannot hide false spots from the gate. `--missed-spots indeterminate` (a
restart in the window, or no sample before it) makes the line INDETERMINATE:
D-I cannot GO.

Calibration deltas are node minus primary on matched pairs (the nearest
primary row in time among the candidates). SNR is compared at the 500 Hz
RBN reference: node `snr` + 10*log10(snrRefHz/500). p90 uses the
nearest-rank method.

Exit codes: 0 on success, 2 on a usage or input error.
"""
import argparse
import contextlib
import csv
import hashlib
import importlib.util
import io
import json
import math
import os
import re
import sys
import zipfile
from collections import Counter
from datetime import datetime, timedelta, timezone

HERE = os.path.dirname(os.path.abspath(__file__))
_spec = importlib.util.spec_from_file_location(
    "score_against_rbn", os.path.join(HERE, "score-against-rbn.py"))
scorer = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(scorer)

RBN_REQUIRED_COLUMNS = ("callsign", "freq", "dx", "date")
SNR_REF_HZ = 500.0
DEFAULT_SNR_REF_HZ = 2500.0
SSID_RE = re.compile(r"-\d+$")

# Amateur band edges in kHz, for the per-band table.
BANDS = (
    ("160m", 1800.0, 2000.0), ("80m", 3500.0, 4000.0), ("60m", 5250.0, 5450.0),
    ("40m", 7000.0, 7300.0), ("30m", 10100.0, 10150.0), ("20m", 14000.0, 14350.0),
    ("17m", 18068.0, 18168.0), ("15m", 21000.0, 21450.0), ("12m", 24890.0, 24990.0),
    ("10m", 28000.0, 29700.0), ("6m", 50000.0, 54000.0),
)


class InputError(Exception):
    """An unreadable or malformed input; reported on stderr, exit 2."""


class _Parser(argparse.ArgumentParser):
    def error(self, message):
        self.print_usage(sys.stderr)
        sys.stderr.write("%s: error: %s\n" % (self.prog, message))
        raise SystemExit(2)


def band_of(freq_hz):
    khz = freq_hz / 1000.0
    for name, lo, hi in BANDS:
        if lo <= khz <= hi:
            return name
    return "other"


def strip_ssid(call):
    return SSID_RE.sub("", call)


def parse_iso(s):
    try:
        return scorer.parse_iso(s)
    except ValueError:
        raise argparse.ArgumentTypeError("not an ISO-8601 time: %r" % s)


def parse_passband(s):
    parts = s.split("-")
    if len(parts) != 2:
        raise argparse.ArgumentTypeError("passband must be LO-HI in kHz, got %r" % s)
    try:
        lo, hi = float(parts[0]), float(parts[1])
    except ValueError:
        raise argparse.ArgumentTypeError("passband must be LO-HI in kHz, got %r" % s)
    if not (math.isfinite(lo) and math.isfinite(hi)) or lo >= hi:
        raise argparse.ArgumentTypeError("passband needs finite LO < HI, got %r" % s)
    return (lo, hi)


def non_negative_int(s):
    v = int(s)
    if v < 0:
        raise argparse.ArgumentTypeError("must be >= 0, got %r" % s)
    return v


def missed_spots_arg(s):
    """--missed-spots: a count, or `indeterminate` (field-node.py report
    could not count them: manta restarted in the window) -> None."""
    if s == "indeterminate":
        return None
    return non_negative_int(s)


def sha256_of(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def in_passband(freq_hz, passbands):
    khz = freq_hz / 1000.0
    return any(lo <= khz <= hi for lo, hi in passbands)


def in_window(t, start, end):
    return start <= t < end


def _num(v):
    if v is None:
        return None
    v = str(v).strip()
    if not v:
        return None
    try:
        x = float(v)
    except ValueError:
        return None
    return x if math.isfinite(x) else None


# ---------------------------------------------------------------- loading

def expand_paths(paths, dir_glob_suffixes, dir_prefix=""):
    """Files as given; directories expand to their matching files, sorted.
    The same file named twice (directly or via a directory) is read once."""
    out, seen = [], set()
    for p in paths:
        if os.path.isdir(p):
            names = sorted(
                n for n in os.listdir(p)
                if n.startswith(dir_prefix) and n.lower().endswith(dir_glob_suffixes))
            if not names:
                raise InputError("directory has no matching files: %s" % p)
            cands = [os.path.join(p, n) for n in names]
        elif os.path.isfile(p):
            cands = [p]
        else:
            raise InputError("no such file or directory: %s" % p)
        for c in cands:
            key = os.path.realpath(c)
            if key not in seen:
                seen.add(key)
                out.append(c)
    return out


def load_node_spots(paths):
    """SpotMessage JSONL -> (spots, file_infos, malformed_count).

    Only a file's final line with no newline (a write torn by a kill) may be
    malformed. Any other malformed record would silently leave the
    uncorroborated share D-I reads, so it is an input error."""
    spots, infos, malformed = [], [], 0
    for path in expand_paths(paths, (".jsonl",), dir_prefix="spots-"):
        rows = 0
        try:
            f = open(path, encoding="utf-8")
        except OSError as e:
            raise InputError("cannot read %s: %s" % (path, e))
        with f:
            torn = None
            for raw in f:
                line = raw.strip()
                if not line:
                    continue
                if torn is not None:
                    raise InputError("%s line %d: malformed node spot record" % (path, torn))
                rows += 1
                try:
                    d = json.loads(line)
                    ts = float(d["timestamp"])
                    freq = float(d["frequency"])
                    call = str(d["dxCall"]).upper()
                    de = str(d.get("deCall") or "").upper()
                    when = datetime.fromtimestamp(ts, tz=timezone.utc)
                except (ValueError, KeyError, TypeError, OverflowError, OSError):
                    if raw.endswith("\n"):
                        raise InputError("%s line %d: malformed node spot record" % (path, rows))
                    malformed += 1  # torn final line; an error if another line follows
                    torn = rows
                    continue
                snr = _num(d.get("snr"))
                ref = _num(d.get("snrRefHz")) or DEFAULT_SNR_REF_HZ
                snr500 = None if snr is None else snr + 10.0 * math.log10(ref / SNR_REF_HZ)
                spots.append({
                    "callsign": call,
                    "freq_hz": freq,
                    "time": when,
                    "snr500": snr500,
                    "wpm": _num(d.get("wpm")),
                    "confidence": _num(d.get("decodeConfidence")),
                    "de_call": de,
                    "decoder_version": d.get("decoderVersion"),
                })
        infos.append({"kind": "node", "path": path, "rows": rows, "sha256": sha256_of(path)})
    return spots, infos, malformed


def _open_rbn_texts(path):
    """Yields text streams for one RBN input: a .csv, or every .csv in a .zip."""
    if path.lower().endswith(".zip"):
        try:
            zf = zipfile.ZipFile(path)
        except (zipfile.BadZipFile, OSError) as e:
            raise InputError("cannot read zip %s: %s" % (path, e))
        with zf:
            members = sorted(n for n in zf.namelist() if n.lower().endswith(".csv"))
            if not members:
                raise InputError("zip holds no .csv: %s" % path)
            for m in members:
                with zf.open(m) as raw:
                    yield io.TextIOWrapper(raw, encoding="utf-8", errors="replace", newline="")
    else:
        try:
            f = open(path, encoding="utf-8", errors="replace", newline="")
        except OSError as e:
            raise InputError("cannot read %s: %s" % (path, e))
        with f:
            yield f


def load_rbn(paths, passbands, start, end, margin_s=0.0):
    """Streams RBN rows, filtering CW (when `tx_mode` exists), then passband,
    then window. Rows outside [start, end) but within `margin_s` of it are
    still returned, marked `in_window: False`: `compare` uses them only as
    match references, so a spot near a window edge is not counted as
    unconfirmed for want of a row just outside the window. They are counted
    under `out_of_window` like any other row outside the window.
    Returns (rows, file_infos, filter_counts)."""
    margin = timedelta(seconds=margin_s)
    kept, infos = [], []
    counts = Counter()
    for path in expand_paths(paths, (".csv", ".zip")):
        rows = 0
        for text in _open_rbn_texts(path):
            reader = csv.DictReader(text)
            fields = reader.fieldnames or []
            missing = [c for c in RBN_REQUIRED_COLUMNS if c not in fields]
            if missing:
                raise InputError("%s: RBN CSV lacks column(s) %s" % (path, ", ".join(missing)))
            has_tx_mode = "tx_mode" in fields
            for row in reader:
                rows += 1
                if has_tx_mode and (row.get("tx_mode") or "").strip().upper() != "CW":
                    counts["non_cw"] += 1
                    continue
                freq_khz = _num(row.get("freq"))
                if freq_khz is None:
                    counts["malformed"] += 1
                    continue
                freq_hz = freq_khz * 1000.0
                if not in_passband(freq_hz, passbands):
                    counts["out_of_passband"] += 1
                    continue
                try:
                    t = datetime.strptime((row.get("date") or "").strip(),
                                          "%Y-%m-%d %H:%M:%S").replace(tzinfo=timezone.utc)
                except ValueError:
                    counts["malformed"] += 1
                    continue
                inside = in_window(t, start, end)
                if not inside:
                    counts["out_of_window"] += 1
                    if not in_window(t, start - margin, end + margin):
                        continue
                else:
                    counts["kept"] += 1
                spotter = (row.get("callsign") or "").strip().upper()
                if spotter.endswith("-#"):   # RBN's telnet skimmer marker, if a dump carries it
                    spotter = spotter[:-2]
                kept.append({
                    "spotter": spotter,
                    "callsign": (row.get("dx") or "").strip().upper(),
                    "freq_hz": freq_hz,
                    "time": t,
                    "db": _num(row.get("db")),
                    "wpm": _num(row.get("speed")),
                    "in_window": inside,
                })
        counts["read"] += rows
        infos.append({"kind": "rbn", "path": path, "rows": rows, "sha256": sha256_of(path)})
    return kept, infos, counts


# ---------------------------------------------------------------- comparison

def _pct(n, d):
    return None if not d else 100.0 * n / d


def _fmt_pct(p):
    return "n/a" if p is None else "%.1f%%" % p


def _p90(sorted_vals):
    # Nearest-rank p90.
    return sorted_vals[max(0, int(math.ceil(0.9 * len(sorted_vals))) - 1)]


def _median(sorted_vals):
    n = len(sorted_vals)
    mid = n // 2
    return sorted_vals[mid] if n % 2 else (sorted_vals[mid - 1] + sorted_vals[mid]) / 2.0


def _stats(vals):
    if not vals:
        return {"n": 0, "median": None, "median_abs": None, "p90_abs": None}
    s = sorted(vals)
    a = sorted(abs(v) for v in vals)
    return {"n": len(vals), "median": _median(s), "median_abs": _median(a), "p90_abs": _p90(a)}


def nearest_primary(node, primary_by_key, freq_tol_hz, time_tol_s):
    best, best_rank = None, None
    for df_khz in (0, -1, 1):
        key = (node["callsign"], round(node["freq_hz"] / 1000.0) + df_khz)
        for t in primary_by_key.get(key, ()):
            df = abs(t["freq_hz"] - node["freq_hz"])
            dt = abs((t["time"] - node["time"]).total_seconds())
            if df > freq_tol_hz or dt > time_tol_s:
                continue
            rank = (dt, df)
            if best_rank is None or rank < best_rank:
                best, best_rank = t, rank
    return best


def compare(node_all, rbn_rows, primary, passbands, start, end,
            freq_tol_hz, time_tol_s, extra_excludes, missed_spots=0):
    primary = primary.upper()
    node_calls = sorted({s["de_call"] for s in node_all if s["de_call"]})
    auto_excl = set()
    for c in node_calls:
        auto_excl.add(c)
        auto_excl.add(strip_ssid(c))
    manual_excl = {c.upper() for c in extra_excludes}
    excluded_spotters = auto_excl | manual_excl

    # Spots and rows within the time tolerance of the window edges are match
    # REFERENCES only: they are never counted as compared spots themselves.
    margin = timedelta(seconds=time_tol_s)
    node_counts = Counter()
    node, node_ref = [], []
    for s in node_all:
        if not in_passband(s["freq_hz"], passbands):
            node_counts["out_of_passband"] += 1
        elif not in_window(s["time"], start, end):
            node_counts["out_of_window"] += 1
            if in_window(s["time"], start - margin, end + margin):
                node_ref.append(s)
        else:
            node.append(s)
            node_ref.append(s)
    node.sort(key=lambda s: (s["time"], s["freq_hz"], s["callsign"]))
    node_ref.sort(key=lambda s: (s["time"], s["freq_hz"], s["callsign"]))

    primary_rows, primary_ref, corroborators = [], [], []
    grammar_excluded = 0
    own_rows = 0
    for r in rbn_rows:
        inside = r.get("in_window", True)
        if r["spotter"] == primary:
            if scorer._is_plausible_callsign(r["callsign"]):
                primary_ref.append(r)
                if inside:
                    primary_rows.append(r)
            elif inside:
                grammar_excluded += 1
        elif r["spotter"] in excluded_spotters:
            if inside:
                own_rows += 1
        else:
            corroborators.append(r)
    primary_rows.sort(key=lambda r: (r["time"], r["freq_hz"], r["callsign"]))
    primary_ref.sort(key=lambda r: (r["time"], r["freq_hz"], r["callsign"]))

    primary_by_key = scorer.dedup_truth(primary_ref)
    confirmed, node_only, _ = scorer.match(node, primary_by_key, freq_tol_hz, time_tol_s)
    heard, missed, _ = scorer.match(primary_rows, scorer.dedup_truth(node_ref), freq_tol_hz, time_tol_s)
    corroborated, uncorroborated, _ = scorer.match(
        node_only, scorer.dedup_truth(corroborators), freq_tol_hz, time_tol_s)

    dfs, dsnr, dwpm = [], [], []
    for s in confirmed:
        p = nearest_primary(s, primary_by_key, freq_tol_hz, time_tol_s)
        if p is None:
            continue
        dfs.append(s["freq_hz"] - p["freq_hz"])
        if s["snr500"] is not None and p["db"] is not None:
            dsnr.append(s["snr500"] - p["db"])
        if s["wpm"] is not None and p["wpm"] is not None:
            dwpm.append(s["wpm"] - p["wpm"])

    conf_ids = {id(s) for s in confirmed}
    corr_ids = {id(s) for s in corroborated}
    heard_ids = {id(r) for r in heard}

    def bucketed(keyfn):
        table = {}

        def row(k):
            return table.setdefault(k, Counter(node_spots=0, confirmed=0, corroborated=0,
                                               uncorroborated=0, primary_spots=0, primary_heard=0))
        for s in node:
            c = row(keyfn(s))
            c["node_spots"] += 1
            if id(s) in conf_ids:
                c["confirmed"] += 1
            elif id(s) in corr_ids:
                c["corroborated"] += 1
            else:
                c["uncorroborated"] += 1
        for r in primary_rows:
            c = row(keyfn(r))
            c["primary_spots"] += 1
            if id(r) in heard_ids:
                c["primary_heard"] += 1
        return {k: dict(v) for k, v in sorted(table.items())}

    band_order = {name: i for i, (name, _, _) in enumerate(BANDS)}
    by_band_raw = bucketed(lambda x: band_of(x["freq_hz"]))
    by_band = {k: by_band_raw[k] for k in sorted(by_band_raw, key=lambda b: band_order.get(b, 99))}
    by_day = bucketed(lambda x: x["time"].strftime("%Y-%m-%d"))

    n_node, n_primary = len(node), len(primary_rows)
    missed_counts = Counter(r["callsign"] for r in missed)
    missed_bins = {}
    for r in missed:
        missed_bins.setdefault(r["callsign"], set()).add(int(round(r["freq_hz"] / 1000.0)))
    missed_top = sorted(missed_counts.items(), key=lambda kv: (-kv[1], kv[0]))

    versions = Counter(str(s["decoder_version"]) for s in node_all if s["decoder_version"])

    return {
        "primary": primary,
        "excluded_spotters": {"auto_from_node_deCall": sorted(auto_excl),
                              "manual": sorted(manual_excl)},
        "node_filter": {"loaded": len(node_all), "out_of_passband": node_counts["out_of_passband"],
                        "out_of_window": node_counts["out_of_window"], "compared": n_node},
        "decoder_versions": dict(sorted(versions.items())),
        "agreement": {
            "node_spots": n_node,
            "node_confirmed_by_primary": len(confirmed),
            "node_confirmed_pct": _pct(len(confirmed), n_node),
            "primary_spots": n_primary,
            "primary_heard_by_node": len(heard),
            "primary_heard_pct": _pct(len(heard), n_primary),
            "node_only": len(node_only),
            "node_only_corroborated": len(corroborated),
            "node_only_corroborated_pct": _pct(len(corroborated), n_node),
            "uncorroborated": len(uncorroborated),
            "uncorroborated_pct": _pct(len(uncorroborated), n_node),
            # D-I input: spots the node emitted but record-spots missed
            # (`field-node.py report`'s spots_missed) could be false, so
            # they count as uncorroborated node spots.
            # An indeterminate count (None) leaves D-I without a number:
            # the gate fails closed rather than reading the archive alone.
            "missed_spots": "indeterminate" if missed_spots is None else missed_spots,
            "d_i_indeterminate": missed_spots is None,
            "d_i_node_spots": None if missed_spots is None else n_node + missed_spots,
            "d_i_uncorroborated": None if missed_spots is None else len(uncorroborated) + missed_spots,
            "d_i_uncorroborated_pct": (None if missed_spots is None else
                                       _pct(len(uncorroborated) + missed_spots, n_node + missed_spots)),
            "primary_rows_grammar_excluded": grammar_excluded,
            "rbn_rows_from_excluded_spotters": own_rows,
        },
        "calibration": {"freq_hz": _stats(dfs), "snr_db_500hz": _stats(dsnr), "wpm": _stats(dwpm)},
        "by_band": by_band,
        "by_day": by_day,
        "uncorroborated_spots": [{
            "time": s["time"].strftime("%Y-%m-%dT%H:%M:%SZ"),
            "dxCall": s["callsign"],
            "freq_khz": round(s["freq_hz"] / 1000.0, 2),
            "snr_db_500hz": None if s["snr500"] is None else round(s["snr500"], 1),
            "wpm": s["wpm"],
            "confidence": s["confidence"],
        } for s in uncorroborated],
        "primary_missed_calls": [{
            "dxCall": call, "count": n, "khz_bins": sorted(missed_bins[call]),
        } for call, n in missed_top],
    }


# ---------------------------------------------------------------- rendering

def _fmt_num(v, digits=1):
    return "n/a" if v is None else ("%." + str(digits) + "f") % v


def _iso(dt):
    return dt.strftime("%Y-%m-%dT%H:%M:%SZ")


def render_markdown(res, args, files, rbn_counts, node_malformed, show):
    a = res["agreement"]
    out = []
    w = out.append
    w("# Shadow comparison: node vs primary `%s`" % res["primary"])
    w("")
    w("## Inputs")
    w("")
    w("- Window (UTC): %s to %s (start inclusive, end exclusive)" % (_iso(args.start), _iso(args.end)))
    w("- Primary RBN spotter: %s" % res["primary"])
    w("- Passband(s): %s kHz" % ", ".join("%g-%g" % pb for pb in args.passband_khz))
    w("- Tolerances: freq %g Hz, time %g s" % (args.freq_tol_hz, args.time_tol_s))
    w("- Excluded as corroborators (node deCall, with and without SSID): %s"
      % (", ".join(res["excluded_spotters"]["auto_from_node_deCall"]) or "none"))
    w("- Excluded as corroborators (--exclude-spotter): %s"
      % (", ".join(res["excluded_spotters"]["manual"]) or "none"))
    dv = res["decoder_versions"]
    w("- decoderVersion values seen: %s"
      % (", ".join("%s (%d)" % kv for kv in dv.items()) or "none"))
    nf = res["node_filter"]
    w("- Node spots: %d loaded, %d malformed lines skipped, %d out of passband, %d out of window, %d compared"
      % (nf["loaded"], node_malformed, nf["out_of_passband"], nf["out_of_window"], nf["compared"]))
    w("- RBN rows: %d read, %d non-CW, %d malformed, %d out of passband, %d out of window, %d kept"
      % (rbn_counts["read"], rbn_counts["non_cw"], rbn_counts["malformed"],
         rbn_counts["out_of_passband"], rbn_counts["out_of_window"], rbn_counts["kept"]))
    w("")
    w("| Kind | File | Rows | SHA-256 |")
    w("|---|---|---:|---|")
    for fi in files:
        w("| %s | `%s` | %d | `%s` |" % (fi["kind"], fi["path"], fi["rows"], fi["sha256"]))
    w("")
    w("## Agreement")
    w("")
    w("- Node spots confirmed by primary: %d / %d node spots (%s)"
      % (a["node_confirmed_by_primary"], a["node_spots"], _fmt_pct(a["node_confirmed_pct"])))
    w("- Primary spots heard by node: %d / %d primary spots (%s)"
      % (a["primary_heard_by_node"], a["primary_spots"], _fmt_pct(a["primary_heard_pct"])))
    w("- Node-only spots corroborated by another RBN spotter: %d / %d node spots (%s)"
      % (a["node_only_corroborated"], a["node_spots"], _fmt_pct(a["node_only_corroborated_pct"])))
    if a["d_i_indeterminate"]:
        w("- **Uncorroborated: INDETERMINATE (%d / %d archived node spots, but the spots the "
          "recorder missed cannot be counted; see field-node.py report). D-I cannot GO.**"
          % (a["uncorroborated"], a["node_spots"]))
    elif a["missed_spots"]:
        w("- **Uncorroborated: %d / %d node spots (%s), counting %d spots the recorder missed "
          "as uncorroborated**"
          % (a["d_i_uncorroborated"], a["d_i_node_spots"], _fmt_pct(a["d_i_uncorroborated_pct"]),
             a["missed_spots"]))
    else:
        w("- **Uncorroborated: %d / %d node spots (%s)**"
          % (a["uncorroborated"], a["node_spots"], _fmt_pct(a["uncorroborated_pct"])))
    w("")
    w("The Uncorroborated line is the D-I Stage 1 input (GO needs <= 10% of node spots "
      "uncorroborated, with >= 20 node spots). This tool does not judge it. Pass "
      "`--missed-spots` the `field-node.py report` spots_missed count, or a recorder gap can "
      "hide false spots.")
    w("")
    w("Primary rows excluded (callsign shape manta's grammar never accepts, not in any "
      "denominator): %d. RBN rows from excluded spotters (never corroborate): %d."
      % (a["primary_rows_grammar_excluded"], a["rbn_rows_from_excluded_spotters"]))
    w("")
    w("## Calibration")
    w("")
    w("Node minus primary on matched pairs (nearest primary row in time). SNR at the 500 Hz RBN reference.")
    w("")
    w("| Quantity | Pairs | Median | Median abs | p90 abs |")
    w("|---|---:|---:|---:|---:|")
    for label, key, digits in (("Freq delta (Hz)", "freq_hz", 1),
                               ("SNR delta (dB, 500 Hz)", "snr_db_500hz", 2),
                               ("WPM delta", "wpm", 1)):
        st = res["calibration"][key]
        w("| %s | %d | %s | %s | %s |" % (label, st["n"], _fmt_num(st["median"], digits),
                                         _fmt_num(st["median_abs"], digits),
                                         _fmt_num(st["p90_abs"], digits)))
    for title, table in (("## By band", res["by_band"]), ("## By UTC day", res["by_day"])):
        w("")
        w(title)
        w("")
        w("| %s | Node spots | Confirmed | Corroborated | Uncorroborated | Uncorroborated %% "
          "| Primary spots | Primary heard |" % ("Band" if "band" in title else "Day"))
        w("|---|---:|---:|---:|---:|---:|---:|---:|")
        if not table:
            w("| (none) | 0 | 0 | 0 | 0 | n/a | 0 | 0 |")
        for k, c in table.items():
            w("| %s | %d | %d | %d | %d | %s | %d | %d |" % (
                k, c["node_spots"], c["confirmed"], c["corroborated"], c["uncorroborated"],
                _fmt_pct(_pct(c["uncorroborated"], c["node_spots"])),
                c["primary_spots"], c["primary_heard"]))
    w("")
    w("## Node-only, uncorroborated (likely busted or unique)")
    w("")
    unc = res["uncorroborated_spots"]
    w("Showing %d of %d." % (min(show, len(unc)), len(unc)))
    if unc and show:
        w("")
        w("| Time (UTC) | Call | kHz | SNR (dB, 500 Hz) | WPM | Confidence |")
        w("|---|---|---:|---:|---:|---:|")
        for s in unc[:show]:
            w("| %s | %s | %.2f | %s | %s | %s |" % (
                s["time"], s["dxCall"], s["freq_khz"], _fmt_num(s["snr_db_500hz"]),
                _fmt_num(s["wpm"], 0), _fmt_num(s["confidence"], 2)))
    w("")
    w("## Primary heard, node missed")
    w("")
    mc = res["primary_missed_calls"]
    w("Showing %d of %d calls (%d primary spots)."
      % (min(show, len(mc)), len(mc), sum(m["count"] for m in mc)))
    if mc and show:
        w("")
        w("| Call | Primary spots | kHz bins |")
        w("|---|---:|---|")
        for m in mc[:show]:
            w("| %s | %d | %s |" % (m["dxCall"], m["count"], ", ".join(str(b) for b in m["khz_bins"])))
    return "\n".join(out) + "\n"


# ---------------------------------------------------------------- CLI

def build_parser():
    ap = _Parser(prog="shadow-compare.py", description=__doc__,
                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--node-spots", nargs="+", required=True, metavar="PATH",
                    help="node SpotMessage JSONL files, or directories of spots-*.jsonl")
    ap.add_argument("--rbn", nargs="+", required=True, metavar="PATH",
                    help="RBN daily dumps: .csv, or .zip holding one CSV (or directories of them)")
    ap.add_argument("--primary", required=True, metavar="CALL",
                    help="the primary skimmer's RBN spotter callsign")
    ap.add_argument("--passband-khz", action="append", required=True, type=parse_passband,
                    metavar="LO-HI", help="node passband in kHz (repeatable, required)")
    ap.add_argument("--start", required=True, type=parse_iso, metavar="ISO",
                    help="window start, ISO-8601 (naive = UTC), inclusive")
    ap.add_argument("--end", required=True, type=parse_iso, metavar="ISO",
                    help="window end, ISO-8601 (naive = UTC), exclusive")
    ap.add_argument("--freq-tol-hz", type=scorer._finite("--freq-tol-hz", min_inclusive=0),
                    default=500.0, help="default 500 Hz, as score-against-rbn.py (D-H)")
    ap.add_argument("--time-tol-s", type=scorer._finite("--time-tol-s", min_inclusive=0),
                    default=600.0,
                    help="default 600 s, manta's dedupe SUPPRESSION_SECONDS (D-H)")
    ap.add_argument("--exclude-spotter", action="append", default=[], metavar="CALL",
                    help="also never count this RBN spotter as a corroborator (repeatable)")
    ap.add_argument("--missed-spots", type=missed_spots_arg, default=0, metavar="N",
                    help="spots the node emitted that record-spots missed (`field-node.py report`'s "
                         "spots_missed), counted as uncorroborated in the D-I line; `indeterminate` "
                         "when the report could not count them, which makes D-I fail closed (default 0)")
    ap.add_argument("--show", type=non_negative_int, default=20,
                    help="rows shown in the two listing sections (default 20)")
    ap.add_argument("--json", metavar="OUT", help="also write the numbers as JSON to OUT")
    return ap


def run(argv, stdout=None, stderr=None):
    stdout = stdout or sys.stdout
    stderr = stderr or sys.stderr
    ap = build_parser()
    try:
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            args = ap.parse_args(argv)
    except SystemExit as e:
        return e.code if isinstance(e.code, int) else 2
    if args.end <= args.start:
        stderr.write("shadow-compare.py: error: --end must be after --start\n")
        return 2
    try:
        node_all, node_files, node_malformed = load_node_spots(args.node_spots)
        rbn_rows, rbn_files, rbn_counts = load_rbn(args.rbn, args.passband_khz, args.start, args.end,
                                                   args.time_tol_s)
    except InputError as e:
        stderr.write("shadow-compare.py: error: %s\n" % e)
        return 2
    res = compare(node_all, rbn_rows, args.primary, args.passband_khz, args.start, args.end,
                  args.freq_tol_hz, args.time_tol_s, args.exclude_spotter, args.missed_spots)
    files = node_files + rbn_files
    stdout.write(render_markdown(res, args, files, rbn_counts, node_malformed, args.show))
    if args.json:
        summary = dict(res)
        summary["inputs"] = {
            "start": _iso(args.start), "end": _iso(args.end),
            "passbands_khz": [list(pb) for pb in args.passband_khz],
            "freq_tol_hz": args.freq_tol_hz, "time_tol_s": args.time_tol_s,
            "files": files, "node_malformed_lines": node_malformed,
            "rbn_rows": {k: rbn_counts[k] for k in
                         ("read", "non_cw", "malformed", "out_of_passband", "out_of_window", "kept")},
        }
        try:
            with open(args.json, "w", encoding="utf-8") as f:
                json.dump(summary, f, indent=2, sort_keys=True)
                f.write("\n")
        except OSError as e:
            stderr.write("shadow-compare.py: error: cannot write %s: %s\n" % (args.json, e))
            return 2
    return 0


def main():
    sys.exit(run(sys.argv[1:]))


if __name__ == "__main__":
    main()
