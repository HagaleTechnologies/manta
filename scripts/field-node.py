#!/usr/bin/env python3
"""Evidence kit for a manta secondary-skimmer field node (MAN-96).

MAN-96 asks for a real manta node to run for 30 days behind an RBN
Aggregator "without manual intervention beyond planned restarts". This
script collects the evidence for that claim on the node itself and turns it
into a PASS/FAIL verdict. Four subcommands:

  sample        Scrape the node's /healthz and /metrics once (or every
                --interval-s with --loop) and append one JSON line per
                sample to a ledger. Optionally runs a bounded SDR-service
                watchdog (--recover-cmd): once the node has been continuously
                bad (metrics unreachable, or any manta_source_health == 0)
                for --recover-after-s, run the command, then cool down for
                --recover-cooldown-s, at most --recover-max-per-day times per
                UTC day. Every recovery is logged to the ledger.
  note          Append an operator note to the ledger: `start` (starts the
                30-day clock), `planned` (written *before* a planned
                restart), `manual` (any human intervention; fails the run),
                `end`.
  record-spots  Archive the node's own spots from its JSON Lines port
                (raw TCP, newline-delimited) into spots-<UTC date>.jsonl
                files, byte-identical to the wire. Reconnects with the same
                1 s -> 60 s backoff as crates/manta-server/src/backoff.rs.
  report        Turn the ledger (and optionally the spot archive) into a
                markdown or JSON report with one PASS/FAIL line per
                criterion (availability D-A, Aggregator connected D-B, a spot
                every full UTC day D-C, zero manual interventions D-D) plus
                an overall verdict. Exit 0 PASS, 1 FAIL or in progress,
                2 usage/input error.

Why a ledger rather than Prometheus: a 30-day claim has to survive the
monitoring stack itself restarting, and has to be auditable from one file
whose SHA-256 the field report records (D-N). Source drops shorter than one
sample interval are still counted, via manta_source_outages_total /
manta_source_down_seconds_total.

The thresholds and their rationale (D-A..D-N) are recorded in
docs/DECISIONS/2026-10-07-man96-secondary-skimmer-field-node.md; the
procedure that runs this script is
docs/RUNBOOKS/secondary-skimmer-field-node.md.

Stdlib only; runs on Python >= 3.9.

Usage:
  field-node.py sample --metrics-url http://127.0.0.1:7302 \\
      --ledger /var/lib/manta-field/ledger.jsonl --loop --interval-s 60 \\
      --rss-unit manta-field.service --recover-cmd /usr/local/bin/manta-field-recover
  field-node.py note --ledger ledger.jsonl --kind planned --reason "kernel update"
  field-node.py record-spots --json-addr 127.0.0.1:7301 --out-dir /var/lib/manta-field/spots
  field-node.py report --ledger ledger.jsonl --spots-dir spots/ --format markdown
"""
from __future__ import annotations

import argparse
import collections
import json
import math
import os
import re
import shlex
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from datetime import datetime, timezone

DECISION_DOC = "docs/DECISIONS/2026-10-07-man96-secondary-skimmer-field-node.md"
RUNBOOK = "docs/RUNBOOKS/secondary-skimmer-field-node.md"

# Every series `sample` reads. crates/manta-server/tests/field_node_contract.rs fails if
# Metrics stops rendering one of them; keep this tuple a plain literal (the test parses it).
REQUIRED_METRICS = (
    "manta_start_time_seconds",
    "manta_uptime_seconds",
    "manta_build_info",
    "manta_spots_total",
    "manta_spots_dropped_lagged_total",
    "manta_source_health",
    "manta_source_outages_total",
    "manta_source_down_seconds_total",
    "manta_telnet_clients_connected",
    "manta_json_clients_connected",
    "manta_healthy",
    "manta_decode_latency_seconds",
)
OPTIONAL_METRICS = ("manta_spots_by_band_total",)  # appears after the first spot (R4)

LEDGER_VERSION = 1
HTTP_TIMEOUT_S = 5.0
NOTE_KINDS = ("start", "planned", "manual", "end")
# A restart counts as planned/automated if a planned note / recovery record
# falls in (a.ts - this, b.ts] around the restart (plan Phase 3).
RESTART_ATTRIBUTION_LOOKBACK_S = 1800.0
# D-F: a gap between samples longer than this many intervals is unobserved.
UNOBSERVED_GAP_FACTOR = 3.0
# Informational: warn when the spot archive holds fewer than this fraction
# of the spots the node emitted.
RECORDED_COVERAGE_WARN = 0.99
BACKOFF_INITIAL_S = 1
BACKOFF_MAX_S = 60
# A connection open at least this long counts as productive (resets backoff).
BACKOFF_RESET_AFTER_S = 60.0
WATCHDOG_TAIL_BYTES = 4 * 1024 * 1024


class UsageError(Exception):
    """Bad flags or unusable input: exit 2."""


# ---------------------------------------------------------------------------
# Prometheus text parsing


_SAMPLE_RE = re.compile(r"^([a-zA-Z_:][a-zA-Z0-9_:]*)(?:\{(.*)\})?\s+(\S+)(?:\s+-?\d+)?\s*$")
_LABEL_RE = re.compile(r'\s*([a-zA-Z_][a-zA-Z0-9_]*)\s*=\s*"((?:[^"\\]|\\.)*)"\s*,?')
_TYPE_RE = re.compile(r"^#\s*TYPE\s+([a-zA-Z_:][a-zA-Z0-9_:]*)\s+(\S+)")


def _unescape_label(value):
    out = []
    i = 0
    while i < len(value):
        c = value[i]
        if c == "\\" and i + 1 < len(value):
            n = value[i + 1]
            out.append("\n" if n == "n" else n)
            i += 2
            continue
        out.append(c)
        i += 1
    return "".join(out)


def _parse_labels(text):
    labels = {}
    pos = 0
    while pos < len(text):
        m = _LABEL_RE.match(text, pos)
        if not m:
            break
        labels[m.group(1)] = _unescape_label(m.group(2))
        pos = m.end()
    return labels


def parse_prometheus_text(text):
    """Parse Prometheus text exposition into (samples, types).

    samples: list of (name, labels dict, float value); types: {family: type}.
    Comments, HELP lines and blank lines are skipped; unparseable lines too.
    """
    samples = []
    types = {}
    for raw in text.splitlines():
        line = raw.strip()
        if not line:
            continue
        if line.startswith("#"):
            m = _TYPE_RE.match(line)
            if m:
                types[m.group(1)] = m.group(2)
            continue
        m = _SAMPLE_RE.match(line)
        if not m:
            continue
        name, label_text, value_text = m.group(1), m.group(2), m.group(3)
        try:
            value = float(value_text)
        except ValueError:
            continue
        samples.append((name, _parse_labels(label_text) if label_text else {}, value))
    return samples, types


