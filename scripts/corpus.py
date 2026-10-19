#!/usr/bin/env python3
"""Fetch and score the real-audio test corpus (MAN-119).

The corpus is listed in `crates/manta-testkit/audio-corpus/manifest.json`.
Each recording entry has `file`, `kind` ("iq" for a stereo IQ WAV, "audio"
for a rig-audio WAV), `sha256` (64 lowercase hex, or null while the file is
unpinned) and `sample_rate_hz`. IQ entries also have `bits_per_sample` and
`center_freq_hz` (null when unknown). An entry may carry `capture_start`
(ISO-8601 UTC), `truth` and `floors`:

  truth   {"kind": "rbn", "path": ..., "spotter": CALL}: an RBN daily-dump
          CSV, scored by score-against-rbn.py's `score_report`;
          {"kind": "transcript", "path": ...}: a hand transcript, scored by
          score-transcript.py. `path` is relative to the manifest's directory.
  floors  {"min_spots": N, "min_precision": P, "min_recall": R}, any subset.
          min_spots counts the run's spots. With RBN truth, precision and
          recall are the all-RBN score; the reference-spotter-only score is
          reported but never gated. With transcript truth, min_precision is
          spot precision and min_recall is call recall.

fetch downloads every pinned entry from <base-url>/<file> into --dest,
hashing as it streams, and keeps it only when the SHA-256 matches the pin.
A file already in --dest with the pinned hash is not downloaded again.
Unpinned entries are never fetched. The base URL is --base-url, else
$MANTA_CORPUS_BASE_URL; any URL urllib opens works (https://, http://,
file://). Exit status: 0 when every pinned entry is present and verified,
1 when a download failed or a hash did not match (the partial file is
deleted), 2 on a usage error such as no base URL.

score decodes every manifest recording present in --corpus-dir: `manta
decode <file> --json [--center-freq-hz C]` for IQ, `manta run --source
<file> --json` for audio. It scores each against its truth, prints a
Markdown table (also appended to $GITHUB_STEP_SUMMARY when that is set) and
writes every number to --json-out. The binary is --manta, then $MANTA_BIN,
then `manta` on PATH, then target/release/manta, then target/debug/manta.
Exit status: 0 when every present recording decoded and met its floors, 1
on a failed decode or a missed floor, 2 when --corpus-dir holds none of
the manifest's recordings or no binary is found.

Both subcommands read the repository's manifest unless --manifest PATH is
given before the subcommand. See docs/RUNBOOKS/real-audio-corpus.md.

Usage:
  corpus.py fetch --dest DIR [--base-url URL] [--only FILE]
  corpus.py score --corpus-dir DIR [--manta BIN] [--json-out FILE]
"""
import argparse
import hashlib
import http.client
import importlib.util
import json
import os
import re
import subprocess
import sys
import tempfile
import time
import urllib.parse
import urllib.request

PROG = "corpus.py"
HERE = os.path.dirname(os.path.abspath(__file__))
REPO_ROOT = os.path.dirname(HERE)
MANIFEST = os.path.join(REPO_ROOT, "crates", "manta-testkit", "audio-corpus", "manifest.json")
READ_BYTES = 1 << 20
# Seconds any one connect or read may block, so a host that stops sending
# fails the fetch (exit 1, partial file deleted) instead of hanging the
# nightly job until its timeout.
FETCH_TIMEOUT_S = 60
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
FLOOR_KEYS = ("min_spots", "min_precision", "min_recall")


def _load_script(module_name, file_name):
    spec = importlib.util.spec_from_file_location(module_name, os.path.join(HERE, file_name))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


scorer = _load_script("score_against_rbn", "score-against-rbn.py")
transcript_scorer = _load_script("score_transcript", "score-transcript.py")
resolve_manta = scorer.resolve_manta
score_report = scorer.score_report
_is_plausible_callsign = scorer._is_plausible_callsign


class ManifestError(ValueError):
    pass


