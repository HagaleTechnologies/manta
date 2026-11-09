"""MAN-205: scripts/engine-overlap.py, the legacy-vs-HSMM engine-overlap ("oracle gap") tool.
Covers bench mode (`manta bench sensitivity --json` reports) and oracle mode (`manta oracle
--jsonl` files): the 2x2 and every rate, headroom as the smaller one-sided count, the strict
copy threshold, paired false identities, pairing and every refusal, the golden Markdown, the
JSON numbers, the fusion gate's `>=` bar, bootstrap determinism, the committed real-format
fixtures, and this module's CI step and CLAUDE.md pre-push entry.

stdlib unittest only. Run: python3 -m unittest scripts.tests.test_engine_overlap -v

The committed fixtures under scripts/tests/fixtures/engine-overlap/ come from manta 0.1.0 at
212fa51 (release build):

  for e in legacy hsmm; do
    target/release/manta bench sensitivity --engine $e --conditions good --wpm 35 --snr-db 15 \\
      --trials 1 --seed 1 --jobs 1 --json > scripts/tests/fixtures/engine-overlap/bench-$e.json
  done
  S=$(mktemp -d); target/release/manta gen vr7 --out "$S"
  printf 'callsign,de_pfx,de_cont,freq,band,dx,dx_pfx,dx_cont,mode,db,date,speed,tx_mode\\n'\\
'K5TR,K,NA,14012.3,20m,JA1ZZZ,JA,AS,CQ,20,2025-01-01 00:01:00,25,CW\\n' > "$S/vr7.rbn.csv"
  for e in legacy hsmm; do
    target/release/manta oracle "$S/vr7.wav" "$S/vr7.rbn.csv" --spotter K5TR \\
      --capture-start 2025-01-01T00:00:00Z --engine $e \\
      --jsonl scripts/tests/fixtures/engine-overlap/oracle-$e.jsonl
  done
"""
import contextlib
import copy
import importlib.util
import io
import json
import os
import re
import shutil
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
SCRIPT = os.path.join(ROOT, "scripts", "engine-overlap.py")
FIXTURES = os.path.join(HERE, "fixtures", "engine-overlap")
TEST_COMMAND = "python3 -m unittest scripts.tests.test_engine_overlap -v"


def _load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


eo = _load("engine_overlap", SCRIPT)
# Not test_ci_gates.py: it needs tomllib (Python 3.11+), and this module runs on macOS too.
wf = _load("_ci_workflow_text", os.path.join(HERE, "test_ci_trust_boundary.py"))


def station(call, spotted, cer=0.5, trial=0, offset_hz=None, **extra):
    st = {"trial": trial, "call": call,
          "offset_hz": float(offset_hz if offset_hz is not None else 1000 + 100 * len(call)),
          "spotted": spotted, "spots": 1 if spotted else 0, "first_spot_s": 1.0 if spotted else None,
          "spot_snr_db": 10.0 if spotted else None, "cer": cer}
    st.update(extra)
    return st


def point(stations, condition="awgn", wpm=25, snr_db=6.0, bogus=()):
    return {"condition": condition, "wpm": wpm, "snr_db": snr_db, "snr_2500_db": snr_db - 7.0,
            "stations": len(stations), "spotted": sum(s["spotted"] for s in stations),
            "bogus_calls": list(bogus), "cer_mean": 0.5, "good_copy": 0, "spot_snr_db": None,
            "per_station": stations}


def report(points, engine="legacy", seed=1, trials=1, **over):
    rep = {"report": "manta bench sensitivity", "format_version": 1, "manta_version": "0.1.0",
           "engine": engine, "config_file": None, "command": "manta bench sensitivity --json",
           "snr_ref_hz": 500, "sample_rate_hz": 192000, "center_freq_hz": 14025000,
           "stations_per_trial": 10, "duration_s": 60, "trials": trials, "seed": seed,
           "payload": "CQ CQ DE {call} {call} K", "match_hz": 150, "points": points}
    rep.update(over)
    return rep


def four_outcome_stations(a_side):
    """both, A only, B only, neither; A's or B's spotted flags."""
    flags = [(True, True), (True, False), (False, True), (False, False)]
    calls = ["K1AA", "K2BB", "K3CC", "K4DD"]
    i = 0 if a_side else 1
    return [station(c, f[i], offset_hz=500 + 100 * n) for n, (c, f) in enumerate(zip(calls, flags))]


class Base(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.tmp)
        self.n = 0

    def write(self, obj, name=None, raw=None):
        self.n += 1
        path = os.path.join(self.tmp, name or f"r{self.n}.json")
        with open(path, "w", encoding="utf-8") as f:
            f.write(raw if raw is not None else json.dumps(obj))
        return path

    def write_lines(self, objs, name=None):
        self.n += 1
        path = os.path.join(self.tmp, name or f"o{self.n}.jsonl")
        with open(path, "w", encoding="utf-8") as f:
            for o in objs:
                f.write(json.dumps(o) + "\n")
        return path

    def run_tool(self, argv):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            try:
                rc = eo.main(argv)
            except SystemExit as e:
                rc = e.code
        return rc, out.getvalue(), err.getvalue()

    def bench(self, a, b, *extra):
        a = a if isinstance(a, list) else [a]
        b = b if isinstance(b, list) else [b]
        return self.run_tool(["bench", "--a", *a, "--b", *b, "--bootstrap", "0", *extra])

    def bench_json(self, a, b, *extra):
        out = os.path.join(self.tmp, "out.json")
        rc, stdout, err = self.bench(a, b, "--json", out, *extra)
        self.assertEqual(rc, 0, err)
        with open(out, encoding="utf-8") as f:
            return json.load(f), stdout

    def overall(self, a, b, *extra):
        return self.bench_json(a, b, *extra)[0]["overall"]