def _family_present(family, samples, types):
    if family in types:
        return True
    names = (family, family + "_bucket", family + "_sum", family + "_count")
    return any(s[0] in names for s in samples)


def _first(samples, name):
    for s in samples:
        if s[0] == name:
            return s[2]
    return None


def _by_label(samples, name, label):
    out = {}
    for s in samples:
        if s[0] == name and label in s[1]:
            out[s[1][label]] = s[2]
    return out


def _num(v):
    """Integral floats as ints so the ledger reads naturally."""
    if v is None:
        return None
    if isinstance(v, float) and math.isfinite(v) and v == int(v) and abs(v) < 2 ** 53:
        return int(v)
    return v


# ---------------------------------------------------------------------------
# sample


def validate_http_url(url):
    parts = urllib.parse.urlsplit(url)
    if parts.scheme != "http" or not parts.netloc:
        raise UsageError("--metrics-url must be an http:// URL with a host, got %r" % url)
    return url.rstrip("/")


def http_get(url, timeout=HTTP_TIMEOUT_S):
    """GET url. Returns (status, body). An HTTP error status is data, not an
    exception; connection failures raise OSError/URLError."""
    try:
        with urllib.request.urlopen(url, timeout=timeout) as resp:
            return resp.status, resp.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        try:
            body = e.read().decode("utf-8", "replace")
        except Exception:  # noqa: BLE001 - body is best-effort
            body = ""
        return e.code, body


def read_rss_kb(unit):
    """VmRSS (kB) of a systemd unit's main PID; None on any failure."""
    try:
        out = subprocess.run(
            ["systemctl", "show", "-p", "MainPID", "--value", unit],
            capture_output=True, text=True, timeout=10,
        )
        pid = int(out.stdout.strip())
        if pid <= 0:
            return None
        with open("/proc/%d/status" % pid) as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1])
    except Exception:  # noqa: BLE001 - rss is best-effort
        return None
    return None


def build_sample(metrics_status, metrics_body, healthz_status, healthz_body, ts):
    """Pure: turn the two HTTP responses into one ledger sample record."""
    rec = {"v": LEDGER_VERSION, "kind": "sample", "ts": ts}
    rec["healthz"] = healthz_status
    if healthz_body is not None:
        lines = [ln for ln in healthz_body.splitlines() if ln.strip()]
        rec["healthz_status"] = lines[0].strip() if lines else None
        rec["checks"] = [ln.strip() for ln in lines[1:]]
    else:
        rec["healthz_status"] = None
        rec["checks"] = []
    rec["reachable"] = metrics_status == 200
    if metrics_status != 200:
        rec["error"] = "metrics HTTP %s" % metrics_status
        return rec
    samples, types = parse_prometheus_text(metrics_body)
    rec["start_time"] = _first(samples, "manta_start_time_seconds")
    rec["uptime"] = _first(samples, "manta_uptime_seconds")
    build = [s for s in samples if s[0] == "manta_build_info"]
    rec["git_sha"] = build[0][1].get("git_sha") if build else None
    rec["version"] = build[0][1].get("version") if build else None
    rec["spots_total"] = _num(_first(samples, "manta_spots_total"))
    by_band = {}
    for name, labels, value in samples:
        if name == "manta_spots_by_band_total":
            key = "%s/%s" % (labels.get("band", "?"), labels.get("type", "?"))
            by_band[key] = _num(value)
    rec["spots_by_band"] = by_band
    rec["spots_dropped_lagged"] = _num(_first(samples, "manta_spots_dropped_lagged_total"))
    rec["source_health"] = {k: _num(v) for k, v in _by_label(samples, "manta_source_health", "source").items()}
    rec["source_outages"] = {k: _num(v) for k, v in _by_label(samples, "manta_source_outages_total", "source").items()}
    rec["source_down_s"] = _by_label(samples, "manta_source_down_seconds_total", "source")
    rec["telnet_clients"] = _num(_first(samples, "manta_telnet_clients_connected"))
    rec["json_clients"] = _num(_first(samples, "manta_json_clients_connected"))
    rec["healthy_gauge"] = _num(_first(samples, "manta_healthy"))
    rec["decode_latency_sum"] = _first(samples, "manta_decode_latency_seconds_sum")
    rec["decode_latency_count"] = _num(_first(samples, "manta_decode_latency_seconds_count"))
    rec["missing"] = [m for m in REQUIRED_METRICS if not _family_present(m, samples, types)]
    return rec


def take_sample(base_url, rss_unit=None, clock=time.time, timeout=HTTP_TIMEOUT_S):
    ts = clock()
    healthz_status, healthz_body, healthz_error = None, None, None
    try:
        healthz_status, healthz_body = http_get(base_url + "/healthz", timeout)
    except Exception as e:  # noqa: BLE001 - unreachable is data
        healthz_error = "%s: %s" % (type(e).__name__, e)
    try:
        metrics_status, metrics_body = http_get(base_url + "/metrics", timeout)
    except Exception as e:  # noqa: BLE001 - unreachable is data
        rec = build_sample(None, None, healthz_status, healthz_body, ts)
        rec["reachable"] = False
        rec["error"] = "%s: %s" % (type(e).__name__, e)
    else:
        rec = build_sample(metrics_status, metrics_body, healthz_status, healthz_body, ts)
    if healthz_error is not None:
        rec["healthz_error"] = healthz_error
    rec["rss_kb"] = read_rss_kb(rss_unit) if rss_unit else None
    return rec


def append_ledger(path, record):
    """Append one JSON line, flushed and fsynced."""
    line = json.dumps(record, sort_keys=True, separators=(",", ":")) + "\n"
    with open(path, "a", encoding="utf-8") as f:
        f.write(line)
        f.flush()
        os.fsync(f.fileno())


def is_bad(sample):
    """Watchdog "bad": metrics unreachable, or any source reports unhealthy."""
    if not sample.get("reachable"):
        return True
    return any(v == 0 for v in (sample.get("source_health") or {}).values())


def _utc_day(ts):
    return datetime.fromtimestamp(ts, tz=timezone.utc).strftime("%Y-%m-%d")


def bad_streak_s(samples, now):
    """Seconds the node has been continuously bad as of `now` (0 if the
    newest sample is good or there are no samples)."""
    ordered = sorted(samples, key=lambda s: s["ts"])
    if not ordered or not is_bad(ordered[-1]):
        return 0.0
    first_bad = ordered[-1]["ts"]
    for s in reversed(ordered):
        if not is_bad(s):
            break
        first_bad = s["ts"]
    return max(0.0, now - first_bad)


