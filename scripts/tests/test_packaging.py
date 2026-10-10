"""Tests for the MAN-268 macOS service kit in packaging/launchd.

The plists are parsed with plistlib and checked value by value, including the daemon's
ExitTimeOut against manta's shutdown drain constants in crates/manta-cli/src/main.rs.
rotate-log.sh runs for real on temporary files; create-service-account.sh runs against stub
dscl/dscacheutil/id commands that keep a modeled directory in a JSON file and log every call.
The stubs check script behavior, not native directory services. Shell tests need /bin/sh and
are skipped on Windows, where CI only exercises the portable archives.

stdlib unittest only. Run: python3 -m unittest scripts.tests.test_packaging -v
"""
import glob
import json
import os
import plistlib
import re
import shlex
import stat
import subprocess
import sys
import tarfile
import tempfile
import textwrap
import unittest

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
LAUNCHD = os.path.join(ROOT, "packaging", "launchd")
DAEMON_PLIST = os.path.join(LAUNCHD, "com.hagaletechnologies.manta.plist")
ROTATE_PLIST = os.path.join(LAUNCHD, "com.hagaletechnologies.manta-logrotate.plist")
ACCOUNT_SH = os.path.join(LAUNCHD, "create-service-account.sh")
ROTATE_SH = os.path.join(LAUNCHD, "rotate-log.sh")
MAIN_RS = os.path.join(ROOT, "crates", "manta-cli", "src", "main.rs")
SH = "/bin/sh"
NOT_POSIX = os.name == "nt"
NOT_POSIX_REASON = "POSIX shell helpers; Windows CI only checks the portable archives"

MIB = 1024 * 1024
ROTATE_ABOVE = 10 * MIB
ROTATE_KEEP = 1 * MIB
STOP_MARGIN_SECS = 5
ACCOUNT = "_manta"
SUCCESS = "manta service account ready: _manta\n"


def load_plist(path):
    with open(path, "rb") as f:
        return plistlib.load(f)


def typed(value):
    """Value with its exact type at every level, so <true/> never equals <integer>1</integer>."""
    if isinstance(value, dict):
        return {k: typed(v) for k, v in value.items()}
    if isinstance(value, list):
        return [typed(v) for v in value]
    return (type(value).__name__, value)


def rust_secs(pattern, what):
    """The N in one `from_secs(N)` code match in main.rs; comment lines are ignored."""
    with open(MAIN_RS, encoding="utf-8") as f:
        code = "\n".join(line for line in f.read().splitlines() if not line.lstrip().startswith("//"))
    found = re.findall(pattern, code)
    if len(found) != 1:
        raise AssertionError("expected one %s in %s, found %r" % (what, MAIN_RS, found))
    return int(found[0])


class LaunchdPlistTests(unittest.TestCase):
    """packaging/launchd/*.plist against the plan's reviewed contract."""

    @classmethod
    def setUpClass(cls):
        cls.daemon = load_plist(DAEMON_PLIST)
        cls.rotate = load_plist(ROTATE_PLIST)

    def test_daemon_plist_is_the_reviewed_contract(self):
        expected = {
            "Label": "com.hagaletechnologies.manta",
            "ProgramArguments": ["/usr/local/bin/manta", "run", "--config", "/usr/local/etc/manta/manta.toml"],
            "UserName": "_manta",
            "GroupName": "_manta",
            "RunAtLoad": True,
            "KeepAlive": True,
            "ThrottleInterval": 10,
            "ExitTimeOut": 60,
            "StandardOutPath": "/var/log/manta/manta.log",
            "StandardErrorPath": "/var/log/manta/manta.log",
        }
        self.assertEqual(typed(self.daemon), typed(expected))

    def test_rotation_plist_is_the_reviewed_contract(self):
        expected = {
            "Label": "com.hagaletechnologies.manta-logrotate",
            "ProgramArguments": ["/bin/sh", "/usr/local/libexec/manta-rotate-log.sh", "/var/log/manta/manta.log"],
            "UserName": "_manta",
            "GroupName": "_manta",
            "RunAtLoad": True,
            "StartInterval": 60,
            "KeepAlive": False,
            "StandardOutPath": "/dev/null",
            "StandardErrorPath": "/dev/null",
        }
        self.assertEqual(typed(self.rotate), typed(expected))

    def test_labels_match_file_names(self):
        for path, plist in ((DAEMON_PLIST, self.daemon), (ROTATE_PLIST, self.rotate)):
            with self.subTest(path=os.path.basename(path)):
                self.assertEqual(plist["Label"] + ".plist", os.path.basename(path))

    def test_daemon_runs_the_binary_directly_with_the_installed_config(self):
        args = self.daemon["ProgramArguments"]
        self.assertEqual(args[0], "/usr/local/bin/manta")
        self.assertEqual(args[1:3], ["run", "--config"])
        self.assertTrue(os.path.isabs(args[3]), args)
        self.assertNotIn("Program", self.daemon)

    def test_daemon_exit_timeout_covers_the_drain_budget(self):
        drain = rust_secs(
            r"const\s+SHUTDOWN_DRAIN_DEADLINE\s*:\s*[\w:]+\s*=\s*[\w:]*from_secs\((\d+)\)\s*;",
            "SHUTDOWN_DRAIN_DEADLINE")
        cutoff = rust_secs(r"\brt\.shutdown_timeout\(\s*[\w:]*from_secs\((\d+)\)\s*\)", "rt.shutdown_timeout")
        self.assertIsInstance(self.daemon["ExitTimeOut"], int)
        self.assertGreaterEqual(
            self.daemon["ExitTimeOut"], drain + cutoff + STOP_MARGIN_SECS,
            "ExitTimeOut must cover the %ds drain, the %ds runtime cutoff and %ds of margin"
            % (drain, cutoff, STOP_MARGIN_SECS))

    def test_daemon_stdout_and_stderr_share_one_log(self):
        self.assertEqual(self.daemon["StandardOutPath"], self.daemon["StandardErrorPath"])
        self.assertTrue(os.path.isabs(self.daemon["StandardOutPath"]))

    def test_rotation_job_trims_the_daemon_log(self):
        shell, script, log = self.rotate["ProgramArguments"]
        self.assertEqual(shell, "/bin/sh")
        self.assertEqual(script, "/usr/local/libexec/manta-rotate-log.sh")
        self.assertEqual(log, self.daemon["StandardOutPath"])
        self.assertEqual(log, self.daemon["StandardErrorPath"])

    def test_rotation_job_runs_once_a_minute_without_a_log_of_its_own(self):
        self.assertIs(self.rotate["KeepAlive"], False)
        self.assertEqual(typed(self.rotate["StartInterval"]), typed(60))
        self.assertEqual(self.rotate["StandardOutPath"], "/dev/null")
        self.assertEqual(self.rotate["StandardErrorPath"], "/dev/null")

    def test_both_jobs_run_as_the_service_account(self):
        for plist in (self.daemon, self.rotate):
            with self.subTest(label=plist["Label"]):
                self.assertEqual((plist["UserName"], plist["GroupName"]), (ACCOUNT, ACCOUNT))