class TallyTests(Base):
    def test_four_outcomes_give_the_2x2_and_rates(self):
        a = self.write(report([point(four_outcome_stations(True))]))
        b = self.write(report([point(four_outcome_stations(False))], engine="hsmm"))
        o = self.overall(a, b)["identity"]
        self.assertEqual((o["n"], o["both"], o["a_only"], o["b_only"], o["neither"]), (4, 1, 1, 1, 1))
        self.assertEqual(o["recall_a"], 0.5)
        self.assertEqual(o["recall_b"], 0.5)
        self.assertEqual(o["exactly_one"], 0.5)
        self.assertEqual(o["union_recall"], 0.75)
        self.assertEqual(o["headroom"], 0.25)
        _, _, text = None, None, self.bench(a, b)[1]
        self.assertIn("| Identity | 4 | 1 | 1 | 1 | 1 | 50.0% | 50.0% | 50.0% | 75.0% | 25.00 pp |",
                      text)

    def test_headroom_is_the_smaller_only_count(self):
        # A only 3, B only 1, both 4, neither 2
        flags = [(True, False)] * 3 + [(False, True)] + [(True, True)] * 4 + [(False, False)] * 2
        sa = [station(f"K{i}XX", f[0], offset_hz=500 + 50 * i) for i, f in enumerate(flags)]
        sb = [station(f"K{i}XX", f[1], offset_hz=500 + 50 * i) for i, f in enumerate(flags)]
        o = self.overall(self.write(report([point(sa)])),
                         self.write(report([point(sb)], engine="hsmm")))["identity"]
        self.assertAlmostEqual(o["exactly_one"], 0.4)
        self.assertAlmostEqual(o["union_recall"], 0.8)
        self.assertAlmostEqual(max(o["recall_a"], o["recall_b"]), 0.7)
        self.assertAlmostEqual(o["headroom"], 0.1)
        self.assertAlmostEqual(o["union_recall"] - max(o["recall_a"], o["recall_b"]), o["headroom"])
        self.assertAlmostEqual(o["headroom_pp"], 10.0)

    def test_copy_level_is_cer_strictly_below_0_10(self):
        sa = [station("K1AA", True, cer=0.0999, offset_hz=500),
              station("K2BB", True, cer=0.10, offset_hz=600),
              station("K3CC", True, cer=1.0, offset_hz=700)]
        sb = [station("K1AA", True, cer=0.5, offset_hz=500),
              station("K2BB", True, cer=0.5, offset_hz=600),
              station("K3CC", True, cer=0.5, offset_hz=700)]
        o = self.overall(self.write(report([point(sa)])),
                         self.write(report([point(sb)], engine="hsmm")))["copy"]
        self.assertEqual((o["both"], o["a_only"], o["b_only"], o["neither"]), (0, 1, 0, 2))

    def test_a_report_compared_with_itself_has_no_complementary_recovery(self):
        # The measurement-side analogue of the ticket's duplicate-branch invariant.
        sts = four_outcome_stations(True)
        sts[0]["cer"] = 0.01
        sts[2]["cer"] = 0.05
        path = self.write(report([point(sts), point(sts, condition="good")]))
        out, text = self.bench_json(path, path)
        for lv in ("identity", "copy"):
            o = out["overall"][lv]
            self.assertEqual((o["a_only"], o["b_only"]), (0, 0))
            self.assertEqual(o["exactly_one"], 0.0)
            self.assertEqual(o["headroom"], 0.0)
            self.assertEqual(o["union_recall"], o["recall_a"])
            self.assertEqual(o["recall_a"], o["recall_b"])
        self.assertIn("Headroom over the better engine: 0.00 pp (identity), 0.00 pp (copy).", text)
        self.assertEqual(out["false_identities"]["either"], out["false_identities"]["a"])

    def test_claimed_values_never_change_the_result(self):
        a = self.write(report([point(four_outcome_stations(True))]))
        b_rep = report([point(four_outcome_stations(False))], engine="hsmm")
        b = self.write(b_rep)
        mutated = copy.deepcopy(b_rep)
        for p in mutated["points"]:
            p["spotted"] = 99
            p["cer_mean"] = 0.0
            p["good_copy"] = 99
            p["spot_snr_db"] = 40.0
            for st in p["per_station"]:
                st["spots"] = 1000
                st["first_spot_s"] = 0.0
                st["spot_snr_db"] = 99.0
        b2 = self.write(mutated)
        args = ("--bootstrap", "200")
        self.assertEqual(self.run_tool(["bench", "--a", a, "--b", b, *args])[1],
                         self.run_tool(["bench", "--a", a, "--b", b2, *args])[1])


class FalseIdentityTests(Base):
    def test_shared_is_per_point_multiset_intersection(self):
        sts = [station("W1AW", True, offset_hz=500)]
        a = self.write(report([point(sts, snr_db=6.0, bogus=["K1AA", "K2BB"]),
                               point(sts, snr_db=9.0, bogus=["K2BB", "K2BB"])]))
        b = self.write(report([point(sts, snr_db=6.0, bogus=["K2BB", "K3CC"]),
                               point(sts, snr_db=9.0, bogus=["K2BB"])], engine="hsmm"))
        fi = self.bench_json(a, b)[0]["false_identities"]
        self.assertEqual(fi, {"a": 4, "b": 3, "shared": 2, "either": 5})


class PairingTests(Base):
    def test_files_on_one_side_merge_and_seed_is_part_of_the_key(self):
        sts = four_outcome_stations(True)
        a = [self.write(report([point(sts)], seed=1)),
             self.write(report([point(sts, condition="good")], seed=1)),
             self.write(report([point(sts)], seed=2))]
        b = [self.write(report([point(sts)], seed=1, engine="hsmm")),
             self.write(report([point(sts, condition="good")], seed=1, engine="hsmm")),
             self.write(report([point(sts)], seed=2, engine="hsmm"))]
        out, text = self.bench_json(a, b)
        self.assertEqual(out["overall"]["identity"]["n"], 12)
        self.assertIn("12 paired truth identities: 3 points,", text)
        self.assertIn("seeds 1, 2,", text)
        self.assertEqual(out["a"]["seeds"], [1, 2])
        self.assertEqual(len(out["a"]["files"]), 3)

    def test_points_on_one_side_only_are_counted_and_left_out(self):
        sts = four_outcome_stations(True)
        a = self.write(report([point(sts), point(sts, snr_db=9.0)]))
        b = self.write(report([point(sts)], engine="hsmm"))
        out, text = self.bench_json(a, b)
        self.assertIn("Unpaired points left out: legacy 1, hsmm 0.", text)
        self.assertEqual(out["overall"]["identity"]["n"], 4)
        self.assertEqual(out["unpaired_points"], {"a": 1, "b": 0})

    def test_refusals_exit_2_with_one_stderr_line(self):
        sts = four_outcome_stations(True)
        good_a = report([point(sts)])
        good_b = report([point(sts)], engine="hsmm")
        cases = {
            "wrong report name": (dict(good_a, report="manta bench other"), good_b),
            "format_version 2": (dict(good_a, format_version=2), good_b),
            "trials differ": (dict(good_a, trials=3), good_b),
            "duration_s differs": (dict(good_a, duration_s=30), good_b),
            "stations_per_trial differs": (dict(good_a, stations_per_trial=5), good_b),
            "a point twice on one side": (report([point(sts), point(sts)]), good_b),
            "different call": (good_a, report([point(four_outcome_stations(True)[:3]
                                                      + [station("K9ZZ", False, offset_hz=800)])],
                                              engine="hsmm")),
            "different offset_hz": (good_a, report([point(four_outcome_stations(True)[:3]
                                                          + [station("K4DD", False, offset_hz=801)])],
                                                   engine="hsmm")),
            "nothing pairs": (good_a, report([point(sts, condition="poor")], engine="hsmm")),
            "format_version true": (dict(good_a, format_version=True), good_b),
            "no stations pair": (report([point([])]), report([point([])], engine="hsmm")),
            "station missing a field": (good_a, report([dict(point(sts),
                                                             per_station=[{"trial": 0, "call": "K1AA"}])],
                                                       engine="hsmm")),
        }
        for name, (ra, rb) in cases.items():
            with self.subTest(name):
                rc, out, err = self.bench(self.write(ra), self.write(rb))
                self.assertEqual(rc, 2)
                self.assertEqual(out, "")
                self.assertEqual(len(err.strip().splitlines()), 1, err)
                self.assertTrue(err.startswith("engine-overlap.py: error: "), err)
        with self.subTest("one side mixes legacy and hsmm"):
            rc, _, err = self.bench([self.write(good_a),
                                     self.write(report([point(sts, condition="good")], engine="hsmm"))],
                                    self.write(good_b))
            self.assertEqual(rc, 2)
            self.assertIn("mixes engines", err)
            self.assertEqual(len(err.strip().splitlines()), 1)
        with self.subTest("unreadable JSON"):
            rc, _, err = self.bench(self.write(None, raw="{not json"), self.write(good_b))
            self.assertEqual(rc, 2)
            self.assertEqual(len(err.strip().splitlines()), 1)
        with self.subTest("missing file"):
            rc, _, err = self.bench(os.path.join(self.tmp, "nope.json"), self.write(good_b))
            self.assertEqual(rc, 2)
            self.assertEqual(len(err.strip().splitlines()), 1)


