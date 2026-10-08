import csv, importlib.util, io, json, math, os, re, tempfile, unittest, zipfile
from datetime import datetime, timezone

HERE = os.path.dirname(__file__)
spec = importlib.util.spec_from_file_location("shadow_compare", os.path.join(HERE, "..", "shadow-compare.py"))
sc = importlib.util.module_from_spec(spec); spec.loader.exec_module(sc)

RBN_HEADER = ["callsign", "de_pfx", "de_cont", "freq", "band", "dx", "dx_pfx", "dx_cont",
              "mode", "db", "date", "speed", "tx_mode"]
NODE_DE = "N0CALL-1"


def ts(s):
    return int(datetime.strptime(s, "%Y-%m-%d %H:%M:%S").replace(tzinfo=timezone.utc).timestamp())


def node_line(when, khz, call, snr=21, ref=2500, wpm=20, conf=0.55, de=NODE_DE):
    return json.dumps({
        "id": "%s:%d:%s" % (de, ts(when), call), "source": "skimmer", "timestamp": ts(when),
        "frequency": int(round(khz * 1000)), "band": "20m", "dxCall": call, "deCall": de,
        "snr": snr, "snrRefHz": ref, "wpm": wpm, "decodeConfidence": conf,
        "decoderVersion": "manta-0.1.0"})


# Node spots across two UTC days (2026-09-09 and 2026-09-10), deCall N0CALL-1.
NODE_DAY1 = [
    node_line("2026-09-09 12:00:00", 14025.0, "W1AW", snr=21, wpm=20),   # confirmed by K5TR
    node_line("2026-09-09 13:00:00", 14030.0, "K1ABC"),                  # corroborated by DL1ABC
    node_line("2026-09-09 13:30:00", 14100.0, "W1OUT"),                  # outside passband
]
NODE_DAY2 = [
    node_line("2026-09-10 08:00:00", 14040.0, "W9XYZ"),   # only own spotters on RBN -> uncorroborated
    node_line("2026-09-10 09:00:00", 7020.0, "N2QQ", snr=11, wpm=24),   # confirmed by K5TR (40m)
    node_line("2026-09-10 10:00:00", 14050.0, "AB1CDE"),  # only an FT8 row -> uncorroborated
    node_line("2026-09-11 01:00:00", 14025.0, "W1AW"),    # outside window
]

# (spotter, freq kHz, dx, db, date, speed, tx_mode)
RBN_DAY1 = [
    ("K5TR", "14025.1", "W1AW", "28", "2026-09-09 12:01:00", "22", "CW"),
    ("K5TR", "14010.0", "VE3MIS", "15", "2026-09-09 14:00:00", "25", "CW"),
    ("K5TR", "14020.0", "EA8/TF3CW", "12", "2026-09-09 15:00:00", "25", "CW"),   # grammar-excluded
    ("DL1ABC", "14030.1", "K1ABC", "9", "2026-09-09 13:02:00", "18", "CW"),
    ("K5TR", "14100.0", "K7OUT", "20", "2026-09-09 16:00:00", "25", "CW"),        # outside passband
]
RBN_DAY2 = [
    ("K5TR", "7020.0", "N2QQ", "15", "2026-09-10 09:05:00", "25", "CW"),
    ("K5TR", "14010.1", "VE3MIS", "14", "2026-09-10 14:00:00", "25", "CW"),
    ("N0CALL-1", "14040.0", "W9XYZ", "10", "2026-09-10 08:00:30", "20", "CW"),    # node's own
    ("N0CALL", "14040.0", "W9XYZ", "10", "2026-09-10 08:01:00", "20", "CW"),      # node's own, no SSID
    ("K5TR", "14050.0", "AB1CDE", "10", "2026-09-10 10:00:00", "0", "FT8"),       # non-CW
    ("K5TR", "14015.0", "K7LATE", "20", "2026-09-11 05:00:00", "25", "CW"),       # outside window
]

PASSBANDS = ["--passband-khz", "14000-14070", "--passband-khz", "7000-7040"]
WINDOW = ["--start", "2026-09-09T00:00:00Z", "--end", "2026-09-11T00:00:00Z"]


