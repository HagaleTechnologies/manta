"""MAN-119: the real-audio corpus manifest, `scripts/corpus.py fetch|score`,
`score-against-rbn.py`'s extracted `score_report`, and corpus.yml's shape.

No network and no audio: fetch reads a file:// base URL over a temp dir, and
score runs a stub `manta` (a temporary Python script) that records its argv,
prints a canned report and exits with a chosen status. Workflows are read as
text with test_ci_trust_boundary.py's helpers.

Run: python3 -m unittest scripts.tests.test_corpus -v
"""
import contextlib
import csv
import hashlib
import http.client
import importlib.util
import io
import json
import os
import pathlib
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
SCRIPT = os.path.join(ROOT, "scripts", "corpus.py")
CORPUS = os.path.join(ROOT, "crates", "manta-testkit", "audio-corpus")
B2_PIN = "74eba71e00fede9cab8067f9ff5a043396066015bbf8c23232788279b9f1c5f2"
TRANSCRIPT_FIXTURES = os.path.join(HERE, "fixtures", "transcript")


def _load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


corpus = _load("corpus", SCRIPT)
scorer = _load("score_against_rbn", os.path.join(ROOT, "scripts", "score-against-rbn.py"))
rbn_tests = _load("test_score_against_rbn", os.path.join(HERE, "test_score_against_rbn.py"))
wf = _load("_ci_workflow_text", os.path.join(HERE, "test_ci_trust_boundary.py"))

STUB = """#!{python}
import json, sys
with open({log!r}, "a") as f:
    f.write(json.dumps(sys.argv[1:]) + "\\n")
with open({canned!r}[sys.argv[1]], encoding="utf-8") as f:
    sys.stdout.write(f.read())
sys.exit({status})
"""

# RBN truth: N4RV (K5TR and DL1ABC), 9A2L (DL1ABC), N3RS (K5TR).
RBN_ROWS = [
    ("K5TR", "7021.0", "N4RV", "2025-11-29 00:00:30"),
    ("DL1ABC", "7021.0", "N4RV", "2025-11-29 00:00:40"),
    ("DL1ABC", "7032.0", "9A2L", "2025-11-29 00:01:00"),
    ("K5TR", "7049.4", "N3RS", "2025-11-29 00:02:00"),
]


def write_rbn_csv(path):
    with open(path, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["callsign", "de_pfx", "de_cont", "freq", "band", "dx", "dx_pfx", "dx_cont",
                    "mode", "db", "date", "speed", "tx_mode"])
        for spotter, freq, dx, date in RBN_ROWS:
            w.writerow([spotter, "", "", freq, "40m", dx, "", "", "CQ", "20", date, "28", "CW"])


def decode_report(rate=192000):
    """N4RV matches truth; W9ZZZ is a false positive."""
    def s(call, frequency, t):
        return {"dxCall": call, "frequency": frequency, "snr": 12.0, "wpm": 28.0,
                "spotType": "cq", "decodeConfidence": 0.9, "trackId": 1, "sampleTs": int(t * rate)}
    return {"events": [], "spots": [s("N4RV", 7021000.0, 35.0), s("W9ZZZ", 7040000.0, 50.0)]}