GOLDEN_BENCH = """\
## Engine overlap: legacy (A) vs hsmm (B)

8 paired truth identities: 2 points, 1 trial of 10 stations, seed 1, 60 s recordings.
Unpaired points left out: legacy 0, hsmm 0.
Identity: the station's call spotted within 150 Hz of it. Copy: its decoded copy has CER below 0.10.
Exactly one is the ticket's oracle-gap rate: identities one engine recovers and the other misses.

### Overall

| Level | n | Both | legacy only | hsmm only | Neither | legacy recall | hsmm recall | Exactly one | Union recall | Headroom |
|---|---|---|---|---|---|---|---|---|---|---|
| Identity | 8 | 2 | 2 | 1 | 3 | 50.0% | 37.5% | 37.5% | 62.5% | 12.50 pp |
| Copy | 8 | 1 | 1 | 0 | 6 | 25.0% | 12.5% | 12.5% | 25.0% | 0.00 pp |

### False identities

|  | legacy | hsmm | Shared | Either |
|---|---|---|---|---|
| Calls spotted that no station sent | 1 | 2 | 1 | 2 |

Each trial's distinct bogus calls, pooled per point as the bench reports them; shared is the same call at the same point from both engines.

### By condition (identity)

| Condition | n | legacy recall | hsmm recall | legacy only | hsmm only | Exactly one | Headroom |
|---|---|---|---|---|---|---|---|
| awgn | 4 | 50.0% | 50.0% | 1 | 1 | 50.0% | 25.00 pp |
| good | 4 | 50.0% | 25.0% | 1 | 0 | 25.0% | 0.00 pp |

### By condition (copy)

| Condition | n | legacy recall | hsmm recall | legacy only | hsmm only | Exactly one | Headroom |
|---|---|---|---|---|---|---|---|
| awgn | 4 | 25.0% | 25.0% | 0 | 0 | 0.0% | 0.00 pp |
| good | 4 | 25.0% | 0.0% | 1 | 0 | 25.0% | 0.00 pp |

### By speed (identity)

| Speed | n | legacy recall | hsmm recall | legacy only | hsmm only | Exactly one | Headroom |
|---|---|---|---|---|---|---|---|
| 25 WPM | 8 | 50.0% | 37.5% | 2 | 1 | 37.5% | 12.50 pp |

### By speed (copy)

| Speed | n | legacy recall | hsmm recall | legacy only | hsmm only | Exactly one | Headroom |
|---|---|---|---|---|---|---|---|
| 25 WPM | 8 | 25.0% | 12.5% | 1 | 0 | 12.5% | 0.00 pp |

### By SNR, 500 Hz (identity)

| SNR | n | legacy recall | hsmm recall | legacy only | hsmm only | Exactly one | Headroom |
|---|---|---|---|---|---|---|---|
| 7.5 dB | 8 | 50.0% | 37.5% | 2 | 1 | 37.5% | 12.50 pp |

### By SNR, 500 Hz (copy)

| SNR | n | legacy recall | hsmm recall | legacy only | hsmm only | Exactly one | Headroom |
|---|---|---|---|---|---|---|---|
| 7.5 dB | 8 | 25.0% | 12.5% | 1 | 0 | 12.5% | 0.00 pp |

### By trial (identity)

| Trial | n | legacy recall | hsmm recall | legacy only | hsmm only | Exactly one | Headroom |
|---|---|---|---|---|---|---|---|
| 0 | 8 | 50.0% | 37.5% | 2 | 1 | 37.5% | 12.50 pp |

### By trial (copy)

| Trial | n | legacy recall | hsmm recall | legacy only | hsmm only | Exactly one | Headroom |
|---|---|---|---|---|---|---|---|
| 0 | 8 | 25.0% | 12.5% | 1 | 0 | 12.5% | 0.00 pp |

### Fusion gate

Headroom over the better engine: 12.50 pp (identity), 0.00 pp (copy). Bar: 1.00 pp.
Reading: meets the bar at identity.
"""