def should_recover(samples, last_recoveries, now, after_s, cooldown_s, max_per_day):
    """Pure D-G policy: True iff the node has been continuously bad for
    >= after_s, no recovery ran in the last cooldown_s, and fewer than
    max_per_day recoveries ran on now's UTC day."""
    if not samples or bad_streak_s(samples, now) < after_s:
        return False
    for r in last_recoveries:
        if now - r < cooldown_s:
            return False
    today = _utc_day(now)
    if sum(1 for r in last_recoveries if _utc_day(r) == today) >= max_per_day:
        return False
    return True


def run_recovery(cmd, timeout_s, ledger, bad_for_s, clock=time.time):
    """Run the recovery command (no shell) and log a recovery record."""
    argv = shlex.split(cmd)
    rec = {"v": LEDGER_VERSION, "kind": "recovery", "ts": clock(), "cmd": argv,
           "bad_for_s": round(bad_for_s, 3), "exit": None}
    try:
        # Own process group, so a timeout kills the whole recovery script
        # (and anything it spawned) instead of hanging on inherited pipes.
        proc = subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
                                start_new_session=(os.name == "posix"))
    except OSError as e:
        rec["error"] = "%s: %s" % (type(e).__name__, e)
    else:
        try:
            out, _ = proc.communicate(timeout=timeout_s)
            rec["exit"] = proc.returncode
            if out and out.strip():
                rec["output"] = out.strip()[-2000:]
        except subprocess.TimeoutExpired:
            rec["error"] = "timeout"
            try:
                if os.name == "posix":
                    os.killpg(proc.pid, signal.SIGKILL)
                else:
                    proc.kill()
            except OSError:
                pass
            try:
                proc.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                pass
    append_ledger(ledger, rec)
    print("field-node: recovery ran %s -> exit %s%s" % (
        argv, rec["exit"], (" (" + rec["error"] + ")") if "error" in rec else ""), file=sys.stderr)
    return rec


def read_ledger_tail(path, max_bytes=WATCHDOG_TAIL_BYTES):
    """Records from the last max_bytes of a ledger (best-effort)."""
    try:
        with open(path, "rb") as f:
            f.seek(0, os.SEEK_END)
            size = f.tell()
            f.seek(max(0, size - max_bytes))
            data = f.read()
    except OSError:
        return []
    lines = data.split(b"\n")
    if size > max_bytes and lines:
        lines = lines[1:]
    out = []
    for ln in lines:
        try:
            rec = json.loads(ln.decode("utf-8"))
        except Exception:  # noqa: BLE001 - skip partial/corrupt lines
            continue
        if isinstance(rec, dict):
            out.append(rec)
    return out