class ScoreReportTests(unittest.TestCase):
    """score_report(...) returns the numbers score-against-rbn.py's main prints."""

    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="corpus-score-report-")
        self.addCleanup(shutil.rmtree, self.tmp)
        self.csv = os.path.join(self.tmp, "t.csv")
        rbn_tests.write_csv(self.csv)  # test_score_against_rbn.py's fixture rows
        self.report = os.path.join(self.tmp, "r.json")
        spots = [{"dxCall": c, "frequency": f, "snr": 10.0, "decodeConfidence": 0.8,
                  "spotType": "cq", "sampleTs": 0}
                 for c, f in (("N4RV", 7021000.0), ("N3RS", 7049400.0), ("W9ZZZ", 7040000.0))]
        with open(self.report, "w") as f:
            json.dump({"spots": spots}, f)

    def printed(self, *extra):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = scorer.main([self.report, self.csv, "--capture-start", "2025-11-29T00:00:00Z",
                                "--sample-rate-hz", "192000", "--show-samples", "0"] + list(extra))
        self.assertEqual(code, 0)
        return dict(line.split(": ", 1) for line in out.getvalue().splitlines() if ": " in line)

    def check(self, spotters, *extra):
        r = scorer.score_report(self.report, self.csv, rbn_tests.CAPTURE_START, 192000.0,
                                spotters=spotters)
        p = self.printed(*extra)
        self.assertEqual(p["manta spots"], str(r["n_manta"]))
        self.assertEqual(p["RBN truth unique call+kHz bins in window"], str(r["n_truth"]))
        self.assertEqual(p["RBN truth rows excluded (callsign shape manta's grammar can never accept)"],
                         str(r["excluded"]))
        self.assertEqual(p["true positives"], str(len(r["tp"])))
        self.assertEqual(p["false positives"], str(len(r["fp"])))
        self.assertEqual(p["false negatives"], str(len(r["fn_keys"])))
        self.assertEqual(p["precision"], f"{r['precision']:.3%}")
        self.assertEqual(p["recall"].strip(), f"{r['recall']:.3%}")
        return r

    def test_all_rbn(self):
        r = self.check(None)
        self.assertEqual((r["n_manta"], r["n_truth"], len(r["tp"])), (3, 3, 2))
        self.assertAlmostEqual(r["precision"], 2 / 3)
        self.assertAlmostEqual(r["recall"], 2 / 3)

    def test_spotter_only(self):
        r = self.check({"K5TR"}, "--spotter", "K5TR")
        self.assertEqual((r["n_truth"], len(r["tp"])), (2, 2))
        self.assertEqual(r["recall"], 1.0)

    def test_empty_denominators_are_zero(self):
        with open(self.report, "w") as f:
            json.dump({"spots": []}, f)
        r = self.check(None)
        self.assertEqual((r["n_manta"], r["precision"]), (0, 0.0))


class ManifestTests(unittest.TestCase):
    def setUp(self):
        self.manifest = corpus.load_manifest()
        self.by_file = {r["file"]: r for r in self.manifest["recordings"]}

    def test_every_entry_has_the_required_shape(self):
        self.assertTrue(self.manifest["recordings"])
        for rec in self.manifest["recordings"]:
            with self.subTest(file=rec.get("file")):
                self.assertIsInstance(rec["file"], str)
                self.assertIn(rec["kind"], {"iq", "audio"})
                self.assertIsInstance(rec["sample_rate_hz"], int)
                self.assertGreater(rec["sample_rate_hz"], 0)
                self.assertIn("sha256", rec)
                if rec["sha256"] is not None:
                    self.assertRegex(rec["sha256"], r"^[0-9a-f]{64}$")
                if rec["kind"] == "iq":
                    self.assertIn(rec["bits_per_sample"], {16, 24, 32})
                # RBN truth is third-party data, rebuilt locally and ignored by git
                # (MAN-249), so only its path is checked; transcripts are tracked.
                if (rec.get("truth") or {}).get("kind") == "rbn":
                    self.assertRegex(rec["truth"]["path"], r"^ground-truth/[^/]+\.rbn\.csv$")
                elif rec.get("truth"):
                    self.assertTrue(os.path.isfile(os.path.join(CORPUS, rec["truth"]["path"])),
                                    rec["truth"]["path"])

    def test_b2_is_pinned_with_rbn_truth_and_floors(self):
        b2 = self.by_file["B2_20251129_000000_7080kHz.source.wav"]
        self.assertEqual(b2["sha256"], B2_PIN)
        self.assertEqual((b2["kind"], b2["sample_rate_hz"], b2["bits_per_sample"]), ("iq", 192000, 24))
        self.assertEqual(b2["center_freq_hz"], 7080000)
        self.assertEqual(b2["capture_start"], "2025-11-29T00:00:00Z")
        self.assertEqual(b2["truth"]["kind"], "rbn")
        self.assertEqual(b2["truth"]["spotter"], "K5TR")
        self.assertEqual(b2["truth"]["path"], "ground-truth/B2_20251129_000000_7080kHz.rbn.csv")
        self.assertEqual(b2["floors"], {"min_spots": 50, "min_precision": 0.20, "min_recall": 0.02})

    def test_wpx_and_vp8geo_are_unpinned(self):
        self.assertIsNone(self.by_file["wpx_cw_iq_96khz.wav"]["sha256"])
        self.assertEqual(self.by_file["wpx_cw_iq_96khz.wav"]["floors"], {"min_spots": 10})
        self.assertIsNone(self.by_file["vp8geo_cw_48khz.wav"]["sha256"])
        self.assertEqual(self.by_file["vp8geo_cw_48khz.wav"]["kind"], "audio")

    def test_a_malformed_entry_is_refused(self):
        tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, tmp)
        path = os.path.join(tmp, "m.json")
        for bad in ({"file": "x.wav", "kind": "mp3", "sha256": None, "sample_rate_hz": 1},
                    {"file": "x.wav", "kind": "iq", "sha256": "ABC", "sample_rate_hz": 1},
                    {"file": "x.wav", "kind": "iq", "sha256": None, "sample_rate_hz": 0},
                    {"file": "../x.wav", "kind": "iq", "sha256": None, "sample_rate_hz": 1},
                    {"file": "..", "kind": "iq", "sha256": None, "sample_rate_hz": 1},
                    {"file": "x.wav", "kind": "iq", "sha256": None, "sample_rate_hz": 1,
                     "truth": "ground-truth/x.csv"},
                    "x.wav"):
            with self.subTest(bad=bad):
                with open(path, "w") as f:
                    json.dump({"schema": 1, "recordings": [bad]}, f)
                with self.assertRaises(corpus.ManifestError):
                    corpus.load_manifest(path)


class ScriptTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="corpus-test-")
        self.addCleanup(shutil.rmtree, self.tmp)
        self.empty_path = os.path.join(self.tmp, "empty-path")
        os.mkdir(self.empty_path)

    def write_manifest(self, recordings, name="manifest.json"):
        path = os.path.join(self.tmp, name)
        with open(path, "w") as f:
            json.dump({"schema": 1, "recordings": recordings}, f)
        return path

    def run_script(self, args, env=None, script=SCRIPT):
        base = {k: v for k, v in os.environ.items()
                if k not in ("MANTA_BIN", "MANTA_CORPUS_BASE_URL", "GITHUB_STEP_SUMMARY")}
        base["PATH"] = self.empty_path
        base.update(env or {})
        return subprocess.run([sys.executable, script] + args, env=base,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)


class FetchTests(ScriptTest):
    CONTENT = b"RIFF fake corpus bytes" * 1000

    def setUp(self):
        super().setUp()
        self.src = os.path.join(self.tmp, "host")
        os.mkdir(self.src)
        for name in ("a.wav", "b.wav"):
            with open(os.path.join(self.src, name), "wb") as f:
                f.write(self.CONTENT)
        self.base_url = pathlib.Path(self.src).as_uri()
        self.dest = os.path.join(self.tmp, "dest")
        self.pin = hashlib.sha256(self.CONTENT).hexdigest()

    def manifest(self, pin=None):
        return self.write_manifest([
            {"file": "a.wav", "kind": "iq", "sha256": pin or self.pin, "sample_rate_hz": 96000,
             "bits_per_sample": 24},
            {"file": "b.wav", "kind": "iq", "sha256": None, "sample_rate_hz": 96000,
             "bits_per_sample": 24},
        ])

    def fetch(self, manifest, *extra, env=None):
        return self.run_script(["--manifest", manifest, "fetch", "--dest", self.dest] + list(extra), env=env)

    def test_downloads_and_verifies_a_pinned_entry(self):
        out = self.fetch(self.manifest(), "--base-url", self.base_url)
        self.assertEqual(out.returncode, 0, out.stderr)
        with open(os.path.join(self.dest, "a.wav"), "rb") as f:
            self.assertEqual(f.read(), self.CONTENT)
        self.assertFalse(os.path.exists(os.path.join(self.dest, "a.wav.part")))
        self.assertIn("a.wav: fetched %d bytes, SHA-256 verified" % len(self.CONTENT), out.stdout)

    def test_base_url_from_the_environment(self):
        out = self.fetch(self.manifest(), env={"MANTA_CORPUS_BASE_URL": self.base_url})
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertTrue(os.path.isfile(os.path.join(self.dest, "a.wav")))

    def test_skips_an_already_verified_file(self):
        manifest = self.manifest()
        self.assertEqual(self.fetch(manifest, "--base-url", self.base_url).returncode, 0)
        target = os.path.join(self.dest, "a.wav")
        os.utime(target, ns=(1_000_000_000, 1_000_000_000))
        os.remove(os.path.join(self.src, "a.wav"))  # a re-download would now fail
        out = self.fetch(manifest, "--base-url", self.base_url)
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertEqual(os.stat(target).st_mtime_ns, 1_000_000_000)
        self.assertIn("a.wav: already present, SHA-256 verified; skipped", out.stdout)

    def test_hash_mismatch_deletes_the_download_and_exits_1(self):
        wrong = hashlib.sha256(b"something else").hexdigest()
        out = self.fetch(self.manifest(pin=wrong), "--base-url", self.base_url)
        self.assertEqual(out.returncode, 1)
        self.assertIn("expected %s" % wrong, out.stderr)
        self.assertIn("got %s" % self.pin, out.stderr)
        self.assertFalse(os.path.exists(os.path.join(self.dest, "a.wav")))
        self.assertFalse(os.path.exists(os.path.join(self.dest, "a.wav.part")))

    def test_missing_file_exits_1(self):
        os.remove(os.path.join(self.src, "a.wav"))
        out = self.fetch(self.manifest(), "--base-url", self.base_url)
        self.assertEqual(out.returncode, 1)
        self.assertIn("cannot fetch", out.stderr)
        self.assertEqual(os.listdir(self.dest), [])

    def test_a_connection_dropped_mid_body_deletes_the_download(self):
        class Dropped(io.BytesIO):
            def read(self, n=-1):
                if self.tell():
                    raise http.client.IncompleteRead(b"", 100)
                return super().read(n)

        os.mkdir(self.dest)
        rec = {"file": "a.wav", "sha256": self.pin}
        err = io.StringIO()
        with mock.patch.object(corpus.urllib.request, "urlopen",
                               lambda url, timeout: Dropped(b"x" * 10)), contextlib.redirect_stderr(err):
            self.assertFalse(corpus.fetch_one(rec, "https://example.invalid/corpus", self.dest))
        self.assertIn("a.wav: cannot fetch https://example.invalid/corpus/a.wav", err.getvalue())
        self.assertEqual(os.listdir(self.dest), [])

    def test_never_fetches_unpinned_entries(self):
        out = self.fetch(self.manifest(), "--base-url", self.base_url)
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertIn("b.wav: not pinned in manifest.json; skipped", out.stdout)
        self.assertFalse(os.path.exists(os.path.join(self.dest, "b.wav")))

    def test_only_limits_the_fetch(self):
        out = self.fetch(self.manifest(), "--base-url", self.base_url, "--only", "b.wav")
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertFalse(os.path.exists(os.path.join(self.dest, "a.wav")))
        out = self.fetch(self.manifest(), "--base-url", self.base_url, "--only", "zzz.wav")
        self.assertEqual(out.returncode, 2)

    def test_no_base_url_exits_2(self):
        out = self.fetch(self.manifest())
        self.assertEqual(out.returncode, 2)
        self.assertIn("MANTA_CORPUS_BASE_URL", out.stderr)
        self.assertFalse(os.path.exists(os.path.join(self.dest, "a.wav")))