class RenderTests(Base):
    def golden_inputs(self):
        sa = four_outcome_stations(True)
        sb = four_outcome_stations(False)
        sa[0]["cer"] = 0.02
        sb[0]["cer"] = 0.03
        ga = four_outcome_stations(True)
        gb = four_outcome_stations(False)
        gb[2]["spotted"] = False  # good: B loses its one-sided win
        ga[1]["cer"] = 0.0
        # Written out of bench order on purpose: good before awgn.
        a = self.write(report([point(ga, condition="good", snr_db=7.5, bogus=["K7XX"]),
                               point(sa, condition="awgn", snr_db=7.5)]))
        b = self.write(report([point(gb, condition="good", snr_db=7.5, bogus=["K7XX", "K8YY"]),
                               point(sb, condition="awgn", snr_db=7.5)], engine="hsmm"))
        return a, b

    def test_markdown_matches_the_golden_text(self):
        a, b = self.golden_inputs()
        rc, out, err = self.bench(a, b)
        self.assertEqual(rc, 0, err)
        self.maxDiff = None
        self.assertEqual(out, GOLDEN_BENCH)
        self.assertNotIn(self.tmp, out)

    def test_breakdowns_use_bench_order(self):
        sts = four_outcome_stations(True)
        pts = [point(sts, condition=c, wpm=w, snr_db=s)
               for c in ("zeta", "poor", "alpha", "good", "awgn")
               for w in (35, 15) for s in (12.0, 6.0, 7.5)]
        a = self.write(report(pts))
        b = self.write(report(pts, engine="hsmm"))
        text = self.bench(a, b)[1]
        cond = re.findall(r"^\| (\w+) \| 24 \|", text.split("### By condition (identity)")[1]
                          .split("###")[0], re.M)
        self.assertEqual(cond, ["awgn", "good", "poor", "alpha", "zeta"])
        speed = text.split("### By speed (identity)")[1].split("###")[0]
        self.assertLess(speed.index("15 WPM"), speed.index("35 WPM"))
        snr = text.split("### By SNR, 500 Hz (identity)")[1].split("###")[0]
        self.assertEqual(re.findall(r"^\| ([0-9.]+ dB) \|", snr, re.M), ["6 dB", "7.5 dB", "12 dB"])

    def test_same_engine_on_both_sides_is_labelled_a_and_b(self):
        sts = four_outcome_stations(True)
        a = self.write(report([point(sts)]))
        b = self.write(report([point(sts)]))
        text = self.bench(a, b)[1]
        self.assertIn("## Engine overlap: legacy (A) vs legacy (B)\n", text)
        self.assertIn("| legacy (A) only | legacy (B) only |", text)

    def test_json_carries_the_same_numbers(self):
        a, b = self.golden_inputs()
        out, text = self.bench_json(a, b)
        for key in ("mode", "levels", "a", "b", "bar_pp", "bootstrap", "unpaired_points", "overall",
                    "ci95", "false_identities", "by_condition", "by_wpm", "by_snr_db", "by_trial",
                    "gate"):
            self.assertIn(key, out)
        self.assertEqual(out["mode"], "bench")
        self.assertEqual(out["a"]["engine"], "legacy")
        self.assertEqual(out["b"]["engine"], "hsmm")
        self.assertIn(f"| {100 * out['overall']['identity']['exactly_one']:.1f}% |", text)
        self.assertEqual(eo.pct(out["overall"]["identity"]["exactly_one"]), "37.5%")
        self.assertEqual([r["condition"] for r in out["by_condition"]], ["awgn", "good"])
        self.assertEqual(out["by_condition"][1]["identity"]["a_only"], 1)
        self.assertEqual(out["gate"], {"identity_headroom_pp": 12.5, "copy_headroom_pp": 0.0,
                                       "bar_pp": 1.0, "meets_bar": True,
                                       "levels_meeting_bar": ["identity"]})
        self.assertEqual(out["ci95"], {})
        self.assertEqual(out["a"]["files"], [os.path.basename(a)])


class GateTests(Base):
    def stations(self, a_only, b_only, both, neither):
        flags = ([(True, False)] * a_only + [(False, True)] * b_only + [(True, True)] * both
                 + [(False, False)] * neither)
        sa = [station(f"K{i}GT", f[0], offset_hz=100 + 10 * i) for i, f in enumerate(flags)]
        sb = [station(f"K{i}GT", f[1], offset_hz=100 + 10 * i) for i, f in enumerate(flags)]
        return self.write(report([point(sa)])), self.write(report([point(sb)], engine="hsmm"))

    def test_headroom_equal_to_the_bar_meets_it(self):
        a, b = self.stations(a_only=5, b_only=1, both=50, neither=44)  # headroom 1/100 = 1.00 pp
        out, text = self.bench_json(a, b)
        self.assertTrue(out["gate"]["meets_bar"])
        self.assertIn("Reading: meets the bar at identity.", text)
        out, text = self.bench_json(a, b, "--bar-pp", "1.01")
        self.assertFalse(out["gate"]["meets_bar"])
        self.assertIn("Reading: below the bar at both levels.", text)

    def test_either_level_meeting_the_bar_meets_it(self):
        # (A identity, B identity, A copy, B copy): identity A only 4, B only 0 (no headroom);
        # copy A only 2, B only 2 (headroom 20 pp).
        flags = ([(True, False, True, False)] * 2 + [(True, False, False, True)] * 2
                 + [(True, True, True, True)] * 6)
        sa = [station(f"K{i}EL", f[0], cer=0.0 if f[2] else 0.9, offset_hz=100 + 10 * i)
              for i, f in enumerate(flags)]
        sb = [station(f"K{i}EL", f[1], cer=0.0 if f[3] else 0.9, offset_hz=100 + 10 * i)
              for i, f in enumerate(flags)]
        out, text = self.bench_json(self.write(report([point(sa)])),
                                    self.write(report([point(sb)], engine="hsmm")))
        self.assertEqual(out["overall"]["identity"]["headroom"], 0.0)
        self.assertGreater(out["overall"]["copy"]["headroom"], 0.0)
        self.assertTrue(out["gate"]["meets_bar"])
        self.assertIn("Reading: meets the bar at copy.", text)

    def test_bar_must_be_finite_and_positive(self):
        a, b = self.stations(1, 1, 1, 1)
        for bar in ("0", "-1", "nan", "inf"):
            with self.subTest(bar):
                rc, out, err = self.bench(a, b, "--bar-pp", bar)
                self.assertEqual(rc, 2)
                self.assertEqual(out, "")
                self.assertIn("--bar-pp", err)


