"""MAN-301: `score-against-rbn.py --node-log`, the wrapper around
`manta grade`. A stub `manta` (a temporary Python script) records its argv,
prints a canned summary and exits with a chosen status, so these tests need
no Rust build. Stdlib only: `python3 -m unittest
scripts.tests.test_score_against_rbn_replay -v`."""
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
import zipfile

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "..", "score-against-rbn.py")

CANNED = "spots: 4 from 1 file (0 grade records skipped, 0 torn lines)\nband   spots\n"

STUB = """#!{python}
import json, os, sys
with open({log!r}, "a") as f:
    f.write(json.dumps({{"argv0": sys.argv[0], "args": sys.argv[1:],
                         "rbn": [open(a).read() if a.endswith(".csv") and os.path.exists(a) else None
                                 for a in sys.argv[1:]]}}) + "\\n")
sys.stdout.write({canned!r})
sys.exit({status})
"""

CSV = ("callsign,de_pfx,de_cont,freq,band,dx,dx_pfx,dx_cont,mode,db,date,speed,tx_mode\n"
       "K5TR,K,NA,14025.0,20m,W1AW,K,NA,CQ,21,2026-10-11 12:00:20,22,CW\n")


class ReplayTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="score-replay-test-")
        self.addCleanup(shutil.rmtree, self.tmp)
        self.log = os.path.join(self.tmp, "argv.log")
        self.node = os.path.join(self.tmp, "node")
        os.mkdir(self.node)
        self.csv = os.path.join(self.tmp, "20261011.csv")
        with open(self.csv, "w") as f:
            f.write(CSV)
        self.empty_path = os.path.join(self.tmp, "empty-path")
        os.mkdir(self.empty_path)

    def stub(self, where, status=0, canned=CANNED):
        os.makedirs(os.path.dirname(where), exist_ok=True)
        with open(where, "w") as f:
            f.write(STUB.format(python=sys.executable, log=self.log, canned=canned, status=status))
        os.chmod(where, 0o755)
        return where

    def run_script(self, args, env=None, script=SCRIPT):
        base = {k: v for k, v in os.environ.items() if k != "MANTA_BIN"}
        base["PATH"] = self.empty_path
        base.update(env or {})
        return subprocess.run([sys.executable, script] + args, env=base,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)

    def calls(self):
        if not os.path.exists(self.log):
            return []
        with open(self.log) as f:
            return [json.loads(line) for line in f]

    def test_node_log_runs_manta_grade_with_mapped_args(self):
        manta = self.stub(os.path.join(self.tmp, "bin", "manta"))
        out = self.run_script(["--node-log", self.node, "--rbn", self.csv,
                               "--spotter", "K5TR", "--spotter", "W3LPL",
                               "--exclude-spotter", "DL8LAS", "--window-s", "300",
                               "--freq-tol-hz", "250", "--manta", manta])
        self.assertEqual(out.returncode, 0, out.stderr)
        (call,) = self.calls()
        self.assertEqual(call["args"], [
            "grade", "--spots", self.node, "--rbn", self.csv,
            "--spotter", "K5TR", "--spotter", "W3LPL",
            "--exclude-spotter", "DL8LAS", "--window-secs", "300",
            "--freq-tol-hz", "250.0",
        ])

    def test_zip_rbn_is_extracted_and_passed_as_csv(self):
        manta = self.stub(os.path.join(self.tmp, "bin", "manta"))
        zpath = os.path.join(self.tmp, "20261011.zip")
        with zipfile.ZipFile(zpath, "w") as zf:
            zf.write(self.csv, "20261011.csv")
        out = self.run_script(["--node-log", self.node, "--rbn", zpath, "--manta", manta])
        self.assertEqual(out.returncode, 0, out.stderr)
        (call,) = self.calls()
        args = call["args"]
        rbn = args[args.index("--rbn") + 1]
        self.assertTrue(rbn.endswith("20261011.csv"), rbn)
        self.assertNotEqual(os.path.dirname(rbn), self.tmp)
        self.assertEqual(call["rbn"][args.index("--rbn") + 1], CSV)
        self.assertFalse(os.path.exists(rbn), "the temporary CSV outlives the run")
        self.assertIn("(unzipped 20261011.zip)", out.stderr)

    def test_stdout_and_exit_status_are_manta_grade_s(self):
        for status in (0, 1, 3):
            manta = self.stub(os.path.join(self.tmp, "bin%d" % status, "manta"), status=status)
            out = self.run_script(["--node-log", self.node, "--rbn", self.csv, "--manta", manta])
            self.assertEqual(out.returncode, status)
            self.assertEqual(out.stdout, CANNED)
            first = out.stderr.splitlines()[0]
            self.assertTrue(first.startswith("score-against-rbn.py: grading with "), first)
            self.assertTrue(first.endswith(os.path.join("bin%d" % status, "manta") + " grade"), first)

    def test_binary_resolution_order(self):
        explicit = self.stub(os.path.join(self.tmp, "explicit", "manta"))
        env_bin = self.stub(os.path.join(self.tmp, "envbin", "manta"))
        path_bin = self.stub(os.path.join(self.tmp, "pathbin", "manta"))
        # A copy of the script in a fake repo root, so its target/ fallbacks
        # are under this test's control.
        repo = os.path.join(self.tmp, "repo")
        os.makedirs(os.path.join(repo, "scripts"))
        script = os.path.join(repo, "scripts", "score-against-rbn.py")
        shutil.copy(SCRIPT, script)
        release = self.stub(os.path.join(repo, "target", "release", "manta"))
        debug = self.stub(os.path.join(repo, "target", "debug", "manta"))
        base = ["--node-log", self.node, "--rbn", self.csv]
        path_env = {"PATH": os.path.dirname(path_bin)}
        cases = [
            (["--manta", explicit], dict(path_env, MANTA_BIN=env_bin), explicit),
            ([], dict(path_env, MANTA_BIN=env_bin), env_bin),
            ([], path_env, path_bin),
            ([], {}, release),
        ]
        for extra, env, want in cases:
            out = self.run_script(base + extra, env=env, script=script)
            self.assertEqual(out.returncode, 0, out.stderr)
            self.assertEqual(os.path.realpath(self.calls()[-1]["argv0"]), os.path.realpath(want))
        os.remove(release)
        out = self.run_script(base, script=script)
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertEqual(os.path.realpath(self.calls()[-1]["argv0"]), os.path.realpath(debug))

    def test_missing_binary_is_exit_2_with_message(self):
        repo = os.path.join(self.tmp, "repo")
        os.makedirs(os.path.join(repo, "scripts"))
        script = os.path.join(repo, "scripts", "score-against-rbn.py")
        shutil.copy(SCRIPT, script)
        out = self.run_script(["--node-log", self.node, "--rbn", self.csv], script=script)
        self.assertEqual(out.returncode, 2)
        self.assertEqual(out.stdout, "")
        self.assertIn("score-against-rbn.py: error: no manta binary found; build one "
                      "(cargo build --release -p manta-cli) or pass --manta PATH", out.stderr)

    def test_decode_report_only_options_are_refused_with_node_log(self):
        manta = self.stub(os.path.join(self.tmp, "bin", "manta"))
        for opt, value in (("--capture-start", "2026-10-11T00:00:00Z"),
                           ("--sample-rate-hz", "192000"), ("--time-tol-s", "90"),
                           ("--min-spotters", "2"), ("--show-samples", "5")):
            out = self.run_script(["--node-log", self.node, "--rbn", self.csv,
                                   "--manta", manta, opt, value])
            self.assertEqual(out.returncode, 2, opt)
            self.assertIn("%s applies to decode-report scoring, not --node-log" % opt, out.stderr)
        self.assertEqual(self.calls(), [])

    def test_positional_decode_report_mode_still_parses(self):
        report = os.path.join(self.tmp, "report.json")
        with open(report, "w") as f:
            json.dump({"spots": [{"dxCall": "W1AW", "frequency": 14025000.0, "sampleTs": 3840000,
                                  "snr": 20.0, "decodeConfidence": 0.9, "spotType": "cq"}]}, f)
        csv_path = os.path.join(self.tmp, "truth.csv")
        with open(csv_path, "w") as f:
            f.write(CSV.replace("2026-10-11 12:00:20", "2026-10-11 12:00:00"))
        out = self.run_script([report, csv_path, "--capture-start", "2026-10-11T11:59:40Z",
                               "--sample-rate-hz", "192000", "--show-samples", "0"])
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertIn("spotters: all", out.stdout)
        self.assertIn("true positives: 1", out.stdout)
        missing = self.run_script([report, csv_path, "--sample-rate-hz", "192000"])
        self.assertEqual(missing.returncode, 2)
        self.assertIn("--capture-start", missing.stderr)


if __name__ == "__main__":
    unittest.main()
