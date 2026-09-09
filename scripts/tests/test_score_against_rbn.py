import csv, importlib.util, json, os, sys, tempfile
HERE = os.path.dirname(__file__)
spec = importlib.util.spec_from_file_location("scorer", os.path.join(HERE, "..", "score-against-rbn.py"))
scorer = importlib.util.module_from_spec(spec); spec.loader.exec_module(scorer)

ROWS = [
    # callsign(spotter), freq kHz, dx
    ("K5TR", "7021.0", "N4RV"), ("DL1ABC", "7021.0", "N4RV"),
    ("DL1ABC", "7032.0", "9A2L"), ("K5TR", "7049.4", "N3RS"),
]
def write_csv(path):
    with open(path, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["callsign","de_pfx","de_cont","freq","band","dx","dx_pfx","dx_cont","mode","db","date","speed","tx_mode"])
        for sp, fq, dx in ROWS:
            w.writerow([sp,"","",fq,"40m",dx,"","","CQ","20","2025-11-29 00:00:00","33","CW"])

def test_spotter_filter_keeps_only_that_spotters_rows():
    with tempfile.TemporaryDirectory() as d:
        p = os.path.join(d, "t.csv"); write_csv(p)
        truth = scorer.load_rbn_truth(p, spotters={"K5TR"})
        assert sorted(t["callsign"] for t in truth) == ["N3RS", "N4RV"]

def test_min_spotters_requires_agreement():
    with tempfile.TemporaryDirectory() as d:
        p = os.path.join(d, "t.csv"); write_csv(p)
        truth = scorer.load_rbn_truth(p, min_spotters=2)
        assert [t["callsign"] for t in truth] == ["N4RV", "N4RV"]

def test_default_is_unfiltered():
    with tempfile.TemporaryDirectory() as d:
        p = os.path.join(d, "t.csv"); write_csv(p)
        assert len(scorer.load_rbn_truth(p)) == 4