def load_manifest(path=MANIFEST):
    """The manifest as a dict, after checking every entry's shape."""
    with open(path, encoding="utf-8") as f:
        manifest = json.load(f)
    recordings = manifest.get("recordings")
    if not isinstance(recordings, list):
        raise ManifestError("%s: no \"recordings\" list" % path)
    for rec in recordings:
        if not isinstance(rec, dict):
            raise ManifestError("%s: entry %r is not an object" % (path, rec))
        name = rec.get("file")
        if not isinstance(name, str) or name in ("", ".", "..") or os.path.basename(name) != name:
            raise ManifestError("%s: entry with bad \"file\" %r" % (path, name))
        if rec.get("kind") not in ("iq", "audio"):
            raise ManifestError("%s: %s: \"kind\" must be \"iq\" or \"audio\"" % (path, name))
        rate = rec.get("sample_rate_hz")
        if not isinstance(rate, int) or isinstance(rate, bool) or rate <= 0:
            raise ManifestError("%s: %s: \"sample_rate_hz\" must be a positive integer" % (path, name))
        pin = rec.get("sha256")
        if pin is not None and not (isinstance(pin, str) and SHA256_RE.match(pin)):
            raise ManifestError("%s: %s: \"sha256\" must be null or 64 lowercase hex digits" % (path, name))
        truth = rec.get("truth")
        if truth is not None and (not isinstance(truth, dict)
                                  or truth.get("kind") not in ("rbn", "transcript")):
            raise ManifestError("%s: %s: truth \"kind\" must be \"rbn\" or \"transcript\"" % (path, name))
        floors = rec.get("floors")
        if floors is None:
            floors = {}
        if not isinstance(floors, dict):
            raise ManifestError("%s: %s: floors must be an object" % (path, name))
        for key, value in floors.items():
            if key not in FLOOR_KEYS:
                raise ManifestError("%s: %s: unknown floor %r" % (path, name, key))
            if key == "min_spots":
                if not isinstance(value, int) or isinstance(value, bool) or value < 0:
                    raise ManifestError("%s: %s: %s must be a nonnegative integer" % (path, name, key))
            elif (not isinstance(value, (int, float)) or isinstance(value, bool)
                  or not 0 <= value <= 1):
                raise ManifestError("%s: %s: %s must be a finite number in [0, 1]" % (path, name, key))
    return manifest


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while True:
            block = f.read(READ_BYTES)
            if not block:
                return h.hexdigest()
            h.update(block)


def fetch_one(rec, base_url, dest):
    """Downloads one pinned entry; returns True when DIR/file is verified."""
    name, pin = rec["file"], rec["sha256"]
    target = os.path.join(dest, name)
    if os.path.isfile(target) and sha256_file(target) == pin:
        print("%s: already present, SHA-256 verified; skipped" % name)
        return True
    url = base_url.rstrip("/") + "/" + urllib.parse.quote(name)
    part = target + ".part"
    h = hashlib.sha256()
    size = 0
    try:
        with urllib.request.urlopen(url, timeout=FETCH_TIMEOUT_S) as resp, open(part, "wb") as out:
            while True:
                block = resp.read(READ_BYTES)
                if not block:
                    break
                h.update(block)
                out.write(block)
                size += len(block)
    except (OSError, http.client.HTTPException, ValueError) as e:
        # URLError is an OSError; HTTPException covers a connection that
        # drops mid-body (IncompleteRead).
        if os.path.exists(part):
            os.remove(part)
        sys.stderr.write("%s: error: %s: cannot fetch %s: %s\n" % (PROG, name, url, e))
        return False
    actual = h.hexdigest()
    if actual != pin:
        os.remove(part)
        sys.stderr.write("%s: error: %s: SHA-256 mismatch: expected %s, got %s; download deleted\n"
                         % (PROG, name, pin, actual))
        return False
    os.replace(part, target)
    print("%s: fetched %d bytes, SHA-256 verified" % (name, size))
    return True


def cmd_fetch(args):
    base_url = args.base_url or os.environ.get("MANTA_CORPUS_BASE_URL")
    if not base_url:
        sys.stderr.write("%s: error: no base URL; pass --base-url URL or set MANTA_CORPUS_BASE_URL\n" % PROG)
        return 2
    manifest = load_manifest(args.manifest)
    recordings = manifest["recordings"]
    if args.only is not None:
        recordings = [r for r in recordings if r["file"] == args.only]
        if not recordings:
            sys.stderr.write("%s: error: --only %s is not in %s\n" % (PROG, args.only, args.manifest))
            return 2
    os.makedirs(args.dest, exist_ok=True)
    ok = True
    for rec in recordings:
        if rec["sha256"] is None:
            print("%s: not pinned in manifest.json; skipped" % rec["file"])
            continue
        ok = fetch_one(rec, base_url, args.dest) and ok
    return 0 if ok else 1