class Watchdog:
    """Bounded SDR-service watchdog (D-G). Keeps the last
    after_s / interval + 2 samples in memory."""

    def __init__(self, cmd, ledger, after_s, cooldown_s, max_per_day, timeout_s, interval_s,
                 clock=time.time, seed_from_ledger=True):
        self.cmd = cmd
        self.ledger = ledger
        self.after_s = after_s
        self.cooldown_s = cooldown_s
        self.max_per_day = max_per_day
        self.timeout_s = timeout_s
        self.clock = clock
        self.samples = collections.deque(maxlen=int(after_s // max(interval_s, 1e-9)) + 2)
        self.recoveries = []
        if seed_from_ledger:
            now = clock()
            for rec in read_ledger_tail(ledger):
                ts = rec.get("ts")
                if not isinstance(ts, (int, float)):
                    continue
                if rec.get("kind") == "recovery" and now - ts < max(cooldown_s, 2 * 86400):
                    self.recoveries.append(ts)
                elif rec.get("kind") == "sample" and now - ts <= after_s + 3 * interval_s:
                    self.samples.append(rec)

    def observe(self, sample):
        self.samples.append(sample)
        now = self.clock()
        if not should_recover(list(self.samples), self.recoveries, now,
                              self.after_s, self.cooldown_s, self.max_per_day):
            return None
        rec = run_recovery(self.cmd, self.timeout_s, self.ledger,
                           bad_streak_s(self.samples, now), clock=self.clock)
        self.recoveries.append(rec["ts"])
        return rec


def cmd_sample(args, sleep=time.sleep, clock=time.time, max_iterations=None):
    base = validate_http_url(args.metrics_url)
    if args.interval_s <= 0:
        raise UsageError("--interval-s must be > 0")
    watchdog = None
    if args.recover_cmd:
        if not shlex.split(args.recover_cmd):
            raise UsageError("--recover-cmd is empty")
        watchdog = Watchdog(args.recover_cmd, args.ledger, args.recover_after_s,
                            args.recover_cooldown_s, args.recover_max_per_day,
                            args.recover_timeout_s, args.interval_s, clock=clock)
    n = 0
    while True:
        rec = take_sample(base, args.rss_unit, clock=clock)
        append_ledger(args.ledger, rec)
        if watchdog is not None:
            watchdog.observe(rec)
        n += 1
        if not args.loop or (max_iterations is not None and n >= max_iterations):
            return 0
        t = clock()
        sleep(args.interval_s - (t % args.interval_s))


# ---------------------------------------------------------------------------
# note


def cmd_note(args, clock=time.time):
    rec = {"v": LEDGER_VERSION, "kind": "note", "ts": clock(), "note": args.kind,
           "reason": args.reason}
    append_ledger(args.ledger, rec)
    return 0


# ---------------------------------------------------------------------------
# record-spots


def parse_host_port(text):
    host, sep, port = text.rpartition(":")
    if not sep or not host:
        raise UsageError("--json-addr must be HOST:PORT, got %r" % text)
    host = host.strip("[]")
    try:
        p = int(port)
    except ValueError:
        raise UsageError("--json-addr port must be an integer, got %r" % port)
    if not 0 < p < 65536:
        raise UsageError("--json-addr port out of range: %d" % p)
    return host, p


def validate_spot_line(line):
    """Return the spot dict if `line` (bytes, no newline) is a SpotMessage
    with dxCall/frequency/timestamp; else raise ValueError."""
    try:
        obj = json.loads(line.decode("utf-8"))
    except Exception as e:  # noqa: BLE001
        raise ValueError("not JSON: %s" % e)
    if not isinstance(obj, dict):
        raise ValueError("not a JSON object")
    if not isinstance(obj.get("dxCall"), str) or not obj.get("dxCall"):
        raise ValueError("missing dxCall")
    for key in ("frequency", "timestamp"):
        v = obj.get(key)
        if isinstance(v, bool) or not isinstance(v, (int, float)):
            raise ValueError("missing %s" % key)
    try:
        _utc_day(float(obj["timestamp"]))
    except (OverflowError, OSError, ValueError) as e:
        raise ValueError("bad timestamp: %s" % e)
    return obj


class _SpotWriter:
    def __init__(self, out_dir):
        self.out_dir = out_dir
        self.day = None
        self.fh = None

    def write(self, day, raw):
        if day != self.day:
            self.close()
            self.fh = open(os.path.join(self.out_dir, "spots-%s.jsonl" % day), "ab")
            self.day = day
        self.fh.write(raw)
        self.fh.flush()

    def close(self):
        if self.fh is not None:
            self.fh.close()
        self.fh = None
        self.day = None


def next_backoff(current, productive):
    """Mirror of crates/manta-server/src/backoff.rs `next_backoff`: back to
    1 s after a productive connection, else double up to 60 s."""
    if productive:
        return BACKOFF_INITIAL_S
    return min(current * 2, BACKOFF_MAX_S)


def record_spots(host, port, out_dir, sleep=time.sleep, should_stop=lambda: False,
                 monotonic=time.monotonic, log=None, connect_timeout=5.0, read_timeout=1.0,
                 stats_out=None):
    """Archive spots from the JSON Lines port until should_stop() is true.
    A connection is productive (resets backoff to 1 s) once it delivered a
    line or stayed open >= 60 s. Returns counters {connections,
    disconnects, written, invalid}; `stats_out["stats"]` (if given) is the
    same live dict, for tests driving should_stop."""
    if log is None:
        def log(msg):
            print("record-spots: " + msg, file=sys.stderr, flush=True)
    os.makedirs(out_dir, exist_ok=True)
    stats = {"connections": 0, "disconnects": 0, "written": 0, "invalid": 0}
    if stats_out is not None:
        stats_out["stats"] = stats
    writer = _SpotWriter(out_dir)
    backoff = BACKOFF_INITIAL_S
    try:
        while not should_stop():
            productive = False
            try:
                sock = socket.create_connection((host, port), timeout=connect_timeout)
            except OSError as e:
                log("connect to %s:%d failed: %s" % (host, port, e))
            else:
                stats["connections"] += 1
                opened = monotonic()
                lines = 0
                log("connected to %s:%d" % (host, port))
                buf = b""
                try:
                    sock.settimeout(read_timeout)
                    while True:
                        try:
                            chunk = sock.recv(65536)
                        except socket.timeout:
                            if should_stop():
                                break
                            continue
                        if not chunk:
                            break
                        buf += chunk
                        while b"\n" in buf:
                            line, buf = buf.split(b"\n", 1)
                            lines += 1
                            body = line[:-1] if line.endswith(b"\r") else line
                            if not body.strip():
                                continue
                            try:
                                obj = validate_spot_line(body)
                            except ValueError as e:
                                stats["invalid"] += 1
                                log("invalid line skipped (%s): %r" % (e, line[:200]))
                                continue
                            writer.write(_utc_day(float(obj["timestamp"])), line + b"\n")
                            stats["written"] += 1
                except OSError as e:
                    log("read error: %s" % e)
                finally:
                    sock.close()
                productive = lines > 0 or (monotonic() - opened) >= BACKOFF_RESET_AFTER_S
                stats["disconnects"] += 1
                log("disconnected after %d lines" % lines)
            if productive:
                backoff = next_backoff(backoff, True)
            if should_stop():
                break
            log("reconnecting in %ds" % backoff)
            sleep(backoff)
            backoff = next_backoff(backoff, False)
    finally:
        writer.close()
    return stats


def cmd_record_spots(args):
    host, port = parse_host_port(args.json_addr)
    record_spots(host, port, args.out_dir)
    return 0


# ---------------------------------------------------------------------------
# report


def parse_iso_ts(text):
    try:
        dt = datetime.fromisoformat(text.strip().replace("Z", "+00:00"))
    except ValueError:
        raise UsageError("not an ISO-8601 time: %r" % text)
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt.timestamp()


def load_ledger(path):
    """Returns (records, unparseable_line_count)."""
    records, bad = [], 0
    try:
        f = open(path, "rb")
    except OSError as e:
        raise UsageError("cannot read ledger %s: %s" % (path, e))
    with f:
        for raw in f:
            if not raw.strip():
                continue
            try:
                rec = json.loads(raw.decode("utf-8"))
            except Exception:  # noqa: BLE001
                bad += 1
                continue
            if not isinstance(rec, dict) or not isinstance(rec.get("ts"), (int, float)):
                bad += 1
                continue
            records.append(rec)
    return records, bad


def load_recorded_spots(spots_dir, start=None, end=None):
    """Count archived spots per UTC day of their timestamp, within [start, end]."""
    counts = {}
    if not os.path.isdir(spots_dir):
        raise UsageError("--spots-dir %s is not a directory" % spots_dir)
    for name in sorted(os.listdir(spots_dir)):
        if not (name.startswith("spots-") and name.endswith(".jsonl")):
            continue
        with open(os.path.join(spots_dir, name), "rb") as f:
            for raw in f:
                try:
                    ts = float(json.loads(raw.decode("utf-8"))["timestamp"])
                    # An out-of-range or NaN timestamp is a bad line, not a crash.
                    day = _utc_day(ts)
                except Exception:  # noqa: BLE001
                    continue
                if (start is not None and ts < start) or (end is not None and ts > end):
                    continue
                counts[day] = counts.get(day, 0) + 1
    return counts


DEFAULT_PARAMS = {
    "interval_s": 60.0,
    "min_days": 30.0,
    "min_availability": 0.99,
    "min_aggregator": 0.95,
    "planned_window_max_s": 1800.0,
    "from_ts": None,
    "to_ts": None,
    "recorded_by_day": None,
    "unparseable_lines": 0,
}


def _is_up(s):
    return bool(s.get("reachable")) and s.get("healthz") == 200


def _merge(intervals):
    out = []
    for a, b in sorted(intervals):
        if b <= a:
            continue
        if out and a <= out[-1][1]:
            out[-1][1] = max(out[-1][1], b)
        else:
            out.append([a, b])
    return out


def _overlap(a, b, merged):
    total = 0.0
    for x, y in merged:
        lo, hi = max(a, x), min(b, y)
        if hi > lo:
            total += hi - lo
    return total


def _iso(ts):
    if ts is None:
        return None
    return datetime.fromtimestamp(ts, tz=timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _counter_delta(prev_map, cur_map):
    total = 0.0
    per = {}
    for k, v in (cur_map or {}).items():
        p = (prev_map or {}).get(k)
        if p is None or v is None:
            continue
        d = v - p
        if d > 0:
            per[k] = d
            total += d
    return total, per


def _fmt_num(x):
    return "{:g}".format(x)


def summarize(records, params=None):
    """Pure: ledger records -> summary dict both output formats render from."""
    p = dict(DEFAULT_PARAMS)
    p.update(params or {})
    interval = float(p["interval_s"])
    gap_limit = UNOBSERVED_GAP_FACTOR * interval
    warnings = []

    recs = sorted(records, key=lambda r: r["ts"])
    samples = [r for r in recs if r.get("kind") == "sample"]
    notes = [r for r in recs if r.get("kind") == "note"]
    recoveries_all = [r for r in recs if r.get("kind") == "recovery"]
    if not samples:
        raise UsageError("ledger holds no sample records")

    # --- span (D-A window) ---
    if p["from_ts"] is not None:
        start = float(p["from_ts"])
    else:
        starts = [n["ts"] for n in notes if n.get("note") == "start"
                  and (p["to_ts"] is None or n["ts"] <= p["to_ts"])]
        if starts:
            start = starts[-1]
        else:
            start = samples[0]["ts"]
            warnings.append("no start note: span starts at the first sample (%s)" % _iso(start))
    if p["to_ts"] is not None:
        end = float(p["to_ts"])
    else:
        ends = [n["ts"] for n in notes if n.get("note") == "end" and n["ts"] >= start]
        if ends:
            end = ends[0]
        else:
            end = samples[-1]["ts"]
            warnings.append("no end note: span ends at the last sample (%s)" % _iso(end))
    if end <= start:
        raise UsageError("empty evaluation window (%s .. %s)" % (_iso(start), _iso(end)))
    span = end - start
    span_days = span / 86400.0

    in_win = lambda t: start <= t <= end  # noqa: E731
    win_samples = [s for s in samples if in_win(s["ts"])]
    win_notes = [n for n in notes if in_win(n["ts"])]
    win_recoveries = [r for r in recoveries_all if in_win(r["ts"])]

    # --- planned windows (D-E) ---
    # From the note until the node is healthy again, capped. "Again": the
    # first up sample after the restart was observed (a not-up sample or a
    # start_time change), so a sample taken between writing the note and
    # actually restarting does not close the window early.
    planned = []
    for n in notes:
        if n.get("note") != "planned":
            continue
        t = n["ts"]
        cap = t + float(p["planned_window_max_s"])
        before = [s.get("start_time") for s in samples if s["ts"] <= t and s.get("start_time") is not None]
        st_before = before[-1] if before else None
        w_end = cap
        went_down = False
        for s in samples:
            if s["ts"] <= t:
                continue
            if s["ts"] >= cap:
                break
            if not _is_up(s):
                went_down = True
            elif went_down or (s.get("start_time") is not None and s.get("start_time") != st_before):
                w_end = s["ts"]
                break
        lo, hi = max(t, start), min(w_end, end)
        if hi > lo:
            planned.append((lo, hi))
    planned_merged = _merge(planned)
    planned_excluded = sum(b - a for a, b in planned_merged)

    # --- interval walk (D-A, D-F) ---
    up_s = down_s = unobserved_s = counter_down_s = 0.0

    def account(a, b, state, extra_down=0.0):
        nonlocal up_s, down_s, unobserved_s, counter_down_s
        a, b = max(a, start), min(b, end)
        if b <= a:
            return
        length = (b - a) - _overlap(a, b, planned_merged)
        if length <= 0:
            return
        if state == "up":
            moved = min(extra_down, length)
            up_s += length - moved
            down_s += moved
            counter_down_s += moved
        elif state == "down":
            down_s += length
        else:
            unobserved_s += length

    first, last = samples[0], samples[-1]
    if first["ts"] > start:
        account(start, first["ts"],
                "unobserved" if first["ts"] - start > gap_limit else ("up" if _is_up(first) else "down"))
    for a, b in zip(samples, samples[1:]):
        if b["ts"] < start or a["ts"] > end:
            continue
        if b["ts"] - a["ts"] > gap_limit:
            account(a["ts"], b["ts"], "unobserved")
            continue
        sa, sb = a.get("start_time"), b.get("start_time")
        if sa is not None and sb is not None and sa != sb:
            # The process restarted inside (a.ts, b.ts]: it was down from
            # some point after a until the new process started at sb, and
            # that point is unobserved, so count all of (a.ts, sb] as down
            # (D-A). Only (sb, b.ts] is observed, by b. A start time outside
            # the interval cannot be placed, so the whole interval is down.
            if a["ts"] < sb < b["ts"]:
                account(a["ts"], sb, "down")
                account(sb, b["ts"], "up" if _is_up(b) else "down")
            else:
                account(a["ts"], b["ts"], "down")
            continue
        extra = 0.0
        if _is_up(a) and sa is not None and sa == sb:
            extra, _ = _counter_delta(a.get("source_down_s"), b.get("source_down_s"))
        account(a["ts"], b["ts"], "up" if _is_up(a) else "down", extra)
    if last["ts"] < end:
        account(last["ts"], end,
                "unobserved" if end - last["ts"] > gap_limit else ("up" if _is_up(last) else "down"))
    denom = span - planned_excluded
    availability = (up_s / denom) if denom > 0 else 0.0

    # --- restarts, counters, daily (walk reachable samples) ---
    reach = [s for s in samples if s.get("reachable") and s.get("start_time") is not None]
    # A pair (a, b) covers (a.ts, b.ts]; the baseline is the last reachable
    # sample at or before the window start.
    baseline = None
    for s in reach:
        if s["ts"] <= start:
            baseline = s
    reach_win = [s for s in reach if start < s["ts"] <= end]
    seq = ([baseline] if baseline is not None else []) + reach_win
    pairs = list(zip(seq, seq[1:]))
    restarts = []
    days = {}

    def day_entry(day):
        return days.setdefault(day, {"day": day, "spots_emitted": 0, "lat_sum": 0.0,
                                     "lat_count": 0, "rss": []})

    outages_total = 0.0
    outage_down_total = 0.0
    outages_by_source = {}
    # A pair's spots fell somewhere in (a.ts, b.ts], or (b.start_time, b.ts]
    # after a restart. Only a pair inside one UTC day credits that day; one
    # that crosses midnight cannot say which side they fell on, so they are
    # "unplaced" (D-C below).
    spots_unplaced = 0
    for a, b in pairs:
        same = a["start_time"] == b["start_time"]
        since = a["ts"] if same else min(max(a["ts"], b["start_time"]), b["ts"])
        in_day = _utc_day(since) == _utc_day(b["ts"])
        d = day_entry(_utc_day(b["ts"])) if in_day else None
        if not same:
            lo, hi = a["ts"] - RESTART_ATTRIBUTION_LOOKBACK_S, b["ts"]
            if any(n.get("note") == "planned" and lo < n["ts"] <= hi for n in notes):
                cls = "planned"
            elif any(lo < r["ts"] <= hi for r in recoveries_all):
                cls = "automated"
            else:
                cls = "unexplained"
            restarts.append({"ts": b["ts"], "at": _iso(b["ts"]), "start_time": b["start_time"],
                             "previous_start_time": a["start_time"], "class": cls})
        bs, as_ = b.get("spots_total"), a.get("spots_total")
        if bs is not None:
            if same and as_ is not None:
                delta = bs - as_ if bs >= as_ else bs
            else:
                delta = bs
            if in_day:
                d["spots_emitted"] += int(delta)
            else:
                spots_unplaced += int(delta)
        bsum, bcnt = b.get("decode_latency_sum"), b.get("decode_latency_count")
        asum, acnt = a.get("decode_latency_sum"), a.get("decode_latency_count")
        if in_day and same and None not in (bsum, bcnt, asum, acnt) and bcnt > acnt:
            d["lat_sum"] += bsum - asum
            d["lat_count"] += bcnt - acnt
        if same:
            n_out, per_out = _counter_delta(a.get("source_outages"), b.get("source_outages"))
            n_down, per_down = _counter_delta(a.get("source_down_s"), b.get("source_down_s"))
            outages_total += n_out
            outage_down_total += n_down
            for k, v in per_out.items():
                e = outages_by_source.setdefault(k, {"outages": 0, "down_s": 0.0})
                e["outages"] += int(v)
            for k, v in per_down.items():
                e = outages_by_source.setdefault(k, {"outages": 0, "down_s": 0.0})
                e["down_s"] += v
    for s in win_samples:
        if s.get("rss_kb") is not None:
            day_entry(_utc_day(s["ts"]))["rss"].append(s["rss_kb"])

    # full UTC days inside the span (D-C)
    first_midnight = math.ceil(start / 86400.0) * 86400.0
    full_days = []
    t = first_midnight
    while t + 86400.0 <= end:
        full_days.append(_utc_day(t))
        t += 86400.0
    for day in full_days:
        day_entry(day)
    recorded = p["recorded_by_day"]
    daily = []
    for day in sorted(days):
        e = days[day]
        rss = sorted(e["rss"])
        row = {
            "day": day,
            "full_day": day in full_days,
            "spots_emitted": e["spots_emitted"],
            "spots_recorded": (recorded or {}).get(day, 0) if recorded is not None else None,
            "mean_decode_latency_s": (e["lat_sum"] / e["lat_count"]) if e["lat_count"] else None,
            "median_rss_kb": rss[len(rss) // 2] if rss else None,
        }
        daily.append(row)
    # The archive's exact timestamps also prove a day spotted: they place
    # unplaced spots, and spots emitted after their timestamp's midnight.
    zero_days = [r["day"] for r in daily if r["full_day"] and r["spots_emitted"] < 1
                 and (r["spots_recorded"] or 0) < 1]
    spots_emitted_total = sum(r["spots_emitted"] for r in daily) + spots_unplaced
    if zero_days and spots_unplaced and recorded is None:
        warnings.append(
            "D-C: %d spots fell in sample intervals that cross UTC midnight and count for no "
            "day; --spots-dir places them by timestamp" % spots_unplaced)

    # aggregator (D-B)
    reachable_win = [s for s in win_samples if s.get("reachable")]
    agg_ok = sum(1 for s in reachable_win if (s.get("telnet_clients") or 0) >= 1)
    aggregator = (agg_ok / len(reachable_win)) if reachable_win else 0.0

    manual = [n for n in win_notes if n.get("note") == "manual"]

    # builds
    builds = []
    seen = set()
    for s in win_samples:
        sha = s.get("git_sha")
        if sha and sha not in seen:
            seen.add(sha)
            builds.append({"git_sha": sha, "version": s.get("version"), "first_seen": _iso(s["ts"])})

    # integrity
    missing_counts = {}
    for s in win_samples:
        for m in s.get("missing") or []:
            missing_counts[m] = missing_counts.get(m, 0) + 1
    recorded_total = None
    coverage = None
    if recorded is not None:
        recorded_total = sum(v for k, v in recorded.items())
        if spots_emitted_total > 0:
            coverage = recorded_total / float(spots_emitted_total)
            if coverage < RECORDED_COVERAGE_WARN:
                warnings.append(
                    "spot archive coverage %.1f%% (recorded %d / emitted %d) is below %.0f%%: "
                    "record-spots missed spots (informational, not a gate)"
                    % (coverage * 100, recorded_total, spots_emitted_total, RECORDED_COVERAGE_WARN * 100))

    criteria = [
        {"id": "D-A", "name": "Availability", "value": availability,
         "threshold": float(p["min_availability"]), "pass": availability >= float(p["min_availability"])},
        {"id": "D-B", "name": "Aggregator connected", "value": aggregator,
         "threshold": float(p["min_aggregator"]), "pass": aggregator >= float(p["min_aggregator"])},
        {"id": "D-C", "name": "Spots every full UTC day", "value": len(full_days) - len(zero_days),
         "threshold": len(full_days), "pass": not zero_days, "zero_spot_days": zero_days},
        {"id": "D-D", "name": "Manual interventions", "value": len(manual),
         "threshold": 0, "pass": not manual},
    ]
    in_progress = span_days < float(p["min_days"])
    progress = "IN PROGRESS: %.1f/%s days" % (span_days, _fmt_num(float(p["min_days"])))
    if not all(c["pass"] for c in criteria):
        verdict = "FAIL"
    elif in_progress:
        verdict = "IN PROGRESS"
    else:
        verdict = "PASS"

    return {
        "verdict": verdict,
        "in_progress": in_progress,
        "progress": progress,
        "criteria": criteria,
        "params": {k: p[k] for k in ("interval_s", "min_days", "min_availability",
                                      "min_aggregator", "planned_window_max_s")},
        "window": {"start": start, "end": end, "start_iso": _iso(start), "end_iso": _iso(end),
                   "span_s": span, "span_days": span_days},
        "availability": availability,
        "aggregator_fraction": aggregator,
        "up_s": up_s,
        "down_s": down_s,
        "unobserved_s": unobserved_s,
        "counter_down_s": counter_down_s,
        "planned_excluded_s": planned_excluded,
        "planned_windows": [{"start": _iso(a), "end": _iso(b), "s": b - a} for a, b in planned_merged],
        "restarts": restarts,
        "recoveries": [{"at": _iso(r["ts"]), "ts": r["ts"], "cmd": r.get("cmd"), "exit": r.get("exit"),
                        "bad_for_s": r.get("bad_for_s"), "error": r.get("error")} for r in win_recoveries],
        "source_outages": {"outages": int(outages_total), "down_s": outage_down_total,
                           "by_source": outages_by_source},
        "daily": daily,
        "spots_emitted_total": spots_emitted_total,
        "spots_unplaced_total": spots_unplaced,
        "spots_recorded_total": recorded_total,
        "recorded_coverage": coverage,
        "builds": builds,
        "notes": [{"at": _iso(n["ts"]), "ts": n["ts"], "note": n.get("note"), "reason": n.get("reason")}
                  for n in win_notes],
        "manual_interventions": len(manual),
        "integrity": {"unparseable_lines": int(p["unparseable_lines"]),
                      "samples": len(win_samples),
                      "reachable_samples": len(reachable_win),
                      "missing_metrics": missing_counts},
        "warnings": warnings,
    }


def _pct(x):
    return "%.2f%%" % (x * 100.0)


def render_markdown(s):
    out = []
    w = out.append
    w("# manta field-node report (MAN-96)")
    w("")
    w("Thresholds: %s" % DECISION_DOC)
    w("")
    w("## Verdict")
    w("")
    overall = s["progress"] if s["verdict"] == "IN PROGRESS" else s["verdict"]
    w("**Overall: %s**" % overall)
    w("")
    for c in s["criteria"]:
        label = "%s (%s)" % ("PASS" if c["pass"] else "FAIL", c["id"])
        if c["id"] in ("D-A", "D-B"):
            extra = " of reachable samples" if c["id"] == "D-B" else ""
            w("- %s: %s%s (min %s): %s" % (c["name"], _pct(c["value"]), extra, _pct(c["threshold"]), label))
        elif c["id"] == "D-C":
            z = c["zero_spot_days"]
            w("- %s: %d/%d full UTC days with >= 1 spot%s: %s" % (
                c["name"], c["value"], c["threshold"],
                (" (zero-spot days: %s)" % ", ".join(z)) if z else "", label))
        else:
            w("- %s: %d: %s" % (c["name"], c["value"], label))
    span_label = s["progress"] if s["in_progress"] else "PASS (span)"
    w("- Span: %.1f/%s days: %s" % (s["window"]["span_days"], _fmt_num(s["params"]["min_days"]), span_label))
    w("")
    w("## Window")
    w("")
    win = s["window"]
    w("- From %s to %s (%.1f days, %.0f s)" % (win["start_iso"], win["end_iso"], win["span_days"], win["span_s"]))
    w("- Up %.0f s, down %.0f s (of which %.0f s from source-outage counters), unobserved %.0f s"
      % (s["up_s"], s["down_s"], s["counter_down_s"], s["unobserved_s"]))
    w("- Planned-restart windows excluded: %.0f s" % s["planned_excluded_s"])
    w("- Interval %s s; a gap > %s s is unobserved (D-F)" % (
        _fmt_num(s["params"]["interval_s"]), _fmt_num(UNOBSERVED_GAP_FACTOR * s["params"]["interval_s"])))
    w("")
    w("## Restarts")
    w("")
    by_cls = {"planned": [], "automated": [], "unexplained": []}
    for r in s["restarts"]:
        by_cls[r["class"]].append(r)
    w("Planned restarts: %d" % len(by_cls["planned"]))
    for r in by_cls["planned"]:
        w("- %s" % r["at"])
    for pw in s["planned_windows"]:
        w("- planned window %s .. %s (%.0f s excluded)" % (pw["start"], pw["end"], pw["s"]))
    w("")
    w("Automated (watchdog) restarts: %d" % len(by_cls["automated"]))
    for r in by_cls["automated"]:
        w("- %s" % r["at"])
    w("")
    w("Unexplained restarts (classify from `journalctl -u manta-field`): %d" % len(by_cls["unexplained"]))
    for r in by_cls["unexplained"]:
        w("- %s" % r["at"])
    w("")
    w("## Watchdog recoveries")
    w("")
    w("Recoveries: %d (automated; downtime counts against D-A, not D-D)" % len(s["recoveries"]))
    for r in s["recoveries"]:
        w("- %s cmd=%s exit=%s bad_for_s=%s%s" % (
            r["at"], " ".join(r["cmd"] or []), r["exit"], r["bad_for_s"],
            (" error=%s" % r["error"]) if r["error"] else ""))
    w("")
    w("## Source outages")
    w("")
    so = s["source_outages"]
    w("Source outages: %d, down %.0f s (from manta_source_outages_total / manta_source_down_seconds_total)"
      % (so["outages"], so["down_s"]))
    for k in sorted(so["by_source"]):
        e = so["by_source"][k]
        w("- %s: %d outages, %.0f s down" % (k, e["outages"], e["down_s"]))
    w("")
    w("## Daily")
    w("")
    w("| UTC day | full | spots emitted | spots recorded | mean decode latency (s) | median RSS (kB) |")
    w("|---|---|---|---|---|---|")
    for r in s["daily"]:
        w("| %s | %s | %d | %s | %s | %s |" % (
            r["day"], "yes" if r["full_day"] else "no", r["spots_emitted"],
            "-" if r["spots_recorded"] is None else r["spots_recorded"],
            "-" if r["mean_decode_latency_s"] is None else "%.4f" % r["mean_decode_latency_s"],
            "-" if r["median_rss_kb"] is None else r["median_rss_kb"]))
    w("")
    w("Spots emitted: %d (%d in sample intervals that cross UTC midnight, credited to no day)"
      % (s["spots_emitted_total"], s["spots_unplaced_total"]))
    if s["spots_recorded_total"] is not None:
        cov = s["recorded_coverage"]
        w("Spots recorded: %d (coverage %s)" % (s["spots_recorded_total"], "-" if cov is None else _pct(cov)))
    w("")
    w("## Builds")
    w("")
    for b in s["builds"]:
        w("- %s (version %s), first seen %s" % (b["git_sha"], b["version"], b["first_seen"]))
    if not s["builds"]:
        w("- none recorded")
    w("")
    w("## Notes")
    w("")
    for n in s["notes"]:
        w("- %s %s: %s" % (n["at"], n["note"], n["reason"]))
    if not s["notes"]:
        w("- none")
    w("")
    w("## Ledger integrity")
    w("")
    integ = s["integrity"]
    w("- unparseable ledger lines: %d" % integ["unparseable_lines"])
    w("- samples in window: %d (%d reachable)" % (integ["samples"], integ["reachable_samples"]))
    for m in sorted(integ["missing_metrics"]):
        w("- required metric %s missing in %d samples" % (m, integ["missing_metrics"][m]))
    for msg in s["warnings"]:
        w("- WARNING: %s" % msg)
    return "\n".join(out) + "\n"


# A NaN compares false against everything and inf/huge values overflow the gap
# arithmetic, so either would turn a failing ledger into a PASS. Reject them up front.
MAX_REPORT_INTERVAL_S = 86400.0


def validate_report_thresholds(args):
    def finite(flag, v):
        if not math.isfinite(v):
            raise UsageError("%s must be a finite number" % flag)
    finite("--interval-s", args.interval_s)
    if not 0 < args.interval_s <= MAX_REPORT_INTERVAL_S:
        raise UsageError("--interval-s must be > 0 and <= %s" % _fmt_num(MAX_REPORT_INTERVAL_S))
    finite("--min-days", args.min_days)
    if args.min_days <= 0:
        raise UsageError("--min-days must be > 0")
    for flag, v in (("--min-availability", args.min_availability), ("--min-aggregator", args.min_aggregator)):
        finite(flag, v)
        if not 0 <= v <= 1:
            raise UsageError("%s must be between 0 and 1" % flag)
    finite("--planned-window-max-s", args.planned_window_max_s)
    if args.planned_window_max_s < 0:
        raise UsageError("--planned-window-max-s must be >= 0")


def cmd_report(args):
    params = {
        "interval_s": args.interval_s,
        "min_days": args.min_days,
        "min_availability": args.min_availability,
        "min_aggregator": args.min_aggregator,
        "planned_window_max_s": args.planned_window_max_s,
        "from_ts": parse_iso_ts(args.from_) if args.from_ else None,
        "to_ts": parse_iso_ts(args.to) if args.to else None,
    }
    validate_report_thresholds(args)
    records, bad = load_ledger(args.ledger)
    params["unparseable_lines"] = bad
    if args.spots_dir:
        params["recorded_by_day"] = load_recorded_spots(args.spots_dir, params["from_ts"], params["to_ts"])
    summary = summarize(records, params)
    if args.spots_dir and (params["from_ts"] is None or params["to_ts"] is None):
        # Re-count within the resolved window (start/end notes).
        params["recorded_by_day"] = load_recorded_spots(
            args.spots_dir, summary["window"]["start"], summary["window"]["end"])
        summary = summarize(records, params)
    if args.format == "json":
        sys.stdout.write(json.dumps(summary, indent=2, sort_keys=True) + "\n")
    else:
        sys.stdout.write(render_markdown(summary))
    return 0 if summary["verdict"] == "PASS" else 1


# ---------------------------------------------------------------------------
# CLI


def build_parser():
    ap = argparse.ArgumentParser(
        prog="field-node.py",
        description="MAN-96 secondary-skimmer field-node evidence kit: ledger sampler + watchdog, "
                    "operator notes, spot recorder, and the 30-day report. Decisions: %s; "
                    "procedure: %s." % (DECISION_DOC, RUNBOOK),
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
    )
    sub = ap.add_subparsers(dest="cmd", metavar="{sample,note,record-spots,report}")
    sub.required = True

    sp = sub.add_parser("sample", help="append one /healthz + /metrics sample to the ledger (or loop)",
                        formatter_class=argparse.ArgumentDefaultsHelpFormatter)
    sp.add_argument("--metrics-url", required=True,
                    help="base http:// URL of the node's metrics listener, e.g. http://127.0.0.1:7302")
    sp.add_argument("--ledger", required=True, help="ledger JSON Lines file to append to")
    sp.add_argument("--loop", action="store_true", help="keep sampling every --interval-s")
    sp.add_argument("--interval-s", type=float, default=60.0, help="sample interval in seconds (D-F)")
    sp.add_argument("--rss-unit", default=None,
                    help="systemd unit whose MainPID's VmRSS to record (Linux only)")
    sp.add_argument("--recover-cmd", default=None,
                    help="watchdog recovery command (split with shlex, run without a shell) (D-G)")
    sp.add_argument("--recover-after-s", type=float, default=600.0,
                    help="run --recover-cmd after the node has been continuously bad this long")
    sp.add_argument("--recover-cooldown-s", type=float, default=1800.0,
                    help="minimum seconds between recoveries")
    sp.add_argument("--recover-max-per-day", type=int, default=6, help="maximum recoveries per UTC day")
    sp.add_argument("--recover-timeout-s", type=float, default=120.0,
                    help="kill --recover-cmd after this many seconds")

    np_ = sub.add_parser("note", help="append an operator note (start/planned/manual/end) to the ledger",
                         formatter_class=argparse.ArgumentDefaultsHelpFormatter)
    np_.add_argument("--ledger", required=True, help="ledger JSON Lines file to append to")
    np_.add_argument("--kind", required=True, choices=["start", "planned", "manual", "end"],
                     help="start: start the 30-day clock; planned: written BEFORE a planned restart (D-E); "
                          "manual: any human intervention, fails D-D; end: end of the run")
    np_.add_argument("--reason", required=True, help="free-text reason")

    rp_ = sub.add_parser("record-spots", help="archive the node's spots from its JSON Lines port",
                         formatter_class=argparse.ArgumentDefaultsHelpFormatter)
    rp_.add_argument("--json-addr", required=True, help="HOST:PORT of the node's JSON Lines listener")
    rp_.add_argument("--out-dir", required=True, help="directory for spots-YYYY-MM-DD.jsonl files")

    rep = sub.add_parser("report",
        help="turn the ledger into the scenario-1 PASS/FAIL report",
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
        description="Evaluate a field-node ledger against MAN-96 scenario 1. Thresholds and rationale: "
                    "%s (D-A availability, D-B Aggregator connected, D-C a spot every full UTC day, "
                    "D-D zero manual interventions, D-E planned restarts, D-F unobserved gaps). "
                    "Exit 0 PASS, 1 FAIL or in progress, 2 usage/input error." % DECISION_DOC,
    )
    rep.add_argument("--ledger", required=True, help="ledger JSON Lines file written by `sample`/`note`")
    rep.add_argument("--spots-dir", default=None,
                     help="record-spots output directory, for recorded-vs-emitted coverage (informational)")
    rep.add_argument("--from", dest="from_", default=None, metavar="ISO",
                     help="ISO-8601 window start (default: the last `start` note, else the first sample)")
    rep.add_argument("--to", default=None, metavar="ISO",
                     help="ISO-8601 window end (default: the first `end` note after the start, else the last sample)")
    rep.add_argument("--interval-s", type=float, default=60.0,
                     help="the ledger's sample interval; gaps > 3x are unobserved and count as down (D-F)")
    rep.add_argument("--min-days", type=float, default=30.0,
                     help="span required for a verdict; shorter spans report IN PROGRESS")
    rep.add_argument("--min-availability", type=float, default=0.99,
                     help="D-A: minimum fraction of the span (minus planned windows) with /healthz 200")
    rep.add_argument("--min-aggregator", type=float, default=0.95,
                     help="D-B: minimum fraction of reachable samples with >= 1 telnet client")
    rep.add_argument("--planned-window-max-s", type=float, default=1800.0,
                     help="D-E: cap on each planned-restart window excluded from availability")
    rep.add_argument("--format", choices=["markdown", "json"], default="markdown", help="output format")
    return ap


def main(argv=None):
    ap = build_parser()
    args = ap.parse_args(argv)
    try:
        if args.cmd == "sample":
            return cmd_sample(args)
        if args.cmd == "note":
            return cmd_note(args)
        if args.cmd == "record-spots":
            return cmd_record_spots(args)
        if args.cmd == "report":
            return cmd_report(args)
    except UsageError as e:
        print("field-node.py %s: error: %s" % (args.cmd, e), file=sys.stderr)
        return 2
    except KeyboardInterrupt:
        return 130
    ap.error("unknown subcommand")
    return 2


if __name__ == "__main__":
    sys.exit(main())