class BootstrapTests(Base):
    def corpus(self):
        pts = []
        for trial in range(4):
            for snr in (3.0, 6.0, 9.0):
                flags = [((trial + i) % 3 != 0, (trial * i + int(snr)) % 4 != 0) for i in range(5)]
                pts.append((snr, trial, flags))
        sides = []
        for side in (0, 1):
            by_snr = {}
            for snr, trial, flags in pts:
                by_snr.setdefault(snr, []).extend(
                    station(f"K{i}T{trial}", f[side], trial=trial, offset_hz=100 * i)
                    for i, f in enumerate(flags))
            sides.append(self.write(report([point(s, snr_db=snr) for snr, s in by_snr.items()],
                                           engine=("legacy", "hsmm")[side], trials=4)))
        return sides

    def test_two_runs_print_identical_bytes(self):
        a, b = self.corpus()
        args = ["bench", "--a", a, "--b", b, "--bootstrap", "300"]
        first = self.run_tool(args)
        second = self.run_tool(args)
        self.assertEqual(first[0], 0, first[2])
        self.assertEqual(first[1], second[1])
        self.assertIn("95% intervals, 300 signal-set (seed, condition, speed, trial) resamples:",
                      first[1])

    def test_identical_outcomes_give_a_zero_width_interval(self):
        path = self.corpus()[0]
        ci = eo.bootstrap(eo.pair_bench(eo.load_bench([path], "A"), eo.load_bench([path], "B"))[0],
                          eo.BENCH_LEVELS, 200)
        for lv in eo.BENCH_LEVELS:
            self.assertEqual(ci[lv]["exactly_one"], (0.0, 0.0))
            self.assertEqual(ci[lv]["headroom"], (0.0, 0.0))

    def test_resamples_whole_signal_sets_with_the_documented_bounds(self):
        a, b = self.corpus()
        pairs = eo.pair_bench(eo.load_bench([a], "A"), eo.load_bench([b], "B"))[0]
        ci = eo.bootstrap(pairs, eo.BENCH_LEVELS, 40)
        lo, hi = ci["identity"]["recall_a"]
        self.assertLessEqual(lo, hi)
        # The three SNRs of one signal set are one cluster: 1 condition x 1 speed x 4 trials.
        self.assertEqual({p.cluster for p in pairs}, {(1, "awgn", 25, t) for t in range(4)})

    def test_bootstrap_0_prints_no_interval_line(self):
        a, b = self.corpus()
        text = self.bench(a, b)[1]
        self.assertNotIn("95% intervals", text)


class FixtureTests(Base):
    def test_real_bench_reports_parse_and_pair(self):
        out, text = self.bench_json(os.path.join(FIXTURES, "bench-legacy.json"),
                                    os.path.join(FIXTURES, "bench-hsmm.json"))
        ident, copy_ = out["overall"]["identity"], out["overall"]["copy"]
        self.assertEqual((ident["n"], ident["both"], ident["a_only"], ident["b_only"],
                          ident["neither"]), (10, 8, 1, 1, 0))
        # K1KBC's legacy CER is exactly 0.1: not good copy.
        self.assertEqual((copy_["both"], copy_["a_only"], copy_["b_only"], copy_["neither"]),
                         (2, 5, 1, 2))
        self.assertEqual(out["false_identities"], {"a": 0, "b": 1, "shared": 0, "either": 1})
        self.assertIn("## Engine overlap: legacy (A) vs hsmm (B)\n", text)

    def test_a_real_report_compared_with_itself(self):
        path = os.path.join(FIXTURES, "bench-legacy.json")
        out, text = self.bench_json(path, path)
        for lv in ("identity", "copy"):
            self.assertEqual(out["overall"][lv]["exactly_one"], 0.0)
            self.assertEqual(out["overall"][lv]["headroom"], 0.0)


def ref_spot(call, khz, as_word, framed, **extra):
    o = {"call": call, "khz": khz, "as_word": as_word, "framed": framed, "substring": as_word,
         "wpm_ratio": 1.0, "text": f"CQ DE {call}"}
    o.update(extra)
    return o


GOLDEN_ORACLE = """\
## Engine overlap: legacy (A) vs hsmm (B), manta oracle reference spots

4 paired reference spots.
as_word: the reference call is a whole word of the decoded window. framed: it follows CQ, DE or CQ TEST there.
The oracle scores only reference calls, so there is no false-identity count.
Exactly one is the ticket's oracle-gap rate: identities one engine recovers and the other misses.

### Overall

| Level | n | Both | legacy only | hsmm only | Neither | legacy recall | hsmm recall | Exactly one | Union recall | Headroom |
|---|---|---|---|---|---|---|---|---|---|---|
| as_word | 4 | 1 | 1 | 1 | 1 | 50.0% | 50.0% | 50.0% | 75.0% | 25.00 pp |
| framed | 4 | 0 | 1 | 0 | 3 | 25.0% | 0.0% | 25.0% | 25.0% | 0.00 pp |

### Fusion gate

Headroom over the better engine: 25.00 pp (as_word), 0.00 pp (framed). Bar: 1.00 pp.
Reading: meets the bar at as_word.
"""