def _run_to_file(cmd, out_path):
    """Runs `cmd` with stdout into out_path; stderr passes through. (exit, wall_s)."""
    start = time.monotonic()
    with open(out_path, "w", encoding="utf-8") as out:
        sys.stdout.flush()
        try:
            proc = subprocess.run(cmd, stdout=out)
        except OSError as e:
            sys.stderr.write("%s: error: cannot run %s: %s\n" % (PROG, cmd[0], e))
            return 127, time.monotonic() - start
    return proc.returncode, time.monotonic() - start


def _rbn_numbers(r):
    return {
        "manta_spots": r["n_manta"],
        "truth_bins": r["n_truth"],
        "truth_rows_excluded": r["excluded"],
        "true_positives": len(r["tp"]),
        "false_positives": len(r["fp"]),
        "false_negatives": len(r["fn_keys"]),
        "precision": r["precision"],
        "recall": r["recall"],
    }


def _count_run_spots(path):
    n = 0
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
            except ValueError:
                continue
            if isinstance(obj, dict) and "spot" in obj:
                n += 1
    return n


def score_entry(rec, path, manta, tmp, manifest_dir):
    """Decodes and scores one present recording; returns its result dict."""
    result = {"file": rec["file"], "kind": rec["kind"], "truth": None, "score": None,
              "floors": rec.get("floors") or {}, "missed_floors": [], "errors": []}
    truth = rec.get("truth")
    stem = os.path.join(tmp, rec["file"])
    if rec["kind"] == "iq":
        out_path = stem + ".report.json"
        cmd = [manta, "decode", path, "--json"]
        if rec.get("center_freq_hz") is not None:
            cmd += ["--center-freq-hz", str(rec["center_freq_hz"])]
    else:
        out_path = stem + ".run.jsonl"
        cmd = [manta, "run", "--source", path, "--json"]
    code, wall = _run_to_file(cmd, out_path)
    result["exit_code"] = code
    result["wall_s"] = round(wall, 3)
    result["spots"] = None
    if code != 0:
        result["errors"].append("decode exited %d" % code)
        return result

    try:
        if rec["kind"] == "iq":
            with open(out_path, encoding="utf-8") as f:
                result["spots"] = len(json.load(f)["spots"])
        else:
            result["spots"] = _count_run_spots(out_path)
    except (OSError, ValueError, KeyError, TypeError) as e:
        result["errors"].append("cannot read the decode output: %s" % e)
        return result

    precision = recall = None
    if truth is not None:
        result["truth"] = truth["kind"]
        truth_path = os.path.join(manifest_dir, truth["path"])
        try:
            if truth["kind"] == "rbn":
                if not rec.get("capture_start"):
                    raise ValueError("RBN truth needs the entry's capture_start")
                start = scorer.parse_iso(rec["capture_start"])
                rate = rec["sample_rate_hz"]
                all_rbn = score_report(out_path, truth_path, start, rate)
                score = {"rbn_all": _rbn_numbers(all_rbn)}
                if truth.get("spotter"):
                    spotter = truth["spotter"].upper()
                    only = score_report(out_path, truth_path, start, rate, spotters={spotter})
                    score["rbn_spotter"] = dict(_rbn_numbers(only), spotter=spotter)
                precision, recall = all_rbn["precision"], all_rbn["recall"]
            else:
                rows = transcript_scorer.load_transcript(truth_path)
                run = transcript_scorer.load_run(out_path, rec["sample_rate_hz"])
                score = {"transcript": transcript_scorer.score_transcript(rows, run)}
                precision = score["transcript"]["spot_precision"]
                recall = score["transcript"]["call_recall"]
        except (OSError, ValueError, KeyError, TypeError) as e:
            result["errors"].append("cannot score against %s: %s" % (truth["path"], e))
            return result
        result["score"] = score

    floors = result["floors"]
    measured = {"min_spots": result["spots"], "min_precision": precision, "min_recall": recall}
    for key in FLOOR_KEYS:
        if key not in floors:
            continue
        value = measured[key]
        if value is None:
            result["missed_floors"].append("%s %s: nothing to measure it against" % (key, floors[key]))
        elif value < floors[key]:
            result["missed_floors"].append("%s %s: measured %s" % (key, floors[key], _fmt_value(key, value)))
    return result


def _fmt_value(key, value):
    return str(value) if key == "min_spots" else "%.4f" % value


def _pct(x):
    return "n/a" if x is None else "%.1f%%" % (100.0 * x)