def write_rbn_csv(path, rows):
    with open(path, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(RBN_HEADER)
        for sp, fq, dx, db, date, speed, txm in rows:
            w.writerow([sp, "", "", fq, "", dx, "", "", "CQ", db, date, speed, txm])


def write_lines(path, lines):
    with open(path, "w") as f:
        f.write("\n".join(lines) + "\n")


class ShadowCompareTest(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.d = self._tmp.name
        self.spot_dir = os.path.join(self.d, "spots")
        os.mkdir(self.spot_dir)
        write_lines(os.path.join(self.spot_dir, "spots-2026-09-09.jsonl"), NODE_DAY1)
        write_lines(os.path.join(self.spot_dir, "spots-2026-09-10.jsonl"), NODE_DAY2)
        self.rbn1 = os.path.join(self.d, "20260909.csv")
        self.rbn2 = os.path.join(self.d, "20260910.csv")
        write_rbn_csv(self.rbn1, RBN_DAY1)
        write_rbn_csv(self.rbn2, RBN_DAY2)

    def tearDown(self):
        self._tmp.cleanup()

    def run_tool(self, node=None, rbn=None, extra=(), passbands=PASSBANDS, window=WINDOW):
        node = node or [self.spot_dir]
        rbn = rbn or [self.rbn1, self.rbn2]
        jpath = os.path.join(self.d, "out.json")
        argv = (["--node-spots"] + list(node) + ["--rbn"] + list(rbn) + ["--primary", "K5TR"]
                + list(passbands) + list(window) + ["--json", jpath] + list(extra))
        out, err = io.StringIO(), io.StringIO()
        code = sc.run(argv, stdout=out, stderr=err)
        summary = None
        if code == 0:
            with open(jpath) as f:
                summary = json.load(f)
            os.remove(jpath)
        return code, out.getvalue(), err.getvalue(), summary

    def unc_calls(self, summary):
        return sorted(s["dxCall"] for s in summary["uncorroborated_spots"])

    def test_primary_heard_by_node_and_node_confirmed_by_primary_counts(self):
        code, md, _, s = self.run_tool()
        self.assertEqual(code, 0)
        a = s["agreement"]
        self.assertEqual(a["node_spots"], 5)
        self.assertEqual(a["node_confirmed_by_primary"], 2)   # W1AW, N2QQ
        self.assertEqual(a["primary_spots"], 4)               # W1AW, N2QQ, VE3MIS x2
        self.assertEqual(a["primary_heard_by_node"], 2)
        self.assertIn("Node spots confirmed by primary: 2 / 5 node spots (40.0%)", md)
        self.assertIn("Primary spots heard by node: 2 / 4 primary spots (50.0%)", md)
        self.assertIn("| VE3MIS | 2 | 14010 |", md)

    def test_node_only_spot_corroborated_by_other_spotter(self):
        _, md, _, s = self.run_tool()
        self.assertEqual(s["agreement"]["node_only"], 3)
        self.assertEqual(s["agreement"]["node_only_corroborated"], 1)   # K1ABC via DL1ABC
        self.assertNotIn("K1ABC", self.unc_calls(s))
        self.assertIn("corroborated by another RBN spotter: 1 / 5 node spots (20.0%)", md)

    def test_node_only_spot_uncorroborated(self):
        _, md, _, s = self.run_tool()
        self.assertEqual(self.unc_calls(s), ["AB1CDE", "W9XYZ"])
        self.assertEqual(s["agreement"]["uncorroborated"], 2)
        self.assertAlmostEqual(s["agreement"]["uncorroborated_pct"], 40.0)
        self.assertIn("**Uncorroborated: 2 / 5 node spots (40.0%)**", md)
        self.assertIn("GO needs <= 10% of node spots uncorroborated, with >= 20 node spots", md)
        section = md.split("## Node-only, uncorroborated (likely busted or unique)")[1]
        self.assertIn("| W9XYZ |", section.split("## Primary heard, node missed")[0])

    def test_own_spotter_rows_never_corroborate(self):
        _, md, _, s = self.run_tool()
        self.assertIn("W9XYZ", self.unc_calls(s))
        self.assertEqual(s["excluded_spotters"]["auto_from_node_deCall"], ["N0CALL", "N0CALL-1"])
        self.assertEqual(s["agreement"]["rbn_rows_from_excluded_spotters"], 2)
        # --exclude-spotter removes a third-party corroborator too.
        _, _, _, s2 = self.run_tool(extra=["--exclude-spotter", "dl1abc"])
        self.assertEqual(self.unc_calls(s2), ["AB1CDE", "K1ABC", "W9XYZ"])
        self.assertEqual(s2["excluded_spotters"]["manual"], ["DL1ABC"])

    def test_non_cw_rows_ignored(self):
        _, md, _, s = self.run_tool()
        self.assertEqual(s["inputs"]["rbn_rows"]["non_cw"], 1)
        self.assertIn("AB1CDE", self.unc_calls(s))   # the FT8 K5TR row does not confirm it
        self.assertNotIn("AB1CDE", [m["dxCall"] for m in s["primary_missed_calls"]])

    def test_out_of_passband_and_out_of_window_ignored_both_sides(self):
        _, md, _, s = self.run_tool()
        self.assertEqual(s["node_filter"], {"loaded": 7, "out_of_passband": 1, "out_of_window": 1,
                                            "compared": 5})
        self.assertEqual(s["inputs"]["rbn_rows"]["out_of_passband"], 1)
        self.assertEqual(s["inputs"]["rbn_rows"]["out_of_window"], 1)
        missed = [m["dxCall"] for m in s["primary_missed_calls"]]
        self.assertEqual(missed, ["VE3MIS"])
        for call in ("K7OUT", "K7LATE", "W1OUT"):
            self.assertNotIn(call, md)

    def test_grammar_excluded_primary_rows_counted_separately(self):
        _, md, _, s = self.run_tool()
        self.assertEqual(s["agreement"]["primary_rows_grammar_excluded"], 1)
        self.assertEqual(s["agreement"]["primary_spots"], 4)
        self.assertNotIn("EA8/TF3CW", md)
        self.assertIn("grammar never accepts, not in any denominator): 1.", md)

    def test_time_tolerance_edges(self):
        spots = os.path.join(self.d, "edge.jsonl")
        write_lines(spots, [node_line("2026-09-09 12:00:00", 14025.0, "W1AW"),
                            node_line("2026-09-09 12:00:00", 14035.0, "K2TT")])
        rbn = os.path.join(self.d, "edge.csv")
        write_rbn_csv(rbn, [("K5TR", "14025.0", "W1AW", "20", "2026-09-09 12:09:59", "20", "CW"),
                            ("K5TR", "14035.0", "K2TT", "20", "2026-09-09 12:10:01", "20", "CW")])
        _, _, _, s = self.run_tool(node=[spots], rbn=[rbn], extra=["--time-tol-s", "600"])
        self.assertEqual(s["agreement"]["node_confirmed_by_primary"], 1)   # 599 s
        self.assertEqual(self.unc_calls(s), ["K2TT"])                       # 601 s
        self.assertEqual([m["dxCall"] for m in s["primary_missed_calls"]], ["K2TT"])

    def test_freq_tolerance_edges(self):
        spots = os.path.join(self.d, "edge.jsonl")
        write_lines(spots, [node_line("2026-09-09 12:00:00", 14025.0, "W1AW"),
                            node_line("2026-09-09 12:00:00", 14035.0, "K2TT"),
                            node_line("2026-09-09 12:00:00", 14045.0, "K3UU")])
        rbn = os.path.join(self.d, "edge.csv")
        write_rbn_csv(rbn, [("K5TR", "14025.5", "W1AW", "20", "2026-09-09 12:00:00", "20", "CW"),
                            ("K5TR", "14035.6", "K2TT", "20", "2026-09-09 12:00:00", "20", "CW"),
                            ("K5TR", "14044.6", "K3UU", "20", "2026-09-09 12:00:00", "20", "CW")])
        _, _, _, s = self.run_tool(node=[spots], rbn=[rbn])
        self.assertEqual(s["agreement"]["node_confirmed_by_primary"], 2)   # +500 Hz, -400 Hz
        self.assertEqual(self.unc_calls(s), ["K2TT"])                       # +600 Hz
        _, _, _, s2 = self.run_tool(node=[spots], rbn=[rbn], extra=["--freq-tol-hz", "450"])
        self.assertEqual(self.unc_calls(s2), ["K2TT", "W1AW"])

    def test_snr_delta_uses_500_hz_reference(self):
        _, md, _, s = self.run_tool()
        snr = s["calibration"]["snr_db_500hz"]
        self.assertEqual(snr["n"], 2)
        # W1AW: 21 + 10*log10(2500/500) - 28 = -0.0103; N2QQ: 11 + 6.99 - 15 = 2.99.
        w1aw = 21 + 10 * math.log10(5) - 28
        self.assertAlmostEqual(w1aw, -0.01, places=2)
        self.assertAlmostEqual(snr["median_abs"], (abs(w1aw) + 2.9897) / 2, places=3)
        self.assertAlmostEqual(snr["p90_abs"], 2.9897, places=3)
        # Single pair: the signed median is exactly the W1AW delta.
        spots = os.path.join(self.d, "one.jsonl")
        write_lines(spots, [node_line("2026-09-09 12:00:00", 14025.0, "W1AW", snr=21, ref=2500)])
        _, _, _, s1 = self.run_tool(node=[spots], rbn=[self.rbn1])
        self.assertAlmostEqual(s1["calibration"]["snr_db_500hz"]["median"], -0.0103, places=3)

    def test_freq_wpm_deltas_on_matched_pairs(self):
        _, md, _, s = self.run_tool()
        f = s["calibration"]["freq_hz"]
        # W1AW: 14025000 - 14025100 = -100 Hz; N2QQ: 0 Hz.
        self.assertEqual(f["n"], 2)
        self.assertAlmostEqual(f["median_abs"], 50.0)
        self.assertAlmostEqual(f["p90_abs"], 100.0)
        self.assertAlmostEqual(f["median"], -50.0)
        w = s["calibration"]["wpm"]
        # W1AW: 20 - 22 = -2; N2QQ: 24 - 25 = -1.
        self.assertAlmostEqual(w["median_abs"], 1.5)
        self.assertAlmostEqual(w["p90_abs"], 2.0)
        self.assertIn("| Freq delta (Hz) | 2 | -50.0 | 50.0 | 100.0 |", md)

    def test_zip_input_reads_inner_csv(self):
        z = os.path.join(self.d, "20260910.zip")
        with zipfile.ZipFile(z, "w") as zf:
            zf.write(self.rbn2, "20260910.csv")
        code, md, _, s = self.run_tool(rbn=[self.rbn1, z])
        self.assertEqual(code, 0)
        _, _, _, s_csv = self.run_tool()
        self.assertEqual(s["agreement"], s_csv["agreement"])
        self.assertIn("20260910.zip", md)
        zrow = [fi for fi in s["inputs"]["files"] if fi["path"] == z][0]
        self.assertEqual(zrow["rows"], len(RBN_DAY2))
        self.assertEqual(len(zrow["sha256"]), 64)

    def test_multiple_daily_files_and_spot_dir(self):
        code, md, _, s = self.run_tool()
        self.assertEqual(code, 0)
        files = s["inputs"]["files"]
        self.assertEqual([os.path.basename(fi["path"]) for fi in files],
                         ["spots-2026-09-09.jsonl", "spots-2026-09-10.jsonl",
                          "20260909.csv", "20260910.csv"])
        self.assertEqual([fi["rows"] for fi in files], [3, 4, len(RBN_DAY1), len(RBN_DAY2)])
        self.assertEqual(s["inputs"]["rbn_rows"]["read"], len(RBN_DAY1) + len(RBN_DAY2))
        for fi in files:
            self.assertRegex(fi["sha256"], r"^[0-9a-f]{64}$")
            self.assertIn(fi["sha256"], md)
        self.assertIn("manta-0.1.0 (7)", md)
        # Naming a directory and a file inside it reads that file once.
        _, _, _, s2 = self.run_tool(node=[self.spot_dir, os.path.join(self.spot_dir, "spots-2026-09-09.jsonl")])
        self.assertEqual(s2["agreement"], s["agreement"])

    def test_per_band_and_per_day_tables(self):
        _, md, _, s = self.run_tool()
        self.assertEqual(sorted(s["by_band"]), ["20m", "40m"])
        self.assertLess(md.index("| 40m |"), md.index("| 20m |"))   # band-plan order
        self.assertEqual(s["by_band"]["20m"], {"node_spots": 4, "confirmed": 1, "corroborated": 1,
                                               "uncorroborated": 2, "primary_spots": 3,
                                               "primary_heard": 1})
        self.assertEqual(s["by_band"]["40m"]["confirmed"], 1)
        self.assertEqual(s["by_day"]["2026-09-09"], {"node_spots": 2, "confirmed": 1, "corroborated": 1,
                                                     "uncorroborated": 0, "primary_spots": 2,
                                                     "primary_heard": 1})
        self.assertEqual(s["by_day"]["2026-09-10"]["uncorroborated"], 2)
        self.assertIn("| 20m | 4 | 1 | 1 | 2 | 50.0% | 3 | 1 |", md)
        self.assertIn("| 2026-09-10 | 3 | 1 | 0 | 2 | 66.7% | 2 | 1 |", md)
        for header in ("## Inputs", "## Agreement", "## Calibration", "## By band", "## By UTC day",
                       "## Node-only, uncorroborated (likely busted or unique)",
                       "## Primary heard, node missed"):
            self.assertIn("\n" + header + "\n", md)

    def test_missing_passband_is_usage_error_exit_2(self):
        code, md, err, s = self.run_tool(passbands=[])
        self.assertEqual(code, 2)
        self.assertIn("--passband-khz", err)
        self.assertEqual(md, "")
        code, _, _, _ = self.run_tool(passbands=["--passband-khz", "14070-14000"])
        self.assertEqual(code, 2)
        code, _, err, _ = self.run_tool(rbn=[os.path.join(self.d, "missing.csv")])
        self.assertEqual(code, 2)
        self.assertIn("missing.csv", err)

    def test_rows_just_outside_the_window_still_match_edge_spots(self):
        # A node spot 10 s after --start, heard by the primary 20 s BEFORE --start:
        # the out-of-window row is a match reference (confirmed), not a compared row.
        node = [node_line("2026-09-09 00:00:10", 14025.0, "W1EDGE")]
        rbn = [("K5TR", "14025.0", "W1EDGE", "20", "2026-09-08 23:59:50", "20", "CW"),
               ("K5TR", "14030.0", "W2FAR", "20", "2026-09-08 23:40:00", "20", "CW")]
        npath, rpath = os.path.join(self.d, "edge.jsonl"), os.path.join(self.d, "edge.csv")
        write_lines(npath, node)
        write_rbn_csv(rpath, rbn)
        code, _, _, s = self.run_tool(node=[npath], rbn=[rpath])
        self.assertEqual(code, 0)
        self.assertEqual(s["agreement"]["node_confirmed_by_primary"], 1)
        self.assertEqual(s["agreement"]["primary_spots"], 0)
        self.assertEqual(s["agreement"]["uncorroborated"], 0)
        self.assertEqual(s["inputs"]["rbn_rows"]["out_of_window"], 2)
        self.assertEqual(s["inputs"]["rbn_rows"]["kept"], 0)

    def test_out_of_range_node_timestamp_is_malformed_not_a_crash(self):
        npath = os.path.join(self.d, "bad-ts.jsonl")
        bad = json.loads(node_line("2026-09-09 12:00:00", 14025.0, "W1AW"))
        bad["timestamp"] = 1e20
        write_lines(npath, [json.dumps(bad), node_line("2026-09-09 12:00:00", 14025.0, "W1AW")])
        code, _, err, _ = self.run_tool(node=[npath])
        self.assertEqual(code, 2)
        self.assertIn("bad-ts.jsonl line 1: malformed node spot record", err)

    def test_torn_final_node_line_is_tolerated(self):
        npath = os.path.join(self.d, "torn.jsonl")
        with open(npath, "w", encoding="utf-8") as f:
            f.write(node_line("2026-09-09 12:00:00", 14025.0, "W1AW") + "\n")
            f.write('{"timestamp": 17')
        code, md, _, s = self.run_tool(node=[npath])
        self.assertEqual(code, 0)
        self.assertEqual(s["agreement"]["node_spots"], 1)
        self.assertEqual(s["inputs"]["node_malformed_lines"], 1)

    def test_empty_window_reports_zero_without_dividing_by_zero(self):
        code, md, _, s = self.run_tool(window=["--start", "2020-01-01T00:00:00Z",
                                               "--end", "2020-01-02T00:00:00Z"])
        self.assertEqual(code, 0)
        a = s["agreement"]
        self.assertEqual((a["node_spots"], a["primary_spots"], a["uncorroborated"]), (0, 0, 0))
        self.assertIsNone(a["uncorroborated_pct"])
        self.assertIn("**Uncorroborated: 0 / 0 node spots (n/a)**", md)
        self.assertEqual(s["calibration"]["freq_hz"]["n"], 0)

    def test_json_summary_matches_markdown_numbers(self):
        _, md, _, s = self.run_tool()
        a = s["agreement"]
        m = re.search(r"\*\*Uncorroborated: (\d+) / (\d+) node spots \(([\d.]+)%\)\*\*", md)
        self.assertIsNotNone(m)
        self.assertEqual((int(m.group(1)), int(m.group(2))), (a["uncorroborated"], a["node_spots"]))
        self.assertEqual(float(m.group(3)), round(a["uncorroborated_pct"], 1))
        m = re.search(r"confirmed by primary: (\d+) / (\d+)", md)
        self.assertEqual((int(m.group(1)), int(m.group(2))), (a["node_confirmed_by_primary"], a["node_spots"]))
        m = re.search(r"heard by node: (\d+) / (\d+)", md)
        self.assertEqual((int(m.group(1)), int(m.group(2))), (a["primary_heard_by_node"], a["primary_spots"]))
        m = re.search(r"another RBN spotter: (\d+) / (\d+)", md)
        self.assertEqual(int(m.group(1)), a["node_only_corroborated"])
        self.assertEqual(a["node_confirmed_by_primary"] + a["node_only_corroborated"] + a["uncorroborated"],
                         a["node_spots"])


if __name__ == "__main__":
    unittest.main()
