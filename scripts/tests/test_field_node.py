"""Tests for scripts/field-node.py (MAN-96). stdlib unittest only:
python3 -m unittest discover -s scripts/tests -p 'test_field_node.py' -v
"""
import importlib.util
import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
import unittest
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))
SCRIPT = os.path.join(HERE, "..", "field-node.py")
FIXTURES = os.path.join(HERE, "fixtures", "field-node")
spec = importlib.util.spec_from_file_location("field_node", SCRIPT)
fn = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fn)


def fixture(name):
    with open(os.path.join(FIXTURES, name), encoding="utf-8") as f:
        return f.read()


def fixture_value(name, metric):
    for line in fixture(name).splitlines():
        if line.startswith(metric + " "):
            return float(line.split()[1])
    raise AssertionError("%s not in %s" % (metric, name))


def fixture_git_sha():
    for line in fixture("metrics-live.txt").splitlines():
        if line.startswith("manta_build_info{"):
            return line.split('git_sha="', 1)[1].split('"', 1)[0]
    raise AssertionError("no manta_build_info in fixture")


def closed_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


class FixtureServer:
    """Serves {path: (status, body)} on 127.0.0.1:<ephemeral>."""

    def __init__(self, routes):
        routes_ref = routes

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):  # noqa: N802
                status, body = routes_ref.get(self.path, (404, "not found\n"))
                data = body.encode("utf-8")
                self.send_response(status)
                self.send_header("Content-Type", "text/plain; charset=utf-8")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def log_message(self, *a):
                pass

        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = "http://127.0.0.1:%d" % self.httpd.server_address[1]
        self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *exc):
        self.httpd.shutdown()
        self.httpd.server_close()


def read_ledger(path):
    with open(path, encoding="utf-8") as f:
        return [json.loads(line) for line in f if line.strip()]


class Args:
    def __init__(self, **kw):
        self.__dict__.update(kw)


def sample_args(url, ledger, **kw):
    a = dict(metrics_url=url, ledger=ledger, loop=False, interval_s=60.0, rss_unit=None,
             recover_cmd=None, recover_after_s=600.0, recover_cooldown_s=1800.0,
             recover_max_per_day=6, recover_timeout_s=120.0)
    a.update(kw)
    return Args(**a)


# ---------------------------------------------------------------------------


class ParsePrometheusTests(unittest.TestCase):
    def test_parses_live_fixture(self):
        samples, types = fn.parse_prometheus_text(fixture("metrics-live.txt"))
        by = {}
        for name, labels, value in samples:
            by.setdefault(name, []).append((labels, value))
        self.assertEqual(by["manta_spots_total"][0][0], {})
        self.assertEqual(by["manta_spots_total"][0][1], fixture_value("metrics-live.txt", "manta_spots_total"))
        self.assertIn(({"source": "file"}, 1.0), by["manta_source_health"])
        build = by["manta_build_info"][0][0]
        self.assertEqual(build["git_sha"], fixture_git_sha())
        self.assertIn("version", build)
        self.assertIn("features", build)
        les = [lab["le"] for lab, _ in by["manta_decode_latency_seconds_bucket"]]
        self.assertIn("+Inf", les)
        self.assertEqual(types["manta_decode_latency_seconds"], "histogram")
        for m in fn.REQUIRED_METRICS:
            self.assertTrue(fn._family_present(m, samples, types), m)

    def test_ignores_comments_and_blank_lines(self):
        text = "# HELP x a help line\n\n# TYPE x counter\n   \nx 3\n# a comment\n"
        samples, types = fn.parse_prometheus_text(text)
        self.assertEqual(samples, [("x", {}, 3.0)])
        self.assertEqual(types, {"x": "counter"})

    def test_label_values_with_escaped_quotes(self):
        text = 'm{a="say \\"hi\\"",b="back\\\\slash",c="x}y"} 2\n'
        samples, _ = fn.parse_prometheus_text(text)
        self.assertEqual(samples, [("m", {"a": 'say "hi"', "b": "back\\slash", "c": "x}y"}, 2.0)])


class SampleTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.ledger = os.path.join(self.tmp.name, "ledger.jsonl")

    def tearDown(self):
        self.tmp.cleanup()

    def test_sample_record_fields(self):
        routes = {"/metrics": (200, fixture("metrics-live.txt")), "/healthz": (200, fixture("healthz-ok.txt"))}
        with FixtureServer(routes) as srv:
            self.assertEqual(fn.cmd_sample(sample_args(srv.url, self.ledger)), 0)
        [rec] = read_ledger(self.ledger)
        self.assertEqual(rec["v"], 1)
        self.assertEqual(rec["kind"], "sample")
        self.assertIs(rec["reachable"], True)
        self.assertEqual(rec["healthz"], 200)
        self.assertEqual(rec["healthz_status"], "ok")
        self.assertIn("source file: healthy", rec["checks"])
        self.assertIn("decode: ok", rec["checks"])
        self.assertEqual(rec["start_time"], fixture_value("metrics-live.txt", "manta_start_time_seconds"))
        self.assertEqual(rec["uptime"], fixture_value("metrics-live.txt", "manta_uptime_seconds"))
        self.assertEqual(rec["git_sha"], fixture_git_sha())
        self.assertTrue(rec["git_sha"].startswith("ee0d183"))
        self.assertEqual(rec["spots_total"], fixture_value("metrics-live.txt", "manta_spots_total"))
        self.assertIsInstance(rec["spots_by_band"], dict)
        for key in rec["spots_by_band"]:
            self.assertRegex(key, r"^[^/]+/[^/]+$")
        self.assertEqual(rec["source_health"], {"file": 1})
        self.assertEqual(rec["source_outages"], {"file": 0})
        self.assertEqual(rec["source_down_s"], {"file": 0.0})
        self.assertEqual(rec["telnet_clients"], 0)
        self.assertEqual(rec["json_clients"], 0)
        self.assertEqual(rec["spots_dropped_lagged"], 0)
        self.assertIn("decode_latency_sum", rec)
        self.assertIn("decode_latency_count", rec)
        self.assertIsNotNone(rec["decode_latency_count"])
        self.assertIsNone(rec["rss_kb"])
        self.assertEqual(rec["missing"], [])
        self.assertIsInstance(rec["ts"], float)

    def test_spots_by_band_keyed_band_slash_type(self):
        body = fixture("metrics-live.txt") + '\nmanta_spots_by_band_total{band="20m",type="cq"} 3\n'
        rec = fn.build_sample(200, body, 200, fixture("healthz-ok.txt"), 1.0)
        self.assertEqual(rec["spots_by_band"], {"20m/cq": 3})

    def test_healthz_503_is_recorded_not_raised(self):
        routes = {"/metrics": (200, fixture("metrics-live.txt")),
                  "/healthz": (503, fixture("healthz-unhealthy.txt"))}
        with FixtureServer(routes) as srv:
            fn.cmd_sample(sample_args(srv.url, self.ledger))
        [rec] = read_ledger(self.ledger)
        self.assertEqual(rec["healthz"], 503)
        self.assertEqual(rec["healthz_status"], "unhealthy")
        self.assertIn("source soapy: unhealthy", rec["checks"])
        self.assertTrue(rec["reachable"])

    def test_unreachable_metrics_url(self):
        url = "http://127.0.0.1:%d" % closed_port()
        self.assertEqual(fn.cmd_sample(sample_args(url, self.ledger)), 0)
        [rec] = read_ledger(self.ledger)
        self.assertIs(rec["reachable"], False)
        self.assertIsInstance(rec["error"], str)
        self.assertTrue(rec["error"])
        self.assertIsNone(rec["healthz"])

    def test_missing_required_metric_is_reported(self):
        body = "".join(line + "\n" for line in fixture("metrics-live.txt").splitlines()
                       if "manta_source_outages_total" not in line)
        routes = {"/metrics": (200, body), "/healthz": (200, fixture("healthz-ok.txt"))}
        with FixtureServer(routes) as srv:
            fn.cmd_sample(sample_args(srv.url, self.ledger))
        [rec] = read_ledger(self.ledger)
        self.assertEqual(rec["missing"], ["manta_source_outages_total"])
        self.assertEqual(rec["source_outages"], {})

    def test_ledger_append_is_one_json_line_and_flushed(self):
        fn.append_ledger(self.ledger, {"kind": "x", "ts": 1.0, "s": "a\nb"})
        fn.append_ledger(self.ledger, {"kind": "y", "ts": 2.0})
        with open(self.ledger, encoding="utf-8") as f:
            raw = f.read()
        lines = raw.split("\n")
        self.assertEqual(lines[-1], "")
        self.assertEqual(len(lines), 3)
        self.assertEqual(json.loads(lines[0])["s"], "a\nb")
        self.assertEqual(json.loads(lines[1])["kind"], "y")


def wd_sample(ts, reachable=True, health=1):
    return {"kind": "sample", "ts": float(ts), "reachable": reachable,
            "source_health": {"soapy": health} if reachable else {}}


T_NOON = datetime(2026, 10, 1, 12, 0, tzinfo=timezone.utc).timestamp()