class OracleModeTests(Base):
    def sides(self):
        a = [ref_spot("K1AA", 7012.3, True, True), ref_spot("K2BB", 7020.0, True, False),
             ref_spot("K3CC", 7030.1, False, False), ref_spot("K4DD", 7040.0, False, False)]
        b = [ref_spot("K1AA", 7012.3, True, False), ref_spot("K2BB", 7020.0, False, False),
             ref_spot("K3CC", 7030.1, True, False), ref_spot("K4DD", 7040.0, False, False)]
        return a, b

    def oracle(self, a, b, *extra):
        return self.run_tool(["oracle", "--a", a, "--b", b, "--bootstrap", "0", *extra])

    def test_pairs_on_call_and_khz_and_tallies_both_levels(self):
        a, b = self.sides()
        out = os.path.join(self.tmp, "o.json")
        rc, text, err = self.oracle(self.write_lines(a), self.write_lines(list(reversed(b))),
                                    "--json", out)
        self.assertEqual(rc, 0, err)
        with open(out, encoding="utf-8") as f:
            data = json.load(f)
        self.assertEqual(data["levels"], ["as_word", "framed"])
        w, fr = data["overall"]["as_word"], data["overall"]["framed"]
        self.assertEqual((w["both"], w["a_only"], w["b_only"], w["neither"]), (1, 1, 1, 1))
        self.assertEqual((fr["both"], fr["a_only"], fr["b_only"], fr["neither"]), (0, 1, 0, 3))
        self.assertEqual(data["gate"]["as_word_headroom_pp"], 25.0)

    def test_names_default_to_a_and_b_and_take_overrides(self):
        a, b = self.sides()
        pa, pb = self.write_lines(a), self.write_lines(b)
        text = self.oracle(pa, pb)[1]
        self.assertIn("## Engine overlap: A vs B, manta oracle reference spots\n", text)
        self.assertIn("| A only | B only |", text)
        text = self.oracle(pa, pb, "--a-name", "legacy", "--b-name", "hsmm")[1]
        self.assertIn("## Engine overlap: legacy (A) vs hsmm (B), manta oracle", text)
        text = self.oracle(pa, pb, "--a-name", "hsmm", "--b-name", "hsmm")[1]
        self.assertIn("| hsmm (A) only | hsmm (B) only |", text)
        text = self.oracle(pa, pb, "--a-name", "B", "--b-name", "A")[1]
        self.assertIn("| B (A) only | A (B) only |", text)

    def test_different_reference_spots_exit_2(self):
        a, b = self.sides()
        for name, other in (("B lacks a spot", b[:3]),
                            ("different khz", b[:3] + [ref_spot("K4DD", 7040.1, False, False)])):
            with self.subTest(name):
                rc, out, err = self.oracle(self.write_lines(a), self.write_lines(other))
                self.assertEqual(rc, 2)
                self.assertEqual(out, "")
                self.assertIn("not the same reference spots", err)
                self.assertEqual(len(err.strip().splitlines()), 1)

    def test_duplicate_reference_spot_exits_2(self):
        a, b = self.sides()
        rc, _, err = self.oracle(self.write_lines(a + [a[0]]), self.write_lines(b))
        self.assertEqual(rc, 2)
        self.assertIn("twice", err)

    def test_missing_or_non_boolean_fields_exit_2(self):
        a, b = self.sides()
        bad = {
            "no call": {"khz": 7012.3, "as_word": True, "framed": True},
            "khz a string": ref_spot("K1AA", "7012.3", True, True),
            "as_word missing": {"call": "K1AA", "khz": 7012.3, "framed": True},
            "framed an int": ref_spot("K1AA", 7012.3, True, 1),
            "as_word a string": ref_spot("K1AA", 7012.3, "true", True),
        }
        for name, line in bad.items():
            with self.subTest(name):
                rc, out, err = self.oracle(self.write_lines([line] + a[1:]), self.write_lines(b))
                self.assertEqual(rc, 2)
                self.assertEqual(out, "")
                self.assertEqual(len(err.strip().splitlines()), 1)
        with self.subTest("not JSON"):
            path = self.write(None, name="bad.jsonl", raw="{\n")
            self.assertEqual(self.oracle(path, self.write_lines(b))[0], 2)
        with self.subTest("empty"):
            path = self.write(None, name="empty.jsonl", raw="\n")
            self.assertEqual(self.oracle(path, self.write_lines(b))[0], 2)

    def test_text_and_wpm_ratio_never_change_the_result(self):
        a, b = self.sides()
        b2 = [dict(o, text="TOTALLY DIFFERENT", wpm_ratio=9.9, substring=not o["substring"])
              for o in b]
        pa = self.write_lines(a)
        self.assertEqual(self.run_tool(["oracle", "--a", pa, "--b", self.write_lines(b)])[1],
                         self.run_tool(["oracle", "--a", pa, "--b", self.write_lines(b2)])[1])

    def test_no_false_identity_section_and_no_breakdowns(self):
        a, b = self.sides()
        out = os.path.join(self.tmp, "o.json")
        text = self.oracle(self.write_lines(a), self.write_lines(b), "--json", out)[1]
        self.assertNotIn("### False identities", text)
        self.assertNotIn("### By ", text)
        with open(out, encoding="utf-8") as f:
            data = json.load(f)
        self.assertIsNone(data["false_identities"])
        self.assertNotIn("by_condition", data)

    def test_markdown_matches_the_golden_text(self):
        a, b = self.sides()
        rc, out, err = self.oracle(self.write_lines(a), self.write_lines(b),
                                   "--a-name", "legacy", "--b-name", "hsmm")
        self.assertEqual(rc, 0, err)
        self.maxDiff = None
        self.assertEqual(out, GOLDEN_ORACLE)

    def test_reference_spot_resamples(self):
        a, b = self.sides()
        text = self.run_tool(["oracle", "--a", self.write_lines(a), "--b", self.write_lines(b),
                              "--bootstrap", "100"])[1]
        self.assertIn("95% intervals, 100 reference-spot resamples: exactly one (as_word) ", text)

    def test_real_oracle_lines_parse(self):
        rc, text, err = self.run_tool(["oracle", "--a", os.path.join(FIXTURES, "oracle-legacy.jsonl"),
                                       "--b", os.path.join(FIXTURES, "oracle-hsmm.jsonl"),
                                       "--a-name", "legacy", "--b-name", "hsmm"])
        self.assertEqual(rc, 0, err)
        self.assertIn("1 paired reference spot.\n", text)
        self.assertIn("| as_word | 1 | 1 | 0 | 0 | 0 |", text)
        self.assertIn("| framed | 1 | 1 | 0 | 0 | 0 |", text)


def fusion_stations(flags):
    """Stations from (A, B, F) spotted triples, one per tuple, distinct calls."""
    sides = ([], [], [])
    for n, triple in enumerate(flags):
        call = f"K{n}F{n % 7}"
        for i, side in enumerate(sides):
            side.append(station(call, triple[i], offset_hz=400 + 10 * n))
    return sides