class ScoreTests(ScriptTest):
    def setUp(self):
        super().setUp()
        self.log = os.path.join(self.tmp, "argv.log")
        self.dir = os.path.join(self.tmp, "corpus")
        os.mkdir(self.dir)
        self.truth = os.path.join(self.tmp, "truth.csv")
        write_rbn_csv(self.truth)
        self.report = os.path.join(self.tmp, "canned-report.json")
        with open(self.report, "w") as f:
            json.dump(decode_report(), f)
        self.run_jsonl = os.path.join(self.tmp, "canned-run.jsonl")
        shutil.copy(os.path.join(TRANSCRIPT_FIXTURES, "run.jsonl"), self.run_jsonl)
        shutil.copy(os.path.join(TRANSCRIPT_FIXTURES, "transcript.csv"),
                    os.path.join(self.tmp, "vp.transcript.csv"))
        self.json_out = os.path.join(self.tmp, "results.json")

    def iq_entry(self, **over):
        rec = {"file": "iq.wav", "kind": "iq", "sha256": None, "sample_rate_hz": 192000,
               "bits_per_sample": 24, "center_freq_hz": 7080000,
               "capture_start": "2025-11-29T00:00:00Z",
               "truth": {"kind": "rbn", "path": "truth.csv", "spotter": "K5TR"},
               "floors": {"min_spots": 2, "min_precision": 0.20, "min_recall": 0.02}}
        rec.update(over)
        return rec

    def audio_entry(self, **over):
        rec = {"file": "vp.wav", "kind": "audio", "sha256": None, "sample_rate_hz": 48000,
               "truth": {"kind": "transcript", "path": "vp.transcript.csv"},
               "floors": {"min_precision": 0.5}}
        rec.update(over)
        return rec

    def present(self, *names):
        for name in names:
            with open(os.path.join(self.dir, name), "wb") as f:
                f.write(b"RIFF")

    def stub(self, status=0):
        where = os.path.join(self.tmp, "bin", "manta")
        os.makedirs(os.path.dirname(where), exist_ok=True)
        with open(where, "w") as f:
            f.write(STUB.format(python=sys.executable, log=self.log, status=status,
                                canned={"decode": self.report, "run": self.run_jsonl}))
        os.chmod(where, 0o755)
        return where

    def score(self, recordings, status=0, env=None):
        manifest = self.write_manifest(recordings)
        return self.run_script(["--manifest", manifest, "score", "--corpus-dir", self.dir,
                                "--json-out", self.json_out, "--manta", self.stub(status)], env=env)

    def calls(self):
        if not os.path.exists(self.log):
            return []
        with open(self.log) as f:
            return [json.loads(line) for line in f]

    def results(self):
        with open(self.json_out) as f:
            return json.load(f)

    def test_iq_entry_scores_all_rbn_and_spotter_only(self):
        self.present("iq.wav")
        out = self.score([self.iq_entry()])
        self.assertEqual(out.returncode, 0, out.stdout + out.stderr)
        res = self.results()
        self.assertTrue(res["passed"])
        (r,) = res["results"]
        self.assertEqual((r["exit_code"], r["spots"], r["missed_floors"]), (0, 2, []))
        all_rbn, only = r["score"]["rbn_all"], r["score"]["rbn_spotter"]
        self.assertEqual(all_rbn["precision"], 0.5)
        self.assertAlmostEqual(all_rbn["recall"], 1 / 3)
        self.assertEqual(only["spotter"], "K5TR")
        self.assertEqual((only["precision"], only["recall"]), (0.5, 0.5))
        self.assertIn("| iq.wav | 0 |", out.stdout)
        self.assertIn("K5TR only: P 50.0%, R 50.0%", out.stdout)

    def test_missed_floor_exits_1_and_names_it(self):
        self.present("iq.wav")
        out = self.score([self.iq_entry(floors={"min_spots": 5, "min_recall": 0.5})])
        self.assertEqual(out.returncode, 1)
        self.assertIn("min_spots 5: measured 2", out.stdout)
        self.assertIn("min_recall 0.5: measured 0.3333", out.stderr)
        self.assertFalse(self.results()["passed"])

    def test_invalid_floor_values_are_refused_before_decode(self):
        self.present("iq.wav")
        for key in ("min_spots", "min_precision", "min_recall"):
            invalid = [-1, "0.5", None, True, False, [], {},
                       float("nan"), float("inf"), float("-inf")]
            invalid += [0.5, 1.0] if key == "min_spots" else [1.01, 10 ** 400]
            for value in invalid:
                with self.subTest(key=key, value=value):
                    out = self.score([self.iq_entry(floors={key: value})])
                    self.assertEqual(out.returncode, 2, out.stdout + out.stderr)
                    self.assertIn(key, out.stderr)
                    self.assertNotIn("Traceback", out.stderr)
                    self.assertEqual(self.calls(), [])
        for floors in ([], ["min_spots"], "min_spots", False, 0):
            with self.subTest(floors=floors):
                out = self.score([self.iq_entry(floors=floors)])
                self.assertEqual(out.returncode, 2, out.stdout + out.stderr)
                self.assertIn("floors must be an object", out.stderr)
                self.assertEqual(self.calls(), [])

    def test_valid_floor_boundaries_are_accepted(self):
        for floors in ({}, {"min_spots": 0, "min_precision": 0, "min_recall": 0.0},
                       {"min_spots": 10 ** 400, "min_precision": 1.0, "min_recall": 1}):
            with self.subTest(floors=floors):
                path = self.write_manifest([self.iq_entry(floors=floors)])
                self.assertEqual(corpus.load_manifest(path)["recordings"][0]["floors"], floors)

    def test_failed_decode_fails_the_entry(self):
        self.present("iq.wav")
        out = self.score([self.iq_entry(floors={})], status=3)
        self.assertEqual(out.returncode, 1)
        (r,) = self.results()["results"]
        self.assertEqual(r["exit_code"], 3)
        self.assertIn("decode exited 3", r["errors"])
        self.assertIn("decode exited 3", out.stderr)

    def test_no_manifest_recording_present_exits_2(self):
        self.present("unrelated.wav")
        out = self.score([self.iq_entry()])
        self.assertEqual(out.returncode, 2)
        self.assertIn("holds none of the manifest's recordings", out.stderr)
        self.assertEqual(self.calls(), [])

    def test_no_binary_exits_2(self):
        # A copy of the scripts outside the repo, so target/{release,debug}/manta is not found.
        scripts = os.path.join(self.tmp, "repo", "scripts")
        os.makedirs(scripts)
        for name in ("corpus.py", "score-against-rbn.py", "score-transcript.py"):
            shutil.copy(os.path.join(ROOT, "scripts", name), scripts)
        self.present("iq.wav")
        manifest = self.write_manifest([self.iq_entry()])
        out = self.run_script(["--manifest", manifest, "score", "--corpus-dir", self.dir],
                              script=os.path.join(scripts, "corpus.py"))
        self.assertEqual(out.returncode, 2)
        self.assertIn("no manta binary found", out.stderr)

    def test_center_freq_passed_only_when_the_manifest_has_one(self):
        self.present("iq.wav", "plain.wav")
        out = self.score([self.iq_entry(floors={}),
                          self.iq_entry(file="plain.wav", center_freq_hz=None, truth=None, floors={})])
        self.assertEqual(out.returncode, 0, out.stderr)
        calls = self.calls()
        self.assertEqual(calls[0], ["decode", os.path.join(self.dir, "iq.wav"), "--json",
                                    "--center-freq-hz", "7080000"])
        self.assertEqual(calls[1], ["decode", os.path.join(self.dir, "plain.wav"), "--json"])

    def test_absent_entries_are_skipped(self):
        self.present("iq.wav")
        out = self.score([self.iq_entry(), self.iq_entry(file="gone.wav")])
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertEqual(len(self.calls()), 1)
        self.assertEqual(self.results()["absent"], ["gone.wav"])

    def test_markdown_summary_goes_to_github_step_summary(self):
        self.present("iq.wav")
        summary = os.path.join(self.tmp, "summary.md")
        with open(summary, "w") as f:
            f.write("earlier step\n")
        out = self.score([self.iq_entry()], env={"GITHUB_STEP_SUMMARY": summary})
        self.assertEqual(out.returncode, 0, out.stderr)
        with open(summary) as f:
            text = f.read()
        self.assertTrue(text.startswith("earlier step\n"))
        self.assertIn("| recording |", text)
        self.assertIn("| iq.wav |", text)

    def test_audio_entry_with_transcript_truth(self):
        self.present("vp.wav")
        out = self.score([self.audio_entry()])
        self.assertEqual(out.returncode, 0, out.stdout + out.stderr)
        self.assertEqual(self.calls(), [["run", "--source", os.path.join(self.dir, "vp.wav"), "--json"]])
        (r,) = self.results()["results"]
        t = r["score"]["transcript"]
        self.assertEqual(r["spots"], 1)
        self.assertEqual((t["spot_precision"], t["call_recall"]), (1.0, 0.5))
        self.assertAlmostEqual(t["word_recall"], 6 / 7)
        self.assertIn("word recall 85.7%", out.stdout)

    def test_transcript_min_precision_applies_to_spot_precision(self):
        # Move the one spot 60 s past its row: spot precision drops to 0.
        with open(self.run_jsonl) as f:
            text = f.read()
        moved = text.replace('"sampleTs":700800,"rst":null,"qrlQuery":false}}',
                             '"sampleTs":3600000,"rst":null,"qrlQuery":false}}')
        self.assertNotEqual(moved, text)
        with open(self.run_jsonl, "w") as f:
            f.write(moved)
        self.present("vp.wav")
        out = self.score([self.audio_entry()])
        self.assertEqual(out.returncode, 1)
        self.assertIn("min_precision 0.5: measured 0.0000", out.stdout)


