"""MAN-106: `fit-confidence-calibration.py` reproduces the committed
calibration constants from the committed measurement pair byte for
byte (fixed grids, fixed split rule, fixed formatting), and its split
rule keeps the two CSVs disjoint. Stdlib only: `python3 -m unittest
scripts.tests.test_fit_calibration -v`."""
import importlib.util
import os
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
DATA = os.path.join(HERE, "..", "..", "crates", "manta-spot", "data", "calibration")


def _load():
    spec = importlib.util.spec_from_file_location(
        "fit_confidence_calibration",
        os.path.join(HERE, "..", "fit-confidence-calibration.py"),
    )
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


fit = _load()


class FitCalibrationTest(unittest.TestCase):
    def test_committed_constants_reproduce_byte_for_byte(self):
        with tempfile.TemporaryDirectory() as d:
            import shutil
            scratch = os.path.join(d, "calibration")
            shutil.copytree(DATA, scratch)
            real_data = fit.DATA_DIR
            try:
                fit.DATA_DIR = scratch
                rc = fit.main(["fit", "--write"])
                self.assertEqual(rc, 0)
                with open(os.path.join(scratch, "constants.json")) as f:
                    produced = f.read()
            finally:
                fit.DATA_DIR = real_data
        with open(os.path.join(DATA, "constants.json")) as f:
            committed = f.read()
        self.assertEqual(produced, committed)

    def test_extract_splits_disjoint_and_deterministic(self):
        import json

        report = {
            "points": [
                {
                    "confidence": {
                        "rows": [
                            {"score": 0.5, "reps": 1, "tier": "common", "correct": True},
                            {"score": 0.7, "reps": 2, "tier": "listed", "correct": False},
                            {"score": 0.9, "reps": 2, "tier": "common", "correct": True},
                            {"score": 0.2, "reps": 1, "tier": "unlisted", "correct": False},
                        ]
                    }
                }
            ]
        }
        with tempfile.TemporaryDirectory() as d:
            rp = os.path.join(d, "r.json")
            with open(rp, "w") as f:
                json.dump(report, f)
            out = os.path.join(d, "cal")
            fit.extract([rp], out)
            with open(os.path.join(out, "fit.csv")) as f:
                fit_csv = f.read()
            with open(os.path.join(out, "heldout.csv")) as f:
                held_csv = f.read()
            # alternating split: rows 0,2 fit; rows 1,3 held-out
            self.assertIn("0.500000,1,common,1", fit_csv)
            self.assertIn("0.900000,2,common,1", fit_csv)
            self.assertIn("0.700000,2,listed,0", held_csv)
            self.assertIn("0.200000,1,unlisted,0", held_csv)
            # disjoint
            self.assertEqual(len(fit_csv.strip().splitlines()), 3)
            self.assertEqual(len(held_csv.strip().splitlines()), 3)


if __name__ == "__main__":
    unittest.main()