class FusionTests(Base):
    """MAN-545: `fusion` mode, the oracle gap a fused report recovers and its adoption gate."""

    def fusion_files(self, points, bogus=((), (), ())):
        """points: [(snr_db, [(a, b, f), ...])]; writes legacy, hsmm and fused reports."""
        reps = ([], [], [])
        for snr_db, flags in points:
            sides = fusion_stations(flags)
            for i in range(3):
                reps[i].append(point(sides[i], snr_db=snr_db, bogus=bogus[i]))
        a = self.write(report(reps[0]))
        b = self.write(report(reps[1], engine="hsmm"))
        f = self.write(report(reps[2], engine="fused:cal-sub"))
        return a, b, f

    def fusion(self, a, b, f, *extra):
        return self.run_tool(["fusion", "--a", a, "--b", b, "--f", f, "--bootstrap", "0",
                              *extra])

    def fusion_json(self, a, b, f, *extra):
        out = os.path.join(self.tmp, "fusion.json")
        rc, stdout, err = self.fusion(a, b, f, "--json", out, *extra)
        self.assertEqual(rc, 0, err)
        with open(out, encoding="utf-8") as fh:
            return json.load(fh), stdout

    def test_fusion_identical_to_a_has_zero_net(self):
        flags = [(True, True, True), (True, False, True), (False, True, False),
                 (False, False, False)]
        a, b, f = self.fusion_files([(6.0, flags)])
        js, out = self.fusion_json(a, b, f)
        ident = js["overall"]["identity"]
        self.assertEqual((ident["gains"], ident["losses"], ident["net"]), (0, 0, 0))
        self.assertEqual(ident["oracle_gap"], 2)
        # Gap held equals A's share of the gap: A holds its A-only identity.
        self.assertEqual(ident["gap_held"], 0.5)
        self.assertIn("recovered of headroom below 25%", js["gate"]["failed"])
        self.assertIn("Reading: below the adoption bar: recovered of headroom below 25%", out)

    def test_fusion_counts_gains_and_losses_by_origin(self):
        flags = [(False, True, True),    # gained from B only
                 (False, False, True),   # gained from neither
                 (True, True, False),    # lost from both
                 (True, False, True), (False, False, False)]
        a, b, f = self.fusion_files([(6.0, flags)])
        js, _ = self.fusion_json(a, b, f)
        ident = js["overall"]["identity"]
        self.assertEqual(ident["gained_b_only"], 1)
        self.assertEqual(ident["gained_neither"], 1)
        self.assertEqual(ident["lost_both"], 1)
        self.assertEqual(ident["lost_a_only"], 0)
        self.assertEqual(ident["net"], 1)
        self.assertEqual(ident["gains"] - ident["losses"], ident["net"])
        self.assertEqual(ident["both"] + ident["a_only"] + ident["b_only"] + ident["neither"],
                         ident["n"])

    def test_fusion_recovered_of_headroom_uses_b_only(self):
        b_only_gain = [(False, True, True)] + [(False, True, False)] * 3
        a, b, f = self.fusion_files([(6.0, b_only_gain)])
        js, out = self.fusion_json(a, b, f)
        ident = js["overall"]["identity"]
        self.assertEqual(ident["headroom"], 4)
        self.assertEqual(ident["recovered_of_headroom"], 0.25)
        self.assertNotIn("recovered of headroom below 25%", js["gate"]["failed"])
        self.assertIn("Recovered of headroom: 25.0% (bar 25%).", out)
        a, b, f = self.fusion_files([(6.0, [(False, True, False)] * 4)])
        js, _ = self.fusion_json(a, b, f)
        self.assertEqual(js["overall"]["identity"]["recovered_of_headroom"], 0.0)
        self.assertIn("recovered of headroom below 25%", js["gate"]["failed"])

    def test_fusion_zero_headroom_prints_na(self):
        a, b, f = self.fusion_files([(6.0, [(True, False, True), (False, False, False)])])
        js, out = self.fusion_json(a, b, f)
        self.assertIsNone(js["overall"]["identity"]["recovered_of_headroom"])
        self.assertIn("Recovered of headroom: n/a (bar 25%).", out)
        # With no headroom, condition 1 reads as net >= 0.
        self.assertNotIn("recovered of headroom below 25%", js["gate"]["failed"])

    def test_fusion_regression_only_counts_above_10_db(self):
        loss = [(True, False, False), (False, False, False)]
        a, b, f = self.fusion_files([(12.0, loss), (9.0, loss), (10.0, loss)])
        js, out = self.fusion_json(a, b, f)
        regs = js["regressions_above_db"]
        self.assertEqual([r["snr_db"] for r in regs], [12.0])
        self.assertEqual((regs[0]["a_spotted"], regs[0]["f_spotted"]), (1, 0))
        self.assertIn("fewer spots above +10 dB at 1 point", js["gate"]["failed"])
        self.assertIn("| seed 1 awgn/25 WPM/12 dB | 1 | 0 |", out)
        self.assertNotIn("/9 dB |", out)
        self.assertNotIn("/10 dB |", out)

    def test_fusion_false_identities_above_a_fail(self):
        flags = [(True, True, True)]
        a, b, f = self.fusion_files([(6.0, flags)], bogus=(("K7XX",), (), ("K7XX", "K8YY")))
        js, out = self.fusion_json(a, b, f)
        self.assertEqual(js["false_identities"], {"a": 1, "f": 2})
        self.assertIn("more false identities", js["gate"]["failed"])
        self.assertIn("| Calls spotted that no station sent | 1 | 2 |", out)
        a, b, f = self.fusion_files([(6.0, flags)], bogus=(("K7XX",), (), ("K7XX",)))
        js, _ = self.fusion_json(a, b, f)
        self.assertNotIn("more false identities", js["gate"]["failed"])

    def gate_met_files(self):
        """400 identities over 40 signal sets; F gains one B-only identity in each set."""
        points = []
        reps = ([], [], [])
        for trial in range(4):
            for wpm in (15, 20, 25, 30, 35):
                for snr_db in (3.0, 6.0):
                    flags = [(False, True, True)] + [(True, True, True)] * 4 + \
                        [(False, False, False)] * 5
                    sides = ([], [], [])
                    for n, triple in enumerate(flags):
                        for i, side in enumerate(sides):
                            side.append(station(f"K{n}T{trial}", triple[i], trial=trial,
                                                offset_hz=400 + 10 * n))
                    for i in range(3):
                        points.append((i, trial, wpm, snr_db, sides[i]))
        merged = {}
        for i, trial, wpm, snr_db, sts in points:
            merged.setdefault((i, wpm, snr_db), []).extend(sts)
        for (i, wpm, snr_db), sts in sorted(merged.items()):
            reps[i].append(point(sts, wpm=wpm, snr_db=snr_db))
        a = self.write(report(reps[0], trials=4))
        b = self.write(report(reps[1], engine="hsmm", trials=4))
        f = self.write(report(reps[2], engine="fused:cal-sub", trials=4))
        return a, b, f

    def test_fusion_gate_met(self):
        a, b, f = self.gate_met_files()
        rc, out, err = self.run_tool(["fusion", "--a", a, "--b", b, "--f", f,
                                      "--bootstrap", "200"])
        self.assertEqual(rc, 0, err)
        self.assertIn("400 paired truth identities: 10 points, 4 trials of 10 stations", out)
        self.assertIn("Recovered of headroom: 100.0% (bar 25%).", out)
        self.assertIn("Reading: meets the adoption bar.", out)

    def test_fusion_gate_needs_an_interval(self):
        a, b, f = self.gate_met_files()
        js, out = self.fusion_json(a, b, f)
        self.assertEqual(js["gate"]["failed"],
                         ["net gain below 0.25 pp or its interval not above 0"])
        self.assertIn("no interval (--bootstrap 0)", out)

    def test_fusion_bootstrap_is_deterministic(self):
        a, b, f = self.gate_met_files()
        outs = []
        for name in ("one.json", "two.json"):
            path = os.path.join(self.tmp, name)
            rc, text, err = self.run_tool(["fusion", "--a", a, "--b", b, "--f", f,
                                           "--bootstrap", "300", "--json", path])
            self.assertEqual(rc, 0, err)
            with open(path, encoding="utf-8") as fh:
                outs.append((fh.read(), text))
        self.assertEqual(outs[0], outs[1])
        js = json.loads(outs[0][0])
        lo, hi = js["ci95_net"]["identity"]
        est = js["overall"]["identity"]["net"] / js["overall"]["identity"]["n"]
        self.assertLessEqual(lo, est)
        self.assertGreaterEqual(hi, est)
        self.assertEqual(js["bootstrap"], {"resamples": 300, "seed": 205})

    def test_fusion_refuses_mismatched_stations(self):
        a, b, f = self.fusion_files([(6.0, [(True, True, True), (False, False, False)])])
        rep = report([point([station("K9ZZ", True, offset_hz=900)])], engine="fused:cal-sub")
        rc, out, err = self.fusion(a, b, self.write(rep))
        self.assertEqual(rc, 2)
        self.assertEqual(out, "")
        self.assertEqual(len(err.strip().splitlines()), 1)
        self.assertIn("the A and F stations differ", err)

    def test_fusion_refuses_mismatched_meta(self):
        a, b, _ = self.fusion_files([(6.0, [(True, True, True)])])
        sts = fusion_stations([(True, True, True)])[2]
        f = self.write(report([point(sts)], engine="fused:cal-sub", trials=3))
        rc, out, err = self.fusion(a, b, f)
        self.assertEqual(rc, 2)
        self.assertEqual(len(err.strip().splitlines()), 1)
        self.assertIn("trials 3 differs", err)

    def test_fusion_refuses_bad_gate_flags(self):
        a, b, f = self.fusion_files([(6.0, [(True, True, True)])])
        for flags in (["--bar-frac", "-0.1"], ["--min-gain-pp", "nan"], ["--above-db", "inf"]):
            rc, _, err = self.fusion(a, b, f, *flags)
            self.assertEqual(rc, 2, flags)
            self.assertEqual(len(err.strip().splitlines()), 1)

    def test_fusion_counts_points_missing_from_f(self):
        flags = [(True, True, True)]
        sides = fusion_stations(flags)
        a = self.write(report([point(sides[0], snr_db=3.0), point(sides[0], snr_db=6.0)]))
        b = self.write(report([point(sides[1], snr_db=3.0), point(sides[1], snr_db=6.0)],
                              engine="hsmm"))
        f = self.write(report([point(sides[2], snr_db=6.0)], engine="fused:cal-sub"))
        rc, out, err = self.fusion(a, b, f)
        self.assertEqual(rc, 0, err)
        self.assertIn("Points missing from a side: legacy 0, hsmm 0, fused:cal-sub 1.", out)
        self.assertIn("1 paired truth identities: 1 point,", out)

    def test_fusion_golden_markdown(self):
        flags = [(True, True, True), (True, False, False), (False, True, True),
                 (False, False, True), (False, False, False)]
        a, b, f = self.fusion_files([(6.0, flags), (15.0, flags)],
                                    bogus=(("K7XX",), (), ("K7XX",)))
        rc, out, err = self.fusion(a, b, f)
        self.assertEqual(rc, 0, err)
        self.maxDiff = None
        self.assertEqual(out, GOLDEN_FUSION)
        self.assertNotIn(self.tmp, out)

    def test_docstring_defines_fusion_metrics(self):
        doc = eo.__doc__
        for term in ("fusion mode", "Oracle gap", "Gap held", "Headroom", "Gains", "Losses",
                     "Net", "Recovered of headroom", "Regression above +10 dB",
                     "1. recovered of headroom", "2. net gain", "3. no regression above +10 dB",
                     "4. false identities", "engine-overlap.py fusion"):
            self.assertIn(term, doc)

    def test_bench_and_oracle_parsers_keep_their_flags(self):
        out = io.StringIO()
        with contextlib.redirect_stdout(out), self.assertRaises(SystemExit):
            eo.parser().parse_args(["fusion", "--help"])
        for flag in ("--a", "--b", "--f", "--bar-frac", "--min-gain-pp", "--above-db",
                     "--bootstrap", "--json"):
            self.assertIn(flag, out.getvalue())
        self.assertNotIn("--bar-pp", out.getvalue())