@unittest.skipIf(NOT_POSIX, NOT_POSIX_REASON)
class ShellSyntaxTests(unittest.TestCase):
    def test_scripts_parse_with_sh_n(self):
        for script in (ACCOUNT_SH, ROTATE_SH):
            with self.subTest(script=os.path.basename(script)):
                proc = subprocess.run([SH, "-n", script], capture_output=True, text=True, timeout=30)
                self.assertEqual(proc.returncode, 0, proc.stderr)
                self.assertEqual(proc.stderr, "")

    def test_scripts_are_executable(self):
        for script in (ACCOUNT_SH, ROTATE_SH):
            with self.subTest(script=os.path.basename(script)):
                self.assertEqual(stat.S_IMODE(os.stat(script).st_mode), 0o755)


def log_bytes(size):
    """`size` bytes of 16-byte numbered lines, so any suffix is unique to its position."""
    return b"".join(b"spot %010d\n" % i for i in range(size // 16 + 1))[:size]


def read_bytes(path):
    with open(path, "rb") as f:
        return f.read()


def write_bytes(path, data):
    with open(path, "wb") as f:
        f.write(data)


@unittest.skipIf(NOT_POSIX, NOT_POSIX_REASON)
class RotateLogTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.dir = os.path.join(self._tmp.name, "log dir")
        os.mkdir(self.dir)
        self.log = os.path.join(self.dir, "manta.log")
        self.stubs = os.path.join(self._tmp.name, "stubs")
        os.mkdir(self.stubs)

    def tearDown(self):
        os.chmod(self.dir, 0o755)
        self._tmp.cleanup()

    def rotate(self, *args, cwd=None):
        env = dict(os.environ)
        env["PATH"] = self.stubs + os.pathsep + env.get("PATH", os.defpath)
        return subprocess.run([SH, ROTATE_SH] + list(args), capture_output=True, env=env, cwd=cwd,
                              timeout=120)

    def stub(self, name, body):
        path = os.path.join(self.stubs, name)
        with open(path, "w", encoding="utf-8") as f:
            f.write("#!/bin/sh\n" + textwrap.dedent(body))
        os.chmod(path, 0o755)

    def assert_silent_success(self, proc):
        self.assertEqual((proc.returncode, proc.stdout, proc.stderr), (0, b"", b""))

    def assert_failed(self, proc):
        self.assertNotEqual(proc.returncode, 0, proc)
        self.assertEqual(proc.stdout, b"")
        self.assertNotEqual(proc.stderr, b"")

    def assert_no_scratch(self):
        self.assertEqual(glob.glob(os.path.join(glob.escape(self.dir), ".manta-rotate.*")), [])

    def test_log_at_or_below_threshold_is_left_alone(self):
        for size in (0, 4096, ROTATE_ABOVE):
            with self.subTest(size=size):
                original = log_bytes(size)
                write_bytes(self.log, original)
                inode = os.stat(self.log).st_ino
                self.assert_silent_success(self.rotate(self.log))
                self.assertEqual(os.stat(self.log).st_ino, inode)
                self.assertEqual(read_bytes(self.log), original)
                self.assert_no_scratch()

    def test_oversized_log_keeps_its_final_mebibyte_in_the_same_inode(self):
        for size in (ROTATE_ABOVE + 1, ROTATE_ABOVE + 4096 + 7, 12 * MIB):
            with self.subTest(size=size):
                original = log_bytes(size)
                write_bytes(self.log, original)
                inode = os.stat(self.log).st_ino
                self.assert_silent_success(self.rotate(self.log))
                self.assertEqual(os.stat(self.log).st_ino, inode)
                self.assertEqual(os.path.getsize(self.log), ROTATE_KEEP)
                self.assertEqual(read_bytes(self.log), original[-ROTATE_KEEP:])
                self.assert_no_scratch()

    def test_open_append_descriptor_keeps_writing_to_the_rotated_log(self):
        # launchd opens StandardOutPath with O_APPEND and manta never reopens it.
        original = log_bytes(ROTATE_ABOVE + 12345)
        after = b"written after rotation\n"
        fd = os.open(self.log, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o644)
        try:
            os.write(fd, original)
            self.assert_silent_success(self.rotate(self.log))
            os.write(fd, after)
            self.assertEqual(os.fstat(fd).st_ino, os.stat(self.log).st_ino)
        finally:
            os.close(fd)
        self.assertEqual(os.path.getsize(self.log), ROTATE_KEEP + len(after))
        self.assertEqual(read_bytes(self.log), original[-ROTATE_KEEP:] + after)
        self.assert_no_scratch()

    def test_relative_log_path(self):
        for name in ("manta.log", "-manta.log"):
            with self.subTest(name=name):
                original = log_bytes(ROTATE_ABOVE + 100)
                write_bytes(os.path.join(self.dir, name), original)
                self.assert_silent_success(self.rotate(name, cwd=self.dir))
                self.assertEqual(read_bytes(os.path.join(self.dir, name)), original[-ROTATE_KEEP:])
                self.assert_no_scratch()

    def test_missing_log_is_a_silent_no_op(self):
        self.assert_silent_success(self.rotate(self.log))
        self.assertFalse(os.path.lexists(self.log))
        self.assert_no_scratch()

    def test_unsafe_log_paths_are_refused(self):
        target = os.path.join(self._tmp.name, "target.log")
        original = log_bytes(ROTATE_ABOVE + 100)
        write_bytes(target, original)
        os.symlink(target, self.log)
        dangling = os.path.join(self.dir, "dangling.log")
        os.symlink(os.path.join(self._tmp.name, "absent.log"), dangling)
        directory = os.path.join(self.dir, "directory.log")
        os.mkdir(directory)
        fifo = os.path.join(self.dir, "fifo.log")
        os.mkfifo(fifo)
        for path in (self.log, dangling, directory, fifo):
            with self.subTest(path=os.path.basename(path)):
                self.assert_failed(self.rotate(path))
                self.assert_no_scratch()
        self.assertEqual(read_bytes(target), original)
        self.assertFalse(os.path.lexists(os.path.join(self._tmp.name, "absent.log")))

    def test_needs_exactly_one_argument(self):
        for args in ((), (self.log, self.log)):
            with self.subTest(argc=len(args)):
                proc = self.rotate(*args)
                self.assert_failed(proc)
                self.assertIn(b"usage", proc.stderr)

    def check_failure_leaves_log_whole(self):
        original = log_bytes(ROTATE_ABOVE + 100)
        write_bytes(self.log, original)
        inode = os.stat(self.log).st_ino
        self.assert_failed(self.rotate(self.log))
        self.assertEqual(os.stat(self.log).st_ino, inode)
        self.assertEqual(read_bytes(self.log), original)
        self.assert_no_scratch()

    def test_failing_tail_exits_nonzero_and_leaves_the_log_whole(self):
        # A full-length copy, so only the exit status shows the failure.
        self.stub("tail", "head -c %d /dev/zero\nexit 1\n" % ROTATE_KEEP)
        self.check_failure_leaves_log_whole()

    def test_short_copy_exits_nonzero_and_leaves_the_log_whole(self):
        self.stub("tail", "printf 'short'\nexit 0\n")
        self.check_failure_leaves_log_whole()

    def test_failing_mktemp_exits_nonzero_and_leaves_the_log_whole(self):
        self.stub("mktemp", "echo 'mktemp: injected failure' >&2\nexit 1\n")
        self.check_failure_leaves_log_whole()

    @unittest.skipIf(hasattr(os, "geteuid") and os.geteuid() == 0, "root ignores directory permissions")
    def test_unwritable_directory_exits_nonzero_and_leaves_the_log_whole(self):
        original = log_bytes(ROTATE_ABOVE + 100)
        write_bytes(self.log, original)
        os.chmod(self.dir, 0o555)
        self.assert_failed(self.rotate(self.log))
        os.chmod(self.dir, 0o755)
        self.assertEqual(read_bytes(self.log), original)
        self.assert_no_scratch()


# One program installed as dscl, dscacheutil and id. State: {"euid", "local", "directory",
# "uncached", "id_answers", "fail"}. "local"/"directory" hold {"users"|"groups": {name:
# {attribute: [values]}}}; "directory" has the records only a network node holds. dscacheutil and
# id resolve both, except "uncached" names (a stale cache). "id_answers" maps a name to forced
# `id -u`/`id -g` replies. Each "fail" entry is [tool, *argv prefix] and makes that call exit 1.
STUB = r'''
import json, os, sys

tool = os.path.basename(sys.argv[0])
args = sys.argv[1:]
base = os.environ["MANTA_TEST_STUB_DIR"]
state_path = os.path.join(base, "state.json")
with open(state_path, encoding="utf-8") as f:
    state = json.load(f)
with open(os.path.join(base, "calls.jsonl"), "a", encoding="utf-8") as f:
    f.write(json.dumps([tool] + args) + "\n")
for rule in state["fail"]:
    if rule[0] == tool and args[:len(rule) - 1] == rule[1:]:
        sys.stderr.write("%s: injected failure\n" % tool)
        sys.exit(1)

KINDS = {"/Users": "users", "/Groups": "groups"}
ID_ATTR = {"users": "UniqueID", "groups": "PrimaryGroupID"}


def visible(kind):
    records = dict(state["directory"][kind])
    records.update(state["local"][kind])
    return {k: v for k, v in records.items() if k not in state["uncached"]}


def record(path):
    head, _, name = path.rpartition("/")
    if head not in KINDS or not name:
        sys.stderr.write("stub dscl: unsupported path %s\n" % path)
        sys.exit(64)
    return state["local"][KINDS[head]], name


def dscl():
    if len(args) < 3 or args[0] != ".":
        sys.stderr.write("stub dscl: unsupported %r\n" % args)
        sys.exit(64)
    op, path, rest = args[1], args[2], args[3:]
    if op == "-list":
        kind = KINDS[path]
        for name in sorted(state["local"][kind]):
            print(("%-30s %s" % (name, " ".join(state["local"][kind][name].get(rest[0], [])))).rstrip())
        return
    records, name = record(path)
    if op == "-read":
        if name not in records:
            sys.stderr.write("<dscl_cmd> DS Error: -14136 (eDSRecordNotFound)\n")
            sys.exit(56)
        for attr in rest:
            values = records[name].get(attr)
            if values is None:
                sys.stderr.write("No such key: %s\n" % attr)
                sys.exit(181)
            if any(" " in v for v in values):
                print(attr + ":")
                for v in values:
                    print(" " + v)
            else:
                print(attr + ": " + " ".join(values))
        return
    if op == "-create":
        rec = records.setdefault(name, {})
        if rest:
            rec[rest[0]] = rest[1:]
    # Every other operation is only logged; the tests fail on any of them.
    with open(state_path, "w", encoding="utf-8") as f:
        json.dump(state, f)


def dscacheutil():
    if len(args) != 5 or args[0] != "-q" or args[2] != "-a":
        sys.stderr.write("stub dscacheutil: unsupported %r\n" % args)
        sys.exit(64)
    category, key, value = args[1], args[3], args[4]
    kind = {"user": "users", "group": "groups"}[category]
    for name, rec in sorted(visible(kind).items()):
        number = rec.get(ID_ATTR[kind], [])
        if (key == "name" and name == value) or (key in ("uid", "gid") and value in number):
            print("name: %s\n%s: %s\n" % (name, key if key != "name" else "id", " ".join(number)))


def id_():
    if args == ["-u"]:
        print(state["euid"])
        return
    if len(args) == 2 and args[0] in ("-u", "-g"):
        forced = state["id_answers"].get(args[1], {}).get(args[0])
        if forced is not None:
            print(forced)
            return
        rec = visible("users").get(args[1])
        if rec is None:
            sys.stderr.write("id: %s: no such user\n" % args[1])
            sys.exit(1)
        print(rec["UniqueID" if args[0] == "-u" else "PrimaryGroupID"][0])
        return
    sys.stderr.write("stub id: unsupported %r\n" % args)
    sys.exit(64)


{"dscl": dscl, "dscacheutil": dscacheutil, "id": id_}[tool]()
'''

WRITE_OPS = {"-create", "-append", "-merge", "-change", "-changei", "-delete", "-deletepl", "-passwd"}

SYSTEM_USERS = {
    "root": {"UniqueID": ["0"], "PrimaryGroupID": ["0"], "UserShell": ["/bin/sh"]},
    "_www": {"UniqueID": ["70"], "PrimaryGroupID": ["70"], "UserShell": ["/usr/bin/false"]},
    "_oahd": {"UniqueID": ["441"], "PrimaryGroupID": ["441"], "UserShell": ["/usr/bin/false"]},
}
SYSTEM_GROUPS = {
    "wheel": {"PrimaryGroupID": ["0"]},
    "staff": {"PrimaryGroupID": ["20"]},
    "_www": {"PrimaryGroupID": ["70"]},
    "_oahd": {"PrimaryGroupID": ["441"]},
}


def manta_user(uid, gid, shell="/usr/bin/false", home="/var/empty"):
    return {"UniqueID": [str(uid)], "PrimaryGroupID": [str(gid)], "UserShell": [shell],
            "NFSHomeDirectory": [home], "RealName": ["manta CW skimmer"], "Password": ["*"]}


def manta_group(gid):
    return {"PrimaryGroupID": [str(gid)], "RealName": ["manta CW skimmer"], "Password": ["*"]}


def ids(prefix, numbers, attrs):
    return {"%s%d" % (prefix, n): {a: [str(n)] for a in attrs} for n in numbers}


@unittest.skipIf(NOT_POSIX, NOT_POSIX_REASON)
class CreateServiceAccountTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.base = self._tmp.name
        self.bin = os.path.join(self.base, "bin")
        os.mkdir(self.bin)
        for tool in ("dscl", "dscacheutil", "id"):
            path = os.path.join(self.bin, tool)
            with open(path, "w", encoding="utf-8") as f:
                f.write("#!%s\n%s" % (sys.executable, STUB))
            os.chmod(path, 0o755)

    def tearDown(self):
        self._tmp.cleanup()

    def run_account(self, local_users=None, local_groups=None, dir_users=None, dir_groups=None,
                    euid=0, fail=(), uncached=(), id_answers=None):
        users = dict(SYSTEM_USERS, **(local_users or {}))
        groups = dict(SYSTEM_GROUPS, **(local_groups or {}))
        state = {"euid": euid, "fail": [list(rule) for rule in fail], "uncached": list(uncached),
                 "id_answers": id_answers or {},
                 "local": {"users": users, "groups": groups},
                 "directory": {"users": dir_users or {}, "groups": dir_groups or {}}}
        with open(os.path.join(self.base, "state.json"), "w", encoding="utf-8") as f:
            json.dump(state, f)
        calls_path = os.path.join(self.base, "calls.jsonl")
        if os.path.exists(calls_path):
            os.remove(calls_path)  # one call log per run, also across subTests
        env = dict(os.environ)
        env["PATH"] = self.bin + os.pathsep + env.get("PATH", os.defpath)
        env["MANTA_TEST_STUB_DIR"] = self.base
        proc = subprocess.run([SH, ACCOUNT_SH], capture_output=True, text=True, env=env, timeout=120)
        calls = []
        if os.path.exists(calls_path):
            with open(calls_path, encoding="utf-8") as f:
                calls = [json.loads(line) for line in f]
        with open(os.path.join(self.base, "state.json"), encoding="utf-8") as f:
            final = json.load(f)
        self.assertEqual([c for c in calls if "-delete" in c or "-deletepl" in c], [],
                         "the helper must never delete a record")
        return proc, calls, state["local"], final["local"]

    @staticmethod
    def writes(calls):
        return [c for c in calls if c[0] == "dscl" and len(c) > 2 and c[2] in WRITE_OPS]

    def assert_refused(self, proc, calls, before, after):
        self.assertNotEqual(proc.returncode, 0)
        self.assertEqual(proc.stdout, "")
        self.assertNotEqual(proc.stderr, "")
        self.assertEqual(self.writes(calls), [])
        self.assertEqual(after, before)
        self.assertEqual([c for c in calls if c[0] == "id" and ACCOUNT in c], [],
                         "refused before verifying an account")

    def assert_created(self, after, uid, gid):
        self.assertEqual(after["groups"][ACCOUNT], manta_group(gid))
        self.assertEqual(after["users"][ACCOUNT], dict(manta_user(uid, gid), IsHidden=["1"]))

    def test_fresh_host_creates_the_group_then_the_user(self):
        proc, calls, _, after = self.run_account()
        self.assertEqual((proc.returncode, proc.stdout, proc.stderr), (0, SUCCESS, ""))
        user = after["users"][ACCOUNT]
        uid, gid = int(user["UniqueID"][0]), int(after["groups"][ACCOUNT]["PrimaryGroupID"][0])
        self.assertTrue(400 <= uid <= 499 and 400 <= gid <= 499, (uid, gid))
        self.assert_created(after, uid, gid)
        targets = [c[3] for c in self.writes(calls)]
        self.assertEqual(set(c[2] for c in self.writes(calls)), {"-create"})
        self.assertEqual(targets[0], "/Groups/_manta")
        last_group = max(i for i, t in enumerate(targets) if t == "/Groups/_manta")
        first_user = targets.index("/Users/_manta")
        self.assertLess(last_group, first_user, "group records must all precede the user's")
        last_write = calls.index(self.writes(calls)[-1])
        self.assertIn(["id", "-u", ACCOUNT], calls[last_write:])
        self.assertIn(["id", "-g", ACCOUNT], calls[last_write:])

    def test_fresh_host_ids_avoid_every_existing_id(self):
        _, _, before, after = self.run_account()
        uid = after["users"][ACCOUNT]["UniqueID"]
        gid = after["groups"][ACCOUNT]["PrimaryGroupID"]
        self.assertNotIn(uid, [r.get("UniqueID") for r in before["users"].values()])
        self.assertNotIn(gid, [r.get("PrimaryGroupID") for r in before["groups"].values()])

    def test_ids_taken_locally_or_through_the_directory_are_skipped(self):
        # The local IDs are missing from the directory cache, so only `dscl . -list` shows them;
        # the network IDs only resolve through dscacheutil.
        local_users = ids("_local_user", (400, 401), ("UniqueID", "PrimaryGroupID"))
        local_groups = ids("_local_group", (400,), ("PrimaryGroupID",))
        proc, calls, _, after = self.run_account(
            local_users=local_users, local_groups=local_groups,
            dir_users=ids("net_user", (402,), ("UniqueID", "PrimaryGroupID")),
            dir_groups=ids("net_group", (401,), ("PrimaryGroupID",)),
            uncached=list(local_users) + list(local_groups))
        self.assertEqual((proc.returncode, proc.stdout), (0, SUCCESS), proc.stderr)
        self.assert_created(after, 403, 402)
        self.assertIn(["dscacheutil", "-q", "user", "-a", "uid", "402"], calls)
        self.assertIn(["dscacheutil", "-q", "group", "-a", "gid", "401"], calls)

    def test_existing_matching_account_is_accepted_without_writes(self):
        for uid, gid in ((405, 407), (250, 250)):
            with self.subTest(uid=uid, gid=gid):
                proc, calls, before, after = self.run_account(
                    local_users={ACCOUNT: manta_user(uid, gid)}, local_groups={ACCOUNT: manta_group(gid)})
                self.assertEqual((proc.returncode, proc.stdout, proc.stderr), (0, SUCCESS, ""))
                self.assertEqual(self.writes(calls), [])
                self.assertEqual(after, before)
                self.assertIn(["id", "-u", ACCOUNT], calls)

    def test_incompatible_existing_user_is_refused_untouched(self):
        cases = {
            "login shell": manta_user(405, 405, shell="/bin/zsh"),
            "real home": manta_user(405, 405, home="/Users/manta"),
            "other primary group": manta_user(405, 20),
        }
        for label, user in cases.items():
            with self.subTest(label):
                self.assert_refused(*self.run_account(
                    local_users={ACCOUNT: user}, local_groups={ACCOUNT: manta_group(405)}))

    def test_unreadable_existing_user_is_refused_untouched(self):
        user = manta_user(405, 405)
        del user["UserShell"]
        self.assert_refused(*self.run_account(
            local_users={ACCOUNT: user}, local_groups={ACCOUNT: manta_group(405)}))

    def test_partial_setup_is_refused_untouched(self):
        for label, kwargs in (("group only", {"local_groups": {ACCOUNT: manta_group(405)}}),
                              ("user only", {"local_users": {ACCOUNT: manta_user(405, 405)}})):
            with self.subTest(label):
                proc, calls, before, after = self.run_account(**kwargs)
                self.assert_refused(proc, calls, before, after)
                self.assertIn("partial", proc.stderr)

    def test_name_held_only_by_a_directory_service_is_refused(self):
        for label, kwargs in (("user", {"dir_users": {ACCOUNT: manta_user(900, 900)}}),
                              ("group", {"dir_groups": {ACCOUNT: manta_group(900)}})):
            with self.subTest(label):
                self.assert_refused(*self.run_account(**kwargs))

    def test_exhausted_id_range_is_refused(self):
        half = range(400, 450), range(450, 500)
        cases = {
            "user IDs": {"local_users": ids("_u", half[0], ("UniqueID", "PrimaryGroupID")),
                         "dir_users": ids("net_u", half[1], ("UniqueID", "PrimaryGroupID"))},
            "group IDs": {"local_groups": ids("_g", half[0], ("PrimaryGroupID",)),
                          "dir_groups": ids("net_g", half[1], ("PrimaryGroupID",))},
        }
        for label, kwargs in cases.items():
            with self.subTest(label):
                proc, calls, before, after = self.run_account(**kwargs)
                self.assert_refused(proc, calls, before, after)
                self.assertIn("no free", proc.stderr)

    def test_failed_lookup_is_refused_before_any_write(self):
        for rule in (("dscl", ".", "-list", "/Users"), ("dscl", ".", "-list", "/Groups"),
                     ("dscacheutil", "-q", "user", "-a", "name"), ("dscacheutil", "-q", "group", "-a", "gid")):
            with self.subTest(rule=" ".join(rule)):
                self.assert_refused(*self.run_account(fail=[rule]))

    def test_failed_create_stops_without_undoing_or_continuing(self):
        cases = (
            (("dscl", ".", "-create", "/Groups/_manta", "PrimaryGroupID"), "/Groups/_manta", False),
            (("dscl", ".", "-create", "/Users/_manta"), "/Users/_manta", True),
            (("dscl", ".", "-create", "/Users/_manta", "UserShell"), "/Users/_manta", True),
        )
        for rule, failed_target, group_done in cases:
            with self.subTest(rule=" ".join(rule)):
                proc, calls, _, after = self.run_account(fail=[rule])
                self.assertNotEqual(proc.returncode, 0)
                self.assertEqual(proc.stdout, "")
                self.assertIn("-create", proc.stderr)
                writes = self.writes(calls)
                self.assertEqual(writes[-1][:len(rule)], list(rule), "no write after the failed one")
                self.assertEqual(writes[-1][3], failed_target)
                self.assertIn(ACCOUNT, after["groups"])
                if group_done:
                    self.assertEqual(after["groups"][ACCOUNT]["Password"], ["*"])
                else:
                    self.assertNotIn("/Users/_manta", [w[3] for w in writes])
                self.assertNotIn(["id", "-u", ACCOUNT], calls)

    def test_failed_verification_is_reported(self):
        proc, _, _, after = self.run_account(fail=[("id", "-u", ACCOUNT)])
        self.assertNotEqual(proc.returncode, 0)
        self.assertEqual(proc.stdout, "")
        self.assertIn("id _manta", proc.stderr)
        self.assertIn(ACCOUNT, after["users"])

    def test_verification_rejects_a_different_resolved_identity(self):
        for flag in ("-u", "-g"):
            with self.subTest(flag=flag):
                proc, _, _, _ = self.run_account(id_answers={ACCOUNT: {flag: "999"}})
                self.assertNotEqual(proc.returncode, 0)
                self.assertEqual(proc.stdout, "")
                self.assertIn("999", proc.stderr)

    def test_not_root_is_refused_before_touching_the_directory(self):
        proc, calls, before, after = self.run_account(euid=501)
        self.assert_refused(proc, calls, before, after)
        self.assertEqual(calls, [["id", "-u"]])
        self.assertIn("root", proc.stderr)

    def test_writes_target_only_the_service_account(self):
        _, calls, _, _ = self.run_account()
        self.assertEqual({c[3] for c in self.writes(calls)}, {"/Groups/_manta", "/Users/_manta"})



# --- packaging/README.md: the installation guide ------------------------------------------------
# Checks the executable ```sh blocks, never prose: every path an install command copies to must
# be the path the service definition runs from, the macOS account must exist before any plist
# names it, and every relative link must resolve inside an extracted release archive.

GUIDE = os.path.join(ROOT, "packaging", "README.md")
UNIT = os.path.join(ROOT, "packaging", "systemd", "manta.service")
PACKAGE_RELEASE = os.path.join(ROOT, "scripts", "package-release.py")


def sh_blocks(path):
    """Each ```sh fence in `path` as a list of argv lists; comment and blank lines dropped."""
    with open(path, encoding="utf-8") as f:
        text = f.read().replace("\r\n", "\n")
    blocks = []
    for body in re.findall(r"^```sh\n(.*?)^```$", text, flags=re.S | re.M):
        argvs = []
        for line in body.splitlines():
            line = line.strip()
            if line and not line.startswith("#"):
                argvs.append(shlex.split(line))
        blocks.append(argvs)
    return blocks


def install_command(argv):
    """`[sudo] install [-d] [-o O] [-g G] [-m M] operands...` as a dict, or None."""
    if argv[:1] == ["sudo"]:
        argv = argv[1:]
    if argv[:1] != ["install"]:
        return None
    parsed = {"dir": False, "owner": None, "group": None, "mode": None, "operands": []}
    args = iter(argv[1:])
    for arg in args:
        if arg == "-d":
            parsed["dir"] = True
        elif arg in ("-o", "-g", "-m"):
            parsed[{"-o": "owner", "-g": "group", "-m": "mode"}[arg]] = next(args)
        else:
            parsed["operands"].append(arg)
    return parsed


def installs(block):
    return [c for c in (install_command(a) for a in block) if c is not None]


def file_installs(block):
    """{destination: (source, install)} for every non-directory install; a destination ending
    in `/` gets the source's file name, as install(1) does."""
    out = {}
    for c in installs(block):
        if c["dir"]:
            continue
        src, dest = c["operands"]
        if dest.endswith("/"):
            dest += os.path.basename(src)
        out[dest] = (src, c)
    return out


def unit_values(path):
    """A systemd unit as {(section, key): last value}."""
    values, section = {}, None
    with open(path, encoding="utf-8") as f:
        for line in f.read().splitlines():
            line = line.strip()
            if not line or line[0] in "#;":
                continue
            if line.startswith("[") and line.endswith("]"):
                section = line[1:-1]
            elif "=" in line:
                key, value = line.split("=", 1)
                values[(section, key.strip())] = value.strip()
    return values


def block_with(blocks, *words):
    found = [b for b in blocks if any(all(w in a for w in words) for a in b)]
    if not found:
        raise AssertionError("no ```sh block in %s runs %r" % (GUIDE, words))
    return found[0]


class InstallGuideTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.blocks = sh_blocks(GUIDE)
        cls.daemon = load_plist(DAEMON_PLIST)
        cls.rotate = load_plist(ROTATE_PLIST)

    def test_systemd_install_paths_match_the_unit(self):
        block = block_with(self.blocks, "enable", "--now")
        unit = unit_values(UNIT)
        files = file_installs(block)
        exec_start = shlex.split(unit[("Service", "ExecStart")])
        credential_name, credential_path = unit[("Service", "LoadCredential")].split(":", 1)

        self.assertIn(exec_start[0], files, "binary is installed where ExecStart runs it")
        self.assertEqual(files[exec_start[0]][0], "./manta")
        self.assertEqual(exec_start[1:3], ["run", "--config"])
        self.assertEqual(exec_start[3], "${CREDENTIALS_DIRECTORY}/" + credential_name)
        self.assertIn(credential_path, files, "config is installed where LoadCredential reads it")
        self.assertEqual(files[credential_path][0], "manta.toml")
        self.assertEqual(files[credential_path][1]["mode"], "0600")
        unit_dest = "/etc/systemd/system/manta.service"
        self.assertEqual(files[unit_dest][0], os.path.relpath(UNIT, ROOT).replace(os.sep, "/"))
        self.assertIn(["sudo", exec_start[0], "config", "check", "--config", credential_path], block)
        self.assertIn(["sudo", "systemd-analyze", "verify", unit_dest], block)
        self.assertIn(["sudo", "systemctl", "stop", "manta"], block)

    def test_macos_install_paths_match_the_plists(self):
        block = block_with(self.blocks, "bootstrap")
        files = file_installs(block)
        binary, _, _, config = self.daemon["ProgramArguments"]
        user, group = self.daemon["UserName"], self.daemon["GroupName"]

        self.assertEqual(files[binary][0], "./manta")
        self.assertEqual(files[config][0], "manta.toml")
        self.assertEqual((files[config][1]["group"], files[config][1]["mode"]), (group, "0640"))
        self.assertIn(["sudo", "-u", user, binary, "config", "check", "--config", config], block)
        rotate_script = self.rotate["ProgramArguments"][1]
        self.assertEqual(files[rotate_script][0], "packaging/launchd/rotate-log.sh")
        log_dirs = [c for c in installs(block)
                    if c["dir"] and c["operands"] == [os.path.dirname(self.daemon["StandardOutPath"])]]
        self.assertEqual(len(log_dirs), 1, "the log directory is created")
        self.assertEqual((log_dirs[0]["owner"], log_dirs[0]["group"]), (user, group))
        for plist in (DAEMON_PLIST, ROTATE_PLIST):
            name = os.path.basename(plist)
            dest = "/Library/LaunchDaemons/" + name
            self.assertEqual(files[dest][0], "packaging/launchd/" + name)
            self.assertIn(["plutil", "-lint", dest], block)
            self.assertIn(["sudo", "launchctl", "bootstrap", "system", dest], block)
        for label in (self.daemon["Label"], self.rotate["Label"]):
            self.assertIn(["sudo", "launchctl", "bootout", "system/" + label], block)

    def test_account_is_created_before_any_plist_is_installed_or_bootstrapped(self):
        account = ["sudo", "sh", "packaging/launchd/create-service-account.sh"]
        checked = 0
        for block in self.blocks:
            uses = [i for i, a in enumerate(block)
                    if "bootstrap" in a
                    or any(w.endswith(".plist") for w in (install_command(a) or {}).get("operands", []))]
            if not uses:
                continue
            checked += 1
            self.assertIn(account, block)
            self.assertLess(block.index(account), min(uses),
                            "create-service-account.sh runs before %r" % (block[min(uses)],))
        self.assertGreater(checked, 0)

    def test_copy_sources_and_links_resolve_in_the_extracted_archive(self):
        with tempfile.TemporaryDirectory() as tmp:
            binary = os.path.join(tmp, "manta")
            write_bytes(binary, b"fake manta")
            subprocess.run([sys.executable, PACKAGE_RELEASE, "--binary", binary,
                            "--artifact", "manta-test", "--format", "tar.gz", "--out-dir", tmp],
                           check=True, capture_output=True)
            with tarfile.open(os.path.join(tmp, "manta-test.tar.gz")) as tar:
                if hasattr(tarfile, "data_filter"):
                    tar.extractall(tmp, filter="data")
                else:  # Python < 3.11.4; the archive is the one just built above
                    tar.extractall(tmp)
            top = os.path.join(tmp, "manta-test")
            guide = os.path.join(top, "packaging", "README.md")

            with open(guide, encoding="utf-8") as f:
                links = re.findall(r"\]\(([^)\s]+)\)", f.read())
            relative = [l.split("#")[0] for l in links
                        if not re.match(r"[a-z]+:", l) and not l.startswith("#")]
            self.assertTrue(relative)
            for link in relative:
                target = os.path.normpath(os.path.join(os.path.dirname(guide), link))
                self.assertTrue(target.startswith(top + os.sep), link)
                self.assertTrue(os.path.isfile(target), "%s does not ship" % link)

            sources = [a[1] for b in sh_blocks(guide) for a in b if a[:1] == ["cp"]]
            sources += [src for b in sh_blocks(guide) for src, _ in file_installs(b).values()]
            for src in sources:
                if src in ("./manta", "manta.toml"):
                    continue  # the binary and the operator's own edited copy
                self.assertTrue(os.path.isfile(os.path.join(top, src)), "%s does not ship" % src)


class GitignoreTests(unittest.TestCase):
    def ignored(self, path):
        proc = subprocess.run(["git", "check-ignore", "--no-index", "-q", path], cwd=ROOT)
        self.assertIn(proc.returncode, (0, 1), "git check-ignore failed")
        return proc.returncode == 0

    def test_only_the_root_operator_config_is_ignored(self):
        self.assertTrue(self.ignored("manta.toml"))
        self.assertFalse(self.ignored("manta.example.toml"))
        self.assertFalse(self.ignored("docs/RUNBOOKS/field-node/manta.toml"))
        self.assertFalse(self.ignored("packaging/manta.toml"))


if __name__ == "__main__":
    unittest.main()