def markdown(results, absent):
    lines = ["## Real-audio corpus (MAN-119)", "",
             "| recording | decode exit | wall (s) | spots | truth | precision | recall | also | result |",
             "|---|---|---|---|---|---|---|---|---|"]
    for r in results:
        precision = recall = None
        also = ""
        score = r.get("score") or {}
        if "rbn_all" in score:
            precision, recall = score["rbn_all"]["precision"], score["rbn_all"]["recall"]
            if "rbn_spotter" in score:
                s = score["rbn_spotter"]
                also = "%s only: P %s, R %s" % (s["spotter"], _pct(s["precision"]), _pct(s["recall"]))
        elif "transcript" in score:
            t = score["transcript"]
            precision, recall = t["spot_precision"], t["call_recall"]
            also = "word recall %s" % _pct(t["word_recall"])
        problems = r["errors"] + r["missed_floors"]
        verdict = "FAIL: " + "; ".join(problems) if problems else "pass"
        spots = "" if r["spots"] is None else str(r["spots"])
        truth = r["truth"] or "none"
        lines.append("| %s | %d | %.1f | %s | %s | %s | %s | %s | %s |" % (
            r["file"], r["exit_code"], r["wall_s"], spots, truth,
            _pct(precision) if score else "", _pct(recall) if score else "", also, verdict))
    if absent:
        lines += ["", "Not present (skipped): " + ", ".join(absent)]
    return "\n".join(lines) + "\n"


def cmd_score(args):
    manifest = load_manifest(args.manifest)
    manifest_dir = os.path.dirname(os.path.abspath(args.manifest))
    present, absent = [], []
    for rec in manifest["recordings"]:
        path = os.path.join(args.corpus_dir, rec["file"])
        (present if os.path.isfile(path) else absent).append(rec)
    if not present:
        sys.stderr.write("%s: error: %s holds none of the manifest's recordings (%s)\n"
                         % (PROG, args.corpus_dir, ", ".join(r["file"] for r in manifest["recordings"])))
        return 2
    manta = resolve_manta(args.manta)
    if manta is None:
        sys.stderr.write("%s: error: no manta binary found; build one "
                         "(cargo build --release -p manta-cli) or pass --manta PATH\n" % PROG)
        return 2

    results = []
    with tempfile.TemporaryDirectory(prefix="corpus-score-") as tmp:
        for rec in present:
            path = os.path.join(args.corpus_dir, rec["file"])
            sys.stderr.write("%s: decoding %s\n" % (PROG, rec["file"]))
            sys.stderr.flush()
            results.append(score_entry(rec, path, manta, tmp, manifest_dir))

    passed = all(not r["errors"] and not r["missed_floors"] for r in results)
    absent_names = [r["file"] for r in absent]
    table = markdown(results, absent_names)
    sys.stdout.write(table)
    sys.stdout.flush()
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as f:
            f.write(table)
    if args.json_out:
        with open(args.json_out, "w", encoding="utf-8") as f:
            json.dump({"schema": 1, "manta": manta, "corpus_dir": args.corpus_dir,
                       "passed": passed, "results": results, "absent": absent_names},
                      f, indent=2, sort_keys=True)
            f.write("\n")
    for r in results:
        for problem in r["errors"] + r["missed_floors"]:
            sys.stderr.write("%s: %s: %s\n" % (PROG, r["file"], problem))
    return 0 if passed else 1


def main(argv=None):
    ap = argparse.ArgumentParser(prog=PROG, description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--manifest", default=MANIFEST,
                    help="The corpus manifest (default: crates/manta-testkit/audio-corpus/manifest.json)")
    sub = ap.add_subparsers(dest="command", metavar="{fetch,score}")
    sub.required = True
    fetch = sub.add_parser("fetch", help="Download and SHA-256-verify every pinned recording")
    fetch.add_argument("--dest", required=True, metavar="DIR", help="Directory to download into")
    fetch.add_argument("--base-url", metavar="URL",
                       help="Where the files are, as <URL>/<file> (default: $MANTA_CORPUS_BASE_URL)")
    fetch.add_argument("--only", metavar="FILE", help="Fetch only this manifest entry")
    score = sub.add_parser("score", help="Decode and score every recording present in a directory")
    score.add_argument("--corpus-dir", required=True, metavar="DIR", help="Directory holding the recordings")
    score.add_argument("--manta", metavar="BIN", help="The manta binary to run")
    score.add_argument("--json-out", metavar="FILE", help="Write every result as JSON here")
    args = ap.parse_args(argv)
    try:
        if args.command == "fetch":
            return cmd_fetch(args)
        return cmd_score(args)
    except (OSError, ValueError) as e:
        sys.stderr.write("%s: error: %s\n" % (PROG, e))
        return 2


if __name__ == "__main__":
    sys.exit(main())