class WatchdogTests(unittest.TestCase):
    def bad_run(self, start, seconds, step=60):
        out = []
        t = start
        i = 0
        while t <= start + seconds:
            # alternate unreachable and source-unhealthy: both are "bad"
            out.append(wd_sample(t, reachable=(i % 2 == 0), health=0))
            t += step
            i += 1
        return out

    def test_no_recovery_before_threshold(self):
        s = [wd_sample(T_NOON - 60)] + self.bad_run(T_NOON, 540)
        self.assertFalse(fn.should_recover(s, [], T_NOON + 599, 600, 1800, 6))

    def test_recovery_at_threshold(self):
        s = [wd_sample(T_NOON - 60)] + self.bad_run(T_NOON, 600)
        self.assertTrue(fn.should_recover(s, [], T_NOON + 600, 600, 1800, 6))
        unreachable_only = [wd_sample(T_NOON + i * 60, reachable=False) for i in range(11)]
        self.assertTrue(fn.should_recover(unreachable_only, [], T_NOON + 600, 600, 1800, 6))
        unhealthy_only = [wd_sample(T_NOON + i * 60, health=0) for i in range(11)]
        self.assertTrue(fn.should_recover(unhealthy_only, [], T_NOON + 600, 600, 1800, 6))

    def test_healthy_sample_resets_streak(self):
        s = self.bad_run(T_NOON, 600) + [wd_sample(T_NOON + 660)] + self.bad_run(T_NOON + 720, 300)
        self.assertFalse(fn.should_recover(s, [], T_NOON + 1020, 600, 1800, 6))
        # the newest sample healthy -> never recover
        s2 = self.bad_run(T_NOON, 900) + [wd_sample(T_NOON + 960)]
        self.assertFalse(fn.should_recover(s2, [], T_NOON + 960, 600, 1800, 6))

    def test_cooldown_blocks(self):
        s = self.bad_run(T_NOON, 3000)
        now = T_NOON + 3000
        self.assertFalse(fn.should_recover(s, [now - 1799], now, 600, 1800, 6))
        self.assertTrue(fn.should_recover(s, [now - 1800], now, 600, 1800, 6))

    def test_daily_cap_blocks(self):
        midnight = datetime(2026, 10, 2, tzinfo=timezone.utc).timestamp()
        now = midnight - 3600  # 23:00 UTC
        s = self.bad_run(now - 900, 900)
        today = [midnight - 86400 + 3600 * (i + 1) for i in range(6)]  # 01:00..06:00 today
        self.assertFalse(fn.should_recover(s, today, now, 600, 1800, 6))
        self.assertTrue(fn.should_recover(s, today[:5], now, 600, 1800, 6))
        # the cap resets at UTC midnight
        after = midnight + 3600
        s2 = self.bad_run(after - 900, 900)
        self.assertTrue(fn.should_recover(s2, today, after, 600, 1800, 6))

    def _tmp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        return tmp.name

    def test_recovery_runs_command_and_logs(self):
        d = self._tmp()
        ledger = os.path.join(d, "ledger.jsonl")
        marker = os.path.join(d, "marker")
        cmd = '%s -c "open(r\'%s\',\'w\')"' % (sys.executable, marker)
        clock = [T_NOON]
        wd = fn.Watchdog(cmd, ledger, 600, 1800, 6, 30, 60, clock=lambda: clock[0])
        rec = None
        for i in range(11):
            clock[0] = T_NOON + 60 * i
            rec = wd.observe(wd_sample(clock[0], reachable=False))
            if i < 10:
                self.assertIsNone(rec)
        self.assertIsNotNone(rec)
        self.assertTrue(os.path.exists(marker))
        [logged] = read_ledger(ledger)
        self.assertEqual(logged["kind"], "recovery")
        self.assertEqual(logged["cmd"][0], sys.executable)
        self.assertEqual(logged["exit"], 0)
        self.assertEqual(logged["bad_for_s"], 600)
        # cooldown: the next bad sample does not re-run it
        clock[0] += 60
        self.assertIsNone(wd.observe(wd_sample(clock[0], reachable=False)))

    def test_recovery_via_sample_loop_cli_path(self):
        d = self._tmp()
        ledger = os.path.join(d, "ledger.jsonl")
        marker = os.path.join(d, "marker")
        cmd = '%s -c "open(r\'%s\',\'w\')"' % (sys.executable, marker)
        url = "http://127.0.0.1:%d" % closed_port()
        args = sample_args(url, ledger, loop=True, interval_s=1.0, recover_cmd=cmd, recover_after_s=0.0)
        sleeps = []
        self.assertEqual(fn.cmd_sample(args, sleep=sleeps.append, max_iterations=2), 0)
        kinds = [r["kind"] for r in read_ledger(ledger)]
        self.assertEqual(kinds, ["sample", "recovery", "sample"])
        self.assertTrue(os.path.exists(marker))
        self.assertEqual(len(sleeps), 1)

    def test_recovery_command_timeout_is_logged(self):
        d = self._tmp()
        ledger = os.path.join(d, "ledger.jsonl")
        cmd = '%s -c "import time; time.sleep(30)"' % sys.executable
        rec = fn.run_recovery(cmd, 1, ledger, 600.0)
        self.assertIsNone(rec["exit"])
        self.assertEqual(rec["error"], "timeout")
        [logged] = read_ledger(ledger)
        self.assertIsNone(logged["exit"])
        self.assertEqual(logged["error"], "timeout")


def run_cli(*argv):
    return subprocess.run([sys.executable, SCRIPT] + list(argv), capture_output=True, text=True, timeout=120)


class NoteTests(unittest.TestCase):
    def test_note_kinds(self):
        with tempfile.TemporaryDirectory() as d:
            ledger = os.path.join(d, "l.jsonl")
            for kind in ("start", "planned", "manual", "end"):
                r = run_cli("note", "--ledger", ledger, "--kind", kind, "--reason", "why " + kind)
                self.assertEqual(r.returncode, 0, r.stderr)
            recs = read_ledger(ledger)
            self.assertEqual([r["note"] for r in recs], ["start", "planned", "manual", "end"])
            for r in recs:
                self.assertEqual(r["kind"], "note")
                self.assertEqual(r["reason"], "why " + r["note"])
                self.assertEqual(r["v"], 1)
                self.assertIsInstance(r["ts"], float)

    def test_note_rejects_unknown_kind(self):
        with tempfile.TemporaryDirectory() as d:
            ledger = os.path.join(d, "l.jsonl")
            r = run_cli("note", "--ledger", ledger, "--kind", "oops", "--reason", "x")
            self.assertEqual(r.returncode, 2)
            self.assertFalse(os.path.exists(ledger))


def spot_line(call, ts, freq=14012362):
    return json.dumps({"id": "X", "dxCall": call, "frequency": freq, "timestamp": ts, "deCall": "N0CALL-1"},
                      separators=(",", ":")).encode()


class LineServer:
    """Accepts len(batches) connections in turn; sends each batch then closes."""

    def __init__(self, batches):
        self.sock = socket.socket()
        self.sock.bind(("127.0.0.1", 0))
        self.sock.listen(5)
        self.port = self.sock.getsockname()[1]
        self.batches = batches
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def run(self):
        for batch in self.batches:
            conn, _ = self.sock.accept()
            conn.sendall(batch)
            conn.close()
        self.sock.close()


class RecordSpotsTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.out = os.path.join(self.tmp.name, "spots")

    def tearDown(self):
        self.tmp.cleanup()

    def run_until(self, srv, disconnects, sleeps=None, logs=None):
        state = {}
        sleeps = [] if sleeps is None else sleeps
        logs = [] if logs is None else logs
        return fn.record_spots(
            "127.0.0.1", srv.port, self.out, sleep=sleeps.append,
            should_stop=lambda: state.get("stats", {}).get("disconnects", 0) >= disconnects,
            log=logs.append, stats_out=state)

    def files(self):
        return sorted(os.listdir(self.out))

    def read(self, name):
        with open(os.path.join(self.out, name), "rb") as f:
            return f.read()

    def test_writes_by_utc_date_of_timestamp(self):
        d1 = datetime(2026, 10, 1, 23, 59, 59, tzinfo=timezone.utc).timestamp()
        d2 = datetime(2026, 10, 2, 0, 0, 1, tzinfo=timezone.utc).timestamp()
        l1, l2 = spot_line("W1AW", int(d1)), spot_line("K1JT", int(d2))
        srv = LineServer([l1 + b"\n" + l2 + b"\n"])
        stats = self.run_until(srv, 1, logs=[])
        self.assertEqual(stats["written"], 2)
        self.assertEqual(self.files(), ["spots-2026-10-01.jsonl", "spots-2026-10-02.jsonl"])
        self.assertEqual(self.read("spots-2026-10-01.jsonl"), l1 + b"\n")
        self.assertEqual(self.read("spots-2026-10-02.jsonl"), l2 + b"\n")

    def test_invalid_line_counted_and_skipped(self):
        ts = int(datetime(2026, 10, 1, 12, tzinfo=timezone.utc).timestamp())
        good = spot_line("W1AW", ts)
        bad = [b"not json", b'{"frequency":1,"timestamp":2}', b'{"dxCall":"A1A","timestamp":2}',
               b'{"dxCall":"A1A","frequency":1}', b"[1,2]"]
        srv = LineServer([b"\n".join(bad + [good]) + b"\n"])
        logs = []
        stats = self.run_until(srv, 1, logs=logs)
        self.assertEqual(stats["invalid"], 5)
        self.assertEqual(stats["written"], 1)
        self.assertEqual(self.read("spots-2026-10-01.jsonl"), good + b"\n")
        self.assertEqual(sum(1 for m in logs if m.startswith("invalid line")), 5)

    def test_reconnects_after_server_closes(self):
        ts = int(datetime(2026, 10, 1, 12, tzinfo=timezone.utc).timestamp())
        a, b = spot_line("W1AW", ts), spot_line("K1JT", ts + 1)
        srv = LineServer([a + b"\n", b + b"\n"])
        sleeps = []
        stats = self.run_until(srv, 2, sleeps=sleeps)
        self.assertEqual(stats["written"], 2)
        self.assertEqual(sleeps, [1])
        self.assertEqual(self.read("spots-2026-10-01.jsonl"), a + b"\n" + b + b"\n")

    def test_backoff_resets_after_productive_connection(self):
        self.assertEqual(fn.next_backoff(32, True), 1)
        self.assertEqual(fn.next_backoff(32, False), 60)
        self.assertEqual(fn.next_backoff(1, False), 2)

    def test_backoff_doubles_to_60_when_never_connected(self):
        port = closed_port()
        sleeps = []
        fn.record_spots("127.0.0.1", port, self.out, sleep=sleeps.append,
                        should_stop=lambda: len(sleeps) >= 8, log=lambda m: None)
        self.assertEqual(sleeps, [1, 2, 4, 8, 16, 32, 60, 60])


class CliTests(unittest.TestCase):
    def test_help_lists_subcommands(self):
        r = run_cli("--help")
        self.assertEqual(r.returncode, 0)
        for sub in ("sample", "note", "record-spots", "report"):
            self.assertIn(sub, r.stdout)
        self.assertIn("2026-10-07-man96-secondary-skimmer-field-node.md", r.stdout)

    def test_report_help_documents_thresholds_and_decision_doc(self):
        r = run_cli("report", "--help")
        self.assertEqual(r.returncode, 0)
        out = " ".join(r.stdout.split())
        self.assertIn("2026-10-07-man96-secondary-skimmer-field-node.md", out)
        for flag, default in (("--interval-s", "60"), ("--min-days", "30"), ("--min-availability", "0.99"),
                              ("--min-aggregator", "0.95"), ("--planned-window-max-s", "1800")):
            self.assertIn(flag, out)
            self.assertIn("default: %s" % default, out)

    def test_sample_once_cli_writes_ledger(self):
        routes = {"/metrics": (200, fixture("metrics-live.txt")), "/healthz": (200, fixture("healthz-ok.txt"))}
        with tempfile.TemporaryDirectory() as d, FixtureServer(routes) as srv:
            ledger = os.path.join(d, "l.jsonl")
            r = run_cli("sample", "--metrics-url", srv.url + "/", "--ledger", ledger)
            self.assertEqual(r.returncode, 0, r.stderr)
            [rec] = read_ledger(ledger)
            self.assertEqual(rec["kind"], "sample")
            self.assertEqual(rec["healthz"], 200)
            self.assertEqual(rec["git_sha"], fixture_git_sha())

    def test_bad_metrics_url_scheme_exit_2(self):
        with tempfile.TemporaryDirectory() as d:
            ledger = os.path.join(d, "l.jsonl")
            for url in ("https://127.0.0.1:7302", "ftp://x", "127.0.0.1:7302"):
                r = run_cli("sample", "--metrics-url", url, "--ledger", ledger)
                self.assertEqual(r.returncode, 2, url)
                self.assertIn("http://", r.stderr)
            self.assertFalse(os.path.exists(ledger))

    def test_bad_json_addr_exit_2(self):
        with tempfile.TemporaryDirectory() as d:
            r = run_cli("record-spots", "--json-addr", "localhost", "--out-dir", d)
            self.assertEqual(r.returncode, 2)


