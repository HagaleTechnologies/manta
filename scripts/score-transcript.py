#!/usr/bin/env python3
"""Score a `manta run --source <file> --json` capture against a hand
transcript of the same recording (MAN-119).

Transcript format: `ground-truth/<recording-stem>.transcript.csv`, UTF-8, a
header row `start_s,end_s,pitch_hz,text`, then one row per transmission.
Lines starting with `#` are comments.

  start_s   seconds from the recording's first sample when the transmission
            starts (a number >= 0)
  end_s     when it ends (>= start_s)
  pitch_hz  approximate audio tone in Hz, read from a spectrogram; empty when
            unknown
  text      what was sent: uppercase A-Z 0-9 / ? . , = and single spaces
            between words (quote the field if it holds a comma). Write only
            copy you are sure of; leave unreadable characters and words out,
            with no placeholders.

Example:
  start_s,end_s,pitch_hz,text
  12.4,15.9,640,VP8GEO UP
  16.2,17.5,710,K5TR
  18.0,20.1,640,K5TR 5NN

The capture is `run --json`'s stdout: decoder events (`CharDecoded`,
`WordBoundary`, `TrackPromoted`, `TrackMeta`, `TrackClosed`, ...) and
`{"spot": {...}}` lines. A spot's time is `sample_ts / --sample-rate-hz`;
its pitch, and a track's, is `freq_hz - --dial-freq-hz`. --dial-freq-hz
defaults to 0, which matches `run` without --dial-freq-hz (freq_hz is then
the audio tone). A row's window is [start_s - tol, end_s + tol]
(--time-tol-s, default 10); a pitch matches within --pitch-tol-hz (default
100) when the row gives one, and any pitch when it does not.

Metrics:
  spot precision  the fraction of manta spots whose callsign is a word of a
                  transcript row whose window holds the spot's time and whose
                  pitch matches. A transcriber can miss weak callers, so this
                  is a lower bound. null when there are no spots.
  call recall     the fraction of distinct transcript callsigns (words that
                  pass score-against-rbn.py's port of manta's callsign
                  grammar) with at least one matching spot. null when the
                  transcript holds no callsign.
  word recall     the fraction of transcript word occurrences found as a
                  decoded word in the row's window and pitch. Words are built
                  per track from `CharDecoded` characters and end at a
                  `WordBoundary`, a prosign or `TrackClosed`; a word's time is
                  its first character's. Each decoded word counts for at most
                  one transcript word. This covers 5NN, TU and exchange text.

Exit status: 0 on success, 2 on bad arguments or a malformed transcript or
capture.

Usage:
  score-transcript.py <run.jsonl> <transcript.csv> --sample-rate-hz 48000 \
      [--dial-freq-hz 0] [--time-tol-s 10] [--pitch-tol-hz 100] [--json]
"""
import argparse
import csv
import importlib.util
import json
import math
import os
import re
import sys

PROG = "score-transcript.py"
HERE = os.path.dirname(os.path.abspath(__file__))
HEADER = ["start_s", "end_s", "pitch_hz", "text"]
TEXT_RE = re.compile(r"^[A-Z0-9/?.,=]+( [A-Z0-9/?.,=]+)*$")
DEFAULT_TIME_TOL_S = 10.0
DEFAULT_PITCH_TOL_HZ = 100.0

_spec = importlib.util.spec_from_file_location("score_against_rbn", os.path.join(HERE, "score-against-rbn.py"))
scorer = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(scorer)
_is_plausible_callsign = scorer._is_plausible_callsign


class TranscriptError(ValueError):
    pass


def _number(path, lineno, name, raw):
    try:
        v = float(raw)
    except ValueError:
        raise TranscriptError("%s:%d: %s is not a number: %r" % (path, lineno, name, raw)) from None
    if not math.isfinite(v):
        raise TranscriptError("%s:%d: %s must be finite, got %r" % (path, lineno, name, raw))
    return v


