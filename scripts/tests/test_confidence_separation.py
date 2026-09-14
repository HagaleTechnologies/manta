import csv, importlib.util, json, os, sys, tempfile
from datetime import datetime, timezone
from contextlib import redirect_stdout
from io import StringIO

HERE = os.path.dirname(__file__)


def _load(name):
    spec = importlib.util.spec_from_file_location(name, os.path.join(HERE, "..", f"{name.replace('_', '-')}.py"))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


confsep = _load("confidence_separation")

CAPTURE_START = datetime(2025, 11, 29, tzinfo=timezone.utc)
SAMPLE_RATE_HZ = 192000

# Two spots that will match RBN truth (true positives), two that won't (bogus).
SPOTS = [
    {"callsign": "N4RV", "freq_hz": 7021000.0, "sample_ts": 0, "snr_db": 20.0, "confidence": 0.95, "spot_type": "cq"},
    {"callsign": "N3RS", "freq_hz": 7049400.0, "sample_ts": 30 * SAMPLE_RATE_HZ, "snr_db": 15.0, "confidence": 0.85, "spot_type": "cq"},
    {"callsign": "W1ABC", "freq_hz": 7030000.0, "sample_ts": 10 * SAMPLE_RATE_HZ, "snr_db": 8.0, "confidence": 0.40, "spot_type": "cq"},
    {"callsign": "K2XYZ", "freq_hz": 7035000.0, "sample_ts": 20 * SAMPLE_RATE_HZ, "snr_db": 6.0, "confidence": 0.30, "spot_type": "cq"},
]
TRUTH_ROWS = [
    # spotter, freq kHz, dx, date -- matches N4RV at t=0 and N3RS at t=+30s; no truth row for
    # W1ABC or K2XYZ, so they come back as false positives (bogus).
    ("K5TR", "7021.0", "N4RV", "2025-11-29 00:00:00"),
    ("K5TR", "7049.4", "N3RS", "2025-11-29 00:00:30"),
]


def _write_report(path):
    with open(path, "w") as f:
        json.dump({"spots": SPOTS}, f)


def _write_truth(path, rows=TRUTH_ROWS):
    with open(path, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["callsign", "de_pfx", "de_cont", "freq", "band", "dx", "dx_pfx", "dx_cont", "mode", "db", "date", "speed", "tx_mode"])
        for sp, fq, dx, date in rows:
            w.writerow([sp, "", "", fq, "40m", dx, "", "", "CQ", "20", date, "33", "CW"])


def _run(*extra_args):
    report_path = truth_path = None
    with tempfile.TemporaryDirectory() as d:
        report_path = os.path.join(d, "report.json")
        truth_path = os.path.join(d, "truth.csv")
        _write_report(report_path)
        _write_truth(truth_path)
        argv = [
            "confidence-separation.py", report_path, truth_path,
            "--capture-start", "2025-11-29T00:00:00Z",
            "--sample-rate-hz", str(SAMPLE_RATE_HZ),
        ] + list(extra_args)
        old_argv = sys.argv
        sys.argv = argv
        out = StringIO()
        try:
            with redirect_stdout(out):
                try:
                    confsep.main()
                    code = 0
                except SystemExit as e:
                    code = e.code
        finally:
            sys.argv = old_argv
        return code, out.getvalue()


def test_true_bogus_split_and_separation():
    code, output = _run()
    assert code == 0 or code is None
    assert "true positives: 2, bogus (false positives): 2" in output
    # true confidences {0.95, 0.85} -> median 0.90; bogus {0.40, 0.30} -> median 0.35.
    assert "confidence separation (median true - median bogus): 0.550" in output


def test_min_separation_gate_passes_when_met():
    code, _ = _run("--min-separation", "0.5")
    assert code == 0 or code is None


def test_min_separation_gate_fails_when_not_met():
    code, _ = _run("--min-separation", "0.6")
    assert code == 1


def test_empty_class_exits_nonzero_not_silently_passing():
    """Both spots claim to be N4RV/matched truth -> zero bogus spots. Must exit 2, not
    silently treat an undefined separation as passing a --min-separation gate."""
    with tempfile.TemporaryDirectory() as d:
        report_path = os.path.join(d, "report.json")
        truth_path = os.path.join(d, "truth.csv")
        with open(report_path, "w") as f:
            json.dump({"spots": SPOTS[:2]}, f)  # only the two true positives
        _write_truth(truth_path)
        argv = [
            "confidence-separation.py", report_path, truth_path,
            "--capture-start", "2025-11-29T00:00:00Z",
            "--sample-rate-hz", str(SAMPLE_RATE_HZ),
            "--min-separation", "0.0",  # would trivially "pass" if separation were nan
        ]
        old_argv = sys.argv
        sys.argv = argv
        try:
            try:
                confsep.main()
                code = 0
            except SystemExit as e:
                code = e.code
        finally:
            sys.argv = old_argv
        assert code == 2