GOLDEN_FUSION = """\
### Evidence fusion: fused:cal-sub (F) against legacy (A) and hsmm (B)

10 paired truth identities: 2 points, 1 trial of 10 stations, seed 1, 60 s recordings.
Points missing from a side: legacy 0, hsmm 0, fused:cal-sub 0.

| Level | n | A recall | F recall | Oracle gap | Gap held | Headroom (B only) | Gained from B only | Gained from neither | Lost from both | Lost from A only | Net | Recovered of headroom |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| Identity | 10 | 40.0% | 60.0% | 4 | 50.0% | 2 | 2 | 2 | 0 | 2 | +2 | 100.0% |
| Copy | 10 | 0.0% | 0.0% | 0 | n/a | 0 | 0 | 0 | 0 | 0 | +0 | n/a |

#### Points above +10 dB where F spots fewer than A

None.

#### False identities

|  | legacy | fused:cal-sub |
|---|---|---|
| Calls spotted that no station sent | 2 | 2 |

#### Adoption gate (identity)

Recovered of headroom: 100.0% (bar 25%). Net: 20.00 pp, no interval (--bootstrap 0) (bar: at least 0.25 pp, interval above 0). Points above +10 dB with fewer spots: 0. False identities: 2 against 2.
Reading: below the adoption bar: net gain below 0.25 pp or its interval not above 0.
"""


class WiringTests(unittest.TestCase):
    def test_this_module_runs_in_the_required_test_job(self):
        job = wf.job_block(wf.read("ci-full.yml"), "test")
        all_steps = wf.steps(job)
        found = [s for s in all_steps if (wf.run_body(s) or "").strip() == TEST_COMMAND]
        self.assertEqual(len(found), 1)
        self.assertEqual(wf.field(found[0], "if"), "runner.os != 'Windows'")
        here = next(i for i, s in enumerate(all_steps) if (wf.run_body(s) or "").strip() == TEST_COMMAND)
        pins = [i for i, s in enumerate(all_steps)
                if (wf.field(s, "uses") or "").startswith("actions/setup-python@")]
        self.assertTrue(pins and all(here < i for i in pins), "run before the setup-python pin")

    def test_claude_md_pre_push_list_names_this_module(self):
        with open(os.path.join(ROOT, "CLAUDE.md"), encoding="utf-8") as f:
            text = f.read()
        m = re.search(r"python3 -m unittest -v scripts\.tests\.test_\{([^}]*)\}", text)
        self.assertIsNotNone(m)
        self.assertIn("engine_overlap", re.split(r"[,\s\\]+", m.group(1)))


if __name__ == "__main__":
    unittest.main()