def load_transcript(path):
    """Transcript rows: [{line, start_s, end_s, pitch_hz (None if empty), text,
    words}]. Raises TranscriptError naming the file and line."""
    rows = []
    header_seen = False
    with open(path, encoding="utf-8", newline="") as f:
        for lineno, line in enumerate(f, 1):
            stripped = line.strip()
            if not stripped or stripped.startswith("#"):
                continue
            fields = next(csv.reader([line]))
            if not header_seen:
                if [c.strip() for c in fields] != HEADER:
                    raise TranscriptError("%s:%d: the header must be %s, got %r"
                                          % (path, lineno, ",".join(HEADER), stripped))
                header_seen = True
                continue
            if len(fields) < 4:
                raise TranscriptError("%s:%d: expected 4 columns (%s), got %d"
                                      % (path, lineno, ",".join(HEADER), len(fields)))
            start = _number(path, lineno, "start_s", fields[0].strip())
            end = _number(path, lineno, "end_s", fields[1].strip())
            if start < 0:
                raise TranscriptError("%s:%d: start_s must be >= 0, got %s" % (path, lineno, fields[0].strip()))
            if end < start:
                raise TranscriptError("%s:%d: end_s %s is before start_s %s"
                                      % (path, lineno, fields[1].strip(), fields[0].strip()))
            raw_pitch = fields[2].strip()
            pitch = None
            if raw_pitch:
                pitch = _number(path, lineno, "pitch_hz", raw_pitch)
                if pitch <= 0:
                    raise TranscriptError("%s:%d: pitch_hz must be > 0, got %s" % (path, lineno, raw_pitch))
            # An unquoted comma in the text splits it into extra columns; rejoin them.
            text = ",".join(fields[3:]).strip()
            if not text:
                raise TranscriptError("%s:%d: text is empty (leave out rows you could not copy)" % (path, lineno))
            if not TEXT_RE.match(text):
                raise TranscriptError("%s:%d: text %r must be uppercase A-Z 0-9 / ? . , = with single "
                                      "spaces between words" % (path, lineno, text))
            rows.append({"line": lineno, "start_s": start, "end_s": end, "pitch_hz": pitch,
                         "text": text, "words": text.split(" ")})
    if not header_seen:
        raise TranscriptError("%s: no header row (%s)" % (path, ",".join(HEADER)))
    return rows


def load_run(path, sample_rate_hz, dial_freq_hz=0.0):
    """`run --json` capture -> {"spots": [{callsign, time_s, pitch_hz}],
    "words": [{text, track_id, time_s, pitch_hz}]}. Raises ValueError naming
    the line on a line that is not a JSON object."""
    spots, words = [], []
    open_words = {}     # track_id -> (text, first sample_ts)
    track_freq = {}     # track_id -> latest freq_hz
    pending = []        # words whose track had no frequency yet when they ended

    def flush(track_id):
        word = open_words.pop(track_id, None)
        if word is None:
            return
        text, ts = word
        entry = {"text": text, "track_id": track_id, "time_s": ts / sample_rate_hz, "pitch_hz": None}
        if track_id in track_freq:
            entry["pitch_hz"] = track_freq[track_id] - dial_freq_hz
        else:
            pending.append(entry)
        words.append(entry)

    with open(path, encoding="utf-8") as f:
        for lineno, line in enumerate(f, 1):
            if not line.strip():
                continue
            try:
                obj = json.loads(line)
            except ValueError as e:
                raise ValueError("%s:%d: not a JSON line: %s" % (path, lineno, e)) from None
            if not isinstance(obj, dict):
                raise ValueError("%s:%d: not a JSON object" % (path, lineno))
            if "spot" in obj:
                s = obj["spot"]
                spots.append({"callsign": str(s["callsign"]).upper(),
                              "time_s": s["sample_ts"] / sample_rate_hz,
                              "pitch_hz": s["freq_hz"] - dial_freq_hz,
                              "track_id": s.get("track_id")})
                continue
            event = obj.get("event")
            track_id = obj.get("track_id")
            if event in ("TrackPromoted", "TrackMeta") and "freq_hz" in obj:
                track_freq[track_id] = obj["freq_hz"]
            elif event == "CharDecoded":
                glyph = obj.get("glyph") or {}
                if "Char" in glyph:
                    text, ts = open_words.get(track_id, ("", obj["sample_ts"]))
                    open_words[track_id] = (text + str(glyph["Char"]).upper(), ts)
                else:
                    flush(track_id)  # a prosign ends the word
            elif event in ("WordBoundary", "TrackClosed"):
                flush(track_id)
    for track_id in list(open_words):
        flush(track_id)
    for entry in pending:
        if entry["track_id"] in track_freq:
            entry["pitch_hz"] = track_freq[entry["track_id"]] - dial_freq_hz
    return {"spots": spots, "words": words}


def _in_row(row, time_s, pitch_hz, time_tol_s, pitch_tol_hz):
    if not (row["start_s"] - time_tol_s <= time_s <= row["end_s"] + time_tol_s):
        return False
    if row["pitch_hz"] is None:
        return True
    return pitch_hz is not None and abs(pitch_hz - row["pitch_hz"]) <= pitch_tol_hz


def _ratio(num, den):
    return num / den if den else None


