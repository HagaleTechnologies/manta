import csv, importlib.util, json, os, sys, tempfile
from datetime import datetime, timezone

HERE = os.path.dirname(__file__)
spec = importlib.util.spec_from_file_location("scorer", os.path.join(HERE, "..", "score-against-rbn.py"))
scorer = importlib.util.module_from_spec(spec); spec.loader.exec_module(scorer)

CAPTURE_START = datetime(2025, 11, 29, tzinfo=timezone.utc)

ROWS = [
    # callsign(spotter), freq kHz, dx
    ("K5TR", "7021.0", "N4RV"), ("DL1ABC", "7021.0", "N4RV"),
    ("DL1ABC", "7032.0", "9A2L"), ("K5TR", "7049.4", "N3RS"),
]

def write_csv(path, rows=ROWS):
    with open(path, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["callsign","de_pfx","de_cont","freq","band","dx","dx_pfx","dx_cont","mode","db","date","speed","tx_mode"])
        for sp, fq, dx in rows:
            w.writerow([sp,"","",fq,"40m",dx,"","","CQ","20","2025-11-29 00:00:00","33","CW"])

def test_spotter_filter_keeps_only_that_spotters_rows():
    with tempfile.TemporaryDirectory() as d:
        p = os.path.join(d, "t.csv"); write_csv(p)
        truth, excluded = scorer.load_rbn_truth(p, CAPTURE_START, spotters={"K5TR"})
        assert sorted(t["callsign"] for t in truth) == ["N3RS", "N4RV"]
        assert excluded == 0

def test_min_spotters_requires_agreement():
    with tempfile.TemporaryDirectory() as d:
        p = os.path.join(d, "t.csv"); write_csv(p)
        truth, excluded = scorer.load_rbn_truth(p, CAPTURE_START, min_spotters=2)
        assert [t["callsign"] for t in truth] == ["N4RV", "N4RV"]

def test_default_is_unfiltered():
    with tempfile.TemporaryDirectory() as d:
        p = os.path.join(d, "t.csv"); write_csv(p)
        truth, excluded = scorer.load_rbn_truth(p, CAPTURE_START)
        assert len(truth) == 4
        assert excluded == 0

def test_grammar_exclusion_composes_with_spotter_filter():
    """A structurally-invalid callsign (portable suffix manta's grammar rejects) must be
    excluded from the truth set AND counted in `excluded`, even when a --spotter filter
    is also active -- the two features must not short-circuit each other."""
    rows = ROWS + [("K5TR", "7015.0", "EA8/TF3CW")]  # "/TF3CW" is not a valid portable suffix
    with tempfile.TemporaryDirectory() as d:
        p = os.path.join(d, "t.csv"); write_csv(p, rows)
        truth, excluded = scorer.load_rbn_truth(p, CAPTURE_START, spotters={"K5TR"})
        assert sorted(t["callsign"] for t in truth) == ["N3RS", "N4RV"]
        assert excluded == 1

def test_time_field_present_for_match():
    with tempfile.TemporaryDirectory() as d:
        p = os.path.join(d, "t.csv"); write_csv(p)
        truth, _ = scorer.load_rbn_truth(p, CAPTURE_START)
        assert all("time" in t and t["time"].tzinfo is not None for t in truth)