class WorkflowTests(unittest.TestCase):
    def setUp(self):
        self.text = wf.read("corpus.yml")
        self.job = wf.job_block(self.text, "corpus")
        self.steps = wf.steps(self.job)

    def test_nightly_and_manual_triggers(self):
        self.assertRegex(self.text, re.compile(r"^on:\n  schedule:\n    - cron: '[0-9*/ ,-]+'$", re.M))
        self.assertRegex(self.text, re.compile(r"^  workflow_dispatch:", re.M))

    def test_job_waits_for_the_base_url_variable(self):
        self.assertTrue(self.job, "corpus.yml has no job named corpus")
        self.assertEqual(wf.field(self.job, "if"), "vars.MANTA_CORPUS_BASE_URL != ''")

    def test_read_only_token_timeout_and_no_persisted_credentials(self):
        self.assertRegex(self.text, re.compile(r"^permissions:\n  contents: read$", re.M))
        self.assertIsNotNone(wf.field(self.job, "timeout-minutes"))
        checkout = [s for s in self.steps if (wf.field(s, "uses") or "").startswith("actions/checkout@")]
        self.assertEqual(len(checkout), 1)
        self.assertEqual(wf.field(wf.sub(checkout[0], "with"), "persist-credentials"), "false")

    def test_fetches_tests_and_scores(self):
        runs = [(wf.run_body(s) or "").strip() for s in self.steps]
        self.assertTrue(any(r.startswith("python3 scripts/corpus.py fetch ") for r in runs), runs)
        self.assertTrue(any(r.startswith("python3 scripts/corpus.py score ") for r in runs), runs)
        self.assertTrue(any("--test real_audio_corpus" in r for r in runs), runs)
        upload = [s for s in self.steps if (wf.field(s, "uses") or "").startswith("actions/upload-artifact@")]
        self.assertEqual(len(upload), 1)
        self.assertEqual(wf.field(upload[0], "if"), "always()")

    def test_url_reaches_run_only_through_env(self):
        self.assertEqual(wf.field(wf.sub(self.job, "env"), "MANTA_CORPUS_BASE_URL"),
                         "${{ vars.MANTA_CORPUS_BASE_URL }}")
        for step in self.steps:
            body = wf.run_body(step)
            if body is not None:
                with self.subTest(run=body.strip()):
                    self.assertNotIn("${{", body)

    def test_actions_are_pinned_to_a_sha(self):
        for step in self.steps:
            uses = wf.field(step, "uses")
            if uses is not None:
                with self.subTest(uses=uses):
                    self.assertRegex(uses, r"^[^@\s]+@[0-9a-f]{40}( # \S.*)?$")

    def test_cited_decision_record_exists(self):
        refs = set(re.findall(r"docs/DECISIONS/[0-9]{4}-[0-9]{2}-[0-9]{2}-man119-[A-Za-z0-9._-]*?\.md", self.text))
        self.assertEqual(refs, {"docs/DECISIONS/2026-10-14-man119-streaming-wav-and-real-audio-corpus.md"})
        for ref in refs:
            self.assertTrue(os.path.isfile(os.path.join(ROOT, ref)), f"corpus.yml names missing {ref}")

    def test_ci_full_runs_both_unittest_modules(self):
        test_job = wf.job_block(wf.read("ci-full.yml"), "test")
        for module in ("scripts.tests.test_corpus", "scripts.tests.test_score_transcript"):
            wanted = "python3 -m unittest %s -v" % module
            with self.subTest(module=module):
                matches = [s for s in wf.steps(test_job) if wf.field(s, "run") == wanted]
                self.assertEqual(len(matches), 1, f"job test has no step running {wanted!r}")
                self.assertEqual(wf.field(matches[0], "if"), "runner.os != 'Windows'")


if __name__ == "__main__":
    unittest.main()