# ---------------------------------------------------------------------------
# report

DAY = 86400.0
T0 = datetime(2026, 1, 1, tzinfo=timezone.utc).timestamp()
IV = 600.0


def make_ledger(days=30.0, start=T0, step=IV, start_note=True, end_note=True, sha="aaa111",
                overrides=None, skip=None, spots_per_sample=1, telnet=1, extra=None):
    """Synthetic ledger: one sample every `step` s over `days`. `overrides`
    maps sample index -> dict merged into that sample; `skip` is a set or
    predicate of indices to drop. Counters are tracked as the daemon would."""
    recs = []
    if start_note:
        recs.append({"v": 1, "kind": "note", "ts": start, "note": "start", "reason": "go"})
    n = int(round(days * DAY / step)) + 1
    st = start - 5.0
    spots = 0
    for i in range(n):
        ts = start + i * step
        o = (overrides or {}).get(i, {})
        if "start_time" in o and o["start_time"] != st:
            st = o["start_time"]
            spots = 0
        if i > 0:
            spots += o.get("spots_inc", spots_per_sample)
        rec = {"v": 1, "kind": "sample", "ts": ts, "reachable": True, "healthz": 200,
               "start_time": st, "spots_total": spots, "telnet_clients": telnet,
               "json_clients": 1, "source_health": {"soapy": 1}, "source_outages": {"soapy": 0},
               "source_down_s": {"soapy": 0.0}, "git_sha": sha, "version": "0.1.0",
               "decode_latency_sum": 0.01 * i, "decode_latency_count": 2 * i, "rss_kb": 50000,
               "missing": []}
        rec.update({k: v for k, v in o.items() if k != "spots_inc"})
        if skip is not None and (skip(i) if callable(skip) else i in skip):
            continue
        if not rec["reachable"]:
            for k in ("start_time", "spots_total", "telnet_clients", "source_health", "git_sha"):
                rec.pop(k, None)
            rec["healthz"] = None
        recs.append(rec)
    if end_note:
        recs.append({"v": 1, "kind": "note", "ts": start + days * DAY, "note": "end", "reason": "done"})
    recs.extend(extra or [])
    return recs


def summ(recs, **params):
    p = {"interval_s": IV}
    p.update(params)
    return fn.summarize(recs, p)


def crit(summary, cid):
    return [c for c in summary["criteria"] if c["id"] == cid][0]


def write_ledger(path, recs, tail=b""):
    with open(path, "wb") as f:
        for r in recs:
            f.write(json.dumps(r).encode() + b"\n")
        f.write(tail)


class ReportTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.ledger = os.path.join(self.tmp.name, "ledger.jsonl")

    def tearDown(self):
        self.tmp.cleanup()

    def report(self, recs, *extra, tail=b""):
        write_ledger(self.ledger, recs, tail)
        return run_cli("report", "--ledger", self.ledger, "--interval-s", str(IV), *extra)

    def test_clean_30_day_run_passes(self):
        recs = make_ledger()
        s = summ(recs)
        self.assertEqual(s["verdict"], "PASS")
        self.assertAlmostEqual(s["availability"], 1.0)
        r = self.report(recs)
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertIn("**Overall: PASS**", r.stdout)
        self.assertIn("100.00%", r.stdout)
        for section in ("## Verdict", "## Window", "## Restarts", "## Watchdog recoveries",
                        "## Source outages", "## Daily", "## Builds", "## Notes", "## Ledger integrity"):
            self.assertIn(section, r.stdout)

    def test_span_shorter_than_min_days_fails_in_progress(self):
        recs = make_ledger(days=7)
        r = self.report(recs, "--min-days", "30")
        self.assertEqual(r.returncode, 1)
        self.assertIn("IN PROGRESS: 7.0/30 days", r.stdout)
        r = self.report(recs, "--min-days", "7")
        self.assertEqual(r.returncode, 0, r.stdout)

    def test_unobserved_gap_counts_as_down(self):
        # samples 1000..1060 missing -> one 10 h (61 * 600 s - ...) gap
        recs = make_ledger(skip=set(range(1001, 1060)))
        s = summ(recs)
        self.assertAlmostEqual(s["unobserved_s"], 36000.0)
        self.assertAlmostEqual(s["availability"], 1 - 36000.0 / (30 * DAY), places=6)
        self.assertAlmostEqual(s["availability"] * 100, 98.6, places=1)
        self.assertEqual(s["verdict"], "FAIL")
        r = self.report(recs)
        self.assertEqual(r.returncode, 1)
        self.assertIn("FAIL (D-A)", r.stdout)

    def test_planned_restart_window_excluded_and_listed(self):
        unreachable = {"reachable": False}
        ov = {101: unreachable, 102: unreachable}
        for i in range(103, 4321):
            ov[i] = {"start_time": T0 + 102 * IV + 30}
        note = {"v": 1, "kind": "note", "ts": T0 + 100 * IV + 60, "note": "planned", "reason": "upgrade"}
        recs = make_ledger(overrides=ov, extra=[note])
        s = summ(recs)
        self.assertEqual([r["class"] for r in s["restarts"]], ["planned"])
        # window: note .. first healthy sample after it (sample 103), under the 1800 s cap
        self.assertAlmostEqual(s["planned_excluded_s"], 3 * IV - 60)
        self.assertAlmostEqual(s["availability"], 1.0)
        self.assertEqual(s["verdict"], "PASS")
        r = self.report(recs)
        self.assertIn("Planned restarts: 1", r.stdout)

    def test_planned_window_survives_a_healthy_sample_before_the_restart(self):
        # note, one more healthy sample (restart not begun yet), then down
        ov = {102: {"reachable": False}}
        for i in range(103, 4321):
            ov[i] = {"start_time": T0 + 102 * IV + 30}
        note = {"v": 1, "kind": "note", "ts": T0 + 100 * IV + 500, "note": "planned", "reason": "upgrade"}
        s = summ(make_ledger(overrides=ov, extra=[note]))
        self.assertAlmostEqual(s["planned_excluded_s"], 3 * IV - 500)
        self.assertAlmostEqual(s["availability"], 1.0)
        self.assertEqual([r["class"] for r in s["restarts"]], ["planned"])

    def test_planned_window_capped_at_1800_s(self):
        ov = {i: {"reachable": False} for i in range(101, 113)}  # 2 h down
        note = {"v": 1, "kind": "note", "ts": T0 + 101 * IV, "note": "planned", "reason": "x"}
        recs = make_ledger(overrides=ov, extra=[note])
        s = summ(recs)
        self.assertAlmostEqual(s["planned_excluded_s"], 1800.0)
        down = 12 * IV - 1800.0
        self.assertAlmostEqual(s["availability"], 1 - down / (30 * DAY - 1800.0))

    def test_unexplained_restart_listed_not_failed(self):
        ov = {200: {"reachable": False}}
        for i in range(201, 4321):
            ov[i] = {"start_time": T0 + 200 * IV + 10}
        recs = make_ledger(overrides=ov)
        s = summ(recs)
        self.assertEqual([r["class"] for r in s["restarts"]], ["unexplained"])
        self.assertAlmostEqual(s["down_s"], IV)
        self.assertLess(s["availability"], 1.0)
        self.assertEqual(s["verdict"], "PASS")
        r = self.report(recs)
        self.assertEqual(r.returncode, 0)
        self.assertIn("Unexplained restarts (classify from `journalctl -u manta-field`): 1", r.stdout)

    def test_watchdog_recovery_listed_as_automated(self):
        ov = {i: {"reachable": False} for i in range(300, 312)}
        for i in range(312, 4321):
            ov[i] = {"start_time": T0 + 311 * IV + 20}
        rec = {"v": 1, "kind": "recovery", "ts": T0 + 310 * IV + 5, "cmd": ["/usr/local/bin/manta-field-recover"],
               "exit": 0, "bad_for_s": 600.0}
        recs = make_ledger(overrides=ov, extra=[rec])
        s = summ(recs)
        self.assertEqual([r["class"] for r in s["restarts"]], ["automated"])
        self.assertEqual(len(s["recoveries"]), 1)
        self.assertEqual(s["manual_interventions"], 0)
        r = self.report(recs)
        self.assertIn("Automated (watchdog) restarts: 1", r.stdout)
        self.assertIn("Recoveries: 1", r.stdout)
        self.assertIn("manta-field-recover", r.stdout)

    def test_manual_note_fails_d_d(self):
        note = {"v": 1, "kind": "note", "ts": T0 + 5 * DAY, "note": "manual", "reason": "USB replug"}
        recs = make_ledger(extra=[note])
        s = summ(recs)
        self.assertFalse(crit(s, "D-D")["pass"])
        self.assertEqual(s["verdict"], "FAIL")
        r = self.report(recs)
        self.assertEqual(r.returncode, 1)
        self.assertIn("FAIL (D-D)", r.stdout)
        self.assertIn("USB replug", r.stdout)

    def test_zero_spot_utc_day_fails_d_c(self):
        per_day = int(DAY / IV)
        # samples 00:00..23:50 on 2026-01-04 carry no new spots; the 01-05
        # 00:00 sample's interval crosses midnight, so it credits neither day
        ov = {i: {"spots_inc": 0} for i in range(3 * per_day, 4 * per_day)}
        recs = make_ledger(overrides=ov)
        s = summ(recs)
        c = crit(s, "D-C")
        self.assertFalse(c["pass"])
        self.assertEqual(c["zero_spot_days"], ["2026-01-04"])
        r = self.report(recs)
        self.assertIn("FAIL (D-C)", r.stdout)

    def test_cross_midnight_spot_delta_not_credited_to_later_day(self):
        # samples at hh:m0:30. 2026-01-04's only spot is at 23:59:30, seen by
        # the 00:00:30 sample on 2026-01-05, which emits nothing itself.
        per_day = int(DAY / IV)
        ov = {i: {"spots_inc": 0} for i in range(3 * per_day, 5 * per_day + 1)}
        ov[4 * per_day] = {"spots_inc": 1}
        recs = make_ledger(start=T0 + 30, overrides=ov)
        spots_dir = os.path.join(self.tmp.name, "spots")
        os.makedirs(spots_dir)
        with open(os.path.join(spots_dir, "spots-2026-01-04.jsonl"), "wb") as f:
            f.write(spot_line("W1AW", int(T0 + 4 * DAY - 30)) + b"\n")
        s = summ(recs, recorded_by_day=fn.load_recorded_spots(spots_dir))
        self.assertEqual(crit(s, "D-C")["zero_spot_days"], ["2026-01-05"])
        self.assertEqual([r["spots_emitted"] for r in s["daily"] if r["day"] == "2026-01-05"], [0])
        # 30 intervals cross a midnight; the ones into 01-04 and 01-06 emit nothing
        self.assertEqual(s["spots_unplaced_total"], 28)
        self.assertEqual(s["spots_emitted_total"], 30 * per_day - 2 * per_day)
        # without the archive the counters cannot place it: neither day is proven
        s = summ(recs)
        self.assertEqual(crit(s, "D-C")["zero_spot_days"], ["2026-01-04", "2026-01-05"])
        self.assertTrue(any("--spots-dir places them" in w for w in s["warnings"]))
        r = self.report(recs, "--spots-dir", spots_dir)
        self.assertIn("zero-spot days: 2026-01-05)", r.stdout)
        self.assertIn("28 in sample intervals that cross UTC midnight", r.stdout)

    def test_spot_emitted_after_its_midnight_counts_for_its_day(self):
        # the 23:59:30 spot on 2026-01-04 is emitted (counted) only after
        # the 00:00:30 sample, so the counters put it inside 2026-01-05
        per_day = int(DAY / IV)
        ov = {i: {"spots_inc": 0} for i in range(3 * per_day, 4 * per_day + 1)}
        recs = make_ledger(start=T0 + 30, overrides=ov)
        self.assertEqual(crit(summ(recs), "D-C")["zero_spot_days"], ["2026-01-04"])
        s = summ(recs, recorded_by_day={"2026-01-04": 1})
        self.assertTrue(crit(s, "D-C")["pass"])

    def test_restart_after_midnight_credits_its_day(self):
        # restart at 00:00:05 on 2026-01-05; its 1 spot is that day's only one
        per_day = int(DAY / IV)
        ov = {i: {"spots_inc": 0} for i in range(4 * per_day + 1, 5 * per_day + 1)}
        ov[4 * per_day] = {"start_time": T0 + 4 * DAY + 5, "spots_inc": 1}
        s = summ(make_ledger(start=T0 + 30, overrides=ov))
        self.assertEqual([r["spots_emitted"] for r in s["daily"] if r["day"] == "2026-01-05"], [1])
        self.assertTrue(crit(s, "D-C")["pass"])

    def test_spot_deltas_survive_restart(self):
        ov = {}
        for i in range(500, 4321):
            ov[i] = {"start_time": T0 + 500 * IV - 100}
        recs = make_ledger(overrides=ov, spots_per_sample=2)
        # 4320 intervals, each emitting 2 spots, restart included
        s = summ(recs)
        self.assertEqual(s["spots_emitted_total"], 2 * 4320)
        self.assertTrue(all(r["spots_emitted"] >= 0 for r in s["daily"]))

    def test_sub_interval_outage_counted_from_counters(self):
        ov = {}
        for i in range(1000, 4321):
            ov[i] = {"source_outages": {"soapy": 2}, "source_down_s": {"soapy": 45.0}}
        recs = make_ledger(overrides=ov)
        s = summ(recs)
        self.assertEqual(s["source_outages"]["outages"], 2)
        self.assertAlmostEqual(s["source_outages"]["down_s"], 45.0)
        self.assertAlmostEqual(s["counter_down_s"], 45.0)
        self.assertAlmostEqual(s["down_s"], 45.0)
        r = self.report(recs)
        self.assertIn("Source outages: 2, down 45 s", r.stdout)

    def test_aggregator_fraction_below_95_fails_d_b(self):
        ov = {i: {"telnet_clients": 0} for i in range(0, 4321, 10)}  # 10% without
        recs = make_ledger(overrides=ov)
        s = summ(recs)
        self.assertLess(s["aggregator_fraction"], 0.95)
        self.assertFalse(crit(s, "D-B")["pass"])
        r = self.report(recs)
        self.assertEqual(r.returncode, 1)
        self.assertIn("FAIL (D-B)", r.stdout)

    def test_truncated_last_line_tolerated(self):
        recs = make_ledger()
        r = self.report(recs, tail=b'{"v":1,"kind":"sample","ts":17')
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertIn("unparseable ledger lines: 1", r.stdout)

    def test_span_starts_at_start_note_and_ends_at_end_note(self):
        recs = make_ledger(days=10)
        recs.append({"v": 1, "kind": "note", "ts": T0 + DAY + 0.5, "note": "start", "reason": "restart clock"})
        s = summ(recs, min_days=1)
        self.assertEqual(s["window"]["start"], T0 + DAY + 0.5)
        self.assertEqual(s["window"]["end"], T0 + 10 * DAY)
        self.assertEqual(s["warnings"], [])
        # fallback: no notes -> first/last sample, with warnings
        s2 = summ(make_ledger(days=10, start_note=False, end_note=False), min_days=1)
        self.assertEqual(s2["window"]["start"], T0)
        self.assertEqual(s2["window"]["end"], T0 + 10 * DAY)
        self.assertTrue(any("no start note" in w for w in s2["warnings"]))
        self.assertTrue(any("no end note" in w for w in s2["warnings"]))

    def test_recorded_vs_emitted_spot_coverage(self):
        # 100 emitted spots over the span; 98 recorded
        ov = {i: {"spots_inc": 0} for i in range(1, 4321)}
        for i in range(1, 101):
            ov[i * 40] = {"spots_inc": 1}
        recs = make_ledger(overrides=ov)
        spots_dir = os.path.join(self.tmp.name, "spots")
        os.makedirs(spots_dir)
        with open(os.path.join(spots_dir, "spots-2026-01-02.jsonl"), "wb") as f:
            for k in range(98):
                f.write(spot_line("W1AW", int(T0 + DAY + 60 * k)) + b"\n")
        s = summ(recs, recorded_by_day=fn.load_recorded_spots(spots_dir))
        self.assertEqual(s["spots_emitted_total"], 100)
        self.assertEqual(s["spots_recorded_total"], 98)
        self.assertTrue(any("coverage 98.0%" in w for w in s["warnings"]))
        r = self.report(recs, "--spots-dir", spots_dir, "--min-days", "1")
        self.assertIn("WARNING: spot archive coverage 98.0%", r.stdout)
        # informational, not a gate (D-C fails here only because most days are empty)
        self.assertNotIn("coverage", " ".join(c["name"] for c in s["criteria"]))

    def test_recorded_spots_skip_out_of_range_timestamps(self):
        spots_dir = os.path.join(self.tmp.name, "spots")
        os.makedirs(spots_dir)
        with open(os.path.join(spots_dir, "spots-2026-01-02.jsonl"), "wb") as f:
            f.write(b'{"timestamp": 1e20, "dxCall": "W1AW", "frequency": 14025000}\n')
            f.write(b'{"timestamp": NaN, "dxCall": "W1AW", "frequency": 14025000}\n')
            f.write(spot_line("W1AW", int(T0 + DAY)) + b"\n")
        self.assertEqual(sum(fn.load_recorded_spots(spots_dir).values()), 1)

    def test_builds_listed(self):
        ov = {i: {"git_sha": "bbb222", "start_time": T0 + 2000 * IV - 5} for i in range(2000, 4321)}
        note = {"v": 1, "kind": "note", "ts": T0 + 1999 * IV + 1, "note": "planned", "reason": "upgrade"}
        recs = make_ledger(overrides=ov, extra=[note])
        s = summ(recs)
        self.assertEqual([b["git_sha"] for b in s["builds"]], ["aaa111", "bbb222"])
        r = self.report(recs)
        self.assertLess(r.stdout.index("aaa111"), r.stdout.index("bbb222"))

    def test_json_format(self):
        recs = make_ledger(skip=set(range(1001, 1060)))
        r = self.report(recs, "--format", "json")
        self.assertEqual(r.returncode, 1)
        out = json.loads(r.stdout)
        s = summ(recs)
        self.assertEqual(out["verdict"], "FAIL")
        self.assertAlmostEqual(out["availability"], s["availability"])
        self.assertAlmostEqual(out["aggregator_fraction"], s["aggregator_fraction"])
        self.assertEqual(out["spots_emitted_total"], s["spots_emitted_total"])
        self.assertEqual([c["pass"] for c in out["criteria"]], [c["pass"] for c in s["criteria"]])

    def test_from_to_window(self):
        recs = make_ledger(skip=set(range(1001, 1060)))  # gap on day 7
        frm = datetime(2026, 1, 2, tzinfo=timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        to = datetime(2026, 1, 3, tzinfo=timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        r = self.report(recs, "--from", frm, "--to", to, "--min-days", "1", "--format", "json")
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        out = json.loads(r.stdout)
        self.assertAlmostEqual(out["window"]["span_days"], 1.0)
        self.assertAlmostEqual(out["availability"], 1.0)
        self.assertEqual(out["spots_emitted_total"], int(DAY / IV))
        # the same ledger over its whole span fails D-A
        self.assertEqual(self.report(recs).returncode, 1)

    def test_bad_from_is_usage_error(self):
        r = self.report(make_ledger(days=1), "--from", "yesterday")
        self.assertEqual(r.returncode, 2)

    def test_non_finite_or_out_of_range_thresholds_are_usage_errors(self):
        recs = make_ledger(days=1)
        for flag, value in (("--min-days", "nan"), ("--min-days", "inf"), ("--min-days", "-1"),
                            ("--interval-s", "nan"), ("--interval-s", "inf"), ("--interval-s", "1e308"),
                            ("--interval-s", "0"), ("--min-availability", "nan"),
                            ("--min-availability", "1.5"), ("--min-aggregator", "nan"),
                            ("--min-aggregator", "-0.1"), ("--planned-window-max-s", "inf"),
                            ("--planned-window-max-s", "nan"), ("--planned-window-max-s", "-1")):
            with self.subTest(flag=flag, value=value):
                r = self.report(recs, flag, value)
                self.assertEqual(r.returncode, 2, r.stdout + r.stderr)
                self.assertNotIn("PASS", r.stdout)

    def test_missing_ledger_is_usage_error(self):
        r = run_cli("report", "--ledger", os.path.join(self.tmp.name, "nope.jsonl"))
        self.assertEqual(r.returncode, 2)


if __name__ == "__main__":
    unittest.main()