def score_transcript(rows, run, time_tol_s=DEFAULT_TIME_TOL_S, pitch_tol_hz=DEFAULT_PITCH_TOL_HZ):
    """Spot precision, call recall and word recall of `run` (load_run)
    against `rows` (load_transcript). Ratios are None when their
    denominator is 0."""
    spots = run["spots"]

    def spot_matches(spot, call=None):
        for row in rows:
            if spot["callsign"] not in row["words"] or (call is not None and spot["callsign"] != call):
                continue
            if _in_row(row, spot["time_s"], spot["pitch_hz"], time_tol_s, pitch_tol_hz):
                return True
        return False

    spots_matched = sum(1 for s in spots if spot_matches(s))
    calls = sorted({w for row in rows for w in row["words"] if _is_plausible_callsign(w)})
    calls_found = sum(1 for c in calls if any(spot_matches(s, c) for s in spots if s["callsign"] == c))

    used = set()
    transcript_words = 0
    words_found = 0
    by_text = {}
    for i, w in enumerate(run["words"]):
        by_text.setdefault(w["text"], []).append(i)
    for idxs in by_text.values():
        idxs.sort(key=lambda i: run["words"][i]["time_s"])
    for row in rows:
        for word in row["words"]:
            transcript_words += 1
            for i in by_text.get(word, ()):
                if i in used:
                    continue
                w = run["words"][i]
                if _in_row(row, w["time_s"], w["pitch_hz"], time_tol_s, pitch_tol_hz):
                    used.add(i)
                    words_found += 1
                    break

    return {
        "spots": len(spots),
        "spots_matched": spots_matched,
        "spot_precision": _ratio(spots_matched, len(spots)),
        "transcript_calls": len(calls),
        "calls_found": calls_found,
        "call_recall": _ratio(calls_found, len(calls)),
        "transcript_words": transcript_words,
        "words_found": words_found,
        "word_recall": _ratio(words_found, transcript_words),
        "time_tol_s": time_tol_s,
        "pitch_tol_hz": pitch_tol_hz,
    }


def _pct(num, den, ratio):
    return "n/a (0 of 0)" if ratio is None else "%.1f%% (%d of %d)" % (100.0 * ratio, num, den)


def main(argv=None):
    ap = argparse.ArgumentParser(prog=PROG, description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("run_jsonl", help="`manta run --source <file> --json` stdout")
    ap.add_argument("transcript_csv", help="Hand transcript of the same recording")
    ap.add_argument("--sample-rate-hz", required=True,
                    type=scorer._finite("--sample-rate-hz", min_exclusive=0),
                    help="The recording's sample rate (sample_ts units)")
    ap.add_argument("--dial-freq-hz", type=scorer._finite("--dial-freq-hz"), default=0.0,
                    help="Subtracted from freq_hz to get a pitch (default 0, as for run without --dial-freq-hz)")
    ap.add_argument("--time-tol-s", type=scorer._finite("--time-tol-s", min_inclusive=0),
                    default=DEFAULT_TIME_TOL_S, help="Seconds either side of a row (default 10)")
    ap.add_argument("--pitch-tol-hz", type=scorer._finite("--pitch-tol-hz", min_inclusive=0),
                    default=DEFAULT_PITCH_TOL_HZ, help="Pitch tolerance in Hz (default 100)")
    ap.add_argument("--json", action="store_true", help="Print one JSON object")
    args = ap.parse_args(argv)

    try:
        rows = load_transcript(args.transcript_csv)
        run = load_run(args.run_jsonl, args.sample_rate_hz, args.dial_freq_hz)
    except (OSError, ValueError, KeyError, TypeError) as e:
        sys.stderr.write("%s: error: %s\n" % (PROG, e))
        return 2
    r = score_transcript(rows, run, args.time_tol_s, args.pitch_tol_hz)
    if args.json:
        print(json.dumps(r, sort_keys=True))
        return 0
    print("spots: %d" % r["spots"])
    print("spot precision: %s; a lower bound, a transcriber can miss weak callers"
          % _pct(r["spots_matched"], r["spots"], r["spot_precision"]))
    print("transcript calls: %d" % r["transcript_calls"])
    print("call recall: %s" % _pct(r["calls_found"], r["transcript_calls"], r["call_recall"]))
    print("transcript words: %d" % r["transcript_words"])
    print("word recall: %s" % _pct(r["words_found"], r["transcript_words"], r["word_recall"]))
    print("time tolerance: %g s; pitch tolerance: %g Hz" % (r["time_tol_s"], r["pitch_tol_hz"]))
    return 0


if __name__ == "__main__":
    sys.exit(main())
