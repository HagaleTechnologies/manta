"""MAN-119: score-transcript.py, which scores a `manta run --source <file>
--json` capture against a hand transcript. Fixtures are hand-written under
scripts/tests/fixtures/transcript/ (two tracks, one spot, a prosign and word
boundaries), so no Rust build is needed.

Run: python3 -m unittest scripts.tests.test_score_transcript -v
"""
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "..", "score-transcript.py")
FIXTURES = os.path.join(HERE, "fixtures", "transcript")
RUN = os.path.join(FIXTURES, "run.jsonl")
TRANSCRIPT = os.path.join(FIXTURES, "transcript.csv")
RATE = 48000

spec = importlib.util.spec_from_file_location("score_transcript", SCRIPT)
st = importlib.util.module_from_spec(spec)
spec.loader.exec_module(st)

HEADER = "start_s,end_s,pitch_hz,text\n"


class TempDirTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="score-transcript-test-")
        self.addCleanup(shutil.rmtree, self.tmp)

    def write(self, name, text):
        path = os.path.join(self.tmp, name)
        with open(path, "w", encoding="utf-8") as f:
            f.write(text)
        return path


class ParserTests(TempDirTest):
    def test_fixture_parses_with_comments_and_an_empty_pitch(self):
        rows = st.load_transcript(TRANSCRIPT)
        self.assertEqual([r["text"] for r in rows], ["CQ DE VP8GEO K", "K5TR", "5NN TU"])
        self.assertEqual([r["pitch_hz"] for r in rows], [640.0, 710.0, None])
        self.assertEqual(rows[0]["words"], ["CQ", "DE", "VP8GEO", "K"])
        self.assertEqual((rows[1]["start_s"], rows[1]["end_s"]), (16.2, 17.5))
        self.assertEqual(rows[2]["line"], 6)

    def assertRejected(self, body, line, fragment):
        path = self.write("t.csv", HEADER + body)
        with self.assertRaises(st.TranscriptError) as cm:
            st.load_transcript(path)
        self.assertIn("t.csv:%d:" % line, str(cm.exception))
        self.assertIn(fragment, str(cm.exception))

    def test_rejects_end_before_start(self):
        self.assertRejected("1.0,2.0,600,CQ\n5.0,4.0,600,CQ\n", 3, "before start_s")

    def test_rejects_non_numeric_times(self):
        self.assertRejected("abc,2.0,600,CQ\n", 2, "start_s is not a number")
        self.assertRejected("1.0,soon,600,CQ\n", 2, "end_s is not a number")
        self.assertRejected("1.0,nan,600,CQ\n", 2, "finite")

    def test_rejects_lowercase_and_invalid_characters(self):
        self.assertRejected("1.0,2.0,600,cq de vp8geo\n", 2, "uppercase")
        self.assertRejected("1.0,2.0,600,CQ DE VP8GEO!\n", 2, "uppercase")
        self.assertRejected("1.0,2.0,600,CQ  DE\n", 2, "single")
        self.assertRejected("1.0,2.0,600,CQ ? _\n", 2, "uppercase")

    def test_rejects_a_bad_header_and_a_short_row(self):
        path = self.write("t.csv", "start,end,pitch,text\n1,2,600,CQ\n")
        with self.assertRaises(st.TranscriptError) as cm:
            st.load_transcript(path)
        self.assertIn("t.csv:1:", str(cm.exception))
        self.assertRejected("1.0,2.0,600\n", 2, "4 columns")

    def test_accepts_a_quoted_comma_and_every_allowed_character(self):
        path = self.write("t.csv", '# note\n' + HEADER + '\n# mid comment\n1,2,,"TU, 73 = ? / ."\n')
        (row,) = st.load_transcript(path)
        self.assertEqual(row["text"], "TU, 73 = ? / .")
        self.assertIsNone(row["pitch_hz"])
        self.assertEqual(row["line"], 5)


class WordAssemblyTests(unittest.TestCase):
    def test_words_split_at_boundaries_prosigns_and_track_close(self):
        run = st.load_run(RUN, RATE)
        self.assertEqual([(w["text"], w["track_id"]) for w in run["words"]], [
            ("CQ", 1), ("TE", 1), ("VP8GEO", 1), ("K", 1),   # K ends at TrackClosed
            ("K5TR", 2), ("5NN", 2), ("TU", 2),              # TU ends at the <SK> prosign
        ])
        by_text = {w["text"]: w for w in run["words"]}
        self.assertAlmostEqual(by_text["VP8GEO"]["time_s"], 13.5)  # its first character
        self.assertEqual(by_text["CQ"]["pitch_hz"], 641.5)
        self.assertEqual(by_text["TU"]["pitch_hz"], 708.0)

    def test_spots_and_dial_frequency(self):
        run = st.load_run(RUN, RATE, dial_freq_hz=100.0)
        (spot,) = run["spots"]
        self.assertEqual(spot["callsign"], "VP8GEO")
        self.assertAlmostEqual(spot["time_s"], 14.6)
        self.assertAlmostEqual(spot["pitch_hz"], 541.5)
        self.assertEqual(run["words"][0]["pitch_hz"], 541.5)

    def test_a_word_before_its_tracks_frequency_gets_it_later(self):
        tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, tmp)
        path = os.path.join(tmp, "r.jsonl")
        with open(path, "w") as f:
            for ev in ({"event": "CharDecoded", "track_id": 7, "sample_ts": 48000, "glyph": {"Char": "R"},
                        "confidence": 1.0},
                       {"event": "WordBoundary", "track_id": 7, "sample_ts": 96000},
                       {"event": "TrackMeta", "track_id": 7, "sample_ts": 96000, "snr_2500_db": 0.0,
                        "freq_hz": 655.0}):
                f.write(json.dumps(ev) + "\n")
        (word,) = st.load_run(path, RATE)["words"]
        self.assertEqual((word["text"], word["pitch_hz"]), ("R", 655.0))

    def test_a_non_json_line_is_an_error_naming_it(self):
        tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, tmp)
        path = os.path.join(tmp, "r.jsonl")
        with open(path, "w") as f:
            f.write('{"event":"SpeedUpdate","track_id":1,"wpm":20}\nSPOT: W1AW\n')
        with self.assertRaisesRegex(ValueError, "r.jsonl:2:"):
            st.load_run(path, RATE)


def spot(call, t, pitch):
    return {"callsign": call, "time_s": t, "pitch_hz": pitch, "track_id": 1}


def row(start, end, pitch, text):
    return {"line": 0, "start_s": start, "end_s": end, "pitch_hz": pitch, "text": text,
            "words": text.split(" ")}


class MetricTests(unittest.TestCase):
    def test_fixture_metrics(self):
        r = st.score_transcript(st.load_transcript(TRANSCRIPT), st.load_run(RUN, RATE))
        self.assertEqual(r["spots"], 1)
        self.assertEqual(r["spot_precision"], 1.0)
        # VP8GEO and K5TR; CQ, DE, K, 5NN and TU are not callsigns (grammar.rs's tokens).
        self.assertEqual(r["transcript_calls"], 2)
        self.assertEqual(r["call_recall"], 0.5)
        # Seven words; DE was decoded as TE.
        self.assertEqual(r["transcript_words"], 7)
        self.assertEqual(r["words_found"], 6)
        self.assertAlmostEqual(r["word_recall"], 6 / 7)
        self.assertEqual((r["time_tol_s"], r["pitch_tol_hz"]), (10.0, 100.0))

    def test_5nn_and_tu_are_not_transcript_calls(self):
        r = st.score_transcript([row(0, 5, None, "5NN TU K QRZ")], {"spots": [], "words": []})
        self.assertEqual(r["transcript_calls"], 0)
        self.assertIsNone(r["call_recall"])

    def test_out_of_window_spot_is_a_false_positive(self):
        rows = [row(12.0, 16.0, 640.0, "VP8GEO")]
        inside = st.score_transcript(rows, {"spots": [spot("VP8GEO", 25.9, 640.0)], "words": []})
        outside = st.score_transcript(rows, {"spots": [spot("VP8GEO", 26.1, 640.0)], "words": []})
        self.assertEqual(inside["spot_precision"], 1.0)
        self.assertEqual(outside["spot_precision"], 0.0)
        self.assertEqual(outside["call_recall"], 0.0)

    def test_pitch_outside_tolerance_does_not_match(self):
        rows = [row(12.0, 16.0, 640.0, "VP8GEO")]
        r = st.score_transcript(rows, {"spots": [spot("VP8GEO", 13.0, 741.0)], "words": []})
        self.assertEqual(r["spot_precision"], 0.0)
        r = st.score_transcript(rows, {"spots": [spot("VP8GEO", 13.0, 741.0)], "words": []}, pitch_tol_hz=150)
        self.assertEqual(r["spot_precision"], 1.0)

    def test_row_without_pitch_matches_on_time_only(self):
        rows = [row(12.0, 16.0, None, "VP8GEO")]
        r = st.score_transcript(rows, {"spots": [spot("VP8GEO", 13.0, 2400.0)], "words": []})
        self.assertEqual(r["spot_precision"], 1.0)
        self.assertEqual(r["call_recall"], 1.0)

    def test_zero_spots_reports_null_precision(self):
        r = st.score_transcript(st.load_transcript(TRANSCRIPT), {"spots": [], "words": []})
        self.assertEqual(r["spots"], 0)
        self.assertIsNone(r["spot_precision"])
        self.assertEqual(r["call_recall"], 0.0)

    def test_each_decoded_word_counts_once(self):
        rows = [row(0, 5, None, "CQ CQ")]
        words = [{"text": "CQ", "track_id": 1, "time_s": 1.0, "pitch_hz": 600.0}]
        r = st.score_transcript(rows, {"spots": [], "words": words})
        self.assertEqual((r["transcript_words"], r["words_found"]), (2, 1))


class CliTests(TempDirTest):
    def run_script(self, *args):
        return subprocess.run([sys.executable, SCRIPT] + list(args),
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)

    def test_json_output_keys(self):
        out = self.run_script(RUN, TRANSCRIPT, "--sample-rate-hz", "48000", "--json")
        self.assertEqual(out.returncode, 0, out.stderr)
        r = json.loads(out.stdout)
        for key in ("spots", "spot_precision", "transcript_calls", "call_recall",
                    "transcript_words", "word_recall", "time_tol_s", "pitch_tol_hz"):
            self.assertIn(key, r)
        self.assertEqual(r["spot_precision"], 1.0)

    def test_zero_spots_is_null_in_json(self):
        run = self.write("r.jsonl", "")
        out = self.run_script(run, TRANSCRIPT, "--sample-rate-hz", "48000", "--json")
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertIsNone(json.loads(out.stdout)["spot_precision"])

    def test_text_output(self):
        out = self.run_script(RUN, TRANSCRIPT, "--sample-rate-hz", "48000")
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertIn("spot precision: 100.0% (1 of 1)", out.stdout)
        self.assertIn("lower bound", out.stdout)
        self.assertIn("call recall: 50.0% (1 of 2)", out.stdout)

    def test_bad_arguments_exit_2(self):
        self.assertEqual(self.run_script(RUN, TRANSCRIPT).returncode, 2)  # no --sample-rate-hz
        self.assertEqual(self.run_script(RUN, TRANSCRIPT, "--sample-rate-hz", "0").returncode, 2)
        self.assertEqual(self.run_script(RUN, TRANSCRIPT, "--sample-rate-hz", "48000",
                                         "--time-tol-s", "nan").returncode, 2)

    def test_malformed_transcript_exits_2_naming_the_line(self):
        bad = self.write("t.csv", HEADER + "1.0,2.0,600,cq\n")
        out = self.run_script(RUN, bad, "--sample-rate-hz", "48000")
        self.assertEqual(out.returncode, 2)
        self.assertIn("t.csv:2:", out.stderr)


if __name__ == "__main__":
    unittest.main()
